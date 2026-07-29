use chrono::{DateTime, Local, Utc};
use colored::Colorize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::RuntimeAgent;
use crate::scanner::{ResolvedTarget, Visibility};
use crate::usage::ScheduleResolver;

pub fn print_status(agents: &[RuntimeAgent], resolver: &ScheduleResolver) -> anyhow::Result<()> {
    println!("{}", "=== Agent Status ===".bold());
    println!();
    if let Some(err) = resolver.failure() {
        println!("  {} {}", "ai-usage error:".red().bold(), err.dimmed());
        println!();
    }
    for agent in agents {
        println!("  {} {}", "Agent:".bold(), agent.name.cyan());
        match resolver.schedule_for(agent)? {
            Some(schedule) => {
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
                println!("    Source:     {}", schedule.source.label().dimmed());
            }
            None => {
                println!("    {}", "skipped (ai-usage fallback=skip)".dimmed());
            }
        }
        println!();
    }
    Ok(())
}

/// ターゲット一覧を表示する。
///
/// `modified` は実行順の根拠になった最終ファイル変更日時。渡された分だけローカル時刻で
/// 併記し、「なぜこの順番になったのか」を目視で確認できるようにする。
pub fn print_targets(targets: &[ResolvedTarget], modified: &HashMap<PathBuf, DateTime<Utc>>) {
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
        let last_modified = match modified.get(&target.directory) {
            Some(ts) => format!(
                " (modified: {})",
                ts.with_timezone(&Local).format("%Y-%m-%d %H:%M")
            ),
            None => String::new(),
        };
        println!(
            "  {} {} {}{}",
            format!("[{}]", i + 1).yellow(),
            vis_colored,
            target.display_name,
            last_modified.dimmed()
        );
        println!("      {}", target.directory.display().to_string().dimmed());
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
        let d = Duration::from_secs(90061); // 1日1時間1分1秒
        assert_eq!(format_duration(d), "1d 1h 1m");
    }

    #[test]
    fn format_duration_hours() {
        let d = Duration::from_secs(7260); // 2時間1分
        assert_eq!(format_duration(d), "2h 1m");
    }

    #[test]
    fn format_duration_minutes_only() {
        let d = Duration::from_secs(300); // 5分
        assert_eq!(format_duration(d), "5m");
    }

    #[test]
    fn format_duration_zero() {
        let d = Duration::from_secs(0);
        assert_eq!(format_duration(d), "0m");
    }

    #[test]
    fn format_duration_under_one_minute() {
        // 1分未満は "0m" として表示される
        let d = Duration::from_secs(59);
        assert_eq!(format_duration(d), "0m");
    }

    #[test]
    fn format_duration_exact_one_day() {
        let d = Duration::from_secs(86400);
        assert_eq!(format_duration(d), "1d 0h 0m");
    }

    #[test]
    fn format_duration_large_value() {
        // 30日以上の長期間
        let d = Duration::from_secs(30 * 86400 + 5 * 3600 + 30 * 60);
        assert_eq!(format_duration(d), "30d 5h 30m");
    }

    #[test]
    // ちょうど1時間（3600秒）は "1h 0m" を返す
    fn format_duration_exact_one_hour() {
        let d = Duration::from_secs(3600);
        assert_eq!(format_duration(d), "1h 0m");
    }

    #[test]
    // ちょうど1分（60秒）は "1m" を返す
    fn format_duration_exact_one_minute() {
        let d = Duration::from_secs(60);
        assert_eq!(format_duration(d), "1m");
    }
}
