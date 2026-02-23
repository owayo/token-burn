use anyhow::Result;
use chrono::{Datelike, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use std::time::Duration;

use crate::config::{parse_time, parse_weekday, Agent};

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
