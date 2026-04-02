use anyhow::Result;
use chrono::{Datelike, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use std::time::Duration;

use crate::config::{Agent, parse_time, parse_weekday};

#[derive(Debug)]
pub struct AgentSchedule {
    pub agent_name: String,
    pub next_reset: chrono::DateTime<Tz>,
    pub previous_reset: chrono::DateTime<Tz>,
    pub time_until_reset: Duration,
}

pub fn calculate_next_reset(agent: &Agent) -> Result<AgentSchedule> {
    let tz: Tz = agent
        .timezone
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid timezone: {}", agent.timezone))?;
    let now = Utc::now().with_timezone(&tz);

    let target_weekday = parse_weekday(&agent.reset_weekday)?;
    let (hour, minute) = parse_time(&agent.reset_time)?;
    let target_time = NaiveTime::from_hms_opt(hour, minute, 0)
        .ok_or_else(|| anyhow::anyhow!("Invalid time: {}:{}", hour, minute))?;

    let days_until = days_until_weekday(now.weekday(), target_weekday);
    let next_reset_date = now.date_naive() + chrono::Duration::days(days_until as i64);
    let next_reset_naive = next_reset_date.and_time(target_time);

    let next_reset = tz
        .from_local_datetime(&next_reset_naive)
        .single()
        .ok_or_else(|| anyhow::anyhow!("Ambiguous or invalid datetime"))?;

    let next_reset = if next_reset <= now {
        let next_date = next_reset_date + chrono::Duration::days(7);
        let next_naive = next_date.and_time(target_time);
        tz.from_local_datetime(&next_naive)
            .single()
            .ok_or_else(|| anyhow::anyhow!("Ambiguous or invalid datetime"))?
    } else {
        next_reset
    };

    let previous_reset = {
        let prev_date = next_reset.date_naive() - chrono::Duration::days(7);
        let prev_naive = prev_date.and_time(target_time);
        tz.from_local_datetime(&prev_naive)
            .single()
            .ok_or_else(|| anyhow::anyhow!("Ambiguous or invalid datetime for previous reset"))?
    };

    let duration = (next_reset - now)
        .to_std()
        .unwrap_or(Duration::from_secs(0));

    Ok(AgentSchedule {
        agent_name: agent.name.clone(),
        next_reset,
        previous_reset,
        time_until_reset: duration,
    })
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

pub fn select_nearest_agent(agents: &[Agent]) -> Result<(usize, AgentSchedule)> {
    let mut nearest_idx = 0;
    let mut nearest_schedule = calculate_next_reset(&agents[0])?;

    for (i, agent) in agents.iter().enumerate().skip(1) {
        let schedule = calculate_next_reset(agent)?;
        if schedule.time_until_reset < nearest_schedule.time_until_reset {
            nearest_idx = i;
            nearest_schedule = schedule;
        }
    }

    Ok((nearest_idx, nearest_schedule))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Timelike, Utc, Weekday};

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

    fn make_agent(name: &str, weekday: &str, time: &str) -> Agent {
        Agent {
            name: name.to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: weekday.to_string(),
            reset_time: time.to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        }
    }

    #[test]
    fn calculate_next_reset_returns_future_time() {
        let agent = make_agent("test", "monday", "09:00");
        let sched = calculate_next_reset(&agent).unwrap();
        assert!(sched.time_until_reset.as_secs() > 0);
        assert!(sched.next_reset > sched.previous_reset);
    }

    #[test]
    fn previous_reset_is_seven_days_before_next() {
        let agent = make_agent("test", "wednesday", "14:00");
        let sched = calculate_next_reset(&agent).unwrap();
        let diff = sched.next_reset - sched.previous_reset;
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn select_nearest_agent_picks_valid_agent() {
        let agents = vec![
            make_agent("a", "monday", "09:00"),
            make_agent("b", "thursday", "09:00"),
        ];
        let (idx, sched) = select_nearest_agent(&agents).unwrap();
        assert!(idx < agents.len());
        assert_eq!(sched.agent_name, agents[idx].name);
        assert!(sched.time_until_reset.as_secs() > 0);
    }

    #[test]
    fn select_nearest_agent_single_agent() {
        let agents = vec![make_agent("only", "wednesday", "12:00")];
        let (idx, sched) = select_nearest_agent(&agents).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(sched.agent_name, "only");
    }

    #[test]
    fn calculate_next_reset_includes_agent_name() {
        let agent = make_agent("test-agent", "friday", "18:00");
        let sched = calculate_next_reset(&agent).unwrap();
        assert_eq!(sched.agent_name, "test-agent");
    }

    #[test]
    fn select_nearest_agent_all_same_schedule() {
        // 全エージェントが同じスケジュールの場合、いずれかのエージェントが返る
        let agents = vec![
            make_agent("first", "monday", "09:00"),
            make_agent("second", "monday", "09:00"),
        ];
        let (idx, sched) = select_nearest_agent(&agents).unwrap();
        // 同一スケジュールなので < 比較により最初のエージェントが保持される
        assert!(idx < agents.len());
        assert_eq!(sched.agent_name, agents[idx].name);
    }

    #[test]
    fn calculate_next_reset_different_timezones() {
        // 異なるタイムゾーンでも正しく計算される
        let agent_tokyo = Agent {
            name: "tokyo".to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            prompt: None,
        };
        let agent_utc = make_agent("utc", "monday", "09:00");

        let sched_tokyo = calculate_next_reset(&agent_tokyo).unwrap();
        let sched_utc = calculate_next_reset(&agent_utc).unwrap();

        // 東京の方がUTCより早くリセットされる（UTC+9）
        // 両方とも未来であること
        assert!(sched_tokyo.time_until_reset.as_secs() > 0);
        assert!(sched_utc.time_until_reset.as_secs() > 0);
    }

    #[test]
    fn calculate_next_reset_midnight() {
        // 深夜0時のリセットが正しく計算される
        let agent = make_agent("midnight", "friday", "00:00");
        let sched = calculate_next_reset(&agent).unwrap();
        assert!(sched.time_until_reset.as_secs() > 0);
        assert_eq!(sched.next_reset.time().hour(), 0);
        assert_eq!(sched.next_reset.time().minute(), 0);
    }

    #[test]
    fn calculate_next_reset_end_of_day() {
        // 23:59のリセットが正しく計算される
        let agent = make_agent("late", "sunday", "23:59");
        let sched = calculate_next_reset(&agent).unwrap();
        assert!(sched.time_until_reset.as_secs() > 0);
    }

    #[test]
    fn days_until_weekday_all_combinations() {
        // 全曜日の組み合わせで 0〜6 の範囲に収まることを確認
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
    #[should_panic]
    fn select_nearest_agent_空スライスはパニック() {
        // 空の agents スライスを渡すと agents[0] のインデックスアクセスでパニックになる
        let agents: Vec<Agent> = vec![];
        let _ = select_nearest_agent(&agents);
    }

    #[test]
    fn calculate_next_reset_現在時刻と同一曜日同一時刻は7日後() {
        // 現在時刻（UTC）の曜日と1分前の時刻をリセット設定に指定する
        // days_until_weekday が 0 を返し、かつリセット時刻 < 現在時刻となるため
        // next_reset <= now の条件に該当し、7日後にシフトされることを検証する
        use chrono_tz::Tz;
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

        // 現在時刻の1分前をリセット時刻に設定（同一曜日 + 過去時刻 → 7日後にシフト）
        let past_minute = now - chrono::Duration::minutes(1);
        let reset_time = format!("{:02}:{:02}", past_minute.hour(), past_minute.minute());

        let agent = make_agent("same-day", weekday_str, &reset_time);
        let sched = calculate_next_reset(&agent).unwrap();

        // next_reset は必ず現在より未来であること
        assert!(
            sched.next_reset > now,
            "next_reset は現在時刻より未来でなければならない"
        );

        // 同一曜日・過去時刻のため7日後にシフトされるので、残り時間は6日超（余裕を持って5日以上）
        let days_until = sched.time_until_reset.as_secs() / 86400;
        assert!(
            days_until >= 5,
            "同一曜日の過去時刻を指定した場合、next_reset は7日後になるべきだが {} 日後だった",
            days_until
        );

        // next_reset と previous_reset の差は常に7日
        let diff = sched.next_reset - sched.previous_reset;
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn select_nearest_agent_picks_closest_reset() {
        // 異なるタイムゾーンのエージェントを用意し、最も近いリセットが選ばれることを確認
        let now = Utc::now();
        let today_weekday = now.weekday();

        // 今日の曜日の次の曜日（1日後）と、その次（2日後）を設定
        let next_day = today_weekday.succ();
        let day_after = next_day.succ();
        let weekday_to_str = |w: Weekday| -> &'static str {
            match w {
                Weekday::Mon => "monday",
                Weekday::Tue => "tuesday",
                Weekday::Wed => "wednesday",
                Weekday::Thu => "thursday",
                Weekday::Fri => "friday",
                Weekday::Sat => "saturday",
                Weekday::Sun => "sunday",
            }
        };

        let agents = vec![
            make_agent("far", weekday_to_str(day_after), "09:00"),
            make_agent("near", weekday_to_str(next_day), "09:00"),
        ];
        let (idx, sched) = select_nearest_agent(&agents).unwrap();
        assert_eq!(idx, 1, "最も近いリセットのエージェントが選ばれるべき");
        assert_eq!(sched.agent_name, "near");
    }

    #[test]
    fn calculate_next_reset_invalid_timezone_returns_error() {
        let agent = Agent {
            name: "bad-tz".to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "Invalid/Timezone".to_string(),
            prompt: None,
        };
        let result = calculate_next_reset(&agent);
        assert!(result.is_err(), "無効なタイムゾーンはエラーになるべき");
    }

    #[test]
    fn calculate_next_reset_previous_is_always_past() {
        // すべてのテストエージェントで previous_reset が現在時刻より過去であることを確認
        let agents = vec![
            make_agent("a", "monday", "00:00"),
            make_agent("b", "wednesday", "12:00"),
            make_agent("c", "friday", "23:59"),
        ];
        let now = Utc::now();
        for agent in &agents {
            let sched = calculate_next_reset(agent).unwrap();
            assert!(
                sched.previous_reset.with_timezone(&Utc) <= now,
                "{} の previous_reset が未来になっている",
                agent.name
            );
        }
    }
}
