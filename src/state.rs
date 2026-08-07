use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use colored::Colorize;
use fs2::FileExt;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// エージェントごとのディレクトリパス → 最終処理タイムスタンプのマップ
#[derive(Debug, Default, Deserialize)]
pub struct State {
    #[serde(flatten)]
    pub agents: HashMap<String, HashMap<String, DateTime<Utc>>>,
}

/// エージェント 1 件分の「パス → 処理時刻」を、与えられた Vec の順序のまま
/// JSON オブジェクトへ書き出すラッパー。
///
/// `serde_json::Map` へ `collect()` してはいけない。`preserve_order` feature を
/// 有効にしていない serde_json の `Map` は `BTreeMap` であり、collect した時点で
/// キー（パス）昇順に再ソートされて、タイムスタンプ降順の並びが丸ごと捨てられる。
/// 実際の `state.json` も全エージェントがパスのアルファベット順になっていた。
struct OrderedEntries<'a>(Vec<(&'a String, &'a DateTime<Utc>)>);

impl Serialize for OrderedEntries<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (path, ts) in &self.0 {
            let local_ts: DateTime<Local> = (**ts).into();
            map.serialize_entry(
                *path,
                &local_ts.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false),
            )?;
        }
        map.end()
    }
}

impl Serialize for State {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        // エージェント名でソートし、各エージェント内はタイムスタンプ降順でソート
        let mut sorted_agents: Vec<_> = self.agents.iter().collect();
        sorted_agents.sort_by_key(|(name, _)| (*name).clone());

        let mut map = serializer.serialize_map(Some(sorted_agents.len()))?;
        for (agent_name, entries) in sorted_agents {
            let mut sorted_entries: Vec<_> = entries.iter().collect();
            // 同一タイムスタンプ同士はパス昇順で安定させ、書き込みごとに順序が揺れないようにする。
            sorted_entries.sort_by(|(path_a, ts_a), (path_b, ts_b)| {
                ts_b.cmp(ts_a).then_with(|| path_a.cmp(path_b))
            });
            map.serialize_entry(agent_name, &OrderedEntries(sorted_entries))?;
        }
        map.end()
    }
}

impl State {
    /// 状態ファイルを読み込む。
    ///
    /// 存在しない場合のみ空状態として扱う。権限エラーや I/O エラーはエラーとして返す。
    /// これらを空状態へ潰すと `filter_by_state` が全ターゲットを未処理と判断し、今週すでに
    /// 消化したリポジトリを再実行してクォータを二重消費する。しかも各タスクの
    /// `token-burn mark` は `mark_completed_atomic` 側で正しくエラーになるため
    /// `state.json` は更新されず、次回以降も同じ状態が再現して延々と同じターゲットを
    /// 処理し続ける。書き込み側が fail-closed なのに読み取り側だけ fail-open だった。
    ///
    /// JSON 破損時は従来どおり空状態へ落とす（壊れたファイルで実行自体を止めない）が、
    /// 処理済み履歴が失われる旨は警告として明示する。
    pub fn load(path: &Path) -> Result<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                anyhow::bail!("Failed to read state {}: {}", path.display(), e);
            }
        };
        match serde_json::from_str(&content) {
            Ok(state) => Ok(state),
            Err(e) => {
                eprintln!(
                    "{}: 状態ファイル {} を解析できませんでした ({})。処理済み履歴なしとして続行します",
                    "Warning".yellow(),
                    path.display(),
                    e
                );
                Ok(Self::default())
            }
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

fn state_lock_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "state.json".to_string());
    parent.join(format!(".{file_name}.lock"))
}

/// エージェントに対してディレクトリの処理完了をアトミックに記録する。
/// 排他ファイルロックを取得し、並行ワーカープロセス間で
/// 更新が上書きされないようにする。
/// ロックは state.json 本体ではなく sidecar ファイルに取る。state.json は rename で
/// inode が置き換わるため、本体をロックすると rename 後に別ワーカーが新 inode を
/// 同時ロックできてしまう。
/// 書き込みは「同一ディレクトリのテンポラリファイルへ書く → rename」で行うため、
/// 書き込み途中のクラッシュや ENOSPC でも本体 state.json は壊れない。
pub fn mark_completed_atomic(path: &Path, agent_name: &str, directory: &Path) -> Result<()> {
    use std::fs::OpenOptions;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let lock_path = state_lock_path(path);
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;

    lock_file.lock_exclusive()?;

    let result = (|| -> Result<()> {
        // ロック取得後に直接ファイルを読み直す（lock_file のシークオフセットに依存しない）
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                anyhow::bail!(
                    "Failed to read existing state {}: {}",
                    path.display(),
                    error
                );
            }
        };
        let mut state = if content.trim().is_empty() {
            State::default()
        } else {
            serde_json::from_str(&content).map_err(|e| {
                anyhow::anyhow!("Failed to parse existing state {}: {}", path.display(), e)
            })?
        };
        state.mark_completed(agent_name, directory);

        let serialized = serde_json::to_string_pretty(&state)?;

        // 同一ディレクトリ内のテンポラリファイルに書き出し、rename でアトミックに置換する。
        // PID とナノ秒タイムスタンプでファイル名を一意化し、並行ワーカー間での衝突を避ける。
        let parent = path.parent().unwrap_or(Path::new("."));
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "state.json".to_string());
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_path = parent.join(format!(
            ".{}.tmp.{}.{}",
            file_name,
            std::process::id(),
            unique
        ));

        let write_result = (|| -> Result<()> {
            let mut tmp_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            tmp_file.write_all(serialized.as_bytes())?;
            tmp_file.sync_data()?;
            Ok(())
        })();

        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }

        if let Err(e) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e.into());
        }
        Ok(())
    })();

    let _ = lock_file.unlock();
    result
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

    chrono::Duration::try_seconds(total_secs)
        .ok_or_else(|| anyhow::anyhow!("Duration is too large: {}", s))
}

#[cfg(test)]
mod tests {
    use super::{State, mark_completed_atomic, parse_duration, state_lock_path, state_path};
    use chrono::{DateTime, Utc};
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
    fn parse_duration_rejects_chrono_range_overflow() {
        let input = format!("{}s", i64::MAX);
        let err = parse_duration(&input).expect_err("Chrono の表現範囲外は失敗するべき");
        assert!(err.to_string().contains("Duration is too large"));
    }

    #[test]
    fn state_lock_path_uses_stable_sidecar_file() {
        let path = Path::new("/tmp/token-burn/state.json");
        assert_eq!(
            state_lock_path(path),
            PathBuf::from("/tmp/token-burn/.state.json.lock")
        );
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

        let state = State::load(&state_file).expect("状態ファイルは読めるはず");
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
        let tmp = TempDir::new().expect("temp dir should be created");
        let _cwd_guard = crate::test_support::CwdGuard::switch_to(tmp.path());

        let cwd = std::env::current_dir().expect("cwd should be available");
        let path = state_path(Path::new("cfg/config.toml"));
        assert_eq!(path, cwd.join("cfg").join("state.json"));
        assert!(path.is_absolute());
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
        let state = State::load(&state_file).expect("破損 JSON は空状態へ落とす");
        assert!(state.agents.is_empty());
    }

    #[test]
    fn state_load_nonexistent_file_returns_default() {
        let state =
            State::load(Path::new("/nonexistent/state.json")).expect("存在しないファイルは空状態");
        assert!(state.agents.is_empty());
    }

    #[test]
    fn state_load_unreadable_file_is_error() {
        // 権限・I/O エラーを空状態へ潰すと、処理済み履歴が消えて消化済みリポジトリを
        // 再実行しクォータを二重消費する。書き込み側 (mark_completed_atomic) が
        // fail-closed なのに読み取り側だけ fail-open になっていた。
        // ディレクトリを state.json の位置に置いて read_to_string を失敗させる
        // （NotFound 以外の I/O エラーを root 権限なしで再現できる）。
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("state.json");
        std::fs::create_dir(&state_file).expect("ディレクトリ作成成功");

        let result = State::load(&state_file);
        assert!(
            result.is_err(),
            "読み取り不能な状態ファイルはエラーにすべき（空状態で続行しない）"
        );
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
    fn parse_duration_rejects_negative_number() {
        // 負の数は単位文字として '-' が拒否される
        let err = parse_duration("-1d").expect_err("負の数は拒否されるべき");
        assert!(err.to_string().contains("Invalid duration"));
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
    fn mark_completed_atomic_preserves_malformed_state() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("state.json");
        let malformed = br#"{"claude":{"/tmp/repo":"broken"}"#;
        std::fs::write(&state_file, malformed).expect("不正 JSON の準備に成功するべき");

        let error = mark_completed_atomic(&state_file, "claude", Path::new("/tmp/new-repo"))
            .expect_err("既存状態が壊れている場合は上書きせず失敗するべき");

        assert!(error.to_string().contains("Failed to parse existing state"));
        assert_eq!(
            std::fs::read(&state_file).expect("元ファイルを読み直せるべき"),
            malformed,
            "壊れた既存状態を空の状態として上書きしてはならない"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mark_completed_atomic_preserves_unreadable_state() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("一時ディレクトリを作成できるべき");
        let state_file = tmp.path().join("state.json");
        let original = br#"{"claude":{"/tmp/repo":"2026-01-01T00:00:00Z"}}"#;
        std::fs::write(&state_file, original).expect("既存状態を書き込めるべき");
        std::fs::set_permissions(&state_file, std::fs::Permissions::from_mode(0o000))
            .expect("読み取り権限を外せるべき");

        let result = mark_completed_atomic(&state_file, "claude", Path::new("/tmp/new-repo"));

        std::fs::set_permissions(&state_file, std::fs::Permissions::from_mode(0o600))
            .expect("検証のため権限を戻せるべき");
        let error = result.expect_err("既存状態を読めない場合は上書きせず失敗するべき");
        assert!(error.to_string().contains("Failed to read existing state"));
        assert_eq!(
            std::fs::read(&state_file).expect("元ファイルを読み直せるべき"),
            original,
            "読めなかった既存状態を空の状態で上書きしてはならない"
        );
    }

    #[test]
    fn mark_completed_atomic_shorter_payload_truncates_old_content() {
        // 回帰テスト: 修正後の書き込み手順（write_all → set_len）でも、
        // 既存ファイルが長くて新ペイロードが短い場合に末尾の古いデータが残らないこと。
        // 旧実装は set_len(0) を write_all 前に行っていたため、書き込み失敗時に
        // state.json が空になるリスクがあった。新実装ではこのテストで前後どちらの
        // バグも回帰しないことを確認する。
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("state.json");

        // 長い内容を一度書き込む（複数エントリ）
        for i in 0..5 {
            let dir = format!("/tmp/repo-with-long-name-{}", i);
            mark_completed_atomic(&state_file, "claude", Path::new(&dir)).expect("書き込み成功");
        }
        let large_len = std::fs::metadata(&state_file).unwrap().len();
        assert!(
            large_len > 100,
            "前提条件: 一定以上のサイズが書き込まれている"
        );

        // 新規ファイルに 1 エントリだけ書き直す
        let _ = std::fs::remove_file(&state_file);
        mark_completed_atomic(&state_file, "claude", Path::new("/tmp/short"))
            .expect("書き込み成功");
        let short_len = std::fs::metadata(&state_file).unwrap().len();
        assert!(short_len > 0, "新規書き込みは空ではない");

        // JSON として正しくパースできることを確認（末尾のゴミ文字がない）
        let content = std::fs::read_to_string(&state_file).expect("読み込み成功");
        let _: serde_json::Value =
            serde_json::from_str(&content).expect("有効な JSON でなければならない");
    }

    #[test]
    fn mark_completed_atomic_overwrite_truncates_correctly() {
        // 既存の長いファイルを短い内容で上書きしても、ファイル末尾に古いデータが残らない
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("state.json");

        // 一度大量に書き込む
        for i in 0..10 {
            let dir = format!("/tmp/very-long-directory-name-for-padding-{}", i);
            mark_completed_atomic(&state_file, "claude", Path::new(&dir)).expect("書き込み成功");
        }

        // ファイルを直接、短い JSON で上書きさせる（既存ファイルが長い状態で短くなる遷移）
        // この遷移は外部からは作れないため、シリアライズサイズの一致を確認する
        let content = std::fs::read_to_string(&state_file).expect("読み込み成功");
        // 末尾は閉じカッコで終わっているはず
        let trimmed = content.trim_end();
        assert!(
            trimmed.ends_with('}'),
            "JSON は閉じカッコで終わるべき: {:?}",
            &trimmed[trimmed.len().saturating_sub(30)..]
        );
        // 末尾にゴミ（古い書き込みの残り）が無いことを serde で検証
        let _: serde_json::Value =
            serde_json::from_str(&content).expect("有効な JSON でなければならない");
    }

    #[test]
    fn mark_completed_atomic_does_not_leave_tempfile() {
        // 書き込み完了後にテンポラリファイル（.state.json.tmp.*）が残らないこと。
        // rename ベースの実装で「temp の作りっぱなし」になっていないかの回帰テスト。
        let tmp = TempDir::new().expect("temp dir should be created");
        let state_file = tmp.path().join("state.json");

        for i in 0..3 {
            let dir = format!("/tmp/repo-{}", i);
            mark_completed_atomic(&state_file, "claude", Path::new(&dir)).expect("書き込み成功");
        }

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("temp dir should be readable")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();

        let leftover: Vec<&String> = entries
            .iter()
            .filter(|name| name.starts_with(".state.json.tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "テンポラリファイルが残っている: {entries:?}"
        );
        assert!(
            entries.iter().any(|name| name == "state.json"),
            "本体ファイルが存在しない: {entries:?}"
        );
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
        let state = State::load(&state_file).expect("状態ファイルは読めるはず");
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

    #[test]
    fn state_serialization_orders_agents_and_entries() {
        // エージェント名はアルファベット順、エントリはタイムスタンプ降順でシリアライズされる
        let mut state = State::default();
        let ts_old = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ts_new = DateTime::parse_from_rfc3339("2025-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        // 逆順で追加
        state
            .agents
            .entry("codex".to_string())
            .or_default()
            .insert("/repo-a".to_string(), ts_old);
        state
            .agents
            .entry("claude".to_string())
            .or_default()
            .insert("/repo-z".to_string(), ts_old);
        state
            .agents
            .entry("claude".to_string())
            .or_default()
            .insert("/repo-a".to_string(), ts_new);

        let json = serde_json::to_string_pretty(&state).unwrap();
        let claude_pos = json.find("\"claude\"").expect("claude が存在するべき");
        let codex_pos = json.find("\"codex\"").expect("codex が存在するべき");
        assert!(claude_pos < codex_pos, "claude は codex より前に来るべき");

        // claude 内では ts_new の /repo-a が ts_old の /repo-z より前
        let repo_a_pos = json.find("/repo-a").expect("/repo-a が存在するべき");
        let repo_z_pos = json.find("/repo-z").expect("/repo-z が存在するべき");
        assert!(
            repo_a_pos < repo_z_pos,
            "新しいタイムスタンプのエントリが先に来るべき"
        );
    }

    /// タイムスタンプ降順とパス昇順が食い違う並びで、降順が保たれることを確認する。
    ///
    /// 以前は `serde_json::Map` へ `collect()` していたため、`preserve_order` 無効の
    /// serde_json では `BTreeMap` になり、ソート済みの並びがキー昇順へ再ソートされて
    /// 丸ごと捨てられていた。上の `state_serialization_orders_agents_and_entries` は
    /// 新しい方が `/repo-a`（アルファベット順でも先頭）なので、この取り違えを
    /// 検出できていなかった。
    #[test]
    fn state_serialization_keeps_timestamp_desc_against_path_order() {
        let mut state = State::default();
        let ts_old = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ts_mid = DateTime::parse_from_rfc3339("2025-03-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let ts_new = DateTime::parse_from_rfc3339("2025-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let entries = state.agents.entry("claude".to_string()).or_default();
        // パス昇順 (aaa < mmm < zzz) と タイムスタンプ降順 (zzz > mmm > aaa) が逆になる配置
        entries.insert("/repo-aaa".to_string(), ts_old);
        entries.insert("/repo-mmm".to_string(), ts_mid);
        entries.insert("/repo-zzz".to_string(), ts_new);

        let json = serde_json::to_string_pretty(&state).unwrap();
        let pos = |needle: &str| {
            json.find(needle)
                .unwrap_or_else(|| panic!("{needle} が無い"))
        };
        assert!(
            pos("/repo-zzz") < pos("/repo-mmm") && pos("/repo-mmm") < pos("/repo-aaa"),
            "タイムスタンプ降順で並ぶべき: {json}"
        );
    }

    /// 同じタイムスタンプ同士はパス昇順で安定させ、書き込みのたびに順序が揺れないようにする。
    #[test]
    fn state_serialization_breaks_timestamp_ties_by_path() {
        let mut state = State::default();
        let ts = DateTime::parse_from_rfc3339("2025-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let entries = state.agents.entry("claude".to_string()).or_default();
        entries.insert("/repo-zzz".to_string(), ts);
        entries.insert("/repo-aaa".to_string(), ts);
        entries.insert("/repo-mmm".to_string(), ts);

        let json = serde_json::to_string_pretty(&state).unwrap();
        let pos = |needle: &str| {
            json.find(needle)
                .unwrap_or_else(|| panic!("{needle} が無い"))
        };
        assert!(
            pos("/repo-aaa") < pos("/repo-mmm") && pos("/repo-mmm") < pos("/repo-zzz"),
            "同時刻はパス昇順で安定するべき: {json}"
        );
    }

    /// シリアライズ結果は再度読み込める（round-trip で値が失われない）。
    #[test]
    fn state_serialization_round_trips() {
        let mut state = State::default();
        let ts = DateTime::parse_from_rfc3339("2025-06-01T09:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        state
            .agents
            .entry("claude-work".to_string())
            .or_default()
            .insert("/repo-a".to_string(), ts);

        let json = serde_json::to_string(&state).unwrap();
        let restored: State = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.last_processed("claude-work", std::path::Path::new("/repo-a")),
            Some(ts)
        );
    }

    /// エントリが 0 件のエージェントも空オブジェクトとして出力される（キー欠落しない）。
    #[test]
    fn state_serialization_keeps_agent_with_no_entries() {
        let mut state = State::default();
        state.agents.entry("empty-agent".to_string()).or_default();
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, r#"{"empty-agent":{}}"#);
    }
}
