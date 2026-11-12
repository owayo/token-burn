//! ディスクの空き容量。
//!
//! ai-usage 連携が有効なら `ai-usage --disk --json` を使う（Finder と同じ基準の
//! available / purgeable / free。設定の `[tui].disk_mounts` のボリュームも出る）。使用量の
//! キャッシュとは別に、30 秒ごとに読む。ai-usage が `--disk` に対応していない（古い版）
//! ときと、連携しない構成では、起動ディスクを OS の statvfs で読む。その場合、パージ可能な
//! 領域は分からないので出さない（Finder の「利用可能」と同じ値だと誤解させない）。

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::mpsc;

use super::Event;
use super::process::Runner;

/// 取得が終わってから次の取得を始めるまでの間隔（ai-usage の TUI と同じ）。
pub const INTERVAL: Duration = Duration::from_secs(30);
/// `ai-usage --disk --json` の期限。ai-usage 自身が戻らないボリュームを 2 秒で打ち切るので、
/// 通常はこれより十分早く終わる。
const TIMEOUT: Duration = Duration::from_secs(10);
/// 対応している `--disk --json` の版。
const SCHEMA_VERSION: u32 = 1;

/// 1 ボリュームの容量（バイト）。
#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    pub name: String,
    pub total: u64,
    /// 今すぐ書き込める空き。
    pub free: u64,
    /// Finder の「利用可能」（パージ可能な領域を含む）。分からなければ free と同じ。
    pub available: u64,
    /// available − free。分からなければ None。
    pub purgeable: Option<u64>,
}

impl Volume {
    /// 使用中の容量。パージ可能な領域は含めない（Finder と同じ）。
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.used() as f64 * 100.0 / self.total as f64
    }
}

/// 読み取りの結果。
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Ok,
    NotMounted,
    NotLocal,
    Timeout,
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub target: String,
    pub status: Status,
    pub volume: Option<Volume>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    AiUsage,
    Native,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    Readings {
        source: Source,
        readings: Vec<Reading>,
    },
    /// 取得そのものが失敗した（期限切れ・異常終了）。直前の値を経過時間付きで残す。
    Failed,
}

/// 表示用の 1 ボリューム。
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub target: String,
    /// 最後に読めた値と、それを反映した時刻。
    pub last: Option<(Volume, Instant)>,
    pub status: Status,
}

impl Row {
    /// 表示中の値が直近の取得のものではない（失敗・応答なしのため古い値を出している）。
    pub fn is_stale(&self) -> bool {
        self.status != Status::Ok && self.last.is_some()
    }
}

/// 表示の状態。取得の結果を順に反映する。
#[derive(Debug, Default)]
pub struct DiskState {
    pub source: Option<Source>,
    pub rows: Vec<Row>,
}

impl DiskState {
    pub fn apply(&mut self, update: Update, now: Instant) {
        match update {
            Update::Readings { source, readings } => {
                self.source = Some(source);
                let previous = std::mem::take(&mut self.rows);
                self.rows = readings
                    .into_iter()
                    .map(|reading| {
                        let last = match (&reading.status, reading.volume) {
                            (Status::Ok, Some(volume)) => Some((volume, now)),
                            // 取り外したディスクやネットワークボリュームの古い値は出さない。
                            (Status::NotMounted | Status::NotLocal, _) => None,
                            // 失敗・応答なしは、前回の値を経過時間付きで残す。
                            _ => previous
                                .iter()
                                .find(|row| row.target == reading.target)
                                .and_then(|row| row.last.clone()),
                        };
                        Row {
                            target: reading.target,
                            last,
                            status: reading.status,
                        }
                    })
                    .collect();
            }
            Update::Failed => {
                for row in &mut self.rows {
                    if row.status == Status::Ok {
                        row.status = Status::Timeout;
                    }
                }
            }
        }
    }
}

pub async fn run(ai_usage_disk: Option<String>, events: mpsc::Sender<Event>) {
    let mut runner = Runner::default();
    let mut command = ai_usage_disk.filter(|command| !command.is_empty());
    let mut native = NativeReader::default();
    loop {
        let update = match &command {
            Some(cmd) => match runner.run(cmd, TIMEOUT).await {
                Ok(output) if output.status.success() => match parse(&output.stdout) {
                    Some(readings) => Update::Readings {
                        source: Source::AiUsage,
                        readings,
                    },
                    // 知らない版の出力。以後は OS から読む。
                    None => {
                        command = None;
                        continue;
                    }
                },
                // clap の使い方の誤り（終了コード 2）は `--disk` を知らない古い ai-usage。
                // 何度試しても変わらないので、以後は OS から読む。
                Ok(output) if output.status.code() == Some(2) => {
                    command = None;
                    continue;
                }
                Ok(_) | Err(_) => Update::Failed,
            },
            None => native.read().await,
        };
        if events.send(Event::Disk(update)).await.is_err() {
            return;
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

#[derive(Deserialize)]
struct DiskJson {
    schema_version: u32,
    volumes: Vec<VolumeJson>,
}

#[derive(Deserialize)]
struct VolumeJson {
    target: String,
    status: String,
    name: Option<String>,
    total_bytes: Option<u64>,
    free_bytes: Option<u64>,
    available_bytes: Option<u64>,
    purgeable_bytes: Option<u64>,
    error: Option<String>,
}

/// `ai-usage --disk --json` を読む。対応していない版・形なら None。
fn parse(bytes: &[u8]) -> Option<Vec<Reading>> {
    let json: DiskJson = serde_json::from_slice(bytes).ok()?;
    if json.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(json.volumes.into_iter().map(reading_from_json).collect())
}

fn reading_from_json(volume: VolumeJson) -> Reading {
    let target = super::ansi::strip(&volume.target);
    let status = match volume.status.as_str() {
        "ok" => Status::Ok,
        "not_mounted" => Status::NotMounted,
        "not_local" => Status::NotLocal,
        "timeout" => Status::Timeout,
        _ => Status::Error(
            volume
                .error
                .as_deref()
                .map(super::ansi::strip)
                .unwrap_or_else(|| volume.status.clone()),
        ),
    };
    if status != Status::Ok {
        return Reading {
            target,
            status,
            volume: None,
        };
    }
    let checked = match (
        volume.total_bytes,
        volume.free_bytes,
        volume.available_bytes,
    ) {
        (Some(total), Some(free), Some(available))
            if total > 0 && free <= total && free <= available && available <= total =>
        {
            Some(Volume {
                // ボリューム名は利用者が付けるので、端末を崩す文字を除く。
                name: volume
                    .name
                    .as_deref()
                    .map(super::ansi::strip)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| target.clone()),
                total,
                free,
                available,
                purgeable: volume.purgeable_bytes,
            })
        }
        _ => None,
    };
    match checked {
        Some(volume) => Reading {
            target,
            status: Status::Ok,
            volume: Some(volume),
        },
        None => Reading {
            target,
            status: Status::Error("inconsistent capacity values".to_string()),
            volume: None,
        },
    }
}

/// 起動ディスクを statvfs で読む。戻らない読み取りがあっても、読み取りのスレッドを
/// 増やさない（前回の読み取りが終わるまで次を始めない）。
#[derive(Default)]
struct NativeReader {
    pending: Option<tokio::task::JoinHandle<Reading>>,
}

impl NativeReader {
    async fn read(&mut self) -> Update {
        let mut task = match self.pending.take() {
            Some(task) => task,
            None => tokio::task::spawn_blocking(|| read_statvfs("/")),
        };
        match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
            Ok(Ok(reading)) => Update::Readings {
                source: Source::Native,
                readings: vec![reading],
            },
            Ok(Err(_)) => Update::Failed,
            Err(_) => {
                self.pending = Some(task);
                Update::Failed
            }
        }
    }
}

#[cfg(unix)]
fn read_statvfs(path: &str) -> Reading {
    let target = path.to_string();
    let failed = |message: String| Reading {
        target: target.clone(),
        status: Status::Error(message),
        volume: None,
    };
    let Ok(c_path) = std::ffi::CString::new(path) else {
        return failed("the path contains a NUL byte".to_string());
    };
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: c_path は NUL 終端の文字列で、stat は statvfs が書き込む領域。戻り値が 0 の
    // ときだけ初期化済みとして読む。
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return failed(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: statvfs が成功したので初期化済み。
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::unnecessary_cast)] // フィールドの型は OS ごとに違う
    let (blocks, available_blocks, fragment) = (
        stat.f_blocks as u64,
        stat.f_bavail as u64,
        stat.f_frsize as u64,
    );
    let total = blocks.saturating_mul(fragment);
    // 一般ユーザーが書ける空き（root 用の予約を除く）。
    let free = available_blocks.saturating_mul(fragment).min(total);
    if total == 0 {
        return failed("no capacity reported".to_string());
    }
    Reading {
        target: target.clone(),
        status: Status::Ok,
        volume: Some(Volume {
            name: target,
            total,
            free,
            available: free,
            purgeable: None,
        }),
    }
}

#[cfg(windows)]
fn read_statvfs(_path: &str) -> Reading {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let path = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("C:/"));
    let target = path.to_string_lossy().into_owned();
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut available, mut total, mut free) = (0, 0, 0);
    // The NUL-terminated path and output counters remain valid throughout the call.
    let result =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };
    if result == 0 || total == 0 {
        return Reading {
            target,
            status: Status::Error(std::io::Error::last_os_error().to_string()),
            volume: None,
        };
    }
    Reading {
        target: target.clone(),
        status: Status::Ok,
        volume: Some(Volume {
            name: target,
            total,
            free: available.min(total),
            available: available.min(total),
            purgeable: None,
        }),
    }
}

#[cfg(not(any(unix, windows)))]
fn read_statvfs(path: &str) -> Reading {
    Reading {
        target: path.to_string(),
        status: Status::Error("disk usage is not available on this platform".to_string()),
        volume: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    fn startup(available: u64) -> Volume {
        Volume {
            name: "Macintosh HD".into(),
            total: 500 * GB,
            free: 13 * GB,
            available,
            purgeable: Some(available - 13 * GB),
        }
    }

    #[test]
    fn parses_every_status_of_the_schema() {
        let json = r#"{
          "schema_version": 1,
          "generated_at": "2026-10-22T13:19:01+00:00",
          "volumes": [
            {"target": "/", "status": "ok", "name": "Macintosh HD",
             "total_bytes": 500000000000, "free_bytes": 13000000000,
             "available_bytes": 70000000000, "purgeable_bytes": 57000000000,
             "used_percent": 86.0, "capacity_basis": "important_usage", "error": null},
            {"target": "/Volumes/Backup", "status": "ok", "name": "Backup",
             "total_bytes": 100, "free_bytes": 40, "available_bytes": 40, "purgeable_bytes": null,
             "used_percent": 60.0, "capacity_basis": "free_space", "error": null},
            {"target": "/Volumes/Archive", "status": "not_mounted", "name": null,
             "total_bytes": null, "free_bytes": null, "available_bytes": null, "purgeable_bytes": null,
             "used_percent": null, "capacity_basis": null, "error": null},
            {"target": "/Volumes/Share", "status": "not_local", "name": null,
             "total_bytes": null, "free_bytes": null, "available_bytes": null, "purgeable_bytes": null,
             "used_percent": null, "capacity_basis": null, "error": null},
            {"target": "/Volumes/Slow", "status": "timeout", "name": null,
             "total_bytes": null, "free_bytes": null, "available_bytes": null, "purgeable_bytes": null,
             "used_percent": null, "capacity_basis": null, "error": null},
            {"target": "/Volumes/Bad", "status": "error", "name": null,
             "total_bytes": null, "free_bytes": null, "available_bytes": null, "purgeable_bytes": null,
             "used_percent": null, "capacity_basis": null, "error": "I/O error"}
          ]
        }"#;
        let readings = parse(json.as_bytes()).unwrap();
        assert_eq!(readings[0].volume, Some(startup(70 * GB)));
        assert_eq!(readings[1].volume.as_ref().unwrap().purgeable, None);
        let statuses: Vec<&Status> = readings.iter().map(|reading| &reading.status).collect();
        assert_eq!(
            statuses,
            [
                &Status::Ok,
                &Status::Ok,
                &Status::NotMounted,
                &Status::NotLocal,
                &Status::Timeout,
                &Status::Error("I/O error".into())
            ]
        );
    }

    #[test]
    fn rejects_unknown_versions_and_inconsistent_values() {
        assert!(parse(br#"{"schema_version": 2, "volumes": []}"#).is_none());
        assert!(parse(b"error: unexpected argument '--disk' found").is_none());
        let json = r#"{"schema_version": 1, "volumes": [
            {"target": "/", "status": "ok", "name": "X", "total_bytes": 10, "free_bytes": 20,
             "available_bytes": 20, "purgeable_bytes": null, "error": null}]}"#;
        let readings = parse(json.as_bytes()).unwrap();
        assert!(matches!(readings[0].status, Status::Error(_)));
        assert_eq!(readings[0].volume, None);
    }

    #[test]
    fn failures_keep_the_last_values_but_unmounted_disks_drop_them() {
        let t0 = Instant::now();
        let mut state = DiskState::default();
        let ok = |target: &str| Reading {
            target: target.into(),
            status: Status::Ok,
            volume: Some(startup(70 * GB)),
        };
        state.apply(
            Update::Readings {
                source: Source::AiUsage,
                readings: vec![ok("/"), ok("/Volumes/Archive")],
            },
            t0,
        );
        // 取得そのものの失敗: 値は残り、古いことが分かる。
        state.apply(Update::Failed, t0 + Duration::from_secs(30));
        assert!(state.rows.iter().all(Row::is_stale));
        // 個別の応答なしは値を残し、取り外しは値を消す。
        state.apply(
            Update::Readings {
                source: Source::AiUsage,
                readings: vec![
                    Reading {
                        target: "/".into(),
                        status: Status::Timeout,
                        volume: None,
                    },
                    Reading {
                        target: "/Volumes/Archive".into(),
                        status: Status::NotMounted,
                        volume: None,
                    },
                ],
            },
            t0 + Duration::from_secs(60),
        );
        assert_eq!(state.rows[0].last.as_ref().unwrap().1, t0);
        assert!(state.rows[0].is_stale());
        assert_eq!(state.rows[1].last, None);
        assert!(!state.rows[1].is_stale());
    }

    #[cfg(unix)]
    #[test]
    fn reads_the_root_volume_with_statvfs() {
        let reading = read_statvfs("/");
        assert_eq!(reading.status, Status::Ok);
        let volume = reading.volume.unwrap();
        assert!(volume.total > 0);
        assert!(volume.free <= volume.total);
        assert_eq!(volume.purgeable, None);
    }

    #[tokio::test]
    async fn falls_back_to_statvfs_when_ai_usage_does_not_know_disk() {
        // 古い ai-usage は未知の引数で終了コード 2 を返す。以後は OS から読む。
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(run(
            Some("echo \"error: unexpected argument '--disk' found\" >&2; exit 2".into()),
            tx,
        ));
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        let Event::Disk(Update::Readings { source, readings }) = event else {
            panic!("unexpected event");
        };
        assert_eq!(source, Source::Native);
        assert_eq!(readings.len(), 1);
    }

    #[tokio::test]
    async fn a_transient_failure_keeps_using_ai_usage() {
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(run(Some("exit 1".into()), tx));
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(matches!(event, Event::Disk(Update::Failed)));
    }
}
