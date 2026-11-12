//! 実行中の session ID に一致する記録だけから、モデルと effort の実効値を読む。
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Configuration {
    pub provider: Option<Provider>,
    pub settings: Settings,
    pub session_roots: Vec<PathBuf>,
    #[serde(default)]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn run(
    config: Configuration,
    reports: PathBuf,
    markers: PathBuf,
    events: mpsc::Sender<super::Event>,
) {
    let mut monitor = Monitor::new(config, reports, markers);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut previous = None;
    loop {
        tick.tick().await;
        // セッションの探索や大きな記録の読み取りで、停止要求と画面の更新を待たせない。
        let Ok((next, settings)) = tokio::task::spawn_blocking(move || {
            let settings = monitor.poll();
            (monitor, settings)
        })
        .await
        else {
            return;
        };
        monitor = next;
        if previous.as_ref() != Some(&settings) {
            previous = Some(settings.clone());
            if events
                .send(super::Event::Inference(settings))
                .await
                .is_err()
            {
                return;
            }
        } else if events.is_closed() {
            return;
        }
    }
}

struct Monitor {
    config: Configuration,
    reports: PathBuf,
    markers: PathBuf,
    probes: BTreeMap<PathBuf, Probe>,
}

impl Monitor {
    fn new(config: Configuration, reports: PathBuf, markers: PathBuf) -> Self {
        Self {
            config,
            reports,
            markers,
            probes: BTreeMap::new(),
        }
    }

    fn poll(&mut self) -> Settings {
        let Some(provider) = self.config.provider else {
            return self.config.settings.clone();
        };
        if let Ok(entries) = fs::read_dir(&self.reports) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(task) = task_number(&path) else {
                    continue;
                };
                if path.extension().is_some_and(|ext| ext == "jsonl") {
                    self.probes.entry(path).or_insert_with(|| {
                        let mut probe = Probe::new(task);
                        probe.since = self.config.started_at;
                        probe
                    });
                }
            }
        }
        for (path, probe) in &mut self.probes {
            probe.poll(path, provider, &self.config.session_roots);
        }
        let active: BTreeSet<usize> = fs::read_dir(&self.markers)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("worker-task-")
            })
            .filter_map(|entry| fs::read_to_string(entry.path()).ok()?.parse().ok())
            .collect();
        let mut tasks = BTreeMap::new();
        for (path, probe) in &self.probes {
            if !active.is_empty() && !active.contains(&probe.task) {
                continue;
            }
            // 再開不能から fresh 起動した場合、旧セッションの値を表示し続けない。
            let fresh = path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".fresh.jsonl"));
            let entry = tasks.entry(probe.task).or_insert((false, &probe.settings));
            if fresh {
                *entry = (true, &probe.settings);
            }
        }
        let mut models = BTreeSet::new();
        let mut efforts = BTreeSet::new();
        for (_, settings) in tasks.values() {
            if let Some(value) = settings
                .model
                .as_ref()
                .or(self.config.settings.model.as_ref())
            {
                models.insert(value.clone());
            }
            if let Some(value) = settings
                .effort
                .as_ref()
                .or(self.config.settings.effort.as_ref())
            {
                efforts.insert(value.clone());
            }
        }
        Settings {
            model: joined(models).or_else(|| self.config.settings.model.clone()),
            effort: joined(efforts).or_else(|| self.config.settings.effort.clone()),
        }
    }
}

fn joined(values: BTreeSet<String>) -> Option<String> {
    (!values.is_empty()).then(|| values.into_iter().collect::<Vec<_>>().join(", "))
}

fn task_number(path: &Path) -> Option<usize> {
    path.file_name()?.to_str()?.split_once('_')?.0.parse().ok()
}

struct Probe {
    task: usize,
    raw: JsonTail,
    session: Option<String>,
    transcript: Option<(PathBuf, JsonTail)>,
    next_lookup: Option<Instant>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    settings: Settings,
}

impl Probe {
    fn new(task: usize) -> Self {
        Self {
            task,
            raw: JsonTail::default(),
            session: None,
            transcript: None,
            next_lookup: None,
            since: None,
            settings: Settings::default(),
        }
    }

    fn poll(&mut self, path: &Path, provider: Provider, roots: &[PathBuf]) {
        if self.since.is_none() {
            self.since = fs::metadata(path)
                .ok()
                .and_then(|meta| meta.created().ok())
                .map(|time| {
                    chrono::DateTime::<chrono::Utc>::from(time) - chrono::Duration::seconds(1)
                });
        }
        // 起動イベントが見つかったら、大きくなり続ける生ログの再走査は不要。
        let values = if self.session.is_none() {
            self.raw.read(path)
        } else {
            Vec::new()
        };
        for value in values {
            let session = match provider {
                Provider::Claude if value["type"] == "system" && value["subtype"] == "init" => {
                    self.settings.model = string(&value["model"]).or(self.settings.model.take());
                    value["session_id"].as_str()
                }
                Provider::Codex if value["type"] == "thread.started" => value["thread_id"].as_str(),
                _ => None,
            };
            if let Some(session) = session.filter(|id| valid_session_id(id))
                && self.session.as_deref() != Some(session)
            {
                self.session = Some(session.into());
                self.transcript = None;
                self.next_lookup = None;
                self.settings.effort = None;
            }
            if provider == Provider::Claude {
                observe_claude(&value, &mut self.settings);
            }
        }
        if self.transcript.is_none() && self.next_lookup.is_none_or(|next| Instant::now() >= next) {
            if let Some(session) = &self.session {
                self.transcript = find_transcript(roots, provider, session).map(|path| {
                    let tail = JsonTail::recent(&path);
                    (path, tail)
                });
            }
            self.next_lookup = Some(Instant::now() + Duration::from_secs(5));
        }
        if let Some((path, tail)) = &mut self.transcript {
            for value in tail.read(path) {
                // resume で同じファイルへ追記されても、前回実行の effort を使わない。
                if let (Some(since), Some(timestamp)) = (self.since, value["timestamp"].as_str())
                    && chrono::DateTime::parse_from_rfc3339(timestamp)
                        .is_ok_and(|time| time < since)
                {
                    continue;
                }
                match provider {
                    Provider::Claude => observe_claude(&value, &mut self.settings),
                    Provider::Codex if value["type"] == "turn_context" => {
                        let payload = &value["payload"];
                        self.settings.model =
                            string(&payload["model"]).or(self.settings.model.take());
                        self.settings.effort =
                            string(&payload["effort"]).or(self.settings.effort.take());
                    }
                    _ => {}
                }
            }
        }
    }
}

fn string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(|s| crate::format_stream::util::truncate_inline(s, 160))
}

fn observe_claude(value: &Value, settings: &mut Settings) {
    if value["type"] != "assistant"
        || !value["parent_tool_use_id"].is_null()
        || value["isSidechain"] == true
    {
        return;
    }
    settings.model = string(&value["message"]["model"]).or(settings.model.take());
    settings.effort = string(&value["perTurnEffort"])
        .or_else(|| string(&value["effort"]))
        .or(settings.effort.take());
}

fn valid_session_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn find_transcript(roots: &[PathBuf], provider: Provider, session: &str) -> Option<PathBuf> {
    let mut found = BTreeSet::new();
    for root in roots {
        match provider {
            Provider::Claude => {
                for entry in fs::read_dir(root.join("projects"))
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let path = entry.path().join(format!("{session}.jsonl"));
                    if path.is_file()
                        && let Ok(path) = path.canonicalize()
                    {
                        found.insert(path);
                    }
                }
            }
            Provider::Codex => {
                // Codex の UUIDv7 は作成日時を含む。通常はその日だけを探せばよい。
                let date = session
                    .as_bytes()
                    .get(14)
                    .filter(|b| **b == b'7')
                    .and_then(|_| u64::from_str_radix(&session[..13].replace('-', ""), 16).ok())
                    .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64));
                if let Some(date) = date {
                    // rollout の日付は CLI 側のローカル時刻。UTC と日付が違う地域も探す。
                    for delta in [-1, 0, 1] {
                        let date = date + chrono::Duration::days(delta);
                        find_rollout(
                            &root
                                .join("sessions")
                                .join(date.format("%Y/%m/%d").to_string()),
                            session,
                            0,
                            &mut found,
                        );
                    }
                } else {
                    find_rollout(&root.join("sessions"), session, 3, &mut found);
                }
                find_rollout(&root.join("archived_sessions"), session, 0, &mut found);
            }
        }
    }
    // 同じ ID の記録が複製されていたら、どちらが実行中かを推測しない。
    (found.len() == 1)
        .then(|| found.into_iter().next())
        .flatten()
}

fn find_rollout(directory: &Path, session: &str, depth: u8, found: &mut BTreeSet<PathBuf>) {
    let suffix = format!("-{session}.jsonl");
    for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().ends_with(&suffix) {
            if let Ok(path) = path.canonicalize() {
                found.insert(path);
            }
        } else if depth > 0 && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            find_rollout(&path, session, depth - 1, found);
        }
    }
}

#[derive(Default)]
struct JsonTail {
    offset: u64,
    pending: Vec<u8>,
    skipping: bool,
}

impl JsonTail {
    fn recent(path: &Path) -> Self {
        let offset = fs::metadata(path).map_or(0, |meta| meta.len().saturating_sub(1024 * 1024));
        Self {
            offset,
            skipping: offset > 0,
            ..Default::default()
        }
    }

    fn read(&mut self, path: &Path) -> Vec<Value> {
        const BUDGET: u64 = 1024 * 1024;
        const MAX_LINE: usize = 4 * 1024 * 1024;
        let Ok(mut file) = File::open(path) else {
            return Vec::new();
        };
        if file.metadata().is_ok_and(|meta| meta.len() < self.offset) {
            *self = Self::default();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if file.take(BUDGET).read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        self.offset += bytes.len() as u64;
        let mut values = Vec::new();
        for part in bytes.split_inclusive(|b| *b == b'\n') {
            if !self.skipping && self.pending.len() + part.len() <= MAX_LINE {
                self.pending.extend_from_slice(part);
            } else {
                self.pending.clear();
                self.skipping = true;
            }
            if part.last() == Some(&b'\n') {
                if !self.skipping
                    && let Ok(value) = serde_json::from_slice(&self.pending)
                {
                    values.push(value);
                }
                self.pending.clear();
                self.skipping = false;
            }
        }
        values
    }
}
