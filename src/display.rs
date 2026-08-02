use chrono::{DateTime, Local, Utc};
use colored::Colorize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::RuntimeAgent;
use crate::scanner::{ResolvedTarget, Visibility};
use crate::usage::ScheduleResolver;

const REDACTED_COMMAND_VALUE: &str = "<redacted>";

/// ログや実行計画へ表示するコマンドから、秘密値になり得る引数を伏せる。
pub fn format_command(command: &[String]) -> String {
    let mut rendered = Vec::with_capacity(command.len());
    let mut redact_next = false;

    for (index, arg) in command.iter().enumerate() {
        if index == 0 {
            rendered.push(arg.clone());
            continue;
        }
        if redact_next {
            rendered.push(REDACTED_COMMAND_VALUE.to_string());
            redact_next = false;
            continue;
        }

        if let Some((key, _)) = arg.split_once('=')
            && (is_env_key(key) || is_sensitive_option(key))
        {
            rendered.push(format!("{key}={REDACTED_COMMAND_VALUE}"));
            continue;
        }

        rendered.push(arg.clone());
        redact_next = is_sensitive_option(arg);
    }

    rendered.join(" ")
}

fn is_env_key(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('A'..='Z' | 'a'..='z' | '_'))
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_sensitive_option(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().replace('_', "-").as_str(),
        "--api-key"
            | "--apikey"
            | "--token"
            | "--access-token"
            | "--auth-token"
            | "--password"
            | "--secret"
            | "--client-secret"
            | "--credential"
            | "--credentials"
    )
}

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
    fn format_command_redacts_environment_assignments() {
        let command = vec![
            "env".to_string(),
            "API_TOKEN=top-secret".to_string(),
            "ai-usage".to_string(),
            "--json".to_string(),
        ];

        assert_eq!(
            format_command(&command),
            "env API_TOKEN=<redacted> ai-usage --json"
        );
    }

    #[test]
    fn format_command_redacts_sensitive_option_values() {
        let command = vec![
            "client".to_string(),
            "--api-key".to_string(),
            "top-secret".to_string(),
            "--auth_token=another-secret".to_string(),
            "--verbose".to_string(),
        ];

        assert_eq!(
            format_command(&command),
            "client --api-key <redacted> --auth_token=<redacted> --verbose"
        );
    }

    #[test]
    fn format_command_keeps_non_sensitive_arguments() {
        let command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "--sandbox".to_string(),
            "workspace-write".to_string(),
        ];

        assert_eq!(
            format_command(&command),
            "codex exec --sandbox workspace-write"
        );
        assert_eq!(format_command(&[]), "");
    }

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
