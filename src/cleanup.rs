use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use colored::Colorize;
use std::path::{Path, PathBuf};

use crate::state::parse_duration;

/// `YYYYMMDD_HHMMSS` で始まるディレクトリ名からタイムスタンプを解析して返す。
fn parse_dir_timestamp(name: &str) -> Option<NaiveDateTime> {
    // タイムスタンプ部分は15文字（YYYYMMDD_HHMMSS）
    // マルチバイト文字を含む場合のパニックを避けるため、char境界を確認
    if !name.is_char_boundary(15) || name.len() < 15 {
        return None;
    }
    NaiveDateTime::parse_from_str(&name[..15], "%Y%m%d_%H%M%S").ok()
}

/// `report_dir` から `max_age` より古いレポートディレクトリを削除する。
/// 削除されたディレクトリパスのリストを返す。
pub fn cleanup_old_reports(report_dir: &Path, max_age: &str) -> Result<Vec<PathBuf>> {
    let duration = parse_duration(max_age)?;
    let cutoff = Local::now()
        .naive_local()
        .checked_sub_signed(duration)
        .ok_or_else(|| anyhow::anyhow!("cleanup_after is too large: {max_age}"))?;

    let entries = match std::fs::read_dir(report_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };

    let mut deleted = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        // path.is_dir() はシンボリックリンクを追跡するため、
        // リンク先のディレクトリを誤って削除しないようリンクは除外する
        if path.is_symlink() || !path.is_dir() {
            continue;
        }

        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue, // UTF-8 でない名前は安全にスキップ
        };

        let Some(ts) = parse_dir_timestamp(&name) else {
            continue; // 解析できない名前は安全にスキップ
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

        // 古いディレクトリ（2020年）
        let old_dir = base.join("20200101_000000_claude");
        fs::create_dir(&old_dir).unwrap();
        fs::write(old_dir.join("log.txt"), "old").unwrap();

        // 新しいディレクトリ（十分未来）
        let new_dir = base.join("20990101_000000_codex");
        fs::create_dir(&new_dir).unwrap();
        fs::write(new_dir.join("log.txt"), "new").unwrap();

        // 解析不能なディレクトリ（削除対象外）
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

    #[test]
    fn parse_dir_timestamp_multibyte_at_boundary_does_not_panic() {
        // バイト位置15がマルチバイト文字の途中にある場合でもパニックしない
        // "20250101_12000"（ASCII 14バイト）+ "あ"（3バイト）= 17バイト
        // バイト位置15はマルチバイト文字の途中
        assert!(parse_dir_timestamp("20250101_12000あ").is_none());
    }

    #[test]
    fn cleanup_skips_files_in_report_dir() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        // ファイル（ディレクトリではない）はスキップされる
        let file_path = base.join("20200101_000000_log.txt");
        fs::write(&file_path, "data").unwrap();

        let deleted = cleanup_old_reports(base, "1d").unwrap();
        assert!(deleted.is_empty());
        assert!(file_path.exists(), "ファイルは削除されるべきでない");
    }

    #[test]
    fn cleanup_empty_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let deleted = cleanup_old_reports(tmp.path(), "1d").unwrap();
        assert!(deleted.is_empty());
    }

    #[test]
    fn parse_dir_timestamp_exact_15_chars_no_suffix() {
        let ts = parse_dir_timestamp("20250101_120000").unwrap();
        assert_eq!(
            ts.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2025-01-01 12:00:00"
        );
    }

    #[test]
    // ちょうど15文字でアンダースコア区切りがない場合もパースに成功する
    fn parse_dir_timestamp_exact_15_chars_various_times() {
        // 末日・最終時刻のバリエーション
        let ts = parse_dir_timestamp("20251231_235959").unwrap();
        assert_eq!(
            ts.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2025-12-31 23:59:59"
        );
        // 後続文字列がアンダースコアではなくスラッシュなどでも先頭15文字が有効なら成功する
        let ts2 = parse_dir_timestamp("20250601_000000/extra").unwrap();
        assert_eq!(
            ts2.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2025-06-01 00:00:00"
        );
    }

    #[test]
    // cleanup_old_reports に不正な max_age を渡した場合はエラーを返す
    fn cleanup_old_reports_invalid_max_age_returns_error() {
        let tmp = TempDir::new().unwrap();
        let result = cleanup_old_reports(tmp.path(), "invalid");
        assert!(result.is_err(), "不正な max_age はエラーになるべき");
    }

    #[test]
    fn cleanup_old_reports_rejects_datetime_range_overflow() {
        let tmp = TempDir::new().unwrap();
        let result = cleanup_old_reports(tmp.path(), "9000000000000000s");
        let error = result.expect_err("日時の表現範囲を超える期間は失敗するべき");
        assert!(error.to_string().contains("cleanup_after is too large"));
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_skips_symlinks_pointing_to_dirs() {
        // 回帰テスト: 旧実装の path.is_dir() はシンボリックリンクを追跡するため、
        // リンク先が古いタイムスタンプ形式のディレクトリを指していると
        // リンクが削除されたり、最悪リンク先まで削除される可能性があった。
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        // 古いタイムスタンプ形式のディレクトリ（削除対象）
        let real_dir = base.join("20200101_000000_claude");
        fs::create_dir(&real_dir).unwrap();
        fs::write(real_dir.join("data.txt"), "real").unwrap();

        // タイムスタンプ形式のシンボリックリンク（削除対象外であるべき）
        let link_path = base.join("20200102_120000_link");
        symlink(&real_dir, &link_path).unwrap();

        let deleted = cleanup_old_reports(base, "1d").unwrap();

        // 削除されるのは実ディレクトリのみで、シンボリックリンクは保持される
        assert_eq!(deleted.len(), 1, "削除されたのは実ディレクトリだけのはず");
        assert!(!real_dir.exists(), "実ディレクトリは削除されるべき");
        // シンボリックリンク自体は残るべき
        assert!(
            link_path.symlink_metadata().is_ok(),
            "シンボリックリンクは削除されるべきでない"
        );
    }
}
