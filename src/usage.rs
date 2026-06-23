use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::config::{
    Config, RuntimeAgent, RuntimeAiUsage, StateWindowPolicy, UsageFallback, UsageWindowPolicy,
};
use crate::schedule::{AgentSchedule, ScheduleSource, UsageWindow, calculate_fixed_reset};

/// ai-usage --json のトップレベル出力。
#[derive(Debug, Deserialize)]
struct AiUsageOutput {
    #[serde(default)]
    accounts: Vec<AiUsageAccount>,
}

/// ai-usage --json の 1 アカウント（profile × provider）。
#[derive(Debug, Deserialize, Clone)]
struct AiUsageAccount {
    profile: String,
    provider: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    weekly: Option<UsageWindowData>,
    #[serde(default)]
    five_hour: Option<UsageWindowData>,
    #[serde(default)]
    error: Option<String>,
}

/// 各枠（weekly / five_hour）のリセット情報。
#[derive(Debug, Deserialize, Clone)]
struct UsageWindowData {
    resets_at: Option<String>,
    /// 使用率（%）。usage-gate の閾値判定に使う。
    used_percent: Option<f64>,
}

/// ai-usage --json の取得結果スナップショット。
#[derive(Debug, Clone)]
pub struct AiUsageSnapshot {
    accounts: Vec<AiUsageAccount>,
}

impl AiUsageSnapshot {
    /// (profile, provider) に一致するアカウントを探す。
    /// claude/codex は (profile, provider) で一意。antigravity は group_label 違いで
    /// 複数行になりうるが、その場合は最初の一致を採用する。
    fn find(&self, profile: &str, provider: &str) -> Option<&AiUsageAccount> {
        self.accounts
            .iter()
            .find(|a| a.profile == profile && a.provider == provider)
    }
}

/// ai-usage 連携の状態。
#[derive(Debug, Clone)]
pub enum UsageState {
    /// 連携無効（[ai_usage] 未設定 or enabled=false）。
    Disabled,
    /// 取得成功。
    Loaded(AiUsageSnapshot),
    /// 取得失敗（理由）。
    Failed(String),
}

/// スケジュール解決器。ai-usage --json を 1 プロセス 1 回だけ取得し、
/// RuntimeAgent ごとに ai-usage / fixed を統合してスケジュールを返す。
pub struct ScheduleResolver {
    usage: UsageState,
    state_window: StateWindowPolicy,
}

impl ScheduleResolver {
    /// ai-usage --json を（有効なら）1 回だけ実行してリゾルバを構築する。
    pub async fn load(config: &Config) -> Self {
        let Some(global) = config.ai_usage.as_ref() else {
            return Self {
                usage: UsageState::Disabled,
                state_window: StateWindowPolicy::Weekly,
            };
        };
        if !global.enabled {
            return Self {
                usage: UsageState::Disabled,
                state_window: global.state_window,
            };
        }
        let usage = match run_ai_usage(&global.command).await {
            Ok(snapshot) => UsageState::Loaded(snapshot),
            Err(e) => UsageState::Failed(e.to_string()),
        };
        Self {
            usage,
            state_window: global.state_window,
        }
    }

    /// ai-usage の取得に失敗したか（status 表示用）。
    pub fn failure(&self) -> Option<&str> {
        match &self.usage {
            UsageState::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// RuntimeAgent のスケジュールを解決する。
    /// `Ok(None)` は fallback=skip により候補から外れたことを表す。
    pub fn schedule_for(&self, agent: &RuntimeAgent) -> Result<Option<AgentSchedule>> {
        match &agent.ai_usage {
            None => Ok(Some(calculate_fixed_reset(agent)?)),
            Some(rt) => {
                let account = match &self.usage {
                    UsageState::Loaded(s) => s.find(&rt.profile, &rt.provider),
                    _ => None,
                };
                match account {
                    Some(acc) if acc.ok => match self.build_from_account(agent, acc) {
                        Ok(Some(sched)) => Ok(Some(sched)),
                        Ok(None) => self.fallback(agent, "ai-usage has no usable reset window"),
                        Err(e) => self.fallback(agent, &e.to_string()),
                    },
                    Some(acc) => {
                        let reason = acc
                            .error
                            .clone()
                            .unwrap_or_else(|| "ai-usage account not ok".to_string());
                        self.fallback(agent, &reason)
                    }
                    None => self.fallback(agent, &self.miss_reason(rt)),
                }
            }
        }
    }

    /// 全 RuntimeAgent のうち、最も近いリセットのものを選ぶ。
    pub fn select_nearest(&self, agents: &[RuntimeAgent]) -> Result<(usize, AgentSchedule)> {
        anyhow::ensure!(!agents.is_empty(), "No agents configured");
        let mut best: Option<(usize, AgentSchedule)> = None;
        for (i, agent) in agents.iter().enumerate() {
            if let Some(sched) = self.schedule_for(agent)? {
                let replace = match &best {
                    None => true,
                    Some((_, b)) => sched.time_until_reset < b.time_until_reset,
                };
                if replace {
                    best = Some((i, sched));
                }
            }
        }
        best.ok_or_else(|| {
            anyhow::anyhow!("No agent could be scheduled (all skipped via fallback=skip)")
        })
    }

    fn miss_reason(&self, rt: &RuntimeAiUsage) -> String {
        match &self.usage {
            UsageState::Failed(e) => format!("ai-usage command failed: {e}"),
            UsageState::Disabled => "ai-usage disabled".to_string(),
            UsageState::Loaded(_) => {
                format!("no ai-usage entry for ({}, {})", rt.profile, rt.provider)
            }
        }
    }

    fn fallback(&self, agent: &RuntimeAgent, reason: &str) -> Result<Option<AgentSchedule>> {
        match agent.fallback {
            UsageFallback::Fixed => {
                let mut sched = calculate_fixed_reset(agent).with_context(|| {
                    format!(
                        "ai-usage failed for '{}' ({}) and fixed fallback is unavailable",
                        agent.name, reason
                    )
                })?;
                sched.source = ScheduleSource::FixedFallback(reason.to_string());
                Ok(Some(sched))
            }
            UsageFallback::Skip => Ok(None),
            UsageFallback::Error => {
                anyhow::bail!(
                    "ai-usage resolution failed for '{}': {}",
                    agent.name,
                    reason
                )
            }
        }
    }

    fn build_from_account(
        &self,
        agent: &RuntimeAgent,
        acc: &AiUsageAccount,
    ) -> Result<Option<AgentSchedule>> {
        let chosen = match agent.window {
            UsageWindowPolicy::Weekly => acc.weekly.as_ref().map(|w| (UsageWindow::Weekly, w)),
            UsageWindowPolicy::FiveHour => {
                acc.five_hour.as_ref().map(|w| (UsageWindow::FiveHour, w))
            }
            UsageWindowPolicy::Nearest => nearest_window(acc),
        };
        let Some((window, data)) = chosen else {
            return Ok(None);
        };
        let Some(next_reset) = parse_resets_at(data) else {
            return Ok(None);
        };

        // state_cutoff の枠を決める。state_window=weekly なら weekly 基準を優先し、
        // weekly が無い場合だけ選択枠に落とす。deadline が five_hour 由来でも、
        // 処理済みカットオフは週次基準を保つのが安全。
        let (cutoff_anchor, cutoff_window) = match self.state_window {
            StateWindowPolicy::Weekly => acc
                .weekly
                .as_ref()
                .and_then(parse_resets_at)
                .map(|w| (w, UsageWindow::Weekly))
                .unwrap_or((next_reset, window)),
            StateWindowPolicy::Selected => (next_reset, window),
        };
        let state_cutoff = cutoff_anchor - cutoff_window.period();

        let time_until_reset = (next_reset.with_timezone(&Utc) - Utc::now())
            .to_std()
            .unwrap_or(Duration::from_secs(0));

        Ok(Some(AgentSchedule {
            agent_name: agent.name.clone(),
            next_reset,
            state_cutoff,
            time_until_reset,
            source: ScheduleSource::AiUsage(window),
        }))
    }
}

/// weekly / five_hour のうち、resets_at が最も近い方を選ぶ。
/// deadline 算出に使う resets_at と同じ基準で選択し、選択枠と締切の不整合を防ぐ
/// （resets_in_seconds は resets_at と独立に欠損し得るため選択基準には使わない）。
/// resets_at がパースできない枠は候補から除外する（採用すると build_from_account が
/// 丸ごと None を返し、スケジュール解決全体が失敗してしまうため）。
fn nearest_window(acc: &AiUsageAccount) -> Option<(UsageWindow, &UsageWindowData)> {
    [
        acc.weekly.as_ref().map(|w| (UsageWindow::Weekly, w)),
        acc.five_hour.as_ref().map(|w| (UsageWindow::FiveHour, w)),
    ]
    .into_iter()
    .flatten()
    .filter_map(|(win, w)| parse_resets_at(w).map(|r| (win, w, r)))
    .min_by_key(|(_, _, r)| r.with_timezone(&Utc))
    .map(|(win, w, _)| (win, w))
}

fn parse_resets_at(data: &UsageWindowData) -> Option<DateTime<FixedOffset>> {
    let s = data.resets_at.as_deref()?;
    DateTime::parse_from_rfc3339(s).ok()
}

async fn run_ai_usage(command: &[String]) -> Result<AiUsageSnapshot> {
    anyhow::ensure!(!command.is_empty(), "ai_usage.command is empty");
    let output = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .output()
        .await
        .with_context(|| format!("failed to run ai-usage command: {}", command.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ai-usage exited with {}: {}", output.status, stderr.trim());
    }
    let parsed: AiUsageOutput =
        serde_json::from_slice(&output.stdout).context("failed to parse ai-usage --json output")?;
    Ok(AiUsageSnapshot {
        accounts: parsed.accounts,
    })
}

/// 使用率ゲート: `(profile, provider)` の weekly / five_hour 使用率のうち、いずれかが
/// `threshold`（%）以上なら `stop_file` を作成し、後続タスクの実行を止める。
///
/// - ai-usage の取得に失敗したときは fail-closed（使用率を確認できない以上、安全側で停止）。
/// - 該当エントリが無い / 使用率欠損のときは過剰停止を避けて続行する。
/// - `stop_file` の作成は `create_new` で冪等（並列 worker から同時に呼ばれても安全）。
/// - `cache_file` に短 TTL で ai-usage 出力をキャッシュし、並列実行時の重複取得を抑える。
pub async fn run_usage_gate(
    profile: &str,
    provider: &str,
    threshold: u8,
    stop_file: &Path,
    cache_file: &Path,
    command: &[String],
) -> Result<()> {
    // 既に停止シグナルがあれば何もしない。
    if stop_file.exists() {
        return Ok(());
    }

    let snapshot = match load_usage_cached(command, cache_file).await {
        Ok(s) => s,
        Err(e) => {
            if write_stop_file(stop_file, &format!("usage-gate failed: {e}")) {
                println!(
                    "\x1b[31m  \u{26d4} usage-gate: ai-usage を確認できないため停止 ({e})\x1b[0m"
                );
            }
            return Ok(());
        }
    };

    let Some(acc) = snapshot.find(profile, provider) else {
        eprintln!("usage-gate: ({profile}, {provider}) のエントリが無いため続行します");
        return Ok(());
    };

    // weekly / five_hour のうち最大の使用率で判定する（安全側）。
    let max_used = [acc.weekly.as_ref(), acc.five_hour.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|w| w.used_percent)
        .fold(None::<f64>, |max, p| Some(max.map_or(p, |m| m.max(p))));

    if let Some(used) = max_used
        && used >= threshold as f64
        && write_stop_file(
            stop_file,
            &format!("usage {used:.0}% >= threshold {threshold}%"),
        )
    {
        println!(
            "\x1b[31m  \u{26d4} usage-gate: {profile}/{provider} 使用率 {used:.0}% >= {threshold}% のため後続を停止\x1b[0m"
        );
    }
    Ok(())
}

/// 短 TTL のキャッシュを介して ai-usage --json を取得する。
async fn load_usage_cached(command: &[String], cache_file: &Path) -> Result<AiUsageSnapshot> {
    const TTL_SECS: u64 = 20;
    if let Ok(meta) = std::fs::metadata(cache_file)
        && let Ok(modified) = meta.modified()
        && modified
            .elapsed()
            .map(|e| e.as_secs() < TTL_SECS)
            .unwrap_or(false)
        && let Ok(content) = std::fs::read(cache_file)
        && let Ok(parsed) = serde_json::from_slice::<AiUsageOutput>(&content)
    {
        return Ok(AiUsageSnapshot {
            accounts: parsed.accounts,
        });
    }

    anyhow::ensure!(!command.is_empty(), "ai_usage.command is empty");
    let output = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .output()
        .await
        .with_context(|| format!("failed to run ai-usage command: {}", command.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ai-usage exited with {}: {}", output.status, stderr.trim());
    }
    let parsed: AiUsageOutput =
        serde_json::from_slice(&output.stdout).context("failed to parse ai-usage --json output")?;
    // 取得成功時のみ raw JSON をキャッシュへ書き込む。
    let _ = std::fs::write(cache_file, &output.stdout);
    Ok(AiUsageSnapshot {
        accounts: parsed.accounts,
    })
}

/// stop_file を冪等に作成する。新規作成できたら true、既存なら false。
fn write_stop_file(stop_file: &Path, reason: &str) -> bool {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stop_file)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{reason}");
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_data(resets_at: Option<&str>) -> UsageWindowData {
        UsageWindowData {
            resets_at: resets_at.map(String::from),
            used_percent: None,
        }
    }

    fn account(
        profile: &str,
        provider: &str,
        ok: bool,
        weekly: Option<UsageWindowData>,
        five_hour: Option<UsageWindowData>,
        error: Option<&str>,
    ) -> AiUsageAccount {
        AiUsageAccount {
            profile: profile.to_string(),
            provider: provider.to_string(),
            ok,
            weekly,
            five_hour,
            error: error.map(String::from),
        }
    }

    fn rt_agent(
        profile: &str,
        provider: &str,
        window: UsageWindowPolicy,
        fb: UsageFallback,
    ) -> RuntimeAgent {
        RuntimeAgent {
            name: format!("claude-{profile}"),
            provider: Some(provider.to_string()),
            command: vec!["claude".to_string()],
            reset_weekday: Some("monday".to_string()),
            reset_time: Some("09:00".to_string()),
            timezone: Some("UTC".to_string()),
            ai_usage: Some(RuntimeAiUsage {
                profile: profile.to_string(),
                provider: provider.to_string(),
            }),
            window,
            fallback: fb,
            ..Default::default()
        }
    }

    fn resolver(state: UsageState) -> ScheduleResolver {
        ScheduleResolver {
            usage: state,
            state_window: StateWindowPolicy::Weekly,
        }
    }

    #[test]
    fn parse_resets_at_parses_rfc3339() {
        let d = window_data(Some("2099-01-02T03:04:05+00:00"));
        let parsed = parse_resets_at(&d).expect("RFC3339 should parse");
        assert_eq!(parsed.to_utc().to_rfc3339(), "2099-01-02T03:04:05+00:00");
    }

    #[test]
    fn parse_resets_at_none_for_missing() {
        assert!(parse_resets_at(&window_data(None)).is_none());
        assert!(parse_resets_at(&window_data(Some("not-a-date"))).is_none());
    }

    #[test]
    fn nearest_window_picks_nearer_resets_at() {
        // deadline 算出に使う resets_at 基準で、最も近い枠を選ぶ。
        let acc = account(
            "P",
            "claude",
            true,
            Some(window_data(Some("2099-01-10T00:00:00+00:00"))),
            Some(window_data(Some("2099-01-05T00:00:00+00:00"))),
            None,
        );
        let (w, _) = nearest_window(&acc).expect("should pick one");
        // five_hour(1/05) が weekly(1/10) より近い。
        assert_eq!(w, UsageWindow::FiveHour);
    }

    #[test]
    fn nearest_window_ignores_unparseable_resets_at() {
        // resets_at がパース不能な枠は候補から除外する。
        // （採用すると build_from_account が None を返してスケジュール解決全体が失敗するため）
        let acc = account(
            "P",
            "claude",
            true,
            Some(window_data(Some("not-a-date"))),
            Some(window_data(Some("2099-01-10T00:00:00+00:00"))),
            None,
        );
        let (w, _) = nearest_window(&acc).expect("should pick the parseable window");
        assert_eq!(w, UsageWindow::FiveHour);
    }

    #[test]
    fn schedule_for_uses_ai_usage_weekly() {
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "Work",
                "claude",
                true,
                Some(window_data(Some("2099-01-01T00:00:00+00:00"))),
                None,
                None,
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "Work",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("should resolve");
        assert_eq!(sched.source, ScheduleSource::AiUsage(UsageWindow::Weekly));
        // 状態カットオフは次回リセットの 7 日前
        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn schedule_for_falls_back_to_fixed_when_entry_missing() {
        let snapshot = AiUsageSnapshot { accounts: vec![] };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "UNKNOWN",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("fixed fallback");
        match sched.source {
            ScheduleSource::FixedFallback(reason) => {
                assert!(reason.contains("no ai-usage entry"));
            }
            other => panic!("expected FixedFallback, got {other:?}"),
        }
    }

    #[test]
    fn schedule_for_skip_returns_none() {
        let r = resolver(UsageState::Failed("boom".to_string()));
        let agent = rt_agent(
            "P",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Skip,
        );
        assert!(r.schedule_for(&agent).unwrap().is_none());
    }

    #[test]
    fn schedule_for_error_propagates() {
        let r = resolver(UsageState::Failed("boom".to_string()));
        let agent = rt_agent(
            "P",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Error,
        );
        assert!(r.schedule_for(&agent).is_err());
    }

    #[test]
    fn schedule_for_ok_false_falls_back_with_error_message() {
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "P",
                "claude",
                false,
                None,
                None,
                Some("rate limited"),
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "P",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().unwrap();
        match sched.source {
            ScheduleSource::FixedFallback(reason) => assert!(reason.contains("rate limited")),
            other => panic!("expected FixedFallback, got {other:?}"),
        }
    }

    #[test]
    fn schedule_for_non_ai_usage_agent_uses_fixed() {
        let r = resolver(UsageState::Disabled);
        let agent = RuntimeAgent {
            name: "claude".to_string(),
            command: vec!["claude".to_string()],
            reset_weekday: Some("monday".to_string()),
            reset_time: Some("09:00".to_string()),
            timezone: Some("UTC".to_string()),
            ..Default::default()
        };
        let sched = r.schedule_for(&agent).unwrap().unwrap();
        assert_eq!(sched.source, ScheduleSource::Fixed);
    }

    #[test]
    fn select_nearest_skips_none_and_picks_closest() {
        let snapshot = AiUsageSnapshot {
            accounts: vec![
                account(
                    "FAR",
                    "claude",
                    true,
                    Some(window_data(Some("2099-12-31T00:00:00+00:00"))),
                    None,
                    None,
                ),
                account(
                    "NEAR",
                    "claude",
                    true,
                    Some(window_data(Some("2099-01-01T00:00:00+00:00"))),
                    None,
                    None,
                ),
            ],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agents = vec![
            rt_agent(
                "FAR",
                "claude",
                UsageWindowPolicy::Weekly,
                UsageFallback::Skip,
            ),
            rt_agent(
                "NEAR",
                "claude",
                UsageWindowPolicy::Weekly,
                UsageFallback::Skip,
            ),
        ];
        let (idx, sched) = r.select_nearest(&agents).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(sched.agent_name, "claude-NEAR");
    }

    #[test]
    fn select_nearest_errors_when_all_skipped() {
        let r = resolver(UsageState::Failed("boom".to_string()));
        let agents = vec![rt_agent(
            "P",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Skip,
        )];
        assert!(r.select_nearest(&agents).is_err());
    }

    #[tokio::test]
    async fn usage_gate_stops_when_over_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let cache = tmp.path().join("cache.json");
        // cache が新鮮なので ai-usage コマンド（ダミー）は実行されない。
        std::fs::write(
            &cache,
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":true,"weekly":{"used_percent":95.0}}]}"#,
        )
        .unwrap();
        run_usage_gate("Work", "claude", 90, &stop, &cache, &["false".to_string()])
            .await
            .unwrap();
        assert!(stop.exists(), "閾値超過で stop_file が作られるべき");
    }

    #[tokio::test]
    async fn usage_gate_continues_when_under_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let cache = tmp.path().join("cache.json");
        std::fs::write(
            &cache,
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":true,"weekly":{"used_percent":50.0},"five_hour":{"used_percent":10.0}}]}"#,
        )
        .unwrap();
        run_usage_gate("Work", "claude", 90, &stop, &cache, &["false".to_string()])
            .await
            .unwrap();
        assert!(!stop.exists(), "閾値未満では stop_file は作られない");
    }

    #[tokio::test]
    async fn usage_gate_uses_max_of_weekly_and_five_hour() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let cache = tmp.path().join("cache.json");
        // weekly は低いが five_hour が閾値超過 → 停止する。
        std::fs::write(
            &cache,
            r#"{"accounts":[{"profile":"Work","provider":"codex","ok":true,"weekly":{"used_percent":20.0},"five_hour":{"used_percent":92.0}}]}"#,
        )
        .unwrap();
        run_usage_gate("Work", "codex", 90, &stop, &cache, &["false".to_string()])
            .await
            .unwrap();
        assert!(stop.exists(), "five_hour が閾値超過なら停止すべき");
    }

    #[tokio::test]
    async fn usage_gate_fail_closed_on_fetch_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let cache = tmp.path().join("missing-cache.json"); // 存在しない → fetch する
        // command が失敗する（false は exit 1）→ fail-closed で停止。
        run_usage_gate("Work", "claude", 90, &stop, &cache, &["false".to_string()])
            .await
            .unwrap();
        assert!(stop.exists(), "取得失敗時は fail-closed で停止すべき");
    }

    #[tokio::test]
    async fn usage_gate_continues_when_entry_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let cache = tmp.path().join("cache.json");
        std::fs::write(
            &cache,
            r#"{"accounts":[{"profile":"Other","provider":"claude","ok":true,"weekly":{"used_percent":99.0}}]}"#,
        )
        .unwrap();
        run_usage_gate("Work", "claude", 90, &stop, &cache, &["false".to_string()])
            .await
            .unwrap();
        assert!(!stop.exists(), "該当エントリ無しは続行（過剰停止しない）");
    }

    #[tokio::test]
    async fn usage_gate_noop_when_stop_file_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        std::fs::write(&stop, "pre-existing").unwrap();
        let cache = tmp.path().join("missing-cache.json");
        // stop_file が既存なら ai-usage を呼ばず即 return（fail-closed もしない）。
        run_usage_gate("Work", "claude", 90, &stop, &cache, &["false".to_string()])
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&stop).unwrap(), "pre-existing");
    }
}
