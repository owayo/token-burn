//! レート制限で中断した Claude Code セッションを、次回の実行で `--resume` して続きから
//! 処理するための記録（`resume.json`）。
//!
//! レート制限で終わったタスクは `state.json` に記録されないため、次回の実行で同じ
//! ターゲットが選ばれる。以前はそこで元のプロンプトから新しいセッションを始めていたので、
//! 中断までに積み上げた調査・判断の文脈（実ログでは 95 分・327 ターン）が丸ごと失われ、
//! 同じ調査をやり直すところからトークンを使っていた。Claude Code の transcript は
//! アカウントの config dir に残っており、`claude --resume <session_id>` で同じセッション
//! ID のまま会話を引き継げる（fork しない。stream には新しいイベントだけが流れる）。
//!
//! `state.json` とは別ファイルにする。`state.json` は `#[serde(flatten)]` の
//! `HashMap<agent, HashMap<dir, time>>` なので、型の違うトップレベルキーを足すと旧バイナリが
//! 解析に失敗して空状態へ落ち、全ターゲットを再処理してクォータを二重消費する。
//! 書き込みは `state.json` と同じく sidecar ロック + テンポラリファイル → rename で行う。

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local, SubsecRound, TimeZone};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::rate_control::{self, FileLock};

/// 再開に続けて失敗してよい回数。これに達したら記録を捨て、次回は新規セッションで始める。
///
/// 1 回の失敗で捨てないのは、認証切れのようにセッションと無関係な失敗もあるため。
/// 上限を設けないと、二度と続けられないセッション（壊れた transcript 等）を実行のたびに
/// 再開しては失敗し続ける。
pub const MAX_FAILED_RESUMES: u32 = 3;

/// `resume.json` に保存する stream-json 由来のメッセージの上限（文字数）。表示用なので
/// 長大なエラー本文を丸ごと抱えない。
const MESSAGE_MAX_CHARS: usize = 300;

/// 再開時に送る既定の継続プロンプト（`[prompts] resume` で上書きできる）。
///
/// 元の指示と途中経過は会話履歴に残っているので繰り返さない。書くのは、中断のせいで
/// 履歴と実態が食い違っている点だけ: 中断時に動いていたサブエージェントや background
/// タスクは完了しておらず、完了済みの操作（commit / push 等）を繰り返してはいけない。
pub const DEFAULT_RESUME_PROMPT: &str = "\
The previous session in this repository was cut off by an API rate limit before it finished. \
This session continues that conversation: the original instructions and your earlier progress \
are in the history above.

Before continuing:
- Check the current state of the repository: `git status`, the current branch, \
`git worktree list`, and any uncommitted changes.
- Subagents and background tasks that were running when the session was interrupted have \
stopped. Do not assume they finished; check what they actually produced (files, commits, \
outputs) and redo only what is missing.
- Do not repeat operations that already completed, such as commits, pushes, or releases.

Then pick up where you left off and finish the remaining parts of the original instructions.";

/// 中断した 1 セッションの記録。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeEntry {
    /// 再開する Claude Code のセッション ID（UUID）。
    pub session_id: String,
    /// 中断を記録した時刻。
    pub interrupted_at: DateTime<FixedOffset>,
    /// これより前に再開しても同じ枠で拒否される時刻（5 時間枠のリセット + 猶予）。
    /// 構造化された `rate_limit_event` から読めたときだけ持つ。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<DateTime<FixedOffset>>,
    /// 中断したタスクの元の実効プロンプトのハッシュ（`fnv1a64:<16 桁>`）。
    pub prompt_hash: String,
    /// 中断理由のメッセージ（表示用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 中断した実行の jsonl（表示・追跡用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<PathBuf>,
    /// 再開してレート制限以外の理由で失敗した回数。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failed_resumes: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl ResumeEntry {
    /// 表示用の短いセッション ID（先頭 8 文字）。
    pub fn short_id(&self) -> &str {
        short_session_id(&self.session_id)
    }

    /// `rate limited 2026-09-23 13:04` 形式の中断の要約。
    pub fn summary(&self) -> String {
        format!(
            "rate limited {}",
            self.interrupted_at
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
        )
    }
}

/// UUID の先頭ブロック（8 文字）。短すぎる ID はそのまま返す。
pub fn short_session_id(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}

/// `resume.json` 全体。エージェント展開名 → 絶対ディレクトリ → 記録。
///
/// エージェント単位で分けるのは、transcript がそのアカウントの `CLAUDE_CONFIG_DIR` に
/// あり、別アカウントからは再開できないため。
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResumeStore {
    #[serde(default)]
    sessions: BTreeMap<String, BTreeMap<String, ResumeEntry>>,
}

impl ResumeStore {
    /// 計画用に読み込む。
    ///
    /// 存在しなければ空。読めない / 壊れている場合も警告を出して空として扱う。再開は
    /// 中断した文脈を引き継ぐための最適化であり、読めなくても新規セッションで始まる
    /// だけで処理済み判定は狂わない（`state.json` と違い、実行自体を止める理由にならない）。
    pub fn load_or_warn(path: &Path) -> Self {
        match read_store(path) {
            Ok(store) => store,
            Err(e) => {
                eprintln!(
                    "{}: {:#} — interrupted sessions will not be resumed in this run",
                    "Warning".yellow(),
                    e
                );
                Self::default()
            }
        }
    }

    pub fn get(&self, agent_name: &str, directory: &Path) -> Option<&ResumeEntry> {
        self.sessions
            .get(agent_name)
            .and_then(|entries| entries.get(&directory_key(directory)))
    }

    /// 指定エージェントの記録を持つディレクトリの一覧（テスト・診断用）。
    #[cfg(test)]
    fn directories_of(&self, agent_name: &str) -> Vec<String> {
        self.sessions
            .get(agent_name)
            .map(|entries| entries.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn insert(&mut self, agent_name: &str, directory: &Path, entry: ResumeEntry) {
        self.sessions
            .entry(agent_name.to_string())
            .or_default()
            .insert(directory_key(directory), entry);
    }

    fn remove(&mut self, agent_name: &str, directory: &Path) -> Option<ResumeEntry> {
        let entries = self.sessions.get_mut(agent_name)?;
        let removed = entries.remove(&directory_key(directory));
        if entries.is_empty() {
            self.sessions.remove(agent_name);
        }
        removed
    }

    fn get_mut(&mut self, agent_name: &str, directory: &Path) -> Option<&mut ResumeEntry> {
        self.sessions
            .get_mut(agent_name)
            .and_then(|entries| entries.get_mut(&directory_key(directory)))
    }
}

/// `state.json` のキーと同じ形（`to_string_lossy`）で揃える。
fn directory_key(directory: &Path) -> String {
    directory.to_string_lossy().to_string()
}

/// `state.json` と同じディレクトリに置く `resume.json` のパス。
pub fn resume_path(state_file: &Path) -> PathBuf {
    state_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("resume.json")
}

fn read_store(path: &Path) -> Result<ResumeStore> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ResumeStore::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    if content.trim().is_empty() {
        return Ok(ResumeStore::default());
    }
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

/// sidecar ロックの下で read-modify-write する。
///
/// 既存ファイルが読めない / 壊れている場合は書き戻さずにエラーにする（他の中断記録を
/// 黙って消さない）。`f` が変更なし（`false`）を返したら書き込まない。
fn with_locked_store<T>(path: &Path, f: impl FnOnce(&mut ResumeStore) -> (T, bool)) -> Result<T> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let _lock = FileLock::acquire(&rate_control::sidecar_lock_path(path))?;
    let mut store = read_store(path)?;
    let (result, changed) = f(&mut store);
    if changed {
        let serialized = serde_json::to_string_pretty(&store)?;
        crate::state::write_file_atomic(path, serialized.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(result)
}

/// 中断したセッションを保存する（同じエージェント・ディレクトリの既存記録は置き換える）。
///
/// 無条件に置き換えてよいのは、同じターゲットを 2 つの試行が同時に処理しないため。
/// 1 回の実行の中ではターゲットは重複排除されて 1 ワーカーだけが扱い、実行どうしも
/// 並行しない（`execute_plan_tmux` は起動時に既存の token-burn セッションを終了させ、
/// 一時ディレクトリも作り直す。終了させられたワーカーのシェルは後続の保存まで進まない）。
/// 前提が崩れる構成（同じ設定ディレクトリを共有する並行実行）を持ち込むなら、ここと
/// `mark` の削除を世代付きの条件付き更新にする必要がある。
pub fn save_atomic(
    path: &Path,
    agent_name: &str,
    directory: &Path,
    entry: ResumeEntry,
) -> Result<()> {
    with_locked_store(path, |store| {
        store.insert(agent_name, directory, entry);
        ((), true)
    })
}

/// 記録を消す。`session` を渡したときは、保存されている ID と一致する場合だけ消す
/// （遅れて終わった試行が、その後に保存された別セッションの記録を消さないように）。
///
/// `resume.json` が無ければ何もしない（ロックファイルも作らない）。codex など再開と
/// 無関係なエージェントの `mark` でも呼ばれるため。戻り値は実際に消したかどうか。
pub fn forget_atomic(
    path: &Path,
    agent_name: &str,
    directory: &Path,
    session: Option<&str>,
) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    with_locked_store(path, |store| {
        let matches = store
            .get(agent_name, directory)
            .is_some_and(|entry| session.is_none_or(|id| entry.session_id == id));
        if matches {
            store.remove(agent_name, directory);
        }
        (matches, matches)
    })
}

/// 再開の失敗を数えた結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureOutcome {
    /// 失敗を数えた（まだ上限未満なので次回も再開する）。値は累計回数。
    Counted(u32),
    /// 上限に達したので記録を捨てた。次回は新規セッションで始まる。
    Dropped,
    /// 対象の記録が無い（別セッションに置き換わった等）。
    Missing,
}

/// 再開したセッションがレート制限以外の理由で失敗したことを記録する。
pub fn record_failure_atomic(
    path: &Path,
    agent_name: &str,
    directory: &Path,
    session: &str,
) -> Result<FailureOutcome> {
    if !path.exists() {
        return Ok(FailureOutcome::Missing);
    }
    with_locked_store(path, |store| {
        let Some(entry) = store
            .get_mut(agent_name, directory)
            .filter(|entry| entry.session_id == session)
        else {
            return (FailureOutcome::Missing, false);
        };
        entry.failed_resumes = entry.failed_resumes.saturating_add(1);
        if entry.failed_resumes >= MAX_FAILED_RESUMES {
            store.remove(agent_name, directory);
            (FailureOutcome::Dropped, true)
        } else {
            (FailureOutcome::Counted(entry.failed_resumes), true)
        }
    })
}

/// 元の実効プロンプトのハッシュ。
///
/// std の `DefaultHasher` はアルゴリズムが Rust のリリース間で保証されないため、
/// 永続化するハッシュには使えない。暗号学的な強度は要らない（同一性の確認だけ）ので、
/// 依存を増やさず FNV-1a（64bit）を使い、アルゴリズム名を前置して将来の変更に備える。
pub fn prompt_hash(prompt: &str) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = prompt.as_bytes().iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    });
    format!("fnv1a64:{hash:016x}")
}

/// Claude Code のセッション ID（UUID 形式）か。
///
/// `claude --resume` は値が UUID でなければ検索語として扱い、対話的なピッカーを開こうと
/// する。無人実行でそれを踏まないよう、保存時と再開時の両方で形式を確かめる。
pub fn is_valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// 中断した実行の jsonl から読み取れた情報。
#[derive(Debug, Default, PartialEq)]
pub struct Interruption {
    /// 最後の `result`（無ければ `system/init`）の session_id。UUID でなければ `None`。
    pub session_id: Option<String>,
    /// 5 時間枠の拒否から読めた再開可能時刻（Unix epoch 秒、猶予込み）。
    pub retry_after: Option<i64>,
    /// 最後の `result` のメッセージ。
    pub message: Option<String>,
}

/// jsonl の内容から、再開に必要な情報を取り出す。`now` は Unix epoch 秒。
pub fn inspect_interruption(content: &str, now: i64) -> Interruption {
    let mut result_session: Option<String> = None;
    let mut init_session: Option<String> = None;
    let mut message: Option<String> = None;
    let mut last_rejected: Option<serde_json::Value> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("result") => {
                result_session = v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
                message = crate::classify::result_message(&v).filter(|m| !m.is_empty());
            }
            Some("system") if v.get("subtype").and_then(|s| s.as_str()) == Some("init") => {
                if let Some(id) = v.get("session_id").and_then(|s| s.as_str()) {
                    init_session = Some(id.to_string());
                }
            }
            Some("rate_limit_event")
                if v["rate_limit_info"]["status"].as_str() == Some("rejected") =>
            {
                last_rejected = Some(v);
            }
            _ => {}
        }
    }

    Interruption {
        session_id: result_session
            .filter(|id| is_valid_session_id(id))
            .or_else(|| init_session.filter(|id| is_valid_session_id(id))),
        retry_after: last_rejected.and_then(|v| five_hour_retry_after(&v, now)),
        message: message.map(|m| truncate_message(&m)),
    }
}

/// 5 時間枠による拒否なら、その枠のリセット時刻（+ 猶予）を返す。
///
/// 自然言語のメッセージ（`resets 2:30pm`）は解析しない。構造化された `resetsAt` だけを
/// 根拠にする。overage を使っている拒否は月次の追加課金枠まで使い切った状態で、5 時間枠が
/// リセットされても再開できない（`format_stream` の一時停止判定と同じ理由）ため対象外。
/// 枠 1 周期より先を指す値や、とうに過ぎた値は壊れているか枠の取り違えなので使わない。
fn five_hour_retry_after(event: &serde_json::Value, now: i64) -> Option<i64> {
    let info = &event["rate_limit_info"];
    if info["rateLimitType"].as_str()? != "five_hour" {
        return None;
    }
    if info["isUsingOverage"].as_bool() == Some(true)
        || info["overageInUse"].as_bool() == Some(true)
    {
        return None;
    }
    let reset_at = info["unifiedWindows"]["five_hour"]["resetsAt"]
        .as_i64()
        .or_else(|| info["resetsAt"].as_i64())?;
    let max_ahead = 5 * 3600 + rate_control::RESET_SKEW_MARGIN_SECS;
    if reset_at > now + max_ahead {
        return None;
    }
    let retry_after = reset_at + rate_control::RESET_PROPAGATION_GRACE_SECS;
    (retry_after > now).then_some(retry_after)
}

fn truncate_message(message: &str) -> String {
    let single_line = message.replace(['\n', '\r'], " ");
    if single_line.chars().count() <= MESSAGE_MAX_CHARS {
        return single_line;
    }
    let kept: String = single_line.chars().take(MESSAGE_MAX_CHARS - 3).collect();
    format!("{kept}...")
}

/// `resume-entry save` の本体。jsonl から session_id を取り出して保存する。
///
/// 戻り値は保存した記録。session_id が読めなければエラー（再開できないので保存しない）。
pub fn save_from_jsonl(
    path: &Path,
    agent_name: &str,
    directory: &Path,
    jsonl: &Path,
    prompt_file: &Path,
) -> Result<ResumeEntry> {
    let content = std::fs::read_to_string(jsonl)
        .with_context(|| format!("failed to read {}", jsonl.display()))?;
    let prompt = std::fs::read_to_string(prompt_file)
        .with_context(|| format!("failed to read {}", prompt_file.display()))?;
    let now = Local::now().fixed_offset().trunc_subsecs(0);
    let interruption = inspect_interruption(&content, now.timestamp());
    let session_id = interruption.session_id.with_context(|| {
        format!(
            "no Claude Code session id found in {} — the session cannot be resumed",
            jsonl.display()
        )
    })?;
    let entry = ResumeEntry {
        session_id,
        interrupted_at: now,
        retry_after: interruption
            .retry_after
            .and_then(|epoch| Local.timestamp_opt(epoch, 0).single())
            .map(|t| t.fixed_offset()),
        prompt_hash: prompt_hash(&prompt),
        message: interruption.message,
        log: Some(jsonl.to_path_buf()),
        // 同じセッションの再保存でも失敗回数は持ち越さない。レート制限まで走れたこと
        // 自体が、そのセッションを続けられる（transcript を読み込めて作業が進む）証拠で、
        // 上限（MAX_FAILED_RESUMES）が止めたいのは「毎回すぐ落ちて進まない」セッションの方。
        failed_resumes: 0,
    };
    save_atomic(path, agent_name, directory, entry.clone())?;
    Ok(entry)
}

/// 計画時に、保存済みセッションを使わない理由。
#[derive(Debug, Clone, PartialEq)]
pub enum SkipReason {
    /// この実行では再開しない（`--no-resume` / `--fresh` / 設定 / 競合フラグ）。
    Disabled(String),
    /// 中断後にプロンプトが変わった。古い指示の続きをしても意味が無い。
    PromptChanged,
    /// 中断より新しい処理済み記録がある（別エージェントが処理した等）。
    Superseded { agent_name: String },
    /// 保存された session_id が UUID 形式でない（壊れた記録）。
    InvalidSessionId,
}

impl SkipReason {
    pub fn describe(&self) -> String {
        match self {
            SkipReason::Disabled(why) => format!("resuming is disabled ({why})"),
            SkipReason::PromptChanged => "the prompt changed since the interruption".to_string(),
            SkipReason::Superseded { agent_name } => {
                format!("{agent_name} processed it after the interruption")
            }
            SkipReason::InvalidSessionId => "the saved session id is malformed".to_string(),
        }
    }
}

/// ターゲットごとの判定。
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeDecision {
    /// 保存済みセッションを再開する。
    Resume(ResumeEntry),
    /// 保存済みセッションはあるが使わない（従来どおり新規セッションで始める）。
    Skip {
        session_id: String,
        reason: SkipReason,
    },
}

impl ResumeDecision {
    /// 一覧の 2 行目に出す説明（先頭の `↻` を含む）。
    pub fn describe(&self) -> String {
        match self {
            ResumeDecision::Resume(entry) => {
                format!("↻ resume {} ({})", entry.short_id(), entry.summary())
            }
            ResumeDecision::Skip { session_id, reason } => format!(
                "↻ not resuming {}: {}",
                short_session_id(session_id),
                reason.describe()
            ),
        }
    }

    pub fn resume_entry(&self) -> Option<&ResumeEntry> {
        match self {
            ResumeDecision::Resume(entry) => Some(entry),
            ResumeDecision::Skip { .. } => None,
        }
    }
}

/// 保存済みセッションを今回再開してよいか判定する。
///
/// - `current_prompt` はそのターゲットの現在の実効プロンプト。保存時のハッシュと違えば
///   古い指示の続きになるので再開しない。
/// - `processed_after` は処理済み判定の範囲（dedup scope）で見つかった最新の処理記録。
///   中断より新しければ、そのセッションはもう古い（別エージェントが仕上げた等）。
pub fn decide(
    entry: &ResumeEntry,
    current_prompt: &str,
    latest_processed: Option<&crate::state::LastProcessed>,
) -> ResumeDecision {
    let skip = |reason| ResumeDecision::Skip {
        session_id: entry.session_id.clone(),
        reason,
    };
    if !is_valid_session_id(&entry.session_id) {
        return skip(SkipReason::InvalidSessionId);
    }
    if entry.prompt_hash != prompt_hash(current_prompt) {
        return skip(SkipReason::PromptChanged);
    }
    if let Some(last) = latest_processed
        && last.at > entry.interrupted_at
    {
        return skip(SkipReason::Superseded {
            agent_name: last.agent_name.clone(),
        });
    }
    ResumeDecision::Resume(entry.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    const SESSION: &str = "0633fa18-8643-40f7-9106-a35330ed247d";

    fn entry(session_id: &str, prompt: &str) -> ResumeEntry {
        ResumeEntry {
            session_id: session_id.to_string(),
            interrupted_at: DateTime::parse_from_rfc3339("2026-09-23T13:04:23+09:00").unwrap(),
            retry_after: None,
            prompt_hash: prompt_hash(prompt),
            message: Some("You've hit your session limit".to_string()),
            log: None,
            failed_resumes: 0,
        }
    }

    #[test]
    fn prompt_hash_matches_fnv1a_test_vectors() {
        // 公開されている FNV-1a 64bit のテストベクタ。版をまたいで値が変わらないこと。
        assert_eq!(prompt_hash(""), "fnv1a64:cbf29ce484222325");
        assert_eq!(prompt_hash("a"), "fnv1a64:af63dc4c8601ec8c");
        assert_eq!(prompt_hash("foobar"), "fnv1a64:85944171f73967e8");
    }

    #[test]
    fn session_id_must_be_a_uuid() {
        assert!(is_valid_session_id(SESSION));
        assert!(is_valid_session_id("00000000-0000-4000-8000-000000000000"));
        // 検索語として解釈されると対話ピッカーが開くため、UUID 以外は拒否する
        assert!(!is_valid_session_id("astro-sight"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("0633fa18-8643-40f7-9106-a35330ed247"));
        assert!(!is_valid_session_id("0633fa18_8643_40f7_9106_a35330ed247d"));
        assert!(!is_valid_session_id("0633fa18-8643-40f7-9106-a35330ed247z"));
    }

    #[test]
    fn resume_path_is_a_sibling_of_state_json() {
        assert_eq!(
            resume_path(Path::new("/home/u/.config/token-burn/state.json")),
            PathBuf::from("/home/u/.config/token-burn/resume.json")
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let dir = Path::new("/tmp/repo");
        save_atomic(&path, "claude", dir, entry(SESSION, "review")).unwrap();

        let store = ResumeStore::load_or_warn(&path);
        assert_eq!(store.get("claude", dir), Some(&entry(SESSION, "review")));
        // 別エージェントからは見えない（transcript は別アカウントの config dir にある）
        assert_eq!(store.get("claude-home", dir), None);

        // 時刻はローカルのオフセット付き RFC3339 で書かれ、人が読める
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"interrupted_at\": \"2026-09-23T13:04:23+09:00\""),
            "{raw}"
        );
        // 既定値のフィールドは書かない
        assert!(!raw.contains("failed_resumes"), "{raw}");
        assert!(!raw.contains("retry_after"), "{raw}");
    }

    #[test]
    fn save_replaces_the_previous_entry_for_the_same_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let dir = Path::new("/tmp/repo");
        let mut first = entry(SESSION, "review");
        first.failed_resumes = 2;
        save_atomic(&path, "claude", dir, first).unwrap();
        let second = entry("11111111-2222-4333-8444-555555555555", "review");
        save_atomic(&path, "claude", dir, second.clone()).unwrap();

        let store = ResumeStore::load_or_warn(&path);
        assert_eq!(store.get("claude", dir), Some(&second));
    }

    #[test]
    fn load_treats_missing_and_broken_files_as_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("resume.json");
        assert_eq!(ResumeStore::load_or_warn(&missing), ResumeStore::default());

        std::fs::write(&missing, "{ not json").unwrap();
        assert_eq!(ResumeStore::load_or_warn(&missing), ResumeStore::default());
    }

    #[test]
    fn updates_refuse_to_overwrite_a_broken_file() {
        // 壊れた resume.json を空として上書きすると、他のターゲットの中断記録まで消える。
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = save_atomic(&path, "claude", Path::new("/tmp/repo"), entry(SESSION, "p"))
            .expect_err("broken file must not be overwritten");
        assert!(format!("{err:#}").contains("failed to parse"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn forget_only_removes_the_matching_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let dir = Path::new("/tmp/repo");
        save_atomic(&path, "claude", dir, entry(SESSION, "p")).unwrap();

        // 別セッション ID を指定した削除は、後から保存された記録を消さない
        let removed = forget_atomic(
            &path,
            "claude",
            dir,
            Some("11111111-2222-4333-8444-555555555555"),
        )
        .unwrap();
        assert!(!removed);
        assert!(
            ResumeStore::load_or_warn(&path)
                .get("claude", dir)
                .is_some()
        );

        assert!(forget_atomic(&path, "claude", dir, Some(SESSION)).unwrap());
        let store = ResumeStore::load_or_warn(&path);
        assert!(store.get("claude", dir).is_none());
        // 最後の 1 件を消したエージェントのキーも残さない
        assert!(store.directories_of("claude").is_empty());
        assert_eq!(store, ResumeStore::default());
    }

    #[test]
    fn forget_without_a_session_filter_removes_any_entry() {
        // 成功（mark）時は、どのセッションの記録であれ完了したので消す。
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let dir = Path::new("/tmp/repo");
        save_atomic(&path, "claude", dir, entry(SESSION, "p")).unwrap();
        save_atomic(
            &path,
            "claude",
            Path::new("/tmp/other"),
            entry(SESSION, "p"),
        )
        .unwrap();

        assert!(forget_atomic(&path, "claude", dir, None).unwrap());
        let store = ResumeStore::load_or_warn(&path);
        assert!(store.get("claude", dir).is_none());
        assert!(store.get("claude", Path::new("/tmp/other")).is_some());
    }

    #[test]
    fn forget_does_not_create_files_when_nothing_was_saved() {
        // codex など再開と無関係なエージェントの mark でも呼ばれるため、
        // resume.json もロックファイルも作らない。
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        assert!(!forget_atomic(&path, "codex", Path::new("/tmp/repo"), None).unwrap());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn failures_are_counted_and_the_entry_is_dropped_at_the_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let dir = Path::new("/tmp/repo");
        save_atomic(&path, "claude", dir, entry(SESSION, "p")).unwrap();

        for n in 1..MAX_FAILED_RESUMES {
            assert_eq!(
                record_failure_atomic(&path, "claude", dir, SESSION).unwrap(),
                FailureOutcome::Counted(n)
            );
        }
        assert_eq!(
            ResumeStore::load_or_warn(&path)
                .get("claude", dir)
                .unwrap()
                .failed_resumes,
            MAX_FAILED_RESUMES - 1
        );
        assert_eq!(
            record_failure_atomic(&path, "claude", dir, SESSION).unwrap(),
            FailureOutcome::Dropped
        );
        assert!(
            ResumeStore::load_or_warn(&path)
                .get("claude", dir)
                .is_none()
        );
        // 無い記録への失敗通知は何もしない
        assert_eq!(
            record_failure_atomic(&path, "claude", dir, SESSION).unwrap(),
            FailureOutcome::Missing
        );
    }

    #[test]
    fn failures_of_another_session_are_ignored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let dir = Path::new("/tmp/repo");
        save_atomic(&path, "claude", dir, entry(SESSION, "p")).unwrap();
        assert_eq!(
            record_failure_atomic(&path, "claude", dir, "11111111-2222-4333-8444-555555555555")
                .unwrap(),
            FailureOutcome::Missing
        );
        assert_eq!(
            ResumeStore::load_or_warn(&path)
                .get("claude", dir)
                .unwrap()
                .failed_resumes,
            0
        );
    }

    #[test]
    fn concurrent_saves_keep_every_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let workers = 8usize;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
        let handles: Vec<_> = (0..workers)
            .map(|i| {
                let barrier = std::sync::Arc::clone(&barrier);
                let path = path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    save_atomic(
                        &path,
                        "claude",
                        &PathBuf::from(format!("/tmp/repo-{i}")),
                        entry(SESSION, "p"),
                    )
                    .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            ResumeStore::load_or_warn(&path)
                .directories_of("claude")
                .len(),
            workers
        );
    }

    /// 実ログ（レート制限で中断した 95 分のセッション）の末尾の形。
    fn interrupted_jsonl(reset_at: i64) -> String {
        [
            "claude-wrapper: CLAUDE_CONFIG_DIR=~/.claude".to_string(),
            format!(
                r#"{{"type":"system","subtype":"init","cwd":"/tmp/repo","session_id":"{SESSION}"}}"#
            ),
            format!(
                r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"rejected","resetsAt":{reset_at},"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false,"unifiedWindows":{{"five_hour":{{"utilization":1,"resetsAt":{reset_at}}},"seven_day":{{"utilization":0.35,"resetsAt":{}}}}}}},"session_id":"{SESSION}"}}"#,
                reset_at + 170_000
            ),
            format!(
                r#"{{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"terminal_reason":"api_error","result":"You've hit your session limit · resets 2:30pm (Asia/Tokyo)","session_id":"{SESSION}","num_turns":327}}"#
            ),
            format!(
                r#"{{"type":"system","subtype":"task_notification","task_id":"b2ruqs1yu","status":"stopped","session_id":"{SESSION}"}}"#
            ),
        ]
        .join("\n")
    }

    #[test]
    fn inspect_reads_session_id_and_five_hour_reset() {
        let now = 1_790_136_263;
        let reset_at = 1_790_141_400;
        let got = inspect_interruption(&interrupted_jsonl(reset_at), now);
        assert_eq!(got.session_id.as_deref(), Some(SESSION));
        assert_eq!(
            got.retry_after,
            Some(reset_at + rate_control::RESET_PROPAGATION_GRACE_SECS)
        );
        assert_eq!(
            got.message.as_deref(),
            Some("You've hit your session limit · resets 2:30pm (Asia/Tokyo)")
        );
    }

    #[test]
    fn inspect_falls_back_to_the_init_session_id() {
        // result が出る前に落ちた場合でも、init の session_id で再開できる。
        let content = format!(
            r#"{{"type":"system","subtype":"init","session_id":"{SESSION}"}}
{{"type":"assistant","message":{{"content":[]}}}}"#
        );
        let got = inspect_interruption(&content, 0);
        assert_eq!(got.session_id.as_deref(), Some(SESSION));
        assert_eq!(got.retry_after, None);
        assert_eq!(got.message, None);
    }

    #[test]
    fn inspect_rejects_malformed_session_ids() {
        let content = r#"{"type":"result","is_error":true,"session_id":"not-a-uuid"}"#;
        assert_eq!(inspect_interruption(content, 0).session_id, None);
    }

    #[test]
    fn retry_after_ignores_implausible_or_overage_rejections() {
        let now = 1_790_136_263;
        // 5 時間枠 1 周期より先を指す値は枠の取り違え
        let far = interrupted_jsonl(now + 5 * 3600 + rate_control::RESET_SKEW_MARGIN_SECS + 60);
        assert_eq!(inspect_interruption(&far, now).retry_after, None);
        // とうに過ぎた値は待つ根拠にならない
        let past = interrupted_jsonl(now - 7200);
        assert_eq!(inspect_interruption(&past, now).retry_after, None);
        // overage を使っている拒否は 5 時間枠のリセットで回復しない
        let overage = interrupted_jsonl(now + 600)
            .replace("\"isUsingOverage\":false", "\"isUsingOverage\":true");
        assert_eq!(inspect_interruption(&overage, now).retry_after, None);
        // 週次枠の拒否は対象外（リセットは数日先で、この用途の待機にならない）
        let weekly = interrupted_jsonl(now + 600).replace(
            "\"rateLimitType\":\"five_hour\"",
            "\"rateLimitType\":\"seven_day\"",
        );
        assert_eq!(inspect_interruption(&weekly, now).retry_after, None);
    }

    #[test]
    fn save_from_jsonl_records_the_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let jsonl = tmp.path().join("0001_repo.jsonl");
        let prompt = tmp.path().join("prompt-1.txt");
        let reset_at = Utc::now().timestamp() + 1800;
        std::fs::write(&jsonl, interrupted_jsonl(reset_at)).unwrap();
        std::fs::write(&prompt, "# 進め方\n\n- [ ] リモート同期").unwrap();

        let saved =
            save_from_jsonl(&path, "claude", Path::new("/tmp/repo"), &jsonl, &prompt).unwrap();
        assert_eq!(saved.session_id, SESSION);
        assert_eq!(
            saved.prompt_hash,
            prompt_hash("# 進め方\n\n- [ ] リモート同期")
        );
        assert_eq!(
            saved.retry_after.map(|t| t.timestamp()),
            Some(reset_at + rate_control::RESET_PROPAGATION_GRACE_SECS)
        );
        assert_eq!(saved.log.as_deref(), Some(jsonl.as_path()));
        assert_eq!(
            ResumeStore::load_or_warn(&path).get("claude", Path::new("/tmp/repo")),
            Some(&saved)
        );
    }

    /// 再開後に失敗したことがあっても、同じセッションがレート制限まで走れたら失敗回数は
    /// 0 に戻る（続けられるセッションだと分かったため）。意図した挙動として固定する。
    #[test]
    fn saving_the_same_session_again_resets_the_failure_count() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let dir = Path::new("/tmp/repo");
        let jsonl = tmp.path().join("0001_repo.jsonl");
        let prompt = tmp.path().join("prompt-1.txt");
        std::fs::write(&jsonl, interrupted_jsonl(Utc::now().timestamp() + 600)).unwrap();
        std::fs::write(&prompt, "review").unwrap();

        save_from_jsonl(&path, "claude", dir, &jsonl, &prompt).unwrap();
        record_failure_atomic(&path, "claude", dir, SESSION).unwrap();
        record_failure_atomic(&path, "claude", dir, SESSION).unwrap();
        assert_eq!(
            ResumeStore::load_or_warn(&path)
                .get("claude", dir)
                .unwrap()
                .failed_resumes,
            2
        );

        let saved = save_from_jsonl(&path, "claude", dir, &jsonl, &prompt).unwrap();
        assert_eq!(saved.session_id, SESSION);
        assert_eq!(saved.failed_resumes, 0);
        assert_eq!(
            ResumeStore::load_or_warn(&path)
                .get("claude", dir)
                .unwrap()
                .failed_resumes,
            0
        );
    }

    #[test]
    fn save_from_jsonl_without_session_id_saves_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let jsonl = tmp.path().join("0001_repo.jsonl");
        let prompt = tmp.path().join("prompt-1.txt");
        std::fs::write(&jsonl, "API Error: Connection closed\n").unwrap();
        std::fs::write(&prompt, "p").unwrap();

        let err = save_from_jsonl(&path, "claude", Path::new("/tmp/repo"), &jsonl, &prompt)
            .expect_err("no session id means nothing to resume");
        assert!(
            format!("{err:#}").contains("no Claude Code session id"),
            "{err:#}"
        );
        assert!(!path.exists());
    }

    #[test]
    fn decide_resumes_only_with_the_same_prompt() {
        let saved = entry(SESSION, "review");
        assert_eq!(
            decide(&saved, "review", None),
            ResumeDecision::Resume(saved.clone())
        );
        assert_eq!(
            decide(&saved, "review everything", None),
            ResumeDecision::Skip {
                session_id: SESSION.to_string(),
                reason: SkipReason::PromptChanged,
            }
        );
    }

    #[test]
    fn decide_skips_sessions_superseded_by_a_newer_processed_record() {
        let saved = entry(SESSION, "review");
        let newer = crate::state::LastProcessed {
            agent_name: "claude-home".to_string(),
            at: saved.interrupted_at.with_timezone(&Utc) + chrono::Duration::minutes(5),
        };
        assert_eq!(
            decide(&saved, "review", Some(&newer)),
            ResumeDecision::Skip {
                session_id: SESSION.to_string(),
                reason: SkipReason::Superseded {
                    agent_name: "claude-home".to_string()
                },
            }
        );
        // 中断より前の処理記録（前の周期に完了したもの）は再開を妨げない
        let older = crate::state::LastProcessed {
            agent_name: "claude".to_string(),
            at: saved.interrupted_at.with_timezone(&Utc) - chrono::Duration::days(3),
        };
        assert!(matches!(
            decide(&saved, "review", Some(&older)),
            ResumeDecision::Resume(_)
        ));
    }

    #[test]
    fn decide_rejects_malformed_saved_ids() {
        let saved = entry("not-a-uuid", "review");
        assert!(matches!(
            decide(&saved, "review", None),
            ResumeDecision::Skip {
                reason: SkipReason::InvalidSessionId,
                ..
            }
        ));
    }

    #[test]
    fn decision_descriptions_name_the_session_and_the_reason() {
        let saved = entry(SESSION, "review");
        let resume = ResumeDecision::Resume(saved.clone()).describe();
        assert!(
            resume.starts_with("↻ resume 0633fa18 (rate limited "),
            "{resume}"
        );
        let skip = decide(&saved, "other", None).describe();
        assert_eq!(
            skip,
            "↻ not resuming 0633fa18: the prompt changed since the interruption"
        );
    }
}
