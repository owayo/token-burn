use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use colored::Colorize;
use std::path::{Path, PathBuf};

use crate::state::parse_duration;

/// `YYYYMMDD_HHMMSS` で始まるディレクトリ名からタイムスタンプを解析して返す。
fn parse_dir_timestamp(name: &str) -> Option<NaiveDateTime> {
    // Format: YYYYMMDD_HHMMSS_agentname (at least 15 chars for the timestamp part)
    if name.len() < 15 {
        return None;
    }
    NaiveDateTime::parse_from_str(&name[..15], "%Y%m%d_%H%M%S").ok()
}

/// `report_dir` から `max_age` より古いレポートディレクトリを削除する。
/// 削除されたディレクトリパスのリストを返す。
pub fn cleanup_old_reports(report_dir: &Path, max_age: &str) -> Result<Vec<PathBuf>> {
    let duration = parse_duration(max_age)?;
    let cutoff = Local::now().naive_local() - duration;

    let entries = match std::fs::read_dir(report_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };

    let mut deleted = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue, // non-UTF-8 name, skip
        };

        let Some(ts) = parse_dir_timestamp(&name) else {
            continue; // unparseable name, skip safely
        };

        if ts < cutoff {
            std::fs::remove_dir_all(&path)?;
            deleted.push(path);
        }
    }

    Ok(deleted)
}

/// クリーンアップ結果を表示する。
pub fn print_cleanup_result(deleted: &[PathBuf]) {
    if deleted.is_empty() {
        println!("{}", "No old report directories to clean up.".dimmed());
    } else {
        println!(
            "{} {} report {}",
            "Cleaned up:".green().bold(),
            deleted.len(),
            if deleted.len() == 1 {
                "directory"
            } else {
                "directories"
            }
        );
        for path in deleted {
            println!(
                "  {} {}",
                "Removed:".dimmed(),
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn parse_dir_timestamp_valid() {
        let ts = parse_dir_timestamp("20250101_120000_claude").unwrap();
        assert_eq!(
            ts.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2025-01-01 12:00:00"
        );
    }

    #[test]
    fn parse_dir_timestamp_short_name() {
        assert!(parse_dir_timestamp("2025").is_none());
    }

    #[test]
    fn parse_dir_timestamp_invalid_format() {
        assert!(parse_dir_timestamp("not_a_timestamp_dir").is_none());
    }

    #[test]
    fn cleanup_removes_old_and_keeps_recent() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        // Old directory (2020)
        let old_dir = base.join("20200101_000000_claude");
        fs::create_dir(&old_dir).unwrap();
        fs::write(old_dir.join("log.txt"), "old").unwrap();

        // Recent directory (far future)
        let new_dir = base.join("20990101_000000_codex");
        fs::create_dir(&new_dir).unwrap();
        fs::write(new_dir.join("log.txt"), "new").unwrap();

        // Unparseable directory (should be skipped)
        let skip_dir = base.join("random_dir");
        fs::create_dir(&skip_dir).unwrap();

        let deleted = cleanup_old_reports(base, "1d").unwrap();

        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].file_name().unwrap(), "20200101_000000_claude");
        assert!(!old_dir.exists());
        assert!(new_dir.exists());
        assert!(skip_dir.exists());
    }

    #[test]
    fn cleanup_nonexistent_dir_returns_empty() {
        let deleted = cleanup_old_reports(Path::new("/nonexistent/path/token-burn"), "7d").unwrap();
        assert!(deleted.is_empty());
    }
}
