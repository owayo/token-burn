use anyhow::Result;
use chrono::{DateTime, Datelike, FixedOffset, LocalResult, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use std::time::Duration;

use crate::config::{RuntimeAgent, parse_time, parse_weekday};

/// あるエージェントの次回リセットと、それに付随する情報。
#[derive(Debug, Clone)]
pub struct AgentSchedule {
    pub agent_name: String,
    /// 次回リセット時刻（ローカルオフセット付き）。
    pub next_reset: DateTime<FixedOffset>,
    /// 処理済み判定のカットオフ（この時刻以降に処理済みのターゲットはスキップ）。
    pub state_cutoff: DateTime<FixedOffset>,
    pub time_until_reset: Duration,
    /// このスケジュールの導出元。
    pub source: ScheduleSource,
}

/// スケジュールの導出元。`status` / `run` で表示し、ai-usage が静かに fixed へ戻るのを防ぐ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSource {
    /// ai-usage --json 由来（採用した枠）。
    AiUsage(UsageWindow),
    /// 曜日ベースの固定計算。
    Fixed,
    /// ai-usage 解決に失敗し、固定計算へフォールバックした（理由付き）。
    FixedFallback(String),
}

impl ScheduleSource {
    /// 表示用の短いラベル。
    pub fn label(&self) -> String {
        match self {
            ScheduleSource::AiUsage(w) => format!("ai-usage ({})", w.label()),
            ScheduleSource::Fixed => "fixed".to_string(),
            ScheduleSource::FixedFallback(reason) => format!("fixed fallback: {reason}"),
        }
    }
}

/// ai-usage で解決された実際のリセット枠。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageWindow {
    Weekly,
    FiveHour,
}

impl UsageWindow {
    /// 1 周期の長さ（state_cutoff 導出に使う）。
    pub fn period(self) -> chrono::Duration {
        match self {
            UsageWindow::Weekly => chrono::Duration::days(7),
            UsageWindow::FiveHour => chrono::Duration::hours(5),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UsageWindow::Weekly => "weekly",
            UsageWindow::FiveHour => "five_hour",
        }
    }
}

/// 曜日 + 時刻 + タイムゾーンから次回リセットを固定計算する。
/// `reset_weekday` / `reset_time` / `timezone` が揃っている必要がある。
///
/// リセット日時計算は `naive_local()` をベースに行う。`DateTime::date_naive()` は
/// UTC 日付を返すため、`weekday()` のローカル曜日と整合させるためにローカルタイム
/// ゾーンの日付を基準とする。DST 遷移は `resolve_local_datetime` で吸収する。
pub fn calculate_fixed_reset(agent: &RuntimeAgent) -> Result<AgentSchedule> {
    let tz_str = agent.timezone.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Agent '{}' has no timezone for fixed schedule", agent.name)
    })?;
    let tz: Tz = tz_str
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid timezone: {}", tz_str))?;
    let wd_str = agent.reset_weekday.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Agent '{}' has no reset_weekday for fixed schedule",
            agent.name
        )
    })?;
    let tm_str = agent.reset_time.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Agent '{}' has no reset_time for fixed schedule",
            agent.name
        )
    })?;

    let now_utc = Utc::now();
    let now = now_utc.with_timezone(&tz);

    let target_weekday = parse_weekday(wd_str)?;
    let (hour, minute) = parse_time(tm_str)?;
    let target_time = NaiveTime::from_hms_opt(hour, minute, 0)
        .ok_or_else(|| anyhow::anyhow!("Invalid time: {}:{}", hour, minute))?;

    let days_until = days_until_weekday(now.weekday(), target_weekday);
    let next_reset_date = now.naive_local().date() + chrono::Duration::days(days_until as i64);
    let next_reset_naive = next_reset_date.and_time(target_time);
    let next_reset_tz = resolve_local_datetime(&tz, next_reset_naive)?;

    let next_reset_tz = if next_reset_tz <= now {
        let next_date = next_reset_date + chrono::Duration::days(7);
        resolve_local_datetime(&tz, next_date.and_time(target_time))?
    } else {
        next_reset_tz
    };

    // 前回リセット = 次回の 7 日前。
    let prev_reset_tz = {
        let prev_date = next_reset_tz.naive_local().date() - chrono::Duration::days(7);
        resolve_local_datetime(&tz, prev_date.and_time(target_time))?
    };

    let next_reset = next_reset_tz.fixed_offset();
    let state_cutoff = prev_reset_tz.fixed_offset();
    let time_until_reset = (next_reset.with_timezone(&Utc) - now_utc)
        .to_std()
        .unwrap_or(Duration::from_secs(0));

    Ok(AgentSchedule {
        agent_name: agent.name.clone(),
        next_reset,
        state_cutoff,
        time_until_reset,
        source: ScheduleSource::Fixed,
    })
}

/// ローカル日時を実際の瞬間（タイムゾーン付き日時）に解決する。DST 遷移を正しく扱う。
///
/// - 通常の時刻: そのまま解決する。
/// - 曖昧な時刻（秋の繰り戻しで 2 回出現する時刻）: 早い方を採用する。
/// - 存在しない時刻（春の繰り上げでスキップされる時刻）: 遷移直後の最初の有効な
///   瞬間にフォールバックする。`from_local_datetime().earliest()` は曖昧な時刻には
///   対応できるが、存在しない時刻では `None` を返すため、リセット時刻がたまたま
///   DST ギャップ（例: America/New_York の 02:30）に重なると、設定読み込みは成功
///   するのに status / run が実行時に毎回失敗してしまう。これを防ぐ。
fn resolve_local_datetime(tz: &Tz, naive: chrono::NaiveDateTime) -> Result<DateTime<Tz>> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Ok(dt),
        LocalResult::Ambiguous(earliest, _) => Ok(earliest),
        LocalResult::None => {
            // 存在しない時刻（DST ギャップ）。1 分ずつ進めて遷移直後の有効な瞬間を探す。
            let mut probe = naive;
            for _ in 0..120 {
                probe += chrono::Duration::minutes(1);
                match tz.from_local_datetime(&probe) {
                    LocalResult::Single(dt) => return Ok(dt),
                    LocalResult::Ambiguous(earliest, _) => return Ok(earliest),
                    LocalResult::None => continue,
                }
            }
            Err(anyhow::anyhow!(
                "Could not resolve local datetime {} for timezone {}",
                naive,
                tz
            ))
        }
    }
}

fn days_until_weekday(current: Weekday, target: Weekday) -> u32 {
    let current_num = current.num_days_from_monday();
    let target_num = target.num_days_from_monday();
    if target_num >= current_num {
        target_num - current_num
    } else {
        7 - (current_num - target_num)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Timelike, Utc, Weekday};

    fn make_runtime_agent(name: &str, weekday: &str, time: &str, tz: &str) -> RuntimeAgent {
        RuntimeAgent {
            name: name.to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: Some(weekday.to_string()),
            reset_time: Some(time.to_string()),
            timezone: Some(tz.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn days_until_same_weekday_is_zero() {
        assert_eq!(days_until_weekday(Weekday::Mon, Weekday::Mon), 0);
        assert_eq!(days_until_weekday(Weekday::Fri, Weekday::Fri), 0);
    }

    #[test]
    fn days_until_next_weekday() {
        assert_eq!(days_until_weekday(Weekday::Mon, Weekday::Wed), 2);
        assert_eq!(days_until_weekday(Weekday::Mon, Weekday::Sun), 6);
    }

    #[test]
    fn days_until_previous_weekday_wraps() {
        assert_eq!(days_until_weekday(Weekday::Wed, Weekday::Mon), 5);
        assert_eq!(days_until_weekday(Weekday::Sun, Weekday::Mon), 1);
    }

    #[test]
    fn resolve_local_datetime_handles_spring_forward_gap() {
        // America/New_York の 2025-03-09 02:30 は春の繰り上げ（02:00→03:00）で
        // 存在しない時刻。遷移直後の有効な瞬間（03:00 EDT）にフォールバックすること。
        let tz: Tz = "America/New_York".parse().unwrap();
        let naive = chrono::NaiveDate::from_ymd_opt(2025, 3, 9)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        let resolved = resolve_local_datetime(&tz, naive).unwrap();
        assert_eq!(resolved.hour(), 3);
        assert_eq!(resolved.minute(), 0);
    }

    #[test]
    fn resolve_local_datetime_handles_fall_back_ambiguous() {
        // America/New_York の 2025-11-02 01:30 は秋の繰り戻しで 2 回出現する曖昧な時刻。
        // 早い方（EDT, UTC-4）を採用すること。
        let tz: Tz = "America/New_York".parse().unwrap();
        let naive = chrono::NaiveDate::from_ymd_opt(2025, 11, 2)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let resolved = resolve_local_datetime(&tz, naive).unwrap();
        assert_eq!(resolved.with_timezone(&Utc).hour(), 5);
    }

    #[test]
    fn resolve_local_datetime_handles_normal_time() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let naive = chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let resolved = resolve_local_datetime(&tz, naive).unwrap();
        assert_eq!(resolved.hour(), 12);
        assert_eq!(resolved.minute(), 0);
    }

    #[test]
    fn calculate_fixed_reset_returns_future_time() {
        let agent = make_runtime_agent("test", "monday", "09:00", "UTC");
        let sched = calculate_fixed_reset(&agent).unwrap();
        assert!(sched.time_until_reset.as_secs() > 0);
        assert!(sched.next_reset > sched.state_cutoff);
        assert_eq!(sched.source, ScheduleSource::Fixed);
    }

    #[test]
    fn state_cutoff_is_seven_days_before_next() {
        let agent = make_runtime_agent("test", "wednesday", "14:00", "UTC");
        let sched = calculate_fixed_reset(&agent).unwrap();
        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn calculate_fixed_reset_includes_agent_name() {
        let agent = make_runtime_agent("test-agent", "friday", "18:00", "UTC");
        let sched = calculate_fixed_reset(&agent).unwrap();
        assert_eq!(sched.agent_name, "test-agent");
    }

    #[test]
    fn calculate_fixed_reset_different_timezones() {
        let agent_tokyo = make_runtime_agent("tokyo", "monday", "09:00", "Asia/Tokyo");
        let agent_utc = make_runtime_agent("utc", "monday", "09:00", "UTC");

        let sched_tokyo = calculate_fixed_reset(&agent_tokyo).unwrap();
        let sched_utc = calculate_fixed_reset(&agent_utc).unwrap();

        assert!(sched_tokyo.time_until_reset.as_secs() > 0);
        assert!(sched_utc.time_until_reset.as_secs() > 0);
    }

    #[test]
    fn calculate_fixed_reset_midnight() {
        let agent = make_runtime_agent("midnight", "friday", "00:00", "UTC");
        let sched = calculate_fixed_reset(&agent).unwrap();
        assert!(sched.time_until_reset.as_secs() > 0);
        assert_eq!(sched.next_reset.time().hour(), 0);
        assert_eq!(sched.next_reset.time().minute(), 0);
    }

    #[test]
    fn calculate_fixed_reset_end_of_day() {
        let agent = make_runtime_agent("late", "sunday", "23:59", "UTC");
        let sched = calculate_fixed_reset(&agent).unwrap();
        assert!(sched.time_until_reset.as_secs() > 0);
    }

    #[test]
    fn days_until_weekday_all_combinations() {
        let weekdays = [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
            Weekday::Sun,
        ];
        for &from in &weekdays {
            for &to in &weekdays {
                let days = days_until_weekday(from, to);
                assert!(days <= 6, "{:?} → {:?} = {} (> 6)", from, to, days);
            }
        }
    }

    #[test]
    fn calculate_fixed_reset_same_weekday_same_time_is_seven_days_later() {
        // 現在時刻の 1 分前をリセット時刻に設定（同一曜日 + 過去時刻 → 7 日後にシフト）。
        let tz: Tz = "UTC".parse().unwrap();
        let now = Utc::now().with_timezone(&tz);

        let weekday_str = match now.weekday() {
            Weekday::Mon => "monday",
            Weekday::Tue => "tuesday",
            Weekday::Wed => "wednesday",
            Weekday::Thu => "thursday",
            Weekday::Fri => "friday",
            Weekday::Sat => "saturday",
            Weekday::Sun => "sunday",
        };

        let past_minute = now - chrono::Duration::minutes(1);
        let reset_time = format!("{:02}:{:02}", past_minute.hour(), past_minute.minute());

        let agent = make_runtime_agent("same-day", weekday_str, &reset_time, "UTC");
        let sched = calculate_fixed_reset(&agent).unwrap();

        assert!(sched.next_reset.with_timezone(&Utc) > now.with_timezone(&Utc));
        let days_until = sched.time_until_reset.as_secs() / 86400;
        assert!(
            days_until >= 5,
            "7日後になるべきだが {} 日後だった",
            days_until
        );

        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn calculate_fixed_reset_invalid_timezone_returns_error() {
        let agent = make_runtime_agent("bad-tz", "monday", "09:00", "Invalid/Timezone");
        assert!(calculate_fixed_reset(&agent).is_err());
    }

    #[test]
    fn calculate_fixed_reset_missing_fields_returns_error() {
        // ai-usage 連携で reset_* が無い RuntimeAgent は fixed 計算でエラーになる。
        let agent = RuntimeAgent {
            name: "no-reset".to_string(),
            command: vec!["echo".to_string()],
            ..Default::default()
        };
        let err = calculate_fixed_reset(&agent).expect_err("reset_* が無ければエラー");
        assert!(err.to_string().contains("timezone"));
    }

    #[test]
    fn calculate_fixed_reset_previous_is_always_past() {
        let agents = vec![
            make_runtime_agent("a", "monday", "00:00", "UTC"),
            make_runtime_agent("b", "wednesday", "12:00", "UTC"),
            make_runtime_agent("c", "friday", "23:59", "UTC"),
        ];
        let now = Utc::now();
        for agent in &agents {
            let sched = calculate_fixed_reset(agent).unwrap();
            assert!(
                sched.state_cutoff.with_timezone(&Utc) <= now,
                "{} の state_cutoff が未来になっている",
                agent.name
            );
        }
    }

    #[test]
    fn next_reset_weekday_matches_configured_weekday_for_all_timezones() {
        // 回帰テスト: naive_local().date() を使うので、全 7 曜日 × 主要タイムゾーンで
        // next_reset / state_cutoff の曜日が target と一致する。
        let weekdays_and_strs = [
            ("monday", Weekday::Mon),
            ("tuesday", Weekday::Tue),
            ("wednesday", Weekday::Wed),
            ("thursday", Weekday::Thu),
            ("friday", Weekday::Fri),
            ("saturday", Weekday::Sat),
            ("sunday", Weekday::Sun),
        ];
        let timezones = ["UTC", "Asia/Tokyo", "America/New_York", "Europe/London"];
        for tz in timezones {
            for (weekday_str, expected_weekday) in weekdays_and_strs {
                let agent = make_runtime_agent("test", weekday_str, "09:00", tz);
                let sched = calculate_fixed_reset(&agent).unwrap();
                assert_eq!(
                    sched.next_reset.weekday(),
                    expected_weekday,
                    "{} で {} のリセットが {:?} と一致しない: next_reset={}",
                    tz,
                    weekday_str,
                    expected_weekday,
                    sched.next_reset
                );
                assert_eq!(
                    sched.state_cutoff.weekday(),
                    expected_weekday,
                    "{} で {} の state_cutoff が {:?} と一致しない",
                    tz,
                    weekday_str,
                    expected_weekday
                );
            }
        }
    }

    #[test]
    fn next_reset_local_time_matches_configured_time() {
        // FixedOffset 保持なのでローカル時刻成分が設定値と一致する。
        let agent = make_runtime_agent("tz-test", "monday", "09:00", "Asia/Tokyo");
        let sched = calculate_fixed_reset(&agent).unwrap();
        assert_eq!(sched.next_reset.hour(), 9, "hour mismatch");
        assert_eq!(sched.next_reset.minute(), 0, "minute mismatch");
        assert_eq!(sched.state_cutoff.hour(), 9, "previous hour mismatch");
        assert_eq!(sched.state_cutoff.minute(), 0, "previous minute mismatch");
    }
}
