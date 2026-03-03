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
    use chrono::Weekday;

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
}
