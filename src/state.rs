use anyhow::Result;
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// エージェントごとのディレクトリパス → 最終処理タイムスタンプのマップ
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(flatten)]
    pub agents: HashMap<String, HashMap<String, DateTime<Utc>>>,
}

impl State {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn last_processed(&self, agent_name: &str, directory: &Path) -> Option<DateTime<Utc>> {
        self.agents
            .get(agent_name)
            .and_then(|m| m.get(&directory.to_string_lossy().to_string()))
            .copied()
    }

    pub fn mark_completed(&mut self, agent_name: &str, directory: &Path) {
        self.agents
            .entry(agent_name.to_string())
            .or_default()
            .insert(directory.to_string_lossy().to_string(), Utc::now());
    }
}

/// エージェントに対してディレクトリの処理完了をアトミックに記録する。
/// 排他ファイルロックを取得し、並行ワーカープロセス間で
/// 更新が上書きされないようにする。
pub fn mark_completed_atomic(path: &Path, agent_name: &str, directory: &Path) -> Result<()> {
    use std::fs::OpenOptions;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    file.lock_exclusive()?;

    let mut content = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut content)?;

    let mut state = if content.trim().is_empty() {
        State::default()
    } else {
        serde_json::from_str(&content).unwrap_or_default()
    };
    state.mark_completed(agent_name, directory);

    let serialized = serde_json::to_string_pretty(&state)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(serialized.as_bytes())?;
    file.sync_data()?;
    file.unlock()?;
    Ok(())
}

pub fn state_path(config_path: &Path) -> PathBuf {
    let resolved_config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(config_path)
    };
    resolved_config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("state.json")
}

/// 期間文字列をパースする（例: "7d", "24h", "30m", "1d12h"）
pub fn parse_duration(s: &str) -> Result<chrono::Duration> {
    let mut total_secs: i64 = 0;
    let mut num_buf = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            num_buf.push(c);
        } else {
            let n: i64 = num_buf
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid duration number: {}", num_buf))?;
            num_buf.clear();
            let unit_secs: i64 = match c {
                'd' => 86400,
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => anyhow::bail!("Invalid duration unit '{}' in: {}", c, s),
            };
            let add_secs = n
                .checked_mul(unit_secs)
                .ok_or_else(|| anyhow::anyhow!("Duration is too large: {}", s))?;
            total_secs = total_secs
                .checked_add(add_secs)
                .ok_or_else(|| anyhow::anyhow!("Duration is too large: {}", s))?;
        }
    }

    if !num_buf.is_empty() {
        anyhow::bail!("Duration must end with a unit (d/h/m/s): {}", s);
    }
    if total_secs == 0 {
        anyhow::bail!("Duration must be positive: {}", s);
    }

    Ok(chrono::Duration::seconds(total_secs))
}

#[cfg(test)]
mod tests {
    use super::{State, mark_completed_atomic, parse_duration, state_path};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    #[test]
    fn parse_duration_supports_compound_values() {
        let d = parse_duration("1d12h30m15s").expect("duration should parse");
        assert_eq!(d.num_seconds(), 131_415);
    }

    #[test]
    fn parse_duration_rejects_missing_unit() {
        let err = parse_duration("30").expect_err("duration without unit must fail");
        assert!(err.to_string().contains("Duration must end with a unit"));
    }

    #[test]
    fn parse_duration_rejects_invalid_unit() {
        let err = parse_duration("5w").expect_err("unsupported unit must fail");
        assert!(err.to_string().contains("Invalid duration unit"));
    }

    #[test]
    fn parse_duration_rejects_zero_duration() {
        let err = parse_duration("0s").expect_err("zero duration must fail");
        assert!(err.to_string().contains("Duration must be positive"));
    }

    #[test]
    fn parse_duration_rejects_multiplication_overflow() {
        let input = format!("{}d", i64::MAX);
        let err = parse_duration(&input).expect_err("overflowing duration must fail");
        assert!(err.to_string().contains("Duration is too large"));
    }

    #[test]
    fn parse_duration_rejects_addition_overflow() {
        let input = format!("{}s1s", i64::MAX);
        let err = parse_duration(&input).expect_err("overflowing duration must fail");
        assert!(err.to_string().contains("Duration is too large"));
    }

    #[test]
    fn mark_completed_atomic_preserves_concurrent_updates() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("state.json");
        let workers = 8usize;
        let barrier = Arc::new(Barrier::new(workers));

        let mut handles = Vec::new();
        for i in 0..workers {
            let barrier = Arc::clone(&barrier);
            let state_file = state_file.clone();
            handles.push(std::thread::spawn(move || {
                let dir = PathBuf::from(format!("/tmp/repo-{i}"));
                barrier.wait();
                mark_completed_atomic(&state_file, "claude", &dir)
                    .expect("atomic mark should succeed");
            }));
        }

        for handle in handles {
            handle.join().expect("worker thread should join");
        }

        let state = State::load(&state_file);
        let map = state
            .agents
            .get("claude")
            .expect("agent entry should exist after updates");
        assert_eq!(map.len(), workers);
        for i in 0..workers {
            let key = format!("/tmp/repo-{i}");
            assert!(map.contains_key(&key), "missing key: {key}");
        }
    }

    #[test]
    fn state_path_resolves_relative_config_to_absolute_path() {
        let old_cwd = std::env::current_dir().expect("cwd should be available");
        let tmp = TempDir::new().expect("temp dir should be created");
        std::env::set_current_dir(tmp.path()).expect("should switch cwd");

        let cwd = std::env::current_dir().expect("cwd should be available");
        let path = state_path(Path::new("cfg/config.toml"));
        assert_eq!(path, cwd.join("cfg").join("state.json"));
        assert!(path.is_absolute());

        std::env::set_current_dir(old_cwd).expect("should restore cwd");
    }

    #[test]
    fn state_path_preserves_absolute_config_base() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let abs_config = tmp.path().join("cfg").join("config.toml");
        let path = state_path(&abs_config);
        assert_eq!(path, tmp.path().join("cfg").join("state.json"));
    }

    #[test]
    fn parse_duration_empty_string_rejected() {
        let err = parse_duration("").expect_err("空文字列は拒否されるべき");
        assert!(err.to_string().contains("Duration must be positive"));
    }

    #[test]
    fn parse_duration_unit_without_number_rejected() {
        let err = parse_duration("d").expect_err("数値なし単位は拒否されるべき");
        assert!(err.to_string().contains("Invalid duration number"));
    }

    #[test]
    fn parse_duration_repeated_units_accumulate() {
        let d = parse_duration("1d1d").expect("同一単位の繰り返しは許容されるべき");
        assert_eq!(d.num_seconds(), 172_800); // 2日分
    }

    #[test]
    fn state_load_malformed_json_returns_default() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("state.json");
        std::fs::write(&state_file, "not valid json").expect("ファイル書き込み成功");
        let state = State::load(&state_file);
        assert!(state.agents.is_empty());
    }

    #[test]
    fn state_load_nonexistent_file_returns_default() {
        let state = State::load(Path::new("/nonexistent/state.json"));
        assert!(state.agents.is_empty());
    }

    #[test]
    fn mark_completed_updates_timestamp() {
        let mut state = State::default();
        let dir = Path::new("/tmp/test-repo");
        state.mark_completed("claude", dir);

        let ts = state.last_processed("claude", dir);
        assert!(ts.is_some(), "処理済みタイムスタンプが記録されるべき");
    }

    #[test]
    fn last_processed_returns_none_for_unknown_agent() {
        let state = State::default();
        assert!(
            state
                .last_processed("unknown-agent", Path::new("/tmp/repo"))
                .is_none()
        );
    }

    #[test]
    fn last_processed_returns_none_for_unknown_directory() {
        let mut state = State::default();
        state.mark_completed("claude", Path::new("/tmp/repo-a"));
        assert!(
            state
                .last_processed("claude", Path::new("/tmp/repo-b"))
                .is_none()
        );
    }

    #[test]
    fn parse_duration_single_day() {
        let d = parse_duration("7d").expect("7日はパースできるべき");
        assert_eq!(d.num_seconds(), 604_800);
    }

    #[test]
    fn parse_duration_single_hour() {
        let d = parse_duration("24h").expect("24時間はパースできるべき");
        assert_eq!(d.num_seconds(), 86_400);
    }

    #[test]
    fn parse_duration_single_minute() {
        let d = parse_duration("30m").expect("30分はパースできるべき");
        assert_eq!(d.num_seconds(), 1_800);
    }

    #[test]
    fn parse_duration_single_second() {
        let d = parse_duration("1s").expect("1秒はパースできるべき");
        assert_eq!(d.num_seconds(), 1);
    }

    #[test]
    fn parse_duration_rejects_spaces() {
        // スペースを含む期間文字列は拒否される
        let err = parse_duration("1 d").expect_err("スペース入り期間は拒否されるべき");
        assert!(err.to_string().contains("Invalid duration"));
    }

    #[test]
    fn parse_duration_rejects_decimal() {
        // 小数値は拒否される
        let err = parse_duration("1.5d").expect_err("小数入り期間は拒否されるべき");
        assert!(err.to_string().contains("Invalid duration"));
    }

    #[test]
    fn parse_duration_rejects_uppercase_units() {
        // 大文字単位は拒否される
        let err = parse_duration("1D").expect_err("大文字単位は拒否されるべき");
        assert!(err.to_string().contains("Invalid duration unit"));
    }

    #[test]
    fn mark_completed_atomic_creates_parent_dirs() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("nested").join("dir").join("state.json");
        mark_completed_atomic(&state_file, "agent", Path::new("/tmp/repo"))
            .expect("親ディレクトリが自動作成されるべき");
        assert!(state_file.exists());
    }

    #[test]
    fn state_roundtrip_serialization() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("state.json");

        // 書き込み
        mark_completed_atomic(&state_file, "claude", Path::new("/tmp/repo-a"))
            .expect("書き込み成功");
        mark_completed_atomic(&state_file, "codex", Path::new("/tmp/repo-b"))
            .expect("書き込み成功");

        // 読み込み
        let state = State::load(&state_file);
        assert!(
            state
                .last_processed("claude", Path::new("/tmp/repo-a"))
                .is_some()
        );
        assert!(
            state
                .last_processed("codex", Path::new("/tmp/repo-b"))
                .is_some()
        );
        assert!(
            state
                .last_processed("claude", Path::new("/tmp/repo-b"))
                .is_none()
        );
    }
}
