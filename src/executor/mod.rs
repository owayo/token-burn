use anyhow::{Context, Result};
#[cfg(test)]
use chrono::FixedOffset;
use chrono::{DateTime, Local};
use colored::Colorize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{CodexOutput, RuntimeAgent};
use crate::display;
use crate::rate_control::{
    self, Decision, LiveObservation, PauseCause, PauseSource, PauseState, Recovery,
};
use crate::resume::ResumeEntry;
use crate::scanner::{ResolvedTarget, Visibility};
use crate::usage::LiveError;

pub(crate) mod append;
mod flags;
mod inference;
mod scripts;
mod util;
pub(crate) use util::last_log_line;

use flags::{agent_is_claude, claude_session_flag, ensure_required_flags};
use scripts::{
    TaskCtx, TaskResume, WorkerCtx, build_disk_cmd, build_live_cmd, build_refresh_cmd,
    build_statusline_cmd, build_task_script, build_worker_script, env_prefix_parts, shell_escape,
    shell_escape_path,
};
use util::{sanitize_filename, strip_ansi_from_dir, truncate};

/// 起動時 ai-usage キャッシュ初期化のハング検知タイムアウト。
/// これを超えたら子プロセスを SIGKILL してキャッシュ初期化をスキップし、
/// monitor 起動を進める。tmux 起動前に token-burn 全体が固まるのを防ぐ。
const AI_USAGE_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ExecutionPlan {
    pub append: Option<append::Settings>,
    pub agent: RuntimeAgent,
    pub tasks: Vec<ResolvedTarget>,
    pub usage_gate: Option<UsageGateConfig>,
    pub resume: ResumePlan,
    /// タスクの出力の処理方法。command へのフラグ付与と同時に 1 度だけ決める。
    pub output: OutputPipeline,
    /// 実行開始時の保留・停止。`build_plan` が保存した中断記録から決め、ai-usage 連携が
    /// あれば `check_start` が実データで決め直す。
    pub start: StartGate,
    /// `check_start` が取得した `ai-usage --json`（全アカウント分）。statusline 用
    /// キャッシュの初期化に使い、起動時のもう 1 回の取得を省く。
    pub startup_usage: Option<Vec<u8>>,
}

/// タスクスクリプトが組み立てるパイプラインの形（stdin と出力の扱い）。
///
/// 付与したフラグと出力の処理は対応していなければならない（`--json` を付けたのに素通しする、
/// 付けていないのに JSONL として整形する、を起こさない）ため、フラグを付与する
/// `ensure_required_flags` が返し、`build_plan` が計画に保存する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputPipeline {
    /// claude の stream-json を `format-stream` で整形し、`classify-result` で結果を分類する。
    ClaudeStreamJson,
    /// `codex exec --json` の JSONL を `format-codex` で整形する。stdin は /dev/null。
    CodexJson,
    /// codex の人間向けの出力をそのままログへ流す（`codex_output = "human"`、または
    /// `exec` を見つけられないラッパー）。stdin は /dev/null。
    CodexRaw,
    /// その他のエージェント。出力をそのままログへ流し、stdin はペインの TTY のまま（従来どおり）。
    Raw,
}

impl OutputPipeline {
    /// エージェントの stdin を /dev/null にするか（codex のみ。理由は `build_task_script` 参照）。
    fn closes_stdin(self) -> bool {
        matches!(self, OutputPipeline::CodexJson | OutputPipeline::CodexRaw)
    }

    /// 実行計画に出す出力の扱い（codex のみ。command だけでは読み取れないため）。
    fn plan_label(self, codex_output: CodexOutput) -> Option<&'static str> {
        match self {
            OutputPipeline::CodexJson => Some("codex JSON events → format-codex"),
            OutputPipeline::CodexRaw if codex_output == CodexOutput::Human => {
                Some("raw (codex_output = \"human\")")
            }
            OutputPipeline::CodexRaw => Some(
                "raw (no `exec` subcommand found; set codex_output = \"json\" if the command prints codex JSONL)",
            ),
            OutputPipeline::ClaudeStreamJson | OutputPipeline::Raw => None,
        }
    }
}

/// 中断セッションの再開に関する計画。
#[derive(Debug, Default)]
pub struct ResumePlan {
    /// レート制限で中断したタスクのセッションを保存するか（claude かつ設定で有効）。
    /// `--no-resume` / `--fresh` は「今回は再開しない」だけで、新しい中断は保存する。
    pub save_interrupted: bool,
    /// 今回再開するタスク（ディレクトリ → 保存済み記録）。計画に含まれるものだけ。
    pub sessions: HashMap<PathBuf, ResumeEntry>,
    /// 保存済みの記録があるのに再開しないタスク（ディレクトリ → その session_id）。
    /// 新しいセッションを始めた時点で古い記録は破棄する（下記 `ResumeOptions::discard`）。
    pub discard: HashMap<PathBuf, String>,
    /// 再開時に送る継続プロンプト。
    pub prompt: String,
}

/// `build_plan` へ渡す再開の入力（呼び出し側が保存済み記録から判定した結果）。
#[derive(Debug, Default)]
pub struct ResumeOptions {
    pub save_interrupted: bool,
    pub sessions: HashMap<PathBuf, ResumeEntry>,
    /// 保存済みの記録を使わずに新しいセッションで始めるターゲット（→ 古い session_id）。
    ///
    /// 新しいセッションが作業を始めた時点で、古い記録の文脈はリポジトリの実態より古い。
    /// 残しておくと、`--no-resume` / `--fresh` で始めた新しいセッションがリトライ可能
    /// エラーで終わった（＝保存されない）場合に、次の実行がより古い中断セッションを
    /// 再開してしまう。
    pub discard: HashMap<PathBuf, String>,
    pub prompt: String,
}

/// エージェントが中断セッションの自動再開に対応しているか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeSupport {
    Supported,
    /// claude 以外（stream-json の分類パイプラインが無く、中断を判定できない）。
    NotClaude,
    /// command に既にセッション系フラグがある。自動で `--resume` を足すと衝突する。
    ConflictingFlag(String),
}

impl ResumeSupport {
    /// 対応していない理由（表示用）。対応していれば `None`。
    pub fn unsupported_reason(&self) -> Option<String> {
        match self {
            ResumeSupport::Supported => None,
            ResumeSupport::NotClaude => Some("not a claude agent".to_string()),
            ResumeSupport::ConflictingFlag(flag) => {
                Some(format!("the agent command already passes {flag}"))
            }
        }
    }
}

/// エージェントが中断セッションの自動再開に対応しているかを判定する。
pub fn resume_support(agent: &RuntimeAgent) -> ResumeSupport {
    if !agent_is_claude(agent) {
        return ResumeSupport::NotClaude;
    }
    match claude_session_flag(&agent.command) {
        Some(flag) => ResumeSupport::ConflictingFlag(flag.to_string()),
        None => ResumeSupport::Supported,
    }
}

/// 実行開始時の保留・停止。
///
/// 保存した中断記録の再開時刻（`retry_after`）は、その時刻まで待てば枠が空くという
/// 見積もりにすぎない。利用者は手動リセット（claude.ai の "Reset for free"、Codex の
/// banked reset）で枠を早く戻せるし、逆に前回の実行の後で使用率が閾値を越えていることも
/// ある。実例では、5 時間枠が 100% で止まった直後に手動リセットしたのに、再実行が
/// `retry_after`（1 時間半後）による保留で 1 件も始まらなかった。そこで ai-usage 連携が
/// あれば、対象を確定した後・tmux を作る前に実行中のアカウントを取り直して決める
/// （`check_start`）。連携が無ければ、保存した記録だけで決める（`build_plan`）。
///
/// ドライランの実行計画の表示（`print_plan`）と、実際に書く pause file・始めないことの
/// 判断（`execute_plan_tmux`）は、どちらもこの値を使う（表示と実際の挙動を食い違わせない）。
#[derive(Debug, Clone, PartialEq)]
pub enum StartGate {
    /// そのまま始める。
    Proceed,
    /// 再開するセッションを止めた 5 時間枠が回復していたので、保存した再開時刻（`until`、
    /// Unix epoch 秒）まで待たずに始める。`evidence` は回復を確かめた根拠（表示用）。
    /// `pause` は待たずに済んだ一時停止（保存した記録をまとめたもの）で、解除した状態として
    /// pause file に残す（`write_start_pause`）。
    Recovered {
        until: i64,
        evidence: String,
        pause: PauseState,
    },
    /// `pause` の再開時刻まで一時停止した状態で始める。`why` は表示用の理由。
    Hold { pause: PauseState, why: String },
    /// この実行ではタスクを 1 件も始めない。デッドラインと同じ周期の枠（週次など）が
    /// 閾値に触れていて、待っても実行の残り時間では回復しない等。`reason` は表示用。
    Stop { reason: String },
}

/// 保存した中断記録による保留の理由（表示用）。
const SAVED_HOLD_REASON: &str = "a resumed session hit the five_hour limit";

impl StartGate {
    /// 実行計画に出す行（ラベルと本文）。保留も停止もしないときは `None`。
    ///
    /// `recovery_checked` は、待機中にモニターが実データを取り直して回復を確かめる構成か
    /// （ai-usage 連携あり）。そのときは見積もった時刻より早く再開し得ることを添える。
    /// 見積もりどおりに待つとしか読めないと、手動リセットした利用者が待ち切れずに実行を
    /// 打ち直してしまう。
    fn plan_line(&self, recovery_checked: bool) -> Option<(&'static str, String)> {
        match self {
            StartGate::Proceed => None,
            StartGate::Recovered {
                until, evidence, ..
            } => Some((
                "Hold:",
                format!(
                    "none — {SAVED_HOLD_REASON} (until {}), but the window has already recovered: {evidence}",
                    format_local(*until)
                ),
            )),
            StartGate::Hold { pause, why } => {
                // モニターが早期解除を確かめられない一時停止（根拠の使用率が無い・低すぎる等）で
                // 「早めに再開し得る」と添えると、実際には見積もりどおりに待つのに待ち切れず
                // 打ち直させてしまう。判定はモニターと同じ `not_liftable` で行う。
                let early = match (recovery_checked, crate::monitor::not_liftable(pause)) {
                    (false, _) => String::new(),
                    (true, None) => "; the monitor resumes them earlier if the window recovers (e.g. a manual reset)".to_string(),
                    (true, Some(reason)) => {
                        format!("; the monitor cannot resume them earlier ({reason})")
                    }
                };
                Some((
                    "Hold:",
                    format!(
                        "{why} — workers wait until {} before starting{early}",
                        format_local(pause.resume_at)
                    ),
                ))
            }
            StartGate::Stop { reason } => Some((
                "Stop:",
                format!("{reason} — no tasks will be started in this run"),
            )),
        }
    }
}

/// Unix epoch 秒をローカル時刻の `%Y-%m-%d %H:%M` で表す。
fn format_local(epoch: i64) -> String {
    use chrono::TimeZone;
    match Local.timestamp_opt(epoch, 0).single() {
        Some(at) => at.format("%Y-%m-%d %H:%M").to_string(),
        None => epoch.to_string(),
    }
}

/// 再開するセッションのうち、保存した再開時刻がまだ先のもの。
fn pending_retries<'a>(
    sessions: impl IntoIterator<Item = &'a ResumeEntry>,
    now: i64,
) -> Vec<&'a ResumeEntry> {
    sessions
        .into_iter()
        .filter(|entry| entry.retry_after.is_some_and(|at| at.timestamp() > now))
        .collect()
}

/// 保存した中断記録が見積もった、5 時間枠の一時停止（再開時刻を持つ記録が無ければ `None`）。
///
/// 回復の判定（`rate_control::check_recovery`）の比較の元にもなる。複数の記録は 1 つに
/// まとめる。5 時間枠はアカウント単位なので、どの記録も同じ枠の回復を待っている。待つ時刻は
/// 最も遅い再開時刻、根拠の使用率は最も低いもの（回復とみなすのに最も大きな下がり幅が要る）、
/// 根拠の観測時刻は最も新しい中断にする。どれか 1 件の根拠だけを残すと、モニターがほかの
/// 記録には足りない下がり幅で早期解除してしまう（42% で拒否された記録と 100% で拒否された
/// 記録があるとき、100% の方だけを残すと 40% を回復とみなす）。まとめた条件は、どの記録の
/// 条件より緩くならない（`rate_control::merge_basis`）。
///
/// 拒否されたときの使用率（`retry_basis_percent`）を持たない旧記録は、根拠が不明のまま
/// 扱う。1 件でも不明なら、まとめた根拠も不明にする（既知の使用率で補わない）。拒否は
/// 使用率と別の理由でも起き（42% で拒否された例がある）、不明な記録を 100% とみなすと、
/// 42% のまま「100% から下がった」と誤って回復とみなすため。根拠が不明な一時停止は早期解除
/// せず、保存した再開時刻まで待つ（この版より前に保存した記録だけが該当する）。
fn saved_pause(entries: &[&ResumeEntry]) -> Option<PauseState> {
    let entries: Vec<&ResumeEntry> = entries
        .iter()
        .copied()
        .filter(|entry| entry.retry_after.is_some())
        .collect();
    let retry_after = entries
        .iter()
        .filter_map(|entry| entry.retry_after)
        .max()?
        .timestamp();
    let observed_at = entries
        .iter()
        .map(|entry| entry.interrupted_at.timestamp())
        .max()?;
    let basis = entries
        .iter()
        .map(|entry| entry.retry_basis_percent)
        .reduce(rate_control::merge_basis)
        .flatten();
    let mut pause = PauseState::new(
        retry_after,
        "five_hour",
        format!(
            "a resumed session was rate limited (five_hour) until {}",
            format_local(retry_after)
        ),
    )
    .observed(PauseSource::Saved, PauseCause::Rejected, observed_at);
    pause.used_percent = basis.filter(|v| v.is_finite() && *v >= 0.0);
    pause.window_reset_at = Some(retry_after - rate_control::RESET_PROPAGATION_GRACE_SECS);
    Some(pause)
}

/// 保存した中断記録で保留する。`detail` は実データで確かめられなかった理由など（表示用）。
///
/// 一時停止には拒否されたときの使用率を根拠として記録する。実データを取れる構成では、
/// 待機中にモニターがこれと比べて手動リセット等による回復を確かめ、早めに解除できる。
fn saved_hold(entries: &[&ResumeEntry], detail: Option<&str>) -> StartGate {
    let Some(pause) = saved_pause(entries) else {
        return StartGate::Proceed;
    };
    let why = match detail {
        Some(detail) => format!("{SAVED_HOLD_REASON} ({detail})"),
        None => SAVED_HOLD_REASON.to_string(),
    };
    StartGate::Hold { pause, why }
}

/// 実データを使わずに、保存した中断記録だけで実行開始時の保留を決める（ai-usage 連携が
/// 無いとき。連携があっても `check_start` が決め直すまでの既定値）。
///
/// 再開時刻がまだ先のセッションがあれば、その最も遅い時刻まで実行全体を一時停止した
/// 状態で始める。5 時間枠はアカウント単位なので、再開以外のタスクも同じ時刻まで通らない。
/// 早すぎる再実行で即座に同じ枠へ拒否され、継続プロンプトだけが transcript に積み重なる
/// のを防ぐ。
fn start_from_saved<'a>(
    sessions: impl IntoIterator<Item = &'a ResumeEntry>,
    now: i64,
) -> StartGate {
    saved_hold(&pending_retries(sessions, now), None)
}

/// 実行開始時に取り直した実データで、保留・停止を決める（I/O なし）。
///
/// - 閾値に触れた枠があれば、usage-gate と同じ判定（`rate_control::evaluate`）に従う。
///   デッドラインと同じ周期の枠（週次など）なら 1 件も始めない。5 時間枠などなら、その枠の
///   リセットまで一時停止した状態で始める。再開するセッションの有無は問わない（枠は
///   アカウント単位なので、どのタスクも同じ枠で止まる）。保存した拒否の記録があれば、
///   その条件も一時停止に残す（`rate_control::merge_pauses`。再開時刻は遅い方、早期解除の
///   条件は厳しい方）。実データの枠の方が早く空いても、記録の条件を満たすまでは解除しない。
/// - 閾値未満なら、再開するセッションを止めた 5 時間枠が回復したかを記録ごとに確かめる
///   （`rate_control::check_recovery`）。すべて回復していれば待たない。回復を確かめられ
///   ないものがあれば、それらの最も遅い再開時刻まで一時停止した状態で始める（実行中は
///   モニターが回復を確かめ続けるので、手動リセットで戻れば早めに解除される）。
/// - 使用率を確認できなければ（取得・解釈の失敗、`ok:false`）、1 件も始めない。usage-gate と
///   同じく止める側に倒す（fail-closed）。始めると、使用率の分からないまま並列数ぶんの
///   タスクが走る（codex にはタスク中の停止の経路も無い）。出力に該当アカウントが無いときは、
///   usage-gate と同じく過剰停止を避け、保存した記録だけで決める。
///
/// `fetched_at` は取得を終えた時刻で、ai-usage による一時停止に根拠の観測時刻の上限として
/// 記録する。モニターは、これより後に取得したデータでだけ早期解除を判定する（一時停止の
/// 根拠より前の古いデータが「下がった」ように見えるのを防ぐ）。
fn decide_start<'a>(
    sessions: impl IntoIterator<Item = &'a ResumeEntry>,
    live: &std::result::Result<LiveObservation, LiveError>,
    fetched_at: i64,
    threshold: u8,
    now: i64,
) -> StartGate {
    let pending = pending_retries(sessions, now);
    let live = match live {
        Ok(live) => live,
        Err(LiveError::NoEntry(reason)) => {
            return saved_hold(&pending, Some(&format!("ai-usage: {reason}")));
        }
        Err(LiveError::Unavailable(reason)) => {
            return StartGate::Stop {
                reason: format!("cannot confirm the usage before starting (ai-usage: {reason})"),
            };
        }
    };
    match rate_control::evaluate(&live.windows, threshold, now) {
        Decision::Stop { basis, detail } => StartGate::Stop {
            reason: format!(
                "{} (ai-usage{})",
                basis.reason(threshold),
                detail.map(|d| format!(": {d}")).unwrap_or_default()
            ),
        },
        Decision::Pause { resume_at, basis } => {
            let reason = basis.reason(threshold);
            let live_pause = PauseState::new(
                resume_at,
                basis.window.clone(),
                format!("usage at start: {reason}"),
            )
            .observed(PauseSource::AiUsage, PauseCause::Threshold, fetched_at)
            .with_basis(basis.used_percent, basis.reset_at);
            // 閾値に触れている間は、保存した記録の回復も確かめようが無い（どの枠も閾値未満で
            // なければ回復とみなさない）。記録の条件をすべて残す。
            let (pause, why) = match saved_pause(&pending) {
                Some(saved) => (
                    rate_control::merge_pauses(&live_pause, &saved),
                    format!("{reason} (ai-usage); {SAVED_HOLD_REASON}"),
                ),
                None => (live_pause, format!("{reason} (ai-usage)")),
            };
            StartGate::Hold { pause, why }
        }
        Decision::Proceed => {
            // 記録ごとに確かめる。回復した記録を待つ根拠に含めると、まとめた条件
            // （saved_pause）が回復済みの記録の時刻まで保留を延ばしてしまう。
            let mut waiting: Vec<&ResumeEntry> = Vec::new();
            let mut evidence = None;
            for entry in &pending {
                let Some(pause) = saved_pause(&[entry]) else {
                    continue;
                };
                match rate_control::check_recovery(&pause, live, threshold, now) {
                    Recovery::Recovered(found) => {
                        evidence.get_or_insert(found);
                    }
                    Recovery::NotYet(_) | Recovery::Undecidable(_) => waiting.push(entry),
                }
            }
            if let Some(pause) = saved_pause(&waiting) {
                // 表示する理由は、まとめた条件（モニターが早期解除に使うもの）で判定し直した
                // ものにする。まとめた条件は記録ごとの条件より緩くならないので、どの記録も
                // 回復していなければ回復にはならない（万一なっても待つ側に倒す）。
                let detail = match rate_control::check_recovery(&pause, live, threshold, now) {
                    Recovery::NotYet(detail) => format!("not recovered yet: {detail}"),
                    Recovery::Undecidable(detail) => format!("cannot confirm a recovery: {detail}"),
                    Recovery::Recovered(detail) => {
                        format!("not every resumed session has recovered: {detail}")
                    }
                };
                return StartGate::Hold {
                    pause,
                    why: format!("{SAVED_HOLD_REASON} ({detail})"),
                };
            }
            match (evidence, saved_pause(&pending)) {
                (Some(evidence), Some(pause)) => StartGate::Recovered {
                    until: pause.resume_at,
                    evidence,
                    pause,
                },
                _ => StartGate::Proceed,
            }
        }
    }
}

/// ai-usage 使用率ゲートの設定（各タスク完了後にワーカーが実行）。
pub struct UsageGateConfig {
    pub profile: String,
    pub provider: String,
    /// ai-usage を起動するコマンド（例: ["ai-usage", "--json"]）。
    pub command: Vec<String>,
    /// 選択 agent の env（CLAUDE_CONFIG_DIR 等）。usage-gate / monitor statusline /
    /// 実行開始時の判定を、その agent の実行文脈で動かすために使う。
    /// これにより起動シェルの環境継承で別アカウントの使用率を見るズレを防ぐ。
    pub env: std::collections::BTreeMap<String, String>,
}

/// 実行開始時の保留・停止を、ai-usage の実データを取り直して決め直す（ai-usage 連携が
/// あるときだけ。無ければ `build_plan` が保存した記録から決めたまま）。
///
/// TUI で対象と閾値を確定した後、tmux を作る前に呼ぶ。TUI の操作中にも枠は動く
/// （手動リセット・ほかの作業での消費）ので、選択より前に取った値では決めない。
/// 取得した出力（全アカウント分）は statusline 用キャッシュの初期化に使い、起動時の
/// もう 1 回の取得を省く。ドライランでも呼ぶ（実行計画に実際と同じ判定を出すため）。
pub fn check_start(plan: &mut ExecutionPlan, threshold: u8) {
    let Some(gate) = plan.usage_gate.as_ref() else {
        return;
    };
    let usage = fetch_start_usage(gate, AI_USAGE_STARTUP_TIMEOUT);
    if let Err(reason) = &usage.observation {
        eprintln!(
            "{}: could not check the usage of {}/{} before starting: {}",
            "warning".yellow(),
            gate.profile,
            gate.provider,
            reason
        );
    }
    plan.start = decide_start(
        plan.resume.sessions.values(),
        &usage.observation,
        usage.fetched_at,
        threshold,
        rate_control::now_epoch(),
    );
    plan.startup_usage = usage.raw;
}

/// 実行開始時に取り直した ai-usage の実データ。
struct StartUsage {
    /// 実行中のアカウントの実測値。取得・解釈できなければその理由。
    observation: std::result::Result<LiveObservation, LiveError>,
    /// 取得を終えた時刻（Unix epoch 秒）。一時停止に、根拠の観測時刻の上限として記録する。
    fetched_at: i64,
    /// JSON として読めた出力（全アカウント分）。statusline 用キャッシュの初期化に使う。
    raw: Option<Vec<u8>>,
}

/// 実行開始時の判定に使う `ai-usage --json` を、共有キャッシュを通さずに取り直す。
///
/// statusline の表示用キャッシュの初期化にも使うので、`--refresh` だけを足して対象の
/// アカウントは絞らない。エージェントの env を「空文字は unset」の規則で適用し、期限を
/// 過ぎたらプロセスグループごと止める（`run_ai_usage_sync`）。`--refresh` を知らない
/// 古い ai-usage が使い方エラーで終わったら、設定どおりの command で取り直す。その値は
/// 共有キャッシュを通った可能性があるので、回復の証拠には使わない（`fresh: false`）。
fn fetch_start_usage(gate: &UsageGateConfig, timeout: Duration) -> StartUsage {
    let sent = crate::usage::refresh_command(&gate.command);
    let started = rate_control::now_epoch();
    let fetched = run_ai_usage_sync(&sent, &gate.env, timeout).and_then(|output| {
        if output.status.success() {
            Ok((output.stdout, true))
        } else if crate::usage::retry_without_added_flags(
            output.status.code(),
            &sent,
            &gate.command,
        ) {
            spawn_ai_usage_sync_with_timeout(&gate.command, &gate.env, timeout)
                .map(|stdout| (stdout, false))
        } else {
            Err(crate::usage::exit_error(&output))
        }
    });
    let fetched_at = rate_control::now_epoch();
    match fetched {
        Ok((stdout, fresh)) => StartUsage {
            observation: crate::usage::live_observation(
                &stdout,
                &gate.profile,
                &gate.provider,
                started - rate_control::LIVE_FRESHNESS_MARGIN_SECS,
                fresh,
            ),
            fetched_at,
            // 壊れた出力で statusline 用キャッシュを初期化しない（従来の初期化に任せる）。
            raw: serde_json::from_slice::<serde_json::Value>(&stdout)
                .is_ok()
                .then_some(stdout),
        },
        Err(e) => StartUsage {
            observation: Err(LiveError::Unavailable(format!("{e:#}"))),
            fetched_at,
            raw: None,
        },
    }
}

/// 実際に起動するワーカー数。タスク数を超えるワーカーは claim できる pending が
/// 無いまま終了するだけなので、指定値をタスク数で頭打ちにする。
fn worker_count(parallelism: usize, total: usize) -> usize {
    parallelism.min(total)
}

/// tmux セッション終了後に呼び出し元の端末へ出す最終集計。
///
/// モニターペインに出した完了表示はセッションを閉じた時点で画面から消えるため、
/// 同じ内容をマーカーファイルから読み直して stdout に残す。
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct RunSummary {
    done: usize,
    failed: usize,
    retry: usize,
    /// タスクが失敗・リトライした理由（error-* の中身。タスク番号順・1 行に畳んだもの）。
    pub(crate) errors: Vec<String>,
}

/// 最終集計の後に端末へ出すエラーの上限。超えた分は件数だけを出す（全件は
/// [`ERRORS_FILE_NAME`] に残す）。多すぎると集計行やログの場所が流れて読めなくなる。
const SHOWN_ERRORS_LIMIT: usize = 20;

/// エラーの全件を書き出すファイル名（今回の実行用レポートディレクトリの直下に置く）。
const ERRORS_FILE_NAME: &str = "errors.txt";

impl RunSummary {
    #[cfg(test)]
    fn processed(&self) -> usize {
        self.done + self.failed + self.retry
    }

    /// 集計に問題（失敗・リトライ・未処理）が含まれるか。表示色の判定に使う。
    #[cfg(test)]
    fn has_issues(&self, total: usize) -> bool {
        self.failed > 0 || self.retry > 0 || self.processed() < total
    }

    /// モニターペインの完了行と同じ文言を組み立てる。
    pub(crate) fn describe(&self, total: usize) -> String {
        crate::monitor::summary_text(self.done, self.failed, self.retry, total)
    }

    /// 集計行の後に出すエラーの行（字下げと色は呼び出し側で付ける）。
    ///
    /// モニターはセッションごと閉じるので、ペインに出していたエラーの一覧も画面から消える。
    /// 件数だけでは失敗の理由が分からないため、同じ一覧を呼び出し元の端末に残す。
    /// [`SHOWN_ERRORS_LIMIT`] 件を超えた分は件数にまとめ、全件を errors.txt に書けたときだけ
    /// その場所を案内する（書けなかったファイルを指さない）。
    pub(crate) fn error_lines(&self, saved_to_file: bool) -> Vec<String> {
        let mut lines: Vec<String> = self
            .errors
            .iter()
            .take(SHOWN_ERRORS_LIMIT)
            .map(|error| format!("❌ {error}"))
            .collect();
        let hidden = self.errors.len().saturating_sub(SHOWN_ERRORS_LIMIT);
        if hidden > 0 {
            lines.push(if saved_to_file {
                format!("❌ +{hidden} more errors (see {ERRORS_FILE_NAME} in the logs directory)")
            } else {
                format!("❌ +{hidden} more errors")
            });
        }
        lines
    }
}

/// マーカーディレクトリから done / failed / retry を数え、エラーの理由（error-*）を読む。
///
/// 読めない場合は 0 件として扱う。これは表示のための集計であり、実行結果の記録自体は
/// `state.json` が持つため、ここで失敗しても実行の成否には影響しない。エラーの読み方は
/// モニターのペインと同じ（[`crate::monitor::read_errors`]）。
pub(crate) fn collect_run_summary(marker_dir: &Path) -> RunSummary {
    let mut summary = RunSummary::default();
    let Ok(entries) = std::fs::read_dir(marker_dir) else {
        return summary;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // ワーカー完了マーカーは `worker-done-*` で "done-" 始まりではないため混ざらない。
        if name.starts_with("done-") {
            summary.done += 1;
        } else if name.starts_with("failed-") {
            summary.failed += 1;
        } else if name.starts_with("retry-") {
            summary.retry += 1;
        }
    }
    summary.errors = crate::monitor::read_errors(marker_dir);
    summary
}

/// エラーの全件を 1 行 1 件で `<run_dir>/errors.txt` に書く。エラーが無ければ何も作らない。
///
/// 端末に出すのは先頭の [`SHOWN_ERRORS_LIMIT`] 件だけで、error-* は一時ディレクトリと一緒に
/// 消えるため、全件を後から読める場所（ログと同じディレクトリ）に残す。各エラーは
/// [`crate::monitor::read_errors`] が 1 行に畳んで制御文字を除いたもの。書いたら true。
pub(crate) fn write_errors_file(run_dir: &Path, errors: &[String]) -> Result<bool> {
    if errors.is_empty() {
        return Ok(false);
    }
    let path = run_dir.join(ERRORS_FILE_NAME);
    let mut text = errors.join("\n");
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

pub fn build_plan(
    agent: &RuntimeAgent,
    targets: Vec<ResolvedTarget>,
    ai_usage_command: Option<Vec<String>>,
    resume: ResumeOptions,
) -> ExecutionPlan {
    let mut agent = agent.clone();
    // usage-gate / monitor statusline に渡す env は、ユーザーが設定した実行文脈
    // （CLAUDE_CONFIG_DIR 等）のスナップショット。ensure_required_flags が注入する
    // claude 専用のデフォルト env（CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS）を
    // ai-usage 起動にまで持ち込まないよう、注入前に取得する。
    let gate_env = agent.env.clone();
    let output = ensure_required_flags(&mut agent);
    // ai-usage 連携 agent かつグローバルで ai-usage が有効なときだけゲートを設定する。
    let usage_gate = match (agent.ai_usage.as_ref(), ai_usage_command) {
        (Some(rt), Some(command)) => Some(UsageGateConfig {
            profile: rt.profile.clone(),
            provider: rt.provider.clone(),
            command,
            env: gate_env,
        }),
        _ => None,
    };
    // 再開するのは計画に残ったタスクだけ（limit や TUI で外れたターゲットは待機の根拠にも
    // しない）。claude 以外は分類パイプラインが無いので、渡されても再開しない。
    let is_claude = output == OutputPipeline::ClaudeStreamJson;
    let planned = |dir: &PathBuf| is_claude && targets.iter().any(|t| &t.directory == dir);
    let mut sessions = resume.sessions;
    sessions.retain(|dir, _| planned(dir));
    let mut discard = resume.discard;
    discard.retain(|dir, _| planned(dir) && !sessions.contains_key(dir));
    // ai-usage 連携があれば check_start が実データで決め直す。呼ばれなくても、保存した
    // 記録による保留は失わない。
    let start = start_from_saved(sessions.values(), rate_control::now_epoch());
    ExecutionPlan {
        append: None,
        agent,
        tasks: targets,
        usage_gate,
        resume: ResumePlan {
            save_interrupted: resume.save_interrupted && is_claude,
            sessions,
            discard,
            prompt: resume.prompt,
        },
        output,
        start,
        startup_usage: None,
    }
}

/// 同期パスで ai-usage を起動し、`timeout` を超えたら SIGKILL してキルする。
/// 非ゼロ終了はエラーにする（[`run_ai_usage_sync`] 参照）。
fn spawn_ai_usage_sync_with_timeout(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let output = run_ai_usage_sync(command, env, timeout)?;
    if !output.status.success() {
        return Err(crate::usage::exit_error(&output));
    }
    Ok(output.stdout)
}

/// 同期パスで ai-usage を起動し、`timeout` を超えたら SIGKILL してキルする。
/// 専用スレッドの tokio ランタイムで待つため、同期・非同期どちらの呼び出し元でも使える。
///
/// 終了した子の出力を、終了コードごと返す（古い ai-usage の使い方エラーを呼び出し側が
/// 見分けられるように、非ゼロ終了はエラーにしない）。stdout/stderr は非同期 I/O で並行に
/// 読むため、出力サイズがパイプバッファを超えてもデッドロックしない。spawn 失敗・
/// タイムアウトはエラー。
///
/// env は「空文字の値は unset」という設定側の規約（`env_prefix_parts` が `env -u KEY`
/// へ変換するのと同じ意味）に合わせて適用する。`envs()` へ素通しすると空文字がそのまま
/// 子プロセスへ渡り、`CLAUDE_CONFIG_DIR=""` を「既定に戻す」つもりで書いた profile で
/// ai-usage が cwd 相対の設定ディレクトリを見に行って別アカウントの使用率を拾う。
/// この結果は実行開始時の保留・停止の判定と statusline の表示に使われるため、誤った
/// 使用率で実行を止めたり始めたりしてしまう。
fn run_ai_usage_sync(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
) -> Result<std::process::Output> {
    anyhow::ensure!(!command.is_empty(), "ai_usage.command is empty");
    // 呼び出し元が tokio のランタイム内でも使えるよう、専用スレッドで非同期 I/O を動かす。
    // 子の終了と stdout/stderr の EOF をまとめて期限内に待ち、読み取りスレッドの join を避ける。
    std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<std::process::Output> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to create ai-usage runtime")?;
                runtime.block_on(async {
                    use tokio::io::AsyncReadExt;
                    #[cfg(not(windows))]
                    let mut builder = {
                        let mut builder = tokio::process::Command::new(&command[0]);
                        builder.args(&command[1..]);
                        builder
                    };
                    #[cfg(windows)]
                    let gate = crate::platform::windows::Gate::new()?;
                    #[cfg(windows)]
                    let mut builder = {
                        let mut builder = tokio::process::Command::new(crate::platform::bash()?);
                        let payload = scripts::quote_command(command).join(" ");
                        builder.args(["-c", &gate.wrap(&payload)]);
                        builder.envs(crate::platform::shell_environment()?);
                        builder.creation_flags(
                            windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
                        );
                        builder
                    };
                    for (key, value) in env {
                        if value.is_empty() {
                            builder.env_remove(key);
                        } else {
                            builder.env(key, value);
                        }
                    }
                    // ラッパーが孫プロセスを起動しても、タイムアウト時にまとめて終了させる。
                    #[cfg(unix)]
                    builder.process_group(0);
                    let mut child = builder
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .kill_on_drop(true)
                        .spawn()
                        .with_context(|| {
                            format!(
                                "failed to spawn ai-usage command: {}",
                                display::format_command(command)
                            )
                        })?;
                    #[cfg(windows)]
                    let _job = {
                        let result = crate::platform::windows::Job::assign(
                            child.id().context("Missing ai-usage PID")?,
                        );
                        let job = match result {
                            Ok(job) => job,
                            Err(error) => {
                                let _ = child.kill().await;
                                return Err(error.into());
                            }
                        };
                        gate.release()?;
                        job
                    };
                    #[cfg(unix)]
                    let group_pid = child.id();
                    let mut stdout = child
                        .stdout
                        .take()
                        .context("ai-usage stdout is not piped")?;
                    let mut stderr = child
                        .stderr
                        .take()
                        .context("ai-usage stderr is not piped")?;
                    let output = tokio::time::timeout(timeout, async {
                        tokio::try_join!(
                            child.wait(),
                            async {
                                let mut bytes = Vec::new();
                                stdout.read_to_end(&mut bytes).await?;
                                Ok(bytes)
                            },
                            async {
                                let mut bytes = Vec::new();
                                stderr.read_to_end(&mut bytes).await?;
                                Ok(bytes)
                            },
                        )
                    })
                    .await;
                    match output {
                        Ok(Ok((status, stdout, stderr))) => Ok(std::process::Output {
                            status,
                            stdout,
                            stderr,
                        }),
                        failure => {
                            #[cfg(windows)]
                            _job.terminate();
                            #[cfg(unix)]
                            if let Some(pid) = group_pid {
                                // pid は process_group(0) で作った自分の子のグループ ID。負値でグループ全体を指定する。
                                unsafe {
                                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                                }
                            }
                            // 直接の子も必ず回収する。既に終了している場合の kill エラーは無視できる。
                            let _ = child.kill().await;
                            match failure {
                                Ok(Err(error)) => {
                                    Err(error).context("failed to collect ai-usage output")
                                }
                                Err(_) => anyhow::bail!(
                                    "ai-usage timed out after {}s: {}",
                                    timeout.as_secs_f64(),
                                    display::format_command(command)
                                ),
                                Ok(Ok(_)) => unreachable!(),
                            }
                        }
                    }
                })
            })
            .join()
            .map_err(|_| anyhow::anyhow!("ai-usage startup thread panicked"))?
    })
}

/// 指定したパスのファイルに実行ビットを付与する。
/// `chmod` の非ゼロ終了は無視されないことを保証し、tmux ワーカー起動時の
/// 「permission denied」を未然に検知できるようにする。
fn ensure_executable(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // Windows workers explicitly invoke Bash; there is no executable permission bit.
        std::fs::metadata(path)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let output = std::process::Command::new("chmod")
            .args(["+x", &path.to_string_lossy()])
            .output()
            .with_context(|| format!("Failed to run chmod on {}", path.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("chmod +x {} failed: {}", path.display(), stderr.trim());
        }
        Ok(())
    }
}

pub fn print_plan(plan: &ExecutionPlan, parallelism: usize) {
    println!("{}", "=== Execution Plan ===".bold());
    println!("Agent: {}", plan.agent.name.cyan());
    println!(
        "Command: {}",
        crate::display::format_command(&plan.agent.command).dimmed()
    );
    // codex の出力を整形するのか素通しするのかは command だけでは分からない（ラッパーが
    // exec を渡さないと素通しになる）。ドライランの時点で確かめられるようにする。
    if let Some(label) = plan.output.plan_label(plan.agent.codex_output) {
        println!("Output: {}", label.dimmed());
    }
    println!("Tasks: {}", plan.tasks.len());
    // 実際に起動するワーカー数は execute_plan_tmux と同じ算出（タスク数で頭打ち）。
    // 指定値がそのまま使われないケースをドライランの時点で見えるようにする。
    let workers = worker_count(parallelism, plan.tasks.len());
    if workers < parallelism {
        println!(
            "Workers: {} {}",
            workers,
            format!("(requested {parallelism}, capped by task count)").dimmed()
        );
    } else {
        println!("Workers: {workers}");
    }
    println!();
    for (i, task) in plan.tasks.iter().enumerate() {
        let vis = format!("[{}]", task.visibility);
        let vis_colored = match task.visibility {
            Visibility::Public => vis.green(),
            Visibility::Private => vis.yellow(),
            Visibility::Unknown => vis.dimmed(),
        };
        println!(
            "  {} {} {}",
            format!("[{}]", i + 1).yellow(),
            vis_colored,
            task.display_name
        );
        println!(
            "      Path:   {}",
            task.directory.display().to_string().dimmed()
        );
        match plan.resume.sessions.get(&task.directory) {
            // 再開するタスクに送るのは元のプロンプトではなく継続プロンプト（元の指示は
            // 会話履歴に残っている）。実際に送る方を見せる。
            Some(entry) => {
                println!(
                    "      Resume: {} {}",
                    entry.session_id.cyan(),
                    format!("({})", entry.summary()).dimmed()
                );
                println!(
                    "      Prompt: {}",
                    format!("(continuation) {}", truncate(&plan.resume.prompt, 60)).dimmed()
                );
            }
            None => println!("      Prompt: {}", truncate(&task.prompt, 60).dimmed()),
        }
    }
    // 実行開始時の保留・停止。実際に書く pause file（または始めないこと）と同じ値を出す。
    if let Some((label, text)) = plan.start.plan_line(plan.usage_gate.is_some()) {
        println!();
        match plan.start {
            StartGate::Stop { .. } => println!("{} {}", label.red().bold(), text.red()),
            StartGate::Recovered { .. } => println!("{} {}", label.green(), text.green()),
            _ => println!("{} {}", label.yellow(), text.yellow()),
        }
    }
    println!();
}

/// 実行用の一時ディレクトリを、必ず自分が作った空のディレクトリとして用意する。
///
/// 旧実装は `let _ = remove_dir_all(...)` で削除失敗を握り潰していたため、
/// 消せなかったディレクトリをそのまま再利用していた。`temp_dir()` が共有の
/// `/tmp` になる環境（Linux。macOS は `TMPDIR` がユーザーごと）では、
/// 他ユーザーが先に `/tmp/token-burn` を作っておくと sticky bit により削除が
/// 失敗する一方 `create_dir_all` は成功するため、他人の所有ディレクトリへ
/// ワーカースクリプトやプロンプトを書き込んでしまう。削除失敗は
/// （存在しない場合を除き）エラーとして扱い、作成後は所有者のみアクセス可にする。
fn prepare_run_tmp_dir(tmp_dir: &std::path::Path) -> Result<()> {
    match std::fs::remove_dir_all(tmp_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "failed to clear the temporary run directory {}",
                tmp_dir.display()
            )));
        }
    }
    std::fs::create_dir_all(tmp_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 実行開始時の判定で一時停止すると決めたら（ai-usage の実データで枠が閾値に触れている、
/// または再開するセッションを止めた 5 時間枠の回復を確かめられない）、実行全体をその状態で
/// 始める（pause file を書く）。枠はアカウント単位なので、再開以外のタスクも同じ時刻まで
/// 通らない。実行中に枠へ触れたときと同じ pause file を使うため、待機・デッドライン超過時の
/// 停止・再開前の再確認・モニターによる早期解除（手動リセット等で枠が戻ったとき）は、
/// すべて既存の gate-claim とモニターがそのまま扱う。
///
/// 再開時刻が起動準備の間に過ぎていても書く。gate-claim は期限切れの一時停止を「待機を
/// 抜けた直後」として扱い、再開前の再確認（usage-gate）を経てから 1 件目を始める。書かずに
/// 始めると、保留を決めた根拠（閾値超過・未回復）を確かめ直さないまま始めてしまう。
fn write_start_pause(start: &StartGate, stop_file: &Path) -> Result<()> {
    let path = rate_control::pause_path_for(stop_file);
    match start {
        StartGate::Hold { pause, .. } => {
            rate_control::write_pause(&path, pause)
                .context("failed to write the pause file to start the run paused")?;
        }
        StartGate::Recovered {
            pause, evidence, ..
        } => {
            // 実行中のモニターの早期解除と同じく、解除した一時停止として残す。何も書かずに
            // 始めると、直後に API がまた拒否しても再発（`relapse`）の印が付かず、ai-usage が
            // 別のアカウントを見ている構成では、同じ誤った値での解除と拒否を 1 巡余分に
            // 繰り返していた（再発ガードが働かない）。解除できなかったら一時停止のまま始まり、
            // モニターが同じ根拠で確かめ直して解除する（待つ側に倒れるだけ）。
            rate_control::write_pause(&path, pause)
                .context("failed to record the recovered pause before starting")?;
            let lifted = match rate_control::read_pause(&path)? {
                Some(state) => rate_control::lift_pause(
                    &path,
                    state.revision,
                    evidence,
                    Duration::from_secs(2),
                )?,
                None => false,
            };
            if !lifted {
                eprintln!(
                    "{}: could not record the early resume; workers wait until the monitor confirms the recovery again",
                    "warning".yellow()
                );
            }
        }
        StartGate::Proceed | StartGate::Stop { .. } => {}
    }
    Ok(())
}

/// 1 回の実行で使うディレクトリとファイルの置き場所。
struct RunLayout {
    /// 実行用の一時ディレクトリ。ワーカーとモニターが使うキュー・マーカー・スクリプト・
    /// プロンプト・起動情報を置く。
    tmp_dir: PathBuf,
    /// 今回の実行用レポートディレクトリ（ログと errors.txt）。
    run_dir: PathBuf,
    /// 実行を始めた時刻。レポートディレクトリ名に使い、この実行で保存された中断セッションを
    /// 数える基準にもする。
    started_at: DateTime<Local>,
    /// ワーカーが done-* / failed-* / retry-* / error-* / worker-done-* を書くディレクトリ。
    marker_dir: PathBuf,
    /// ワーカー間で共有するタスクキュー（pending-*。gate-claim が claimed-* へ rename する）。
    queue_dir: PathBuf,
    /// 各タスクの実行スクリプト（task-*.sh）。
    task_dir: PathBuf,
    /// 恒久停止のシグナル。一時停止のファイルはこの隣に置かれる。
    stop_file: PathBuf,
    /// 全ワーカーの疑似端末の準備完了を知らせるファイル。現れるまでタスクを取りに行かない。
    start_file: PathBuf,
    /// ワーカーとモニターが呼び直す token-burn の実行ファイル。
    exe_path: PathBuf,
}

pub fn execute_plan(
    plan: ExecutionPlan,
    parallelism: usize,
    deadline_epoch: i64,
    state_file: &Path,
    reset_info: &str,
    report_dir: &Path,
    rate_limit_threshold: u8,
) -> Result<bool> {
    anyhow::ensure!(!plan.tasks.is_empty(), "No tasks to execute");
    if let StartGate::Stop { reason } = &plan.start {
        println!("Not started: {reason}");
        return Ok(true);
    }
    std::fs::create_dir_all(report_dir)?;
    let now = Local::now();
    let prefix = format!(
        "{}_{}_",
        now.format("%Y%m%d_%H%M%S"),
        sanitize_filename(&plan.agent.name)
    );
    let run_dir = tempfile::Builder::new()
        .prefix(&prefix)
        .tempdir_in(report_dir)?
        .keep();
    let tmp_dir = run_dir.join(".runtime");
    prepare_run_tmp_dir(&tmp_dir)?;
    let layout = RunLayout {
        marker_dir: tmp_dir.join("markers"),
        queue_dir: tmp_dir.join("queue"),
        task_dir: tmp_dir.join("tasks"),
        stop_file: tmp_dir.join("stop"),
        start_file: tmp_dir.join("start"),
        exe_path: std::env::current_exe()?,
        tmp_dir,
        run_dir,
        started_at: now,
    };
    for path in [&layout.marker_dir, &layout.queue_dir, &layout.task_dir] {
        std::fs::create_dir_all(path)?;
    }
    let worker_count = worker_count(parallelism, plan.tasks.len());
    prepare_task_queue(&plan, &layout, state_file, rate_limit_threshold)?;
    if let Some(settings) = &plan.append {
        crate::runtime::atomic_json(&layout.tmp_dir.join("append.json"), settings)?;
    }
    let usage_gate_cmd = plan.usage_gate.as_ref().map(|gate| {
        build_usage_gate_cmd(
            gate,
            &layout.exe_path,
            rate_limit_threshold,
            &layout.stop_file,
        )
    });
    let cache_file = layout.tmp_dir.join("ai-usage-cache.json");
    let monitor_ai_usage = plan
        .usage_gate
        .as_ref()
        .and_then(|gate| build_monitor_ai_usage(gate, &cache_file));
    if let Some(gate) = &plan.usage_gate {
        init_statusline_cache(gate, plan.startup_usage.as_deref(), &cache_file);
    }
    let gate_claim_cmd = build_gate_claim_cmd(
        &layout.exe_path,
        &layout.stop_file,
        &layout.queue_dir,
        deadline_epoch,
        usage_gate_cmd.as_deref(),
    );
    let workers = write_worker_scripts(
        &layout,
        worker_count,
        usage_gate_cmd.as_deref(),
        &gate_claim_cmd,
    )?;
    let session = crate::runtime::Session {
        monitor: crate::monitor::Manifest {
            agent: plan.agent.name.clone(),
            command: display::format_command(&plan.agent.command),
            inference: inference::configuration(&plan.agent),
            reset: reset_info.into(),
            total: plan.tasks.len(),
            deadline_epoch,
            marker_dir: layout.marker_dir.clone(),
            session: "native".into(),
            worker_count,
            stop_file: layout.stop_file.clone(),
            report_dir: layout.run_dir.clone(),
            threshold: rate_limit_threshold,
            ai_usage: monitor_ai_usage,
        },
        workers,
        state_file: state_file.to_path_buf(),
        started_at: layout.started_at.fixed_offset(),
        targets: plan
            .tasks
            .iter()
            .map(|task| task.directory.clone())
            .collect(),
        save_interrupted: plan.resume.save_interrupted,
    };
    let session_path = layout.tmp_dir.join("session.json");
    crate::runtime::atomic_json(&session_path, &session)?;
    crate::runtime::launch(&session_path)?;
    if let Some(parent) = state_file.parent()
        && let Err(error) =
            crate::runtime::atomic_json(&parent.join("active-run.json"), &session_path)
    {
        eprintln!("Cannot remember the active run: {error:#}");
    }
    match crate::runtime::attach(&session_path) {
        Ok(completed) => Ok(completed),
        Err(error) => {
            eprintln!(
                "Cannot attach: {error:#}\nReattach: token-burn attach '{}'",
                layout.tmp_dir.display()
            );
            Ok(false)
        }
    }
}

/// ディレクトリと、ワーカー間で共有するキュー・マーカー・タスクのディレクトリを作る。
#[cfg(test)]
fn prepare_run_layout(tmp_dir: PathBuf, report_dir: &Path, agent_name: &str) -> Result<RunLayout> {
    prepare_run_tmp_dir(&tmp_dir)?;

    // 今回の実行用レポートディレクトリを作成
    let now = chrono::Local::now();
    let safe_agent_name = sanitize_filename(agent_name);
    let run_dir = report_dir.join(format!(
        "{}_{}",
        now.format("%Y%m%d_%H%M%S"),
        safe_agent_name
    ));
    std::fs::create_dir_all(&run_dir)?;

    // ワーカー間で共有するタスクキュー
    let marker_dir = tmp_dir.join("markers");
    std::fs::create_dir_all(&marker_dir)?;
    let queue_dir = tmp_dir.join("queue");
    std::fs::create_dir_all(&queue_dir)?;
    let task_dir = tmp_dir.join("tasks");
    std::fs::create_dir_all(&task_dir)?;

    let exe_path =
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("token-burn"));
    let stop_file = tmp_dir.join("stop");
    let start_file = tmp_dir.join("start");

    Ok(RunLayout {
        tmp_dir,
        run_dir,
        started_at: now,
        marker_dir,
        queue_dir,
        task_dir,
        stop_file,
        start_file,
        exe_path,
    })
}

/// ワーカーが処理するキューを書き出す。再開するタスクがあれば継続プロンプトを、実行開始時に
/// 一時停止するなら pause file を書き、続けて各タスクのプロンプト・実行スクリプト・pending
/// マーカーを書く。
fn prepare_task_queue(
    plan: &ExecutionPlan,
    layout: &RunLayout,
    state_file: &Path,
    rate_limit_threshold: u8,
) -> Result<()> {
    let total = plan.tasks.len();

    // 継続プロンプトは全タスク共通なので 1 ファイルにまとめる。
    let resume_prompt_file = layout.tmp_dir.join("resume-prompt.txt");
    if !plan.resume.sessions.is_empty() {
        std::fs::write(&resume_prompt_file, &plan.resume.prompt)?;
    }

    write_start_pause(&plan.start, &layout.stop_file)?;

    // 各タスクの実行スクリプトと pending マーカーを書き出す
    for (idx_zero, task) in plan.tasks.iter().enumerate() {
        let idx = idx_zero + 1;
        let prompt_file = layout.tmp_dir.join(format!("prompt-{}.txt", idx));
        std::fs::write(&prompt_file, &task.prompt)?;

        let resume = plan
            .resume
            .sessions
            .get(&task.directory)
            .map(|entry| TaskResume {
                entry,
                prompt_file: &resume_prompt_file,
            });
        let task_script = build_task_script(&TaskCtx {
            idx,
            total,
            task,
            agent: &plan.agent,
            prompt_file: &prompt_file,
            run_dir: &layout.run_dir,
            marker_dir: &layout.marker_dir,
            exe_path: &layout.exe_path,
            state_file,
            stop_file: &layout.stop_file,
            rate_limit_threshold,
            output: plan.output,
            save_interrupted: plan.resume.save_interrupted,
            resume,
            discard_session: plan.resume.discard.get(&task.directory).map(String::as_str),
        });
        let task_path = layout.task_dir.join(format!("task-{:04}.sh", idx));
        std::fs::write(&task_path, &task_script)?;

        std::fs::write(layout.queue_dir.join(format!("pending-{:04}", idx)), "")?;
    }
    Ok(())
}

/// ai-usage 連携時、各タスク完了後に使用率をチェックする usage-gate のコマンド文字列を
/// 組み立てる（ワーカーが bash で実行し、gate-claim の `--revalidate` にも渡す）。
///
/// usage-gate は呼ぶたびに実行中のアカウントを `--refresh` で取り直すので、キャッシュの
/// パスは渡さない（以前の版が渡していた `--cache-file` は受け付けたまま無視する）。
fn build_usage_gate_cmd(
    gate: &UsageGateConfig,
    exe_path: &Path,
    rate_limit_threshold: u8,
    stop_file: &Path,
) -> String {
    // usage-gate プロセス自体に選択 agent の env を前置きする。`--` 以降に KEY=val を
    // 入れると hidden CLI が実行ファイルと誤認するため、サブコマンドより前に置く。
    let mut parts = env_prefix_parts(&gate.env);
    parts.extend([
        shell_escape_path(exe_path),
        "usage-gate".to_string(),
        // 値は `--<名前>=<値>` の 1 引数で渡す。2 引数にすると、`-` で始まる profile 名を
        // usage-gate 自身の clap が別のフラグと読んで終了コード 2 で失敗し、ワーカーが
        // 「usage-gate failed closed」で止まる（`usage::live_command` と同じ理由）。
        shell_escape(&format!("--profile={}", gate.profile)),
        shell_escape(&format!("--provider={}", gate.provider)),
        "--threshold".to_string(),
        rate_limit_threshold.to_string(),
        "--stop-file".to_string(),
        shell_escape_path(stop_file),
        "--".to_string(),
    ]);
    parts.extend(gate.command.iter().map(|s| shell_escape(s)));
    parts.join(" ")
}

/// モニターペインが使う ai-usage のコマンド（env prefix 付き）。`--json` の出力で
/// statusline 用のキャッシュ（`cache_file`）を atomic に置き換え、statusline は `--input` で
/// そのキャッシュを読む。キャッシュは表示専用で、停止・一時停止の判定には使わない（判定は
/// usage-gate と実行開始時の判定が `--refresh` で取り直す）。ディスク容量は使用量の
/// キャッシュに混ぜず `--disk --json` で別に取る。command が空なら None。
fn build_monitor_ai_usage(
    gate: &UsageGateConfig,
    cache_file: &Path,
) -> Option<crate::monitor::AiUsageCommands> {
    Some(crate::monitor::AiUsageCommands {
        refresh: build_refresh_cmd(&gate.command, &gate.env)?,
        statusline: build_statusline_cmd(
            &gate.command,
            cache_file,
            &gate.env,
            &gate.profile,
            &gate.provider,
        )?,
        disk: build_disk_cmd(&gate.command, &gate.env)?,
        live: build_live_cmd(&gate.command, &gate.env, &gate.profile, &gate.provider)?,
        profile: gate.profile.clone(),
        provider: gate.provider.clone(),
        cache_file: cache_file.to_path_buf(),
    })
}

/// 起動時に statusline 用の ai-usage キャッシュを初期化し、monitor 起動直後から statusline を
/// 表示できるようにする（以後はモニターが 10 秒ごとに更新する）。実行開始時の判定
/// （check_start）が全アカウント分を取り直していれば（`startup_usage`）、その出力を使って
/// もう 1 回の取得を省く。取れていなければ従来どおりここで取得する。ai-usage コマンド自体が
/// ハングすると tmux 起動前に token-burn 全体が固まるため、AI_USAGE_STARTUP_TIMEOUT でキルする。
/// 失敗時はキャッシュ更新をスキップし、monitor 側の初回取得に委ねる。
fn init_statusline_cache(gate: &UsageGateConfig, startup_usage: Option<&[u8]>, cache_file: &Path) {
    let initial = match startup_usage {
        Some(bytes) => Ok(bytes.to_vec()),
        None => {
            spawn_ai_usage_sync_with_timeout(&gate.command, &gate.env, AI_USAGE_STARTUP_TIMEOUT)
        }
    };
    match initial {
        Ok(bytes) => {
            let _ = std::fs::write(cache_file, &bytes);
        }
        Err(e) => {
            eprintln!(
                "{}: failed to initialize ai-usage cache: {} (continuing without startup cache)",
                "warning".yellow(),
                e
            );
        }
    }
}

/// ワーカーの「停止判定 + claim」のコマンド文字列を組み立てる。恒久停止なら 10、pending が
/// 無ければ 20 で終わり、一時停止中（5 時間枠のように待てば回復する枠での停止）は再開時刻まで
/// 待つ。デッドラインは相対秒ではなく絶対 epoch で渡す。ワーカーは別プロセスで、しかも
/// 待機を挟むため、相対値だと基準時刻がプロセスごとにずれる。
fn build_gate_claim_cmd(
    exe_path: &Path,
    stop_file: &Path,
    queue_dir: &Path,
    deadline_epoch: i64,
    usage_gate_cmd: Option<&str>,
) -> String {
    let mut gate_claim_cmd = format!(
        "{} gate-claim --stop-file {} --queue-dir {} --deadline-epoch {}",
        shell_escape_path(exe_path),
        shell_escape_path(stop_file),
        shell_escape_path(queue_dir),
        deadline_epoch,
    );
    // 一時停止から再開する直前に、実データで使用率を確かめ直す。待機の根拠は停止時点の
    // 観測なので、これが無いと状況が変わっていても 1 件は開始してしまう。
    if let Some(cmd) = usage_gate_cmd {
        gate_claim_cmd.push_str(" --revalidate ");
        gate_claim_cmd.push_str(&shell_escape(cmd));
    }
    gate_claim_cmd
}

/// ワーカーごとのスクリプト（worker-N.sh）を書き出して実行権限を付け、tmux のペインで
/// 起動する順にパスを返す。
fn write_worker_scripts(
    layout: &RunLayout,
    worker_count: usize,
    usage_gate_cmd: Option<&str>,
    gate_claim_cmd: &str,
) -> Result<Vec<PathBuf>> {
    let mut script_paths = Vec::new();
    for w in 0..worker_count {
        let script_path = layout.tmp_dir.join(format!("worker-{}.sh", w));
        let worker_script = build_worker_script(&WorkerCtx {
            worker_id: w,
            queue_dir: &layout.queue_dir,
            task_dir: &layout.task_dir,
            marker_dir: &layout.marker_dir,
            usage_gate_cmd,
            gate_claim_cmd,
            start_file: Some(&layout.start_file),
        });
        std::fs::write(&script_path, &worker_script)?;
        ensure_executable(&script_path)?;
        script_paths.push(script_path);
    }
    Ok(script_paths)
}

/// この実行で保存された中断セッションの案内（無ければ `None`）。
///
/// `since` より後に保存された記録だけを数える（前の実行から残っている記録は含めない）。
/// 再開可能時刻が分かっていれば、それより前に打ち直しても待つことになる旨を添える。
///
/// `recovery_checked` は、次の実行が開始時に ai-usage の実データで回復を確かめる構成か。
/// そのときは手動リセットすれば再開可能時刻を待たずに始められることも添える。
#[cfg(test)]
fn saved_sessions_notice(
    store: &crate::resume::ResumeStore,
    agent_name: &str,
    tasks: &[ResolvedTarget],
    since: DateTime<FixedOffset>,
    recovery_checked: bool,
) -> Option<String> {
    let saved: Vec<&ResumeEntry> = tasks
        .iter()
        .filter_map(|task| store.get(agent_name, &task.directory))
        .filter(|entry| entry.interrupted_at >= since)
        .collect();
    if saved.is_empty() {
        return None;
    }
    let count = saved.len();
    let (noun, pronoun) = if count == 1 {
        ("session", "it")
    } else {
        ("sessions", "them")
    };
    let after = saved
        .iter()
        .filter_map(|entry| entry.retry_after)
        .max()
        .map(|at| {
            let early = if recovery_checked {
                ", or earlier if you reset it manually"
            } else {
                ""
            };
            format!(
                " (the rate limit resets at {}{early})",
                at.with_timezone(&Local).format("%Y-%m-%d %H:%M")
            )
        })
        .unwrap_or_default();
    Some(format!(
        "↻ Saved {count} interrupted {noun} — run the same command again to continue {pronoun}{after}"
    ))
}

pub(crate) fn finish_logs(directory: &Path) {
    strip_ansi_from_dir(directory);
}

#[cfg(test)]
mod tests {

    use super::flags::CLAUDE_PRINT_BG_WAIT_ENV;
    use super::*;

    #[test]
    fn worker_count_caps_parallelism_at_remaining_tasks() {
        assert_eq!(worker_count(4, 0), 0);
        assert_eq!(worker_count(4, 1), 1);
        assert_eq!(worker_count(4, 3), 3);
        assert_eq!(worker_count(4, 4), 4);
        assert_eq!(worker_count(4, 10), 4);
    }

    /// モニターがセッションを自動で閉じた後、呼び出し元の端末へ出す集計はマーカーから
    /// 数え直す。ワーカー完了マーカー（`worker-done-*`）やエラー詳細（`error-*`）を
    /// タスク件数に混ぜてはいけない。
    #[test]
    fn collect_run_summary_counts_task_markers_only() {
        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        let dir = tmp.path();
        for name in [
            "done-1",
            "done-2",
            "failed-3",
            "retry-4",
            "worker-done-0",
            "worker-done-1",
            "error-3",
            "pending-9",
        ] {
            std::fs::write(dir.join(name), "").expect("マーカーを作成できるべき");
        }

        let summary = collect_run_summary(dir);

        assert_eq!(
            summary,
            RunSummary {
                done: 2,
                failed: 1,
                retry: 1,
                // 中身の無い error-* は理由として出さない
                errors: Vec::new(),
            }
        );
        assert_eq!(summary.processed(), 4);
    }

    /// tmux を閉じた後に出す理由は、モニターのペインと同じくタスク番号の順に並べる
    /// （辞書順ではない）。改行は畳み、エスケープシーケンスは除く。
    #[test]
    fn collect_run_summary_reads_errors_in_task_order() {
        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        let dir = tmp.path();
        for (name, text) in [
            ("failed-3", ""),
            ("failed-10", ""),
            ("retry-2", ""),
            ("error-10", "[repo-j] later\n"),
            ("error-3", "[repo-c] boom\n\x1b[31mred\x1b[0m\n"),
            ("error-2", "[repo-b] wrapper: setup failed\n"),
        ] {
            std::fs::write(dir.join(name), text).expect("マーカーを作成できるべき");
        }

        let summary = collect_run_summary(dir);

        assert_eq!(
            summary.errors,
            [
                "[repo-b] wrapper: setup failed",
                "[repo-c] boom red",
                "[repo-j] later",
            ]
        );
        assert_eq!((summary.failed, summary.retry), (2, 1));
    }

    fn summary_with_errors(count: usize) -> RunSummary {
        RunSummary {
            failed: count,
            errors: (1..=count).map(|i| format!("[repo-{i}] boom")).collect(),
            ..RunSummary::default()
        }
    }

    /// 端末に出すのは 20 件まで。超えた分は件数にまとめ、全件を書けたときだけ errors.txt を案内する。
    #[test]
    fn run_summary_error_lines_are_capped_with_an_overflow_line() {
        assert!(RunSummary::default().error_lines(false).is_empty());

        let few = summary_with_errors(2).error_lines(true);
        assert_eq!(few, ["❌ [repo-1] boom", "❌ [repo-2] boom"]);

        let exactly = summary_with_errors(SHOWN_ERRORS_LIMIT).error_lines(true);
        assert_eq!(exactly.len(), SHOWN_ERRORS_LIMIT);
        assert_eq!(exactly.last().unwrap(), "❌ [repo-20] boom");

        let many = summary_with_errors(SHOWN_ERRORS_LIMIT + 3);
        let lines = many.error_lines(true);
        assert_eq!(lines.len(), SHOWN_ERRORS_LIMIT + 1);
        assert_eq!(lines[0], "❌ [repo-1] boom");
        assert_eq!(lines[SHOWN_ERRORS_LIMIT - 1], "❌ [repo-20] boom");
        assert_eq!(
            lines[SHOWN_ERRORS_LIMIT],
            "❌ +3 more errors (see errors.txt in the logs directory)"
        );
        // 書けなかったファイルは案内しない
        assert_eq!(
            many.error_lines(false)[SHOWN_ERRORS_LIMIT],
            "❌ +3 more errors"
        );
    }

    /// 全件を 1 行 1 件で残す。エラーが無ければファイルを作らない。
    #[test]
    fn write_errors_file_keeps_every_error_and_skips_when_there_are_none() {
        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        let errors = summary_with_errors(SHOWN_ERRORS_LIMIT + 2).errors;

        assert!(write_errors_file(tmp.path(), &errors).expect("書けるべき"));
        let text = std::fs::read_to_string(tmp.path().join(ERRORS_FILE_NAME))
            .expect("errors.txt を読めるべき");
        assert_eq!(text.lines().collect::<Vec<_>>(), errors);
        assert!(text.ends_with("[repo-22] boom\n"), "{text}");

        let empty = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        assert!(!write_errors_file(empty.path(), &[]).expect("何もしないだけ"));
        assert!(!empty.path().join(ERRORS_FILE_NAME).exists());
    }

    /// 書けなければエラーを返す（呼び出し側は警告して続ける。表示のための記録なので
    /// 実行の成否には関わらない）。
    #[test]
    fn write_errors_file_reports_a_write_failure() {
        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        let missing = tmp.path().join("missing");
        let error = write_errors_file(&missing, &["[repo] boom".to_string()])
            .expect_err("存在しないディレクトリには書けない");
        assert!(format!("{error:#}").contains("errors.txt"), "{error:#}");
    }

    /// マーカーディレクトリが読めない場合も表示だけの集計なので 0 件で続行する。
    #[test]
    fn collect_run_summary_is_empty_for_unreadable_dir() {
        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");

        let summary = collect_run_summary(&tmp.path().join("missing"));

        assert_eq!(summary, RunSummary::default());
    }

    /// stdout の集計はモニターペインの完了行と同じ文言・同じ分岐にする。
    #[test]
    fn run_summary_describe_matches_monitor_wording() {
        let all_done = RunSummary {
            done: 10,
            failed: 0,
            retry: 0,
            ..RunSummary::default()
        };
        assert_eq!(all_done.describe(10), "✅ All 10/10 tasks completed");
        assert!(!all_done.has_issues(10));

        let mixed = RunSummary {
            done: 8,
            failed: 1,
            retry: 1,
            ..RunSummary::default()
        };
        assert_eq!(
            mixed.describe(10),
            "❗ Completed: 8 succeeded / 1 failed / 1 retry"
        );
        assert!(mixed.has_issues(10));

        let stopped = RunSummary {
            done: 4,
            failed: 1,
            retry: 0,
            ..RunSummary::default()
        };
        assert_eq!(
            stopped.describe(10),
            "🛑 Stopped: 5/10 processed (fail:1 retry:0)"
        );
        assert!(stopped.has_issues(10));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_executable_adds_execute_permission() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        let path = tmp.path().join("script.sh");
        std::fs::write(&path, "#!/bin/sh\n").expect("テスト用スクリプトを書き込めるべき");
        let mut permissions = std::fs::metadata(&path)
            .expect("テスト用スクリプトのメタデータを取得できるべき")
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&path, permissions).expect("実行前の権限を設定できるべき");

        ensure_executable(&path).expect("実行権限を付与できるべき");

        let mode = std::fs::metadata(&path)
            .expect("実行後のメタデータを取得できるべき")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_executable_reports_missing_path() {
        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        let missing = tmp.path().join("missing-script.sh");

        let error = ensure_executable(&missing).expect_err("存在しないパスは失敗するべき");

        assert!(error.to_string().contains("chmod +x"));
    }

    #[test]
    fn spawn_ai_usage_sync_redacts_secret_on_spawn_failure() {
        let secret = "actual-secret-value";
        let command = vec![
            "token-burn-command-that-does-not-exist".to_string(),
            "--api-key".to_string(),
            secret.to_string(),
        ];

        let error = spawn_ai_usage_sync_with_timeout(
            &command,
            &std::collections::BTreeMap::new(),
            Duration::from_secs(1),
        )
        .expect_err("存在しないコマンドの起動は失敗するべき");
        let message = error.to_string();

        assert!(message.contains("--api-key <redacted>"));
        assert!(!message.contains(secret));
    }

    #[test]
    fn spawn_ai_usage_sync_kills_hanging_child_within_timeout() {
        // ハングする子プロセス（sleep 60）を 1 秒でキルできることを確認する。
        // タイムアウトなしだと token-burn 起動が固まる回帰を検知するためのテスト。
        let start = std::time::Instant::now();
        let result = spawn_ai_usage_sync_with_timeout(
            &["sleep".to_string(), "60".to_string()],
            &Default::default(),
            Duration::from_secs(1),
        );
        let elapsed = start.elapsed();
        assert!(result.is_err(), "hanging child should return Err");
        let msg = format!("{}", result.err().unwrap());
        assert!(
            msg.contains("timed out"),
            "error should mention timeout, got: {msg}"
        );
        // 1 秒タイムアウト + 50ms ポーリング粒度で最大 ~1.5s まで許容。
        assert!(
            elapsed < Duration::from_secs(3),
            "timeout should fire promptly (took {elapsed:?})"
        );
    }

    #[test]
    fn spawn_ai_usage_sync_returns_stdout_for_fast_command() {
        // 早く終わるコマンドはタイムアウト前に stdout を返す。
        let result = spawn_ai_usage_sync_with_timeout(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf 'hello world'".to_string(),
            ],
            &Default::default(),
            Duration::from_secs(5),
        );
        let bytes = result.expect("should succeed");
        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn spawn_ai_usage_sync_times_out_if_descendant_keeps_pipe_open() {
        // 親だけが正常終了しても、孫が stdout/stderr を持っていれば全出力は確定しない。
        let result = spawn_ai_usage_sync_with_timeout(
            &[
                "sh".into(),
                "-c".into(),
                "sleep 3 & printf done; exit 0".into(),
            ],
            &Default::default(),
            Duration::from_millis(200),
        );
        assert!(result.is_err(), "EOF 待ちもタイムアウトの対象にする");
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[test]
    fn spawn_ai_usage_sync_kills_descendant_on_timeout() {
        let start = std::time::Instant::now();
        let result = spawn_ai_usage_sync_with_timeout(
            &["sh".into(), "-c".into(), "sleep 3 & wait".into()],
            &Default::default(),
            Duration::from_millis(200),
        );
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "孫の終了まで待ってはいけない"
        );
    }

    /// 設定の env は「空文字の値は unset」という規約（シェル側は `env -u KEY`）で扱う。
    /// `envs()` へ素通しすると空文字がそのまま子へ渡り、`CLAUDE_CONFIG_DIR=""` を
    /// 「既定に戻す」つもりで書いた profile で ai-usage が cwd 相対の設定ディレクトリを
    /// 見に行って別アカウントの使用率を拾う。その結果で実行開始時の判定（保留・停止）が
    /// 走り、statusline にも別アカウントの使用率が出る。
    ///
    /// `${VAR-<unset>}` は「未設定」のときだけ既定値へ落ちるため、空文字を素通しする
    /// 旧実装では空文字が返り、除去する現実装では `<unset>` が返る。テストプロセスの
    /// 環境を書き換えなくても両者を区別できる（`std::env::set_var` は並列実行される
    /// 他テストの `getenv` と競合し得るので使わない）。
    #[test]
    fn spawn_ai_usage_sync_unsets_env_keys_with_empty_value() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("TOKEN_BURN_ENV_UNSET_PROBE".to_string(), String::new());

        let bytes = spawn_ai_usage_sync_with_timeout(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf '%s' \"${TOKEN_BURN_ENV_UNSET_PROBE-<unset>}\"".to_string(),
            ],
            &env,
            Duration::from_secs(5),
        )
        .expect("should succeed");

        assert_eq!(
            String::from_utf8_lossy(&bytes),
            "<unset>",
            "空文字の env は子プロセスへ空文字として渡さず、未設定にするべき"
        );
    }

    /// 非空の値は従来どおり子プロセスへ渡す。
    #[test]
    fn spawn_ai_usage_sync_passes_non_empty_env_values() {
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "TOKEN_BURN_ENV_SET_PROBE".to_string(),
            "/tmp/profile".to_string(),
        );

        let bytes = spawn_ai_usage_sync_with_timeout(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf '%s' \"$TOKEN_BURN_ENV_SET_PROBE\"".to_string(),
            ],
            &env,
            Duration::from_secs(5),
        )
        .expect("should succeed");

        assert_eq!(String::from_utf8_lossy(&bytes), "/tmp/profile");
    }

    #[test]
    fn spawn_ai_usage_sync_handles_output_larger_than_pipe_buffer() {
        // stdout がパイプバッファ（macOS で ~16KB）を大きく超えても、非同期 I/O で並行に
        // 読むためデッドロックせず全出力を返す。旧実装（子の終了後にまとめて読む）では
        // 子が write(2) でブロックして終了できず、タイムアウトまでハングする回帰を検知する。
        let size = 100_000;
        let result = spawn_ai_usage_sync_with_timeout(
            &[
                "sh".to_string(),
                "-c".to_string(),
                format!("head -c {size} /dev/zero"),
            ],
            &Default::default(),
            Duration::from_secs(5),
        );
        let bytes = result.expect("large stdout should not deadlock");
        assert_eq!(bytes.len(), size, "全出力が回収されるべき");
    }

    fn make_agent(command: Vec<&str>) -> RuntimeAgent {
        RuntimeAgent {
            name: "claude".to_string(),
            command: command.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn build_plan_clones_agent_and_targets() {
        let agent = make_agent(vec!["claude", "-p"]);
        let targets = vec![ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        }];
        let plan = build_plan(&agent, targets, None, ResumeOptions::default());
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].display_name, "repo");
        // claude エージェントにはフラグが自動付与される
        assert!(plan.agent.command.contains(&"--verbose".to_string()));
        assert!(
            plan.agent
                .command
                .contains(&"--disallowedTools=AskUserQuestion".to_string())
        );
        assert_eq!(plan.output, OutputPipeline::ClaudeStreamJson);
    }

    /// 出力の処理方法は付与したフラグと対応する（--json を付けたら整形、付けなければ素通し）。
    #[test]
    fn build_plan_resolves_the_output_pipeline_with_the_flags() {
        let plan_for = |provider: Option<&str>, command: &[&str], mode: CodexOutput| {
            let agent = RuntimeAgent {
                name: "agent".to_string(),
                provider: provider.map(String::from),
                command: command.iter().map(|s| s.to_string()).collect(),
                codex_output: mode,
                ..Default::default()
            };
            build_plan(
                &agent,
                vec![repo_target("/tmp/repo")],
                None,
                ResumeOptions::default(),
            )
        };

        let plan = plan_for(None, &["codex", "exec"], CodexOutput::Auto);
        assert_eq!(plan.output, OutputPipeline::CodexJson);
        assert_eq!(
            plan.agent.command,
            ["codex", "-c", "approval_policy=never", "exec", "--json"]
        );

        let plan = plan_for(
            Some("codex"),
            &["/opt/bin/codex-wrapper.sh"],
            CodexOutput::Auto,
        );
        assert_eq!(plan.output, OutputPipeline::CodexRaw);
        assert!(!plan.agent.command.iter().any(|arg| arg == "--json"));

        let plan = plan_for(None, &["codex", "exec"], CodexOutput::Human);
        assert_eq!(plan.output, OutputPipeline::CodexRaw);
        assert!(!plan.agent.command.iter().any(|arg| arg == "--json"));

        let plan = plan_for(None, &["aider", "--yes"], CodexOutput::Auto);
        assert_eq!(plan.output, OutputPipeline::Raw);
        assert_eq!(plan.agent.command, ["aider", "--yes"]);
    }

    /// 実行計画には codex の出力の扱いだけを出す。claude とその他は従来の表示のまま。
    #[test]
    fn plan_label_explains_codex_output_only() {
        assert_eq!(
            OutputPipeline::CodexJson.plan_label(CodexOutput::Auto),
            Some("codex JSON events → format-codex")
        );
        assert_eq!(
            OutputPipeline::CodexRaw.plan_label(CodexOutput::Human),
            Some("raw (codex_output = \"human\")")
        );
        // exec を見つけられずに素通しになった場合は、整形させる方法を案内する
        let auto = OutputPipeline::CodexRaw
            .plan_label(CodexOutput::Auto)
            .expect("label");
        assert!(auto.contains("codex_output = \"json\""), "{auto}");
        assert_eq!(
            OutputPipeline::ClaudeStreamJson.plan_label(CodexOutput::Auto),
            None
        );
        assert_eq!(OutputPipeline::Raw.plan_label(CodexOutput::Auto), None);
        // stdin を閉じるのは codex だけ
        assert!(OutputPipeline::CodexJson.closes_stdin());
        assert!(OutputPipeline::CodexRaw.closes_stdin());
        assert!(!OutputPipeline::ClaudeStreamJson.closes_stdin());
        assert!(!OutputPipeline::Raw.closes_stdin());
    }

    fn saved_session(retry_after: Option<DateTime<FixedOffset>>) -> ResumeEntry {
        ResumeEntry {
            session_id: "0633fa18-8643-40f7-9106-a35330ed247d".to_string(),
            interrupted_at: Local::now().fixed_offset(),
            retry_after,
            retry_basis_percent: None,
            prompt_hash: crate::resume::prompt_hash("review"),
            message: None,
            log: None,
            failed_resumes: 0,
        }
    }

    fn repo_target(dir: &str) -> ResolvedTarget {
        ResolvedTarget {
            directory: PathBuf::from(dir),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        }
    }

    #[test]
    fn build_plan_keeps_resumes_only_for_planned_tasks() {
        // limit や TUI で外れたターゲットの記録は、再開にも待機の根拠にも使わない。
        let agent = make_agent(vec!["claude", "-p"]);
        let later = Local::now().fixed_offset() + chrono::Duration::hours(1);
        let sessions = HashMap::from([
            (PathBuf::from("/tmp/repo"), saved_session(None)),
            (PathBuf::from("/tmp/dropped"), saved_session(Some(later))),
        ]);
        let plan = build_plan(
            &agent,
            vec![repo_target("/tmp/repo")],
            None,
            ResumeOptions {
                save_interrupted: true,
                sessions,
                prompt: "continue".to_string(),
                discard: HashMap::new(),
            },
        );
        assert_eq!(plan.resume.sessions.len(), 1);
        assert!(plan.resume.sessions.contains_key(Path::new("/tmp/repo")));
        assert!(plan.resume.save_interrupted);
        assert_eq!(plan.start, StartGate::Proceed);
    }

    #[test]
    fn saved_sessions_notice_counts_only_sessions_saved_in_this_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.json");
        let started = Local::now().fixed_offset();
        let reset = started + chrono::Duration::hours(1);
        let mut fresh = saved_session(Some(reset));
        fresh.interrupted_at = started + chrono::Duration::seconds(5);
        let mut stale = saved_session(None);
        stale.interrupted_at = started - chrono::Duration::days(1);
        crate::resume::save_atomic(&path, "claude", Path::new("/tmp/a"), fresh).unwrap();
        crate::resume::save_atomic(&path, "claude", Path::new("/tmp/b"), stale).unwrap();
        let store = crate::resume::ResumeStore::load_or_warn(&path);
        let tasks = vec![repo_target("/tmp/a"), repo_target("/tmp/b")];

        let notice =
            saved_sessions_notice(&store, "claude", &tasks, started, false).expect("notice");
        assert!(
            notice.starts_with(
                "↻ Saved 1 interrupted session — run the same command again to continue it"
            ),
            "{notice}"
        );
        assert!(
            notice.contains(&format!(
                "(the rate limit resets at {})",
                reset.with_timezone(&Local).format("%Y-%m-%d %H:%M")
            )),
            "{notice}"
        );
        // 次の実行が実データで回復を確かめる構成なら、手動リセットで早く始められる。
        let notice =
            saved_sessions_notice(&store, "claude", &tasks, started, true).expect("notice");
        assert!(
            notice.contains(&format!(
                "(the rate limit resets at {}, or earlier if you reset it manually)",
                reset.with_timezone(&Local).format("%Y-%m-%d %H:%M")
            )),
            "{notice}"
        );
        // 別エージェントの記録や、この実行より前の記録だけなら案内しない
        assert_eq!(
            saved_sessions_notice(&store, "claude-home", &tasks, started, false),
            None
        );
        assert_eq!(
            saved_sessions_notice(&store, "claude", &tasks[1..], started, false),
            None
        );
    }

    #[test]
    fn build_plan_discards_only_unused_sessions_of_planned_tasks() {
        let agent = make_agent(vec!["claude", "-p"]);
        let old = "11111111-2222-4333-8444-555555555555".to_string();
        let plan = build_plan(
            &agent,
            vec![repo_target("/tmp/repo"), repo_target("/tmp/fresh")],
            None,
            ResumeOptions {
                save_interrupted: true,
                sessions: HashMap::from([(PathBuf::from("/tmp/repo"), saved_session(None))]),
                discard: HashMap::from([
                    (PathBuf::from("/tmp/fresh"), old.clone()),
                    // 再開するタスクの記録は破棄しない
                    (PathBuf::from("/tmp/repo"), old.clone()),
                    // 計画外（limit / TUI で外れた）ターゲットの記録には触れない
                    (PathBuf::from("/tmp/dropped"), old.clone()),
                ]),
                prompt: "continue".to_string(),
            },
        );
        assert_eq!(
            plan.resume.discard,
            HashMap::from([(PathBuf::from("/tmp/fresh"), old)])
        );
    }

    #[test]
    fn build_plan_never_resumes_non_claude_agents() {
        let agent = RuntimeAgent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        let plan = build_plan(
            &agent,
            vec![repo_target("/tmp/repo")],
            None,
            ResumeOptions {
                save_interrupted: true,
                sessions: HashMap::from([(PathBuf::from("/tmp/repo"), saved_session(None))]),
                prompt: "continue".to_string(),
                discard: HashMap::new(),
            },
        );
        assert!(plan.resume.sessions.is_empty());
        assert!(!plan.resume.save_interrupted);
    }

    #[test]
    fn build_plan_holds_until_the_latest_future_retry_time() {
        let agent = make_agent(vec!["claude", "-p"]);
        let now = Local::now().fixed_offset();
        let soon = now + chrono::Duration::minutes(10);
        let later = now + chrono::Duration::minutes(40);
        let past = now - chrono::Duration::minutes(40);
        let sessions = HashMap::from([
            (PathBuf::from("/tmp/a"), saved_session(Some(soon))),
            (PathBuf::from("/tmp/b"), saved_session(Some(later))),
            (PathBuf::from("/tmp/c"), saved_session(Some(past))),
        ]);
        let plan = build_plan(
            &agent,
            vec![
                repo_target("/tmp/a"),
                repo_target("/tmp/b"),
                repo_target("/tmp/c"),
            ],
            None,
            ResumeOptions {
                save_interrupted: true,
                sessions,
                prompt: "continue".to_string(),
                discard: HashMap::new(),
            },
        );
        // 全セッションが同じ枠の制限を受けるので、最も遅い時刻まで待つ。実データで確かめる
        // 前の既定値なので、根拠は保存した記録（拒否されたときの使用率）になる。
        let StartGate::Hold { pause, why } = &plan.start else {
            panic!("expected a hold, got {:?}", plan.start);
        };
        assert_eq!(pause.resume_at, later.timestamp());
        assert_eq!(pause.source, PauseSource::Saved);
        assert_eq!(pause.cause, PauseCause::Rejected);
        assert_eq!(why, SAVED_HOLD_REASON);

        // すべて過ぎていれば待たない
        assert_eq!(
            start_from_saved([&saved_session(Some(past))], now.timestamp()),
            StartGate::Proceed
        );
    }

    fn five_hour(used: f64, reset_at: Option<i64>) -> rate_control::WindowObservation {
        rate_control::WindowObservation::new(rate_control::WindowKind::FIVE_HOUR, "five_hour", used)
            .with_reset_at(reset_at)
    }

    fn weekly(used: f64, reset_at: Option<i64>) -> rate_control::WindowObservation {
        rate_control::WindowObservation::new(rate_control::WindowKind::Deadline, "weekly", used)
            .with_reset_at(reset_at)
    }

    /// `now` に `--refresh` で取り直した実データ。
    fn live_at(
        windows: Vec<rate_control::WindowObservation>,
        now: i64,
    ) -> std::result::Result<LiveObservation, LiveError> {
        Ok(LiveObservation {
            windows,
            observed_from: now - rate_control::LIVE_FRESHNESS_MARGIN_SECS,
            fresh: true,
        })
    }

    /// 5 時間枠で拒否されて中断した記録。`interrupted_ago` 秒前に中断し、`retry_in` 秒後に
    /// 再開できると見積もった。
    fn rejected_session(
        now: i64,
        interrupted_ago: i64,
        retry_in: i64,
        basis: Option<f64>,
    ) -> ResumeEntry {
        use chrono::TimeZone;
        let at = |epoch: i64| Local.timestamp_opt(epoch, 0).unwrap().fixed_offset();
        ResumeEntry {
            interrupted_at: at(now - interrupted_ago),
            retry_after: Some(at(now + retry_in)),
            retry_basis_percent: basis,
            ..saved_session(None)
        }
    }

    #[test]
    fn start_stops_when_a_deadline_window_is_over_the_threshold() {
        // 週次枠はデッドラインと同じ周期なので、待ってもこの実行の残り時間では回復しない。
        // 1 件目を始めてから usage-gate で止めるのではなく、最初から始めない。
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 60, 3600, Some(100.0));
        let gate = decide_start(
            [&session],
            &live_at(
                vec![
                    weekly(96.0, Some(now + 3 * 86_400)),
                    five_hour(10.0, Some(now + 3600)),
                ],
                now,
            ),
            now,
            95,
            now,
        );
        let StartGate::Stop { reason } = &gate else {
            panic!("expected a stop, got {gate:?}");
        };
        assert_eq!(reason, "weekly 96% >= threshold 95% (ai-usage)");
    }

    #[test]
    fn start_stop_explains_why_it_cannot_wait() {
        // 5 時間枠でもリセット時刻が読めなければ、いつ再開してよいか分からないので始めない
        // （usage-gate と同じ fail-closed）。理由を添える。
        let now = rate_control::now_epoch();
        let gate = decide_start([], &live_at(vec![five_hour(97.0, None)], now), now, 95, now);
        assert_eq!(
            gate,
            StartGate::Stop {
                reason: "five_hour 97% >= threshold 95% (ai-usage: reset time unavailable)"
                    .to_string()
            }
        );
    }

    #[test]
    fn start_pauses_until_the_window_resets_even_without_resumed_sessions() {
        // 5 時間枠が閾値に触れていれば、再開するセッションが無くても待つ（枠はアカウント単位）。
        let now = rate_control::now_epoch();
        let reset = now + 1800;
        let gate = decide_start(
            [],
            &live_at(
                vec![
                    five_hour(97.0, Some(reset)),
                    weekly(40.0, Some(now + 3 * 86_400)),
                ],
                now,
            ),
            now - 1,
            95,
            now,
        );
        let StartGate::Hold { pause, why } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(
            pause.resume_at,
            reset + rate_control::RESET_PROPAGATION_GRACE_SECS
        );
        assert_eq!(pause.window, "five_hour");
        // モニターが同じ取得元どうしで回復を比べられるよう、根拠と観測時刻を残す。
        assert_eq!(pause.source, PauseSource::AiUsage);
        assert_eq!(pause.cause, PauseCause::Threshold);
        assert_eq!(pause.observed_at, now - 1, "取得を終えた時刻");
        assert_eq!(pause.used_percent, Some(97.0));
        assert_eq!(pause.window_reset_at, Some(reset));
        assert_eq!(why, "five_hour 97% >= threshold 95% (ai-usage)");
    }

    #[test]
    fn start_does_not_wait_for_a_window_that_was_reset_manually() {
        // 実際に起きた事故の再現: 5 時間枠が 100% で止まった直後に手動リセットし、すぐに
        // 実行し直した。保存した再開時刻（1 時間半後）まで待たずに始める。
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 360, 5400, Some(100.0));
        let gate = decide_start(
            [&session],
            &live_at(
                vec![five_hour(0.0, None), weekly(30.0, Some(now + 3 * 86_400))],
                now,
            ),
            now,
            95,
            now,
        );
        let StartGate::Recovered {
            until, evidence, ..
        } = &gate
        else {
            panic!("expected to start without waiting, got {gate:?}");
        };
        assert_eq!(*until, now + 5400);
        assert_eq!(evidence, "five_hour dropped from 100% to 0%");
    }

    /// 待たずに始めた（`Recovered`）ことは、実行中のモニターの早期解除と同じく「解除した
    /// 一時停止」として pause file に残す。直後に API がまた拒否したら再発の印が付き、
    /// 取得元の違う ai-usage の値では解除しない（再発ガード）。何も書かずに始めていた頃は、
    /// 直後の拒否が新しい一時停止（再発でない）になり、同じ誤った値での解除と拒否を 1 巡
    /// 余分に繰り返していた。
    #[test]
    fn recovered_start_records_a_lifted_pause_so_a_relapse_is_detected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop_file = tmp.path().join("stop");
        let pause_file = rate_control::pause_path_for(&stop_file);
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 360, 5400, Some(100.0));
        let gate = decide_start(
            [&session],
            &live_at(
                vec![five_hour(0.0, None), weekly(30.0, Some(now + 3 * 86_400))],
                now,
            ),
            now,
            95,
            now,
        );
        assert!(matches!(gate, StartGate::Recovered { .. }), "{gate:?}");

        write_start_pause(&gate, &stop_file).unwrap();
        let state = rate_control::read_pause(&pause_file)
            .unwrap()
            .expect("解除した状態で残す");
        assert!(state.is_lifted(), "{state:?}");
        assert!(!state.blocks(now), "ワーカーは待たずに claim する");
        assert_eq!(
            state.lift_reason.as_deref(),
            Some("five_hour dropped from 100% to 0%")
        );

        // 解除の後（次の秒以降）に API が拒否したら、再発として書かれる。
        let rejected = PauseState::new(now + 1800, "five_hour", "request rejected (five_hour)")
            .observed(PauseSource::Api, PauseCause::Rejected, now + 1)
            .with_basis(100.0, Some(now + 1770));
        let outcome = rate_control::write_pause(&pause_file, &rejected).unwrap();
        assert_eq!(
            outcome,
            rate_control::PauseWrite::Started { relapse: true },
            "解除の直後の拒否は再発として扱う"
        );
    }

    #[test]
    fn start_holds_until_the_saved_time_when_the_window_has_not_recovered() {
        // 閾値未満というだけでは回復と言えない（拒否は使用率と別の理由でも起き、取得元の
        // 違う値は反映の遅れでずれる）。保存した再開時刻まで待ち、その間はモニターが
        // 回復を確かめ続けられるよう、拒否されたときの根拠を一時停止に残す。
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 360, 5400, Some(100.0));
        let gate = decide_start(
            [&session],
            &live_at(vec![five_hour(85.0, Some(now + 5370))], now),
            now,
            95,
            now,
        );
        let StartGate::Hold { pause, why } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(pause.resume_at, now + 5400);
        assert_eq!(pause.window, "five_hour");
        assert_eq!(pause.source, PauseSource::Saved);
        assert_eq!(pause.cause, PauseCause::Rejected);
        assert_eq!(pause.observed_at, now - 360, "根拠は中断したときの観測");
        assert_eq!(pause.used_percent, Some(100.0));
        assert_eq!(
            pause.window_reset_at,
            Some(now + 5400 - rate_control::RESET_PROPAGATION_GRACE_SECS)
        );
        assert_eq!(
            why,
            "a resumed session hit the five_hour limit (not recovered yet: five_hour at 85% (was 100% when paused))"
        );
    }

    #[test]
    fn legacy_records_without_a_basis_wait_for_the_saved_time() {
        // 以前の版が保存した記録には拒否されたときの使用率が無い。拒否は使用率と別の理由でも
        // 起きる（42% で拒否された例がある）ので、100% と補って比べると、42% のままでも
        // 回復とみなしてしまう。比べる元が無い記録は回復を確かめられないとして、保存した
        // 再開時刻まで保留する。
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 360, 5400, None);
        let gate = decide_start(
            [&session],
            &live_at(vec![five_hour(40.0, None)], now),
            now,
            95,
            now,
        );
        let StartGate::Hold { pause, why } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(pause.resume_at, now + 5400);
        assert_eq!(pause.used_percent, None);
        assert!(why.contains("cannot confirm a recovery"), "{why}");
        // 根拠のある記録と不明な記録をまとめても、不明な方を既知の使用率で補わない。
        let known = rejected_session(now, 300, 3000, Some(100.0));
        let merged = saved_pause(&[&session, &known]).unwrap();
        assert_eq!(merged.used_percent, None);
    }

    #[test]
    fn start_keeps_the_saved_rejection_while_a_window_is_over_the_threshold() {
        // 保存した記録は 42% で拒否されて 90 分後まで待つ見込み。開始時の実データは 5 時間枠が
        // 95% でリセットは 30 分後。実データだけで一時停止を作ると、後で 40% になったとき
        // （記録の拒否の条件は満たしていない）にモニターが解除してしまい、モニターが回復を
        // 確かめられない構成では 30 分で再開してしまう。記録の条件も残す。
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 360, 5400, Some(42.0));
        let gate = decide_start(
            [&session],
            &live_at(vec![five_hour(95.0, Some(now + 1800))], now),
            now,
            95,
            now,
        );
        let StartGate::Hold { pause, why } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(pause.resume_at, now + 5400, "待つ時刻は遅い方");
        assert_eq!(pause.used_percent, Some(42.0), "比較の元は厳しい方");
        assert!(
            why.contains("a resumed session hit the five_hour limit"),
            "{why}"
        );
        let later = LiveObservation {
            windows: vec![five_hour(40.0, Some(now + 1800))],
            observed_from: now + 60,
            fresh: true,
        };
        assert!(matches!(
            rate_control::check_recovery(pause, &later, 95, now + 60),
            Recovery::NotYet(_)
        ));
    }

    #[test]
    fn start_holds_when_the_usage_may_come_from_the_ai_usage_cache() {
        // `--refresh` を使えない古い ai-usage の値は、共有キャッシュ（既定 120 秒）を通って
        // リセット前のものかもしれない。回復の証拠にせず、保存した再開時刻まで待つ。
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 360, 5400, Some(100.0));
        let cached = Ok(LiveObservation {
            windows: vec![five_hour(0.0, None)],
            observed_from: now - rate_control::LIVE_FRESHNESS_MARGIN_SECS,
            fresh: false,
        });
        let gate = decide_start([&session], &cached, now, 95, now);
        let StartGate::Hold { pause, why } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(pause.resume_at, now + 5400);
        assert!(why.contains("cannot confirm a recovery"), "{why}");
    }

    #[test]
    fn start_does_not_begin_when_the_usage_cannot_be_confirmed() {
        // 取得の失敗・ok:false では使用率を確認できない。usage-gate と同じく止める側に倒し、
        // 使用率の分からないまま並列数ぶんのタスクを始めない。
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 360, 5400, Some(100.0));
        let unavailable = Err(LiveError::Unavailable("auth expired".to_string()));
        for sessions in [vec![&session], vec![]] {
            let gate = decide_start(sessions, &unavailable, now, 95, now);
            let StartGate::Stop { reason } = &gate else {
                panic!("expected a stop, got {gate:?}");
            };
            assert!(reason.contains("auth expired"), "{reason}");
        }
    }

    #[test]
    fn start_falls_back_to_the_saved_records_when_the_account_is_missing() {
        // 出力に該当アカウントが無いのは、usage-gate でも過剰停止を避けて続行する扱い。
        // 保存した再開時刻だけで決める。
        let now = rate_control::now_epoch();
        let session = rejected_session(now, 360, 5400, Some(100.0));
        let missing = Err(LiveError::NoEntry(
            "no ai-usage entry for (Work, claude)".to_string(),
        ));
        let gate = decide_start([&session], &missing, now, 95, now);
        let StartGate::Hold { pause, why } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(Some(pause), saved_pause(&[&session]).as_ref());
        assert_eq!(
            why,
            "a resumed session hit the five_hour limit (ai-usage: no ai-usage entry for (Work, claude))"
        );
        // 保留する根拠が無ければ、そのまま始める。
        assert_eq!(decide_start([], &missing, now, 95, now), StartGate::Proceed);
        // 再開時刻を過ぎた記録は待つ根拠にならない。
        let past = rejected_session(now, 9000, -60, Some(100.0));
        assert_eq!(
            decide_start([&past], &missing, now, 95, now),
            StartGate::Proceed
        );
    }

    #[test]
    fn start_waits_for_the_latest_session_that_has_not_recovered() {
        let now = rate_control::now_epoch();
        let soon = rejected_session(now, 600, 1200, Some(100.0));
        let later = rejected_session(now, 300, 3000, Some(100.0));
        let past = rejected_session(now, 9000, -60, Some(100.0));
        let gate = decide_start(
            [&soon, &later, &past],
            &live_at(vec![five_hour(90.0, None)], now),
            now,
            95,
            now,
        );
        let StartGate::Hold { pause, .. } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(pause.resume_at, now + 3000);
        assert_eq!(pause.observed_at, now - 300);
        // 再開時刻を過ぎた記録しか無ければ、待つ根拠が無いのでそのまま始める。
        assert_eq!(
            decide_start(
                [&past],
                &live_at(vec![five_hour(90.0, None)], now),
                now,
                95,
                now
            ),
            StartGate::Proceed
        );
    }

    #[test]
    fn a_combined_hold_keeps_the_strictest_recovery_condition() {
        // 42% で拒否された記録と 100% で拒否された記録がある。後者の根拠だけを pause file に
        // 残すと、モニターは 40% を「100% から下がった」とみなして解除するが、前者にとって
        // 42% → 40% は回復ではない。まとめた一時停止は最も低い根拠と最も新しい観測を持つ。
        let now = rate_control::now_epoch();
        let low = rejected_session(now, 600, 1800, Some(42.0));
        let high = rejected_session(now, 300, 3600, Some(100.0));
        let gate = decide_start(
            [&low, &high],
            &live_at(vec![five_hour(60.0, None)], now),
            now,
            95,
            now,
        );
        let StartGate::Hold { pause, why } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(pause.resume_at, now + 3600, "最も遅い再開時刻まで待つ");
        assert_eq!(pause.used_percent, Some(42.0), "最も厳しい下がり幅を求める");
        assert_eq!(
            pause.observed_at,
            now - 300,
            "最も新しい中断より後のデータを求める"
        );
        assert_eq!(
            why,
            "a resumed session hit the five_hour limit (not recovered yet: five_hour at 60% (was 42% when paused))"
        );
        // モニターが同じ条件で判定したとき、40% では解除しない。0% なら解除する。
        let later = now + 60;
        assert!(matches!(
            rate_control::check_recovery(
                pause,
                &live_at(vec![five_hour(40.0, None)], later).unwrap(),
                95,
                later
            ),
            Recovery::NotYet(_)
        ));
        assert!(matches!(
            rate_control::check_recovery(
                pause,
                &live_at(vec![five_hour(0.0, None)], later).unwrap(),
                95,
                later
            ),
            Recovery::Recovered(_)
        ));
    }

    #[test]
    fn start_holds_only_for_the_sessions_that_have_not_recovered() {
        // 再開時刻の遅い記録（100% で拒否）は 30% で回復とみなせるが、早い記録（42% で拒否）
        // は回復していない。待つのは回復していない記録の再開時刻までで、回復した記録の時刻
        // まで延ばさない。
        let now = rate_control::now_epoch();
        let low = rejected_session(now, 600, 1800, Some(42.0));
        let high = rejected_session(now, 300, 3600, Some(100.0));
        let gate = decide_start(
            [&low, &high],
            &live_at(vec![five_hour(30.0, None)], now),
            now,
            95,
            now,
        );
        let StartGate::Hold { pause, .. } = &gate else {
            panic!("expected a hold, got {gate:?}");
        };
        assert_eq!(pause.resume_at, now + 1800);
        assert_eq!(pause.used_percent, Some(42.0));
        assert_eq!(pause.observed_at, now - 600);
        // どちらも回復していれば、最も遅い見積もり時刻を待たずに始める。
        let recovered = decide_start(
            [&low, &high],
            &live_at(vec![five_hour(0.0, None)], now),
            now,
            95,
            now,
        );
        assert!(
            matches!(&recovered, StartGate::Recovered { until, .. } if *until == now + 3600),
            "{recovered:?}"
        );
    }

    #[test]
    fn write_start_pause_writes_a_hold_even_after_it_expired() {
        // 判定から書き込みまでの間に再開時刻が過ぎても書く。期限切れの pause file があると
        // gate-claim は「待機を抜けた直後」として再開前の再確認を経てから始める。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let now = rate_control::now_epoch();
        let expired = saved_hold(&[&rejected_session(now, 9000, -5, Some(100.0))], None);
        write_start_pause(&expired, &stop).unwrap();
        let written = rate_control::read_pause(&rate_control::pause_path_for(&stop))
            .unwrap()
            .expect("期限切れでも書くべき");
        assert_eq!(written.resume_at, now - 5);
        assert_eq!(written.source, PauseSource::Saved);
        assert!(!stop.exists(), "一時停止は恒久停止ではない");

        // 保留しないなら何も書かない。
        let other = tempfile::TempDir::new().unwrap();
        let stop = other.path().join("stop");
        write_start_pause(&StartGate::Proceed, &stop).unwrap();
        assert!(!rate_control::pause_path_for(&stop).exists());
    }

    #[test]
    fn plan_line_tells_when_the_monitor_can_resume_early() {
        let now = rate_control::now_epoch();
        let hold = saved_hold(&[&rejected_session(now, 60, 3600, Some(100.0))], None);
        let (label, text) = hold.plan_line(true).unwrap();
        assert_eq!(label, "Hold:");
        assert!(
            text.starts_with("a resumed session hit the five_hour limit — workers wait until "),
            "{text}"
        );
        assert!(
            text.ends_with(
                "; the monitor resumes them earlier if the window recovers (e.g. a manual reset)"
            ),
            "{text}"
        );
        // 実データを取れない構成（ai-usage 連携無し）では、見積もりどおりに待つ。
        let (_, text) = hold.plan_line(false).unwrap();
        assert!(text.ends_with(" before starting"), "{text}");

        // 根拠の使用率が無い・低すぎる記録は、モニターも早期解除を確かめられない。
        // 「早めに再開し得る」と添えず、その理由を出す。
        for basis in [None, Some(8.0)] {
            let hold = saved_hold(&[&rejected_session(now, 60, 3600, basis)], None);
            let (_, text) = hold.plan_line(true).unwrap();
            assert!(!text.contains("resumes them earlier"), "{basis:?}: {text}");
            assert!(
                text.contains("; the monitor cannot resume them earlier ("),
                "{basis:?}: {text}"
            );
        }

        let (label, text) = StartGate::Stop {
            reason: "weekly 96% >= threshold 95% (ai-usage)".to_string(),
        }
        .plan_line(true)
        .unwrap();
        assert_eq!(label, "Stop:");
        assert_eq!(
            text,
            "weekly 96% >= threshold 95% (ai-usage) — no tasks will be started in this run"
        );

        let (label, text) = StartGate::Recovered {
            until: now + 3600,
            evidence: "five_hour dropped from 100% to 0%".to_string(),
            pause: PauseState::new(now + 3600, "five_hour", "saved"),
        }
        .plan_line(true)
        .unwrap();
        assert_eq!(label, "Hold:");
        assert!(text.starts_with("none — "), "{text}");
        assert!(
            text.ends_with(
                "but the window has already recovered: five_hour dropped from 100% to 0%"
            ),
            "{text}"
        );
        assert_eq!(StartGate::Proceed.plan_line(true), None);
    }

    /// 偽の ai-usage（`sh -c`）。受け取った引数を 1 回 1 行で記録し、`json` を出力する。
    /// `refuse_refresh` なら、`--refresh` を知らない古い ai-usage と同じく使い方エラー
    /// （終了コード 2）で終わる。
    fn fake_usage_gate(dir: &Path, json: &str, refuse_refresh: bool) -> (UsageGateConfig, PathBuf) {
        let output = dir.join("ai-usage-output.json");
        std::fs::write(&output, json).unwrap();
        let log = dir.join("ai-usage-args.log");
        let refuse = if refuse_refresh {
            "case \" $* \" in *' --refresh '*) exit 2;; esac; "
        } else {
            ""
        };
        let script = format!(
            "printf '%s\\n' \"$*\" >> {}; {refuse}cat {}",
            shell_escape_path(&log),
            shell_escape_path(&output)
        );
        let gate = UsageGateConfig {
            profile: "Work".to_string(),
            provider: "claude".to_string(),
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                script,
                "ai-usage".to_string(),
                "--json".to_string(),
            ],
            env: std::collections::BTreeMap::new(),
        };
        (gate, log)
    }

    fn logged_calls(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .map(|calls| calls.lines().map(String::from).collect())
            .unwrap_or_default()
    }

    const RECOVERED_USAGE: &str = r#"{"accounts":[{"profile":"Other","provider":"codex","ok":true},{"profile":"Work","provider":"claude","ok":true,"five_hour":{"kind":"five_hour","used_percent":0.0},"weekly":{"kind":"weekly","used_percent":30.0}}]}"#;

    #[test]
    fn start_check_refreshes_every_account_once() {
        // 判定と statusline 用キャッシュの初期化を 1 回の取得で済ませる。キャッシュに使う
        // ので対象のアカウントは絞らず、--refresh だけを足す。
        let tmp = tempfile::TempDir::new().unwrap();
        let (gate, log) = fake_usage_gate(tmp.path(), RECOVERED_USAGE, false);
        let before = rate_control::now_epoch();
        let usage = fetch_start_usage(&gate, Duration::from_secs(5));
        assert_eq!(logged_calls(&log), ["--json --refresh"]);
        let observation = usage.observation.expect("実行中のアカウントを読めるべき");
        assert!(observation.fresh);
        assert!(observation.observed_from <= before - rate_control::LIVE_FRESHNESS_MARGIN_SECS);
        assert_eq!(observation.windows.len(), 2);
        assert!(usage.fetched_at >= before);
        assert_eq!(usage.raw.as_deref(), Some(RECOVERED_USAGE.as_bytes()));
    }

    #[test]
    fn start_check_retries_without_refresh_on_an_old_ai_usage() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (gate, log) = fake_usage_gate(tmp.path(), RECOVERED_USAGE, true);
        let usage = fetch_start_usage(&gate, Duration::from_secs(5));
        assert_eq!(logged_calls(&log), ["--json --refresh", "--json"]);
        let observation = usage
            .observation
            .expect("設定どおりの command で取り直すべき");
        assert!(!observation.fresh, "共有キャッシュを通った可能性がある値");
        assert!(usage.raw.is_some());
    }

    #[test]
    fn start_check_reports_failures_without_caching_them() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut gate, _) = fake_usage_gate(tmp.path(), "", false);
        gate.command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo boom >&2; exit 1".to_string(),
        ];
        let usage = fetch_start_usage(&gate, Duration::from_secs(5));
        let error = usage.observation.expect_err("取得の失敗");
        assert!(
            matches!(&error, LiveError::Unavailable(reason) if reason.contains("boom")),
            "{error}"
        );
        assert!(usage.raw.is_none());

        // 読めない出力で statusline 用キャッシュを初期化しない。
        let (gate, _) = fake_usage_gate(tmp.path(), "not json", false);
        let usage = fetch_start_usage(&gate, Duration::from_secs(5));
        assert!(usage.observation.is_err());
        assert!(usage.raw.is_none());

        // 読めても実行中のアカウントが無ければ判定には使えないが、キャッシュには使える。
        let (mut gate, _) = fake_usage_gate(tmp.path(), RECOVERED_USAGE, false);
        gate.profile = "Home".to_string();
        let usage = fetch_start_usage(&gate, Duration::from_secs(5));
        assert!(usage.observation.is_err());
        assert!(usage.raw.is_some());
    }

    #[test]
    fn check_start_decides_with_the_refreshed_usage() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (gate, _) = fake_usage_gate(tmp.path(), RECOVERED_USAGE, false);
        let mut agent = make_agent(vec!["claude", "-p"]);
        agent.ai_usage = Some(crate::config::RuntimeAiUsage {
            profile: "Work".to_string(),
            provider: "claude".to_string(),
        });
        let now = rate_control::now_epoch();
        let mut plan = build_plan(
            &agent,
            vec![repo_target("/tmp/repo")],
            Some(gate.command.clone()),
            ResumeOptions {
                save_interrupted: true,
                sessions: HashMap::from([(
                    PathBuf::from("/tmp/repo"),
                    rejected_session(now, 360, 5400, Some(100.0)),
                )]),
                prompt: "continue".to_string(),
                discard: HashMap::new(),
            },
        );
        // 実データを見る前は、保存した再開時刻まで待つ見積もり。
        assert!(matches!(plan.start, StartGate::Hold { .. }));
        check_start(&mut plan, 95);
        assert!(
            matches!(plan.start, StartGate::Recovered { .. }),
            "手動リセットで戻った枠を待たない: {:?}",
            plan.start
        );
        assert_eq!(
            plan.startup_usage.as_deref(),
            Some(RECOVERED_USAGE.as_bytes())
        );
    }

    #[test]
    fn check_start_keeps_the_saved_hold_without_ai_usage() {
        // ai-usage 連携が無ければ ai-usage を呼ばず、保存した記録による保留のまま。
        let agent = make_agent(vec!["claude", "-p"]);
        let now = rate_control::now_epoch();
        let mut plan = build_plan(
            &agent,
            vec![repo_target("/tmp/repo")],
            None,
            ResumeOptions {
                save_interrupted: true,
                sessions: HashMap::from([(
                    PathBuf::from("/tmp/repo"),
                    rejected_session(now, 360, 5400, Some(100.0)),
                )]),
                prompt: "continue".to_string(),
                discard: HashMap::new(),
            },
        );
        let before = plan.start.clone();
        check_start(&mut plan, 95);
        assert_eq!(plan.start, before);
        assert!(plan.startup_usage.is_none());
    }

    #[test]
    fn resume_support_rejects_non_claude_and_conflicting_flags() {
        assert_eq!(
            resume_support(&make_agent(vec!["claude", "-p"])),
            ResumeSupport::Supported
        );
        assert_eq!(
            resume_support(&make_agent(vec!["claude", "--continue"])),
            ResumeSupport::ConflictingFlag("--continue".to_string())
        );
        let codex = RuntimeAgent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        assert_eq!(resume_support(&codex), ResumeSupport::NotClaude);
        assert!(ResumeSupport::NotClaude.unsupported_reason().is_some());
        assert_eq!(ResumeSupport::Supported.unsupported_reason(), None);
    }

    #[test]
    fn build_plan_usage_gate_env_excludes_injected_bg_wait() {
        let mut agent = make_agent(vec!["claude", "-p"]);
        agent.ai_usage = Some(crate::config::RuntimeAiUsage {
            profile: "Work".to_string(),
            provider: "claude".to_string(),
        });
        agent.env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/home/u/.claude".to_string(),
        );
        let plan = build_plan(
            &agent,
            vec![],
            Some(vec!["ai-usage".to_string(), "--json".to_string()]),
            ResumeOptions::default(),
        );
        let gate = plan.usage_gate.expect("usage_gate should be set");
        // ユーザー設定の env は usage-gate に引き継ぐが、claude 専用の注入 env は含めない
        assert_eq!(
            gate.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/home/u/.claude")
        );
        assert!(!gate.env.contains_key(CLAUDE_PRINT_BG_WAIT_ENV));
        // タスク実行側の env には注入済み
        assert_eq!(
            plan.agent
                .env
                .get(CLAUDE_PRINT_BG_WAIT_ENV)
                .map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn prepare_run_tmp_dir_creates_missing_directory() {
        // 存在しない場合の削除失敗（NotFound）は正常系として無視し、作成まで到達すること。
        let parent = tempfile::TempDir::new().expect("temp dir should be created");
        let target = parent.path().join("run");
        assert!(!target.exists());

        prepare_run_tmp_dir(&target).expect("missing directory should be created");

        assert!(target.is_dir(), "実行用一時ディレクトリが作られるべき");
    }

    #[test]
    fn prepare_run_tmp_dir_clears_existing_contents() {
        // 旧実装は `let _ = remove_dir_all(...)` で削除失敗を握り潰し、消せなかった
        // ディレクトリをそのまま再利用していた。前回実行の残骸（キュー・タスク
        // スクリプト・マーカー）が残ると、完了済みタスクの誤検出につながる。
        let parent = tempfile::TempDir::new().expect("temp dir should be created");
        let target = parent.path().join("run");
        std::fs::create_dir_all(target.join("markers")).expect("nested dir should be created");
        std::fs::write(target.join("stale.sh"), b"old").expect("stale file should be written");
        std::fs::write(target.join("markers/done-1"), b"").expect("stale marker should be written");

        prepare_run_tmp_dir(&target).expect("existing directory should be recreated");

        assert!(target.is_dir());
        assert_eq!(
            std::fs::read_dir(&target)
                .expect("read_dir should succeed")
                .count(),
            0,
            "前回実行の残骸が残ってはいけない"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepare_run_tmp_dir_restricts_permissions_to_owner_only() {
        // 共有 /tmp を temp_dir に持つ環境で他ユーザーにワーカースクリプトや
        // プロンプトを読み書きされないよう、作成後は必ず 0o700 にする。
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::TempDir::new().expect("temp dir should be created");
        let target = parent.path().join("run");
        // 事前に緩い権限のディレクトリがあっても、作り直して 0o700 になること
        std::fs::create_dir_all(&target).expect("dir should be created");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777))
            .expect("chmod should succeed");

        prepare_run_tmp_dir(&target).expect("directory should be prepared");

        let mode = std::fs::metadata(&target)
            .expect("metadata should be readable")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "所有者のみアクセス可にすべき: {mode:o}"
        );
    }

    #[test]
    fn prepare_run_tmp_dir_errors_when_path_is_not_a_directory() {
        // 削除できないパスをそのまま再利用しないこと（旧実装は握り潰していた）。
        // 通常ファイルを渡すと remove_dir_all が ENOTDIR で失敗するため、root 権限も
        // 特殊なファイルシステムも要らずに「削除失敗」を再現できる。
        let parent = tempfile::TempDir::new().expect("temp dir should be created");
        let target = parent.path().join("run");
        std::fs::write(&target, b"not a directory").expect("file should be written");

        let err = prepare_run_tmp_dir(&target).expect_err("削除失敗はエラーにすべき");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to clear the temporary run directory"),
            "エラーは一時ディレクトリの掃除失敗だと分かる文言にすべき: {msg}"
        );
        // 既存パスを黙って上書き・再利用しない
        assert!(target.is_file(), "既存ファイルは触らないべき");
    }

    /// コマンド文字列のテストで使う ai-usage 連携の設定（env だけを変える）。
    fn gate_with_env(env: &[(&str, &str)]) -> UsageGateConfig {
        UsageGateConfig {
            profile: "Work".to_string(),
            provider: "claude".to_string(),
            command: vec!["ai-usage".to_string(), "--json".to_string()],
            env: env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }

    /// シェルがコマンド文字列をどの語に分けるか（クォートを外した後の語）。コマンド自体は
    /// 起動せず、`printf` に語を並べさせて NUL 区切りで読む。先頭の `KEY=val` は代入ではなく
    /// 1 語として出る（env の適用そのものは確かめられない）。
    fn shell_words(command: &str) -> Vec<String> {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {command}"))
            .output()
            .expect("sh を起動できるべき");
        assert!(output.status.success(), "{command}");
        let stdout = output.stdout.strip_suffix(b"\0").unwrap_or(&output.stdout);
        stdout
            .split(|&byte| byte == 0)
            .map(|word| String::from_utf8_lossy(word).into_owned())
            .collect()
    }

    /// usage-gate のコマンドは、選択 agent の env をサブコマンドより前に置く（`--` 以降に
    /// KEY=val を入れると hidden CLI が実行ファイルと誤認する）。空文字の値は `env -u` で外し、
    /// 空白を含むパスはクォートする。
    #[test]
    fn usage_gate_cmd_puts_the_env_prefix_before_the_subcommand() {
        let gate = gate_with_env(&[
            ("CLAUDE_CONFIG_DIR", "/home/u/.claude home"),
            ("EMPTY_PROBE", ""),
        ]);
        let cmd = build_usage_gate_cmd(
            &gate,
            Path::new("/opt/token burn/token-burn"),
            90,
            Path::new("/tmp/tb run/stop"),
        );
        assert_eq!(
            cmd,
            "env -u EMPTY_PROBE CLAUDE_CONFIG_DIR='/home/u/.claude home' \
             '/opt/token burn/token-burn' usage-gate '--profile=Work' '--provider=claude' \
             --threshold 90 --stop-file '/tmp/tb run/stop' -- 'ai-usage' '--json'"
        );
    }

    /// env が無ければ前置きを付けず、値のある env は `KEY='値'`、空文字の env は `env -u KEY`
    /// として実行ファイルの前に置く。
    #[test]
    fn usage_gate_cmd_prefixes_each_kind_of_env() {
        let cmd_with = |env: &[(&str, &str)]| {
            build_usage_gate_cmd(
                &gate_with_env(env),
                Path::new("/bin/token-burn"),
                95,
                Path::new("/tmp/stop"),
            )
        };
        let rest = "'/bin/token-burn' usage-gate '--profile=Work' '--provider=claude' \
                    --threshold 95 --stop-file '/tmp/stop' -- 'ai-usage' '--json'";
        assert_eq!(cmd_with(&[]), rest);
        assert_eq!(cmd_with(&[("A", "1")]), format!("A='1' {rest}"));
        assert_eq!(cmd_with(&[("A", "")]), format!("env -u A {rest}"));
    }

    /// usage-gate のコマンドはワーカーの bash がそのまま実行する。空白・単引用符・`$`・空文字を
    /// 含む値も、それぞれ 1 語のまま渡る。
    #[test]
    fn usage_gate_cmd_keeps_each_argument_as_one_shell_word() {
        let mut gate = gate_with_env(&[("CLAUDE_CONFIG_DIR", "/home/it's me/$HOME")]);
        gate.profile = "Work 'A'".to_string();
        gate.command = vec![
            "/opt/ai usage/ai-usage".to_string(),
            "--json".to_string(),
            String::new(),
        ];
        let cmd = build_usage_gate_cmd(
            &gate,
            Path::new("/opt/token burn/token-burn"),
            90,
            Path::new("/tmp/it's run/stop"),
        );
        assert_eq!(
            shell_words(&cmd),
            [
                "CLAUDE_CONFIG_DIR=/home/it's me/$HOME",
                "/opt/token burn/token-burn",
                "usage-gate",
                "--profile=Work 'A'",
                "--provider=claude",
                "--threshold",
                "90",
                "--stop-file",
                "/tmp/it's run/stop",
                "--",
                "/opt/ai usage/ai-usage",
                "--json",
                "",
            ]
        );
    }

    /// ai-usage 連携が無ければ、再開前の再確認（`--revalidate`）を付けない。パスはクォートする。
    #[test]
    fn gate_claim_cmd_omits_revalidate_without_usage_gate() {
        let cmd = build_gate_claim_cmd(
            Path::new("/opt/token burn/token-burn"),
            Path::new("/tmp/tb run/stop"),
            Path::new("/tmp/tb run/queue"),
            1_793_577_600,
            None,
        );
        assert_eq!(
            cmd,
            "'/opt/token burn/token-burn' gate-claim --stop-file '/tmp/tb run/stop' \
             --queue-dir '/tmp/tb run/queue' --deadline-epoch 1793577600"
        );
    }

    /// usage-gate のコマンド（空白とクォートを含むシェル文字列）は `shell_escape` で
    /// `--revalidate` の 1 引数にまとめ、gate-claim には元の文字列のまま届く。
    #[test]
    fn gate_claim_cmd_passes_the_usage_gate_as_one_revalidate_argument() {
        let exe = Path::new("/opt/token burn/token-burn");
        let stop = Path::new("/tmp/tb run/stop");
        let gate = gate_with_env(&[
            ("CLAUDE_CONFIG_DIR", "/home/u/.claude home"),
            ("EMPTY_PROBE", ""),
        ]);
        let usage_gate_cmd = build_usage_gate_cmd(&gate, exe, 90, stop);
        let cmd = build_gate_claim_cmd(
            exe,
            stop,
            Path::new("/tmp/tb run/queue"),
            1_793_577_600,
            Some(&usage_gate_cmd),
        );
        assert_eq!(
            shell_words(&cmd),
            [
                "/opt/token burn/token-burn",
                "gate-claim",
                "--stop-file",
                "/tmp/tb run/stop",
                "--queue-dir",
                "/tmp/tb run/queue",
                "--deadline-epoch",
                "1793577600",
                "--revalidate",
                usage_gate_cmd.as_str(),
            ]
        );
    }

    /// デッドラインは相対秒に直さず、受け取った絶対 epoch をそのまま渡す（ワーカーは待機を
    /// 挟む別プロセスなので、相対値だと基準時刻がプロセスごとにずれる）。
    #[test]
    fn gate_claim_cmd_passes_the_deadline_as_an_absolute_epoch() {
        for deadline in [1_793_577_600_i64, 4_102_444_800] {
            let words = shell_words(&build_gate_claim_cmd(
                Path::new("token-burn"),
                Path::new("stop"),
                Path::new("queue"),
                deadline,
                None,
            ));
            let at = words
                .iter()
                .position(|word| word == "--deadline-epoch")
                .expect("--deadline-epoch があるべき");
            assert_eq!(words[at + 1], deadline.to_string());
        }
    }

    /// モニターの ai-usage のコマンドは、どれも選択 agent の env を前置きし、ラッパーの前置き
    /// （`env FOO=1 ai-usage`）を保つ。statusline は、モニターが置き換えるのと同じキャッシュを
    /// `--input` で読む。
    #[test]
    fn monitor_ai_usage_commands_share_the_env_and_the_cache_file() {
        let mut gate = gate_with_env(&[("CLAUDE_CONFIG_DIR", "/home/u/.claude home")]);
        gate.command = ["env", "FOO=1", "ai-usage", "--json"]
            .map(String::from)
            .to_vec();
        let cache_file = Path::new("/tmp/tb run/ai-usage-cache.json");

        let commands =
            build_monitor_ai_usage(&gate, cache_file).expect("command があれば組み立てるべき");

        let prefix = "CLAUDE_CONFIG_DIR='/home/u/.claude home' 'env' 'FOO=1' 'ai-usage'";
        assert_eq!(
            commands,
            crate::monitor::AiUsageCommands {
                refresh: format!("{prefix} '--json'"),
                statusline: format!(
                    "{prefix} '--statusline' '--logos' '--compact' '--input' \
                     '/tmp/tb run/ai-usage-cache.json' '--active-profile=Work' \
                     '--active-provider=claude'"
                ),
                disk: format!("{prefix} '--disk' '--json'"),
                live: format!("{prefix} '--json' '--refresh' '--profile=Work' '--only=claude'"),
                profile: "Work".to_string(),
                provider: "claude".to_string(),
                cache_file: cache_file.to_path_buf(),
            }
        );
        let statusline = shell_words(&commands.statusline);
        let input = statusline
            .iter()
            .position(|word| word == "--input")
            .expect("--input があるべき");
        assert_eq!(Path::new(&statusline[input + 1]), commands.cache_file);
    }

    /// ai-usage のコマンドが空なら、モニターは ai-usage を呼ばない。
    #[test]
    fn monitor_ai_usage_commands_need_an_ai_usage_command() {
        let mut gate = gate_with_env(&[]);
        gate.command.clear();
        assert_eq!(
            build_monitor_ai_usage(&gate, Path::new("/tmp/ai-usage-cache.json")),
            None
        );
    }

    /// 実行開始時の判定が取り直した出力があれば、ai-usage を呼ばずにそのままキャッシュにする。
    #[test]
    fn init_statusline_cache_reuses_the_usage_fetched_at_the_start() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (gate, log) = fake_usage_gate(tmp.path(), RECOVERED_USAGE, false);
        let cache_file = tmp.path().join("ai-usage-cache.json");

        init_statusline_cache(&gate, Some(br#"{"accounts":[]}"#), &cache_file);

        assert_eq!(std::fs::read(&cache_file).unwrap(), br#"{"accounts":[]}"#);
        assert!(logged_calls(&log).is_empty(), "ai-usage を呼んではいけない");
    }

    /// 取り直した出力が無ければ、設定どおりのコマンドで 1 回取得してキャッシュにする。
    #[test]
    fn init_statusline_cache_fetches_when_the_start_check_did_not() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (gate, log) = fake_usage_gate(tmp.path(), RECOVERED_USAGE, false);
        let cache_file = tmp.path().join("ai-usage-cache.json");

        init_statusline_cache(&gate, None, &cache_file);

        assert_eq!(logged_calls(&log), ["--json"]);
        assert_eq!(
            std::fs::read(&cache_file).unwrap(),
            RECOVERED_USAGE.as_bytes()
        );
    }

    /// 取得に失敗してもキャッシュを書かずに続ける（モニターの初回取得に任せる）。
    #[test]
    fn init_statusline_cache_skips_the_cache_when_ai_usage_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut gate, _) = fake_usage_gate(tmp.path(), "", false);
        gate.command = vec!["sh".to_string(), "-c".to_string(), "exit 1".to_string()];
        let cache_file = tmp.path().join("ai-usage-cache.json");

        init_statusline_cache(&gate, None, &cache_file);

        assert!(!cache_file.exists());
    }

    fn run_layout(dir: &Path) -> RunLayout {
        prepare_run_layout(dir.join("tmp"), &dir.join("reports"), "claude")
            .expect("実行用のディレクトリを作れるべき")
    }

    /// レポートディレクトリは開始時刻と（パス成分を無害化した）エージェント名で名付け、
    /// ワーカーが共有するディレクトリは実行用の一時ディレクトリの下に作る。stop file は
    /// 停止するまで作らない。
    #[test]
    fn prepare_run_layout_creates_the_run_directories() {
        let tmp = tempfile::TempDir::new().unwrap();

        let layout = prepare_run_layout(
            tmp.path().join("tmp"),
            &tmp.path().join("reports"),
            "claude/work",
        )
        .unwrap();

        assert_eq!(
            layout.run_dir,
            tmp.path().join("reports").join(format!(
                "{}_claude_work",
                layout.started_at.format("%Y%m%d_%H%M%S")
            ))
        );
        assert_eq!(layout.marker_dir, layout.tmp_dir.join("markers"));
        assert_eq!(layout.queue_dir, layout.tmp_dir.join("queue"));
        assert_eq!(layout.task_dir, layout.tmp_dir.join("tasks"));
        for dir in [
            &layout.run_dir,
            &layout.marker_dir,
            &layout.queue_dir,
            &layout.task_dir,
        ] {
            assert!(dir.is_dir(), "{}", dir.display());
        }
        assert_eq!(layout.stop_file, layout.tmp_dir.join("stop"));
        assert!(!layout.stop_file.exists());
    }

    /// タスクごとにプロンプト・実行スクリプト・空の pending マーカーを、1 始まり・4 桁の番号で
    /// 書く。再開するタスクが無ければ継続プロンプトを、一時停止しないなら pause file を作らない。
    #[test]
    fn prepare_task_queue_writes_the_files_of_each_task() {
        let tmp = tempfile::TempDir::new().unwrap();
        let layout = run_layout(tmp.path());
        let state_file = tmp.path().join("state.json");
        let mut targets = vec![repo_target("/tmp/repo-a"), repo_target("/tmp/repo-b")];
        targets[1].prompt = "second".to_string();
        let plan = build_plan(
            &make_agent(vec!["claude", "-p"]),
            targets,
            None,
            ResumeOptions::default(),
        );

        prepare_task_queue(&plan, &layout, &state_file, 90).unwrap();

        assert_eq!(
            std::fs::read_to_string(layout.tmp_dir.join("prompt-1.txt")).unwrap(),
            "review"
        );
        assert_eq!(
            std::fs::read_to_string(layout.tmp_dir.join("prompt-2.txt")).unwrap(),
            "second"
        );
        for pending in ["pending-0001", "pending-0002"] {
            assert_eq!(std::fs::read(layout.queue_dir.join(pending)).unwrap(), b"");
        }
        let script = std::fs::read_to_string(layout.task_dir.join("task-0002.sh")).unwrap();
        for path in [
            layout.tmp_dir.join("prompt-2.txt"),
            layout.marker_dir.join("failed-2"),
            layout.stop_file.clone(),
            state_file,
        ] {
            assert!(
                script.contains(&shell_escape_path(&path)),
                "{}: {script}",
                path.display()
            );
        }
        assert!(!layout.tmp_dir.join("resume-prompt.txt").exists());
        assert!(!rate_control::pause_path_for(&layout.stop_file).exists());
    }

    /// 再開するタスクがあれば継続プロンプトを 1 ファイルにまとめて書き、そのタスクのスクリプト
    /// だけが読む。実行開始時に一時停止するなら pause file も書く。
    #[test]
    fn prepare_task_queue_writes_the_resume_prompt_and_the_start_pause() {
        let tmp = tempfile::TempDir::new().unwrap();
        let layout = run_layout(tmp.path());
        let now = rate_control::now_epoch();
        let plan = build_plan(
            &make_agent(vec!["claude", "-p"]),
            vec![repo_target("/tmp/repo-a"), repo_target("/tmp/repo-b")],
            None,
            ResumeOptions {
                save_interrupted: true,
                sessions: HashMap::from([(
                    PathBuf::from("/tmp/repo-b"),
                    rejected_session(now, 360, 5400, Some(100.0)),
                )]),
                prompt: "continue".to_string(),
                discard: HashMap::new(),
            },
        );
        assert!(
            matches!(plan.start, StartGate::Hold { .. }),
            "保存した再開時刻まで待つ計画のはず: {:?}",
            plan.start
        );

        prepare_task_queue(&plan, &layout, &tmp.path().join("state.json"), 90).unwrap();

        let resume_prompt = layout.tmp_dir.join("resume-prompt.txt");
        assert_eq!(std::fs::read_to_string(&resume_prompt).unwrap(), "continue");
        let quoted = shell_escape_path(&resume_prompt);
        let resumed = std::fs::read_to_string(layout.task_dir.join("task-0002.sh")).unwrap();
        assert!(resumed.contains(&quoted), "{resumed}");
        let fresh = std::fs::read_to_string(layout.task_dir.join("task-0001.sh")).unwrap();
        assert!(!fresh.contains(&quoted), "{fresh}");
        assert!(rate_control::pause_path_for(&layout.stop_file).exists());
    }

    /// ワーカーのスクリプトは 0 始まりの番号で 1 人 1 つ書いて実行権限を付け、ペインで起動する
    /// 順に返す。どのワーカーも同じ gate-claim と usage-gate を使う。
    #[cfg(unix)]
    #[test]
    fn write_worker_scripts_writes_one_executable_script_per_worker() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let layout = run_layout(tmp.path());

        let paths = write_worker_scripts(&layout, 2, Some("'usage-gate'"), "'gate-claim'").unwrap();

        assert_eq!(
            paths,
            [
                layout.tmp_dir.join("worker-0.sh"),
                layout.tmp_dir.join("worker-1.sh")
            ]
        );
        for path in &paths {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "{}", path.display());
            let script = std::fs::read_to_string(path).unwrap();
            assert!(script.contains("CLAIMED=$('gate-claim')"), "{script}");
            assert!(script.contains("if ! 'usage-gate'; then"), "{script}");
        }
    }
}
