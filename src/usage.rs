use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use serde::Deserialize;
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
    resets_in_seconds: Option<i64>,
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

/// weekly / five_hour のうち、より近い方を選ぶ（resets_in_seconds 基準）。
fn nearest_window(acc: &AiUsageAccount) -> Option<(UsageWindow, &UsageWindowData)> {
    [
        acc.weekly.as_ref().map(|w| (UsageWindow::Weekly, w)),
        acc.five_hour.as_ref().map(|w| (UsageWindow::FiveHour, w)),
    ]
    .into_iter()
    .flatten()
    .filter(|(_, w)| w.resets_at.is_some())
    .min_by_key(|(_, w)| w.resets_in_seconds.unwrap_or(i64::MAX))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn window_data(resets_at: Option<&str>, resets_in: Option<i64>) -> UsageWindowData {
        UsageWindowData {
            resets_at: resets_at.map(String::from),
            resets_in_seconds: resets_in,
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
        let d = window_data(Some("2099-01-02T03:04:05+00:00"), None);
        let parsed = parse_resets_at(&d).expect("RFC3339 should parse");
        assert_eq!(parsed.to_utc().to_rfc3339(), "2099-01-02T03:04:05+00:00");
    }

    #[test]
    fn parse_resets_at_none_for_missing() {
        assert!(parse_resets_at(&window_data(None, None)).is_none());
        assert!(parse_resets_at(&window_data(Some("not-a-date"), None)).is_none());
    }

    #[test]
    fn nearest_window_picks_smaller_resets_in_seconds() {
        let acc = account(
            "P",
            "claude",
            true,
            Some(window_data(Some("2099-01-10T00:00:00+00:00"), Some(1000))),
            Some(window_data(Some("2099-01-05T00:00:00+00:00"), Some(50))),
            None,
        );
        let (w, _) = nearest_window(&acc).expect("should pick one");
        assert_eq!(w, UsageWindow::FiveHour);
    }

    #[test]
    fn schedule_for_uses_ai_usage_weekly() {
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "Work",
                "claude",
                true,
                Some(window_data(Some("2099-01-01T00:00:00+00:00"), Some(999))),
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
        // state_cutoff = next_reset - 7d
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
                    Some(window_data(
                        Some("2099-12-31T00:00:00+00:00"),
                        Some(999_999),
                    )),
                    None,
                    None,
                ),
                account(
                    "NEAR",
                    "claude",
                    true,
                    Some(window_data(Some("2099-01-01T00:00:00+00:00"), Some(100))),
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
}
