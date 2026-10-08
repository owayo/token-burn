//! `codex exec --json` の 1 行を表示行へ変換する整形器と、無出力が続いたときの
//! 生存表示（ハートビート）。
//!
//! 時刻は呼び出し側から受け取る（`now: Instant`）。スレッドや実時間に依存させないことで、
//! 経過時間やハートビートの判定を合成した時刻でテストできる。

use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::text::{collapse, dim, format_elapsed, one_line, red, sanitize, yellow};
use crate::format_stream::util::{format_number, truncate_str};

/// 無出力がこれだけ続いたら生存表示（ハートビート）を出す。
///
/// codex は文脈の圧縮や長い推論の間は何も出力せず、実ログでは 3〜6.5 分の沈黙があった。
/// ペインが動かないままだと、ハングしたのか考えている最中なのかを見分けられない。
/// 2 分は通常のイベント間隔（数秒〜数十秒）より十分長く、普段はノイズにならない。
const HEARTBEAT_AFTER: Duration = Duration::from_secs(120);

/// 沈黙が続く間にハートビートを繰り返す間隔。ログを埋めない程度に間引く。
const HEARTBEAT_EVERY: Duration = Duration::from_secs(120);

/// エラー文を 1 行に収めるときの上限（文字数）。原因を読める長さを残す。
pub(super) const MESSAGE_WIDTH: usize = 400;

/// ハートビートに添える実行中コマンドの上限（文字数）。
const HEARTBEAT_COMMAND_WIDTH: usize = 60;

/// JSON のイベントとして読めない行を表示するときの上限（文字数）。
///
/// codex の標準出力と標準エラーは同じパイプへ合流する（`2>&1`）ため、巨大な
/// `aggregated_output` を含む JSON 行を書いている途中に標準エラーの書き込みが割り込むと、
/// 壊れた JSON 行がこの経路へ落ちる。素通しすると数百 KB がペインへ流れ、累積 diff と
/// 同じ問題が戻ってくる。通常の警告やエラー（長くても 1〜2KB）は切らない長さにする。
/// 全体は生ログ（`--raw-output`）に残る。
const PASSTHROUGH_MAX_CHARS: usize = 2_000;

/// item のどの段階のイベントか（`item.started` / `item.updated` / `item.completed`）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Started,
    Updated,
    Completed,
}

/// 実行中の `command_execution`。完了行の経過時間とハートビートに使う。
pub(super) struct RunningCommand {
    /// 表示用に整えたコマンド（シェルの包みとクォートを外し、1 行に畳んだもの）。
    pub(super) display: String,
    pub(super) started: Instant,
}

/// `todo_list` について最後に表示した進捗。変わったときだけ表示し直す。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PlanProgress {
    pub(super) done: usize,
    pub(super) total: usize,
    /// 最初の未完了項目（全部終わっていれば `None`）。
    pub(super) next: Option<String>,
}

pub(super) struct CodexFormatter {
    /// 変更したファイルのパスを相対表示する基準。
    pub(super) cwd: Option<PathBuf>,
    /// 表示行に添える壁時計（`HH:MM:SS`）。テストでは固定値を差し込む。
    clock: fn() -> String,
    /// 最後に何かの行（JSON かどうかを問わない）を受け取った時刻。
    last_event_at: Instant,
    /// 次にハートビートを出してよい最も早い時刻。
    next_heartbeat_at: Instant,
    /// 実行中の `command_execution`（item id → コマンド）。
    pub(super) commands: HashMap<String, RunningCommand>,
    /// 経過時間を出すその他の item（`mcp_tool_call` / `collab_tool_call`）の開始時刻。
    pub(super) started_at: HashMap<String, Instant>,
    /// `todo_list` ごとに最後に表示した進捗（item id → 進捗）。
    pub(super) plans: HashMap<String, PlanProgress>,
}

impl CodexFormatter {
    pub(super) fn new(start: Instant, cwd: Option<PathBuf>) -> Self {
        Self::with_clock(start, cwd, local_clock)
    }

    pub(super) fn with_clock(start: Instant, cwd: Option<PathBuf>, clock: fn() -> String) -> Self {
        Self {
            cwd,
            clock,
            last_event_at: start,
            next_heartbeat_at: later(start, HEARTBEAT_AFTER),
            commands: HashMap::new(),
            started_at: HashMap::new(),
            plans: HashMap::new(),
        }
    }

    /// 入力の 1 行（改行込みの生バイト列）を受け取り、表示行を書く。
    pub(super) fn on_line(
        &mut self,
        line: &[u8],
        now: Instant,
        out: &mut impl Write,
    ) -> io::Result<()> {
        // 行の中身を問わず、何かが届いた時点で沈黙は途切れる。次のハートビートは
        // この行から数えて HEARTBEAT_AFTER 後まで出さない。
        self.last_event_at = now;
        self.next_heartbeat_at = later(now, HEARTBEAT_AFTER);
        let lines = self.render_line(line, now);
        write_lines(out, &lines)
    }

    /// 行が届かないまま待ちが明けたときに呼ぶ。沈黙が長ければハートビートを書く。
    pub(super) fn on_idle(&mut self, now: Instant, out: &mut impl Write) -> io::Result<()> {
        let silence = now.saturating_duration_since(self.last_event_at);
        if silence < HEARTBEAT_AFTER || now < self.next_heartbeat_at {
            return Ok(());
        }
        self.next_heartbeat_at = later(now, HEARTBEAT_EVERY);
        let line = self.heartbeat_line(silence, now);
        write_lines(out, &[line])
    }

    /// 沈黙の長さと、実行中のコマンドがあれば最も長く走っているものを添えた 1 行。
    ///
    /// 沈黙の原因（圧縮・推論・コマンド待ち）は推測で書かない。事実（何分黙っているか、
    /// 何が走っているか）だけを出す。
    fn heartbeat_line(&self, silence: Duration, now: Instant) -> String {
        let mut line = format!(
            "  \u{23f3} No codex events for {} [{}]",
            format_elapsed(silence),
            self.wall_clock()
        );
        let longest = self.commands.values().min_by(|a, b| {
            a.started
                .cmp(&b.started)
                .then_with(|| a.display.cmp(&b.display))
        });
        if let Some(command) = longest {
            line.push_str(&format!(
                " \u{2014} running: $ {} ({})",
                truncate_str(&command.display, HEARTBEAT_COMMAND_WIDTH),
                format_elapsed(now.saturating_duration_since(command.started))
            ));
            let others = self.commands.len() - 1;
            if others > 0 {
                line.push_str(&format!(" and {others} more"));
            }
        }
        dim(&line)
    }

    fn wall_clock(&self) -> String {
        (self.clock)()
    }

    fn render_line(&mut self, line: &[u8], now: Instant) -> Vec<String> {
        let decoded = String::from_utf8_lossy(line);
        let text = decoded.strip_suffix('\n').unwrap_or(&decoded);
        let text = text.strip_suffix('\r').unwrap_or(text);
        if text.trim().is_empty() {
            return Vec::new();
        }
        match serde_json::from_str::<Value>(text) {
            Ok(event) => match event["type"].as_str() {
                Some(kind) => self.render_event(kind, &event, now),
                None => passthrough(text),
            },
            // JSON 以外（codex が標準エラーへ出す警告やエラー、`2>&1` で合流したもの）
            Err(_) => passthrough(text),
        }
    }

    fn render_event(&mut self, kind: &str, event: &Value, now: Instant) -> Vec<String> {
        match kind {
            "thread.started" => {
                let id = one_line(event["thread_id"].as_str().unwrap_or(""), 80);
                let line = if id.is_empty() {
                    "  \u{2139} Codex session".to_string()
                } else {
                    format!("  \u{2139} Codex session {id}")
                };
                vec![dim(&line)]
            }
            "turn.started" => Vec::new(),
            "turn.completed" => {
                self.end_turn();
                vec![self.usage_line(&event["usage"])]
            }
            "turn.failed" => {
                self.end_turn();
                let message = truncate_str(&error_message(event), MESSAGE_WIDTH);
                let clock = self.wall_clock();
                let line = if message.is_empty() {
                    format!("  \u{274c} Turn failed [{clock}]")
                } else {
                    format!("  \u{274c} Turn failed: {message} [{clock}]")
                };
                vec![red(&line)]
            }
            "error" => {
                let message = truncate_str(&error_message(event), MESSAGE_WIDTH);
                let line = if message.is_empty() {
                    "  \u{274c} Error".to_string()
                } else {
                    format!("  \u{274c} Error: {message}")
                };
                vec![red(&line)]
            }
            "item.started" => self.render_item(Phase::Started, &event["item"], now),
            "item.updated" => self.render_item(Phase::Updated, &event["item"], now),
            "item.completed" => self.render_item(Phase::Completed, &event["item"], now),
            // 版によって増えるイベントは種別だけ出す（黙って捨てると、増えたことに気付けない）。
            other => vec![dim(&format!("  \u{b7} {}", one_line(other, 60)))],
        }
    }

    /// ターンが終わったら実行中の扱いを捨てる。完了イベントを伴わずに中断された item が
    /// 残ると、以後のハートビートが走っていないコマンドを「実行中」と表示してしまう。
    fn end_turn(&mut self) {
        self.commands.clear();
        self.started_at.clear();
    }

    /// `turn.completed` のトークン使用量。
    ///
    /// `cached_input_tokens` は `input_tokens` の内訳として括弧で添え、入力へは足さない
    /// （codex が報告した値をそのまま見せる）。0 や欠損の内訳は省く。
    fn usage_line(&self, usage: &Value) -> String {
        let clock = self.wall_clock();
        if !usage.is_object() {
            return yellow(&format!("\u{1f4ca} turn completed [{clock}]"));
        }
        let count = |key: &str| usage[key].as_u64().unwrap_or(0);
        let mut line = format!("\u{1f4ca} in:{}", format_number(count("input_tokens")));
        let cached = count("cached_input_tokens");
        if cached > 0 {
            line.push_str(&format!(" (cached:{})", format_number(cached)));
        }
        line.push_str(&format!(" out:{}", format_number(count("output_tokens"))));
        let reasoning = count("reasoning_output_tokens");
        if reasoning > 0 {
            line.push_str(&format!(" (reasoning:{})", format_number(reasoning)));
        }
        line.push_str(&format!(" [{clock}]"));
        yellow(&line)
    }
}

/// 表示行をまとめて 1 回で書き、すぐ flush する（ペインへ遅れなく出すため）。
fn write_lines(out: &mut impl Write, lines: &[String]) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut buf = String::new();
    for line in lines {
        buf.push_str(line);
        buf.push('\n');
    }
    out.write_all(buf.as_bytes())?;
    out.flush()
}

/// JSON のイベントとして解釈できない行は、制御文字だけ除いてそのまま出す
/// （極端に長い行だけは切り詰める。[`PASSTHROUGH_MAX_CHARS`] を参照）。
fn passthrough(text: &str) -> Vec<String> {
    sanitize(text)
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(|line| cap_passthrough(line.trim_end()))
        .collect()
}

fn cap_passthrough(line: &str) -> String {
    let total = line.chars().count();
    if total <= PASSTHROUGH_MAX_CHARS {
        return line.to_string();
    }
    let kept: String = line.chars().take(PASSTHROUGH_MAX_CHARS).collect();
    let omitted = u64::try_from(total - PASSTHROUGH_MAX_CHARS).unwrap_or(u64::MAX);
    format!("{kept}\u{2026} (+{} chars)", format_number(omitted))
}

/// エラー文を 1 行に畳んで取り出す（切り詰めは呼び出し側）。`error.message`（オブジェクト）・
/// `error`（文字列）・`message` の順に見て、空でない最初のものを返す。版によって形が
/// 揺れても原因を落とさないため。
pub(super) fn error_message(value: &Value) -> String {
    [
        &value["error"]["message"],
        &value["error"],
        &value["message"],
    ]
    .into_iter()
    .filter_map(Value::as_str)
    .map(collapse)
    .find(|message| !message.is_empty())
    .unwrap_or_default()
}

fn later(at: Instant, by: Duration) -> Instant {
    at.checked_add(by).unwrap_or(at)
}

fn local_clock() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}
