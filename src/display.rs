use colored::Colorize;
use std::time::Duration;

use crate::config::Config;
use crate::scanner::{ResolvedTarget, Visibility};
use crate::schedule::calculate_next_reset;

pub fn print_status(config: &Config) -> anyhow::Result<()> {
    println!("{}", "=== Agent Status ===".bold());
    println!();
    for agent in &config.agents {
        let schedule = calculate_next_reset(agent)?;
        println!("  {} {}", "Agent:".bold(), schedule.agent_name.cyan());
        println!(
            "    Next reset: {}",
            schedule
                .next_reset
                .format("%Y-%m-%d %H:%M %Z")
                .to_string()
                .yellow()
        );
        println!(
            "    Remaining:  {}",
            format_duration(schedule.time_until_reset).red()
        );
        println!();
    }
    Ok(())
}

pub fn print_targets(targets: &[ResolvedTarget]) {
    println!("{}", "=== Targets ===".bold());
    println!("  Found {} repositories", targets.len());
    println!();
    for (i, target) in targets.iter().enumerate() {
        let vis = format!("[{}]", target.visibility);
        let vis_colored = match target.visibility {
            Visibility::Public => vis.green(),
            Visibility::Private => vis.yellow(),
            Visibility::Unknown => vis.dimmed(),
        };
        println!(
            "  {} {} {}",
            format!("[{}]", i + 1).yellow(),
            vis_colored,
            target.display_name
        );
    }
    println!();
}

pub fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    if days > 0 {
        format!("{}d {}h {}m", days, hours, minutes)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_days() {
        let d = Duration::from_secs(90061); // 1d 1h 1m 1s
        assert_eq!(format_duration(d), "1d 1h 1m");
    }

    #[test]
    fn format_duration_hours() {
        let d = Duration::from_secs(7260); // 2h 1m
        assert_eq!(format_duration(d), "2h 1m");
    }

    #[test]
    fn format_duration_minutes_only() {
        let d = Duration::from_secs(300); // 5m
        assert_eq!(format_duration(d), "5m");
    }

    #[test]
    fn format_duration_zero() {
        let d = Duration::from_secs(0);
        assert_eq!(format_duration(d), "0m");
    }
}
