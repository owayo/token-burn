//! `item.started` / `item.updated` / `item.completed` の表示。item の種類ごとに 1 行
//! （多くても数行）へまとめる。
//!
//! 版によって種類やフィールドが増減するため、欠けたフィールドや知らない種類でも
//! panic せず、表示できる範囲で出す。

use serde_json::Value;
use std::path::Path;
use std::time::Instant;

use super::formatter::{
    CodexFormatter, MESSAGE_WIDTH, Phase, PlanProgress, RunningCommand, error_message,
};
use super::text::{
    collapse, cyan, dim, display_command, format_elapsed, one_line, output_tail_lines, red,
    sanitize, yellow,
};
use crate::format_stream::util::truncate_str;

/// 開始行に出すコマンドの上限（文字数）。何を始めたかが分かる長さを残す。
const STARTED_COMMAND_WIDTH: usize = 160;
/// 完了行に出すコマンドの上限（文字数）。直前の開始行に全体が出ているので短くする。
const COMPLETED_COMMAND_WIDTH: usize = 80;
/// 成功したコマンドの完了行に添える出力の最終行の上限（文字数）。
const OUTPUT_SUMMARY_WIDTH: usize = 100;
/// 失敗したコマンドの出力を何行まで見せるか（末尾から）。
const FAILED_OUTPUT_LINES: usize = 3;
/// 失敗したコマンドの出力 1 行の上限（文字数）。
const FAILED_OUTPUT_WIDTH: usize = 160;
/// 推論の要約の上限（文字数）。
const REASONING_WIDTH: usize = 200;
/// MCP ツールの失敗理由の上限（文字数）。
const MCP_ERROR_WIDTH: usize = 200;
/// サブエージェントへ渡したプロンプトの上限（文字数）。
const PROMPT_WIDTH: usize = 100;
/// 計画の次の項目の上限（文字数）。
const PLAN_ITEM_WIDTH: usize = 80;
/// 失敗したパッチの対象パス一覧の上限（文字数）。
const PATCH_PATHS_WIDTH: usize = 200;
/// Web 検索のクエリの上限（文字数）。
const QUERY_WIDTH: usize = 160;
/// サーバー名・ツール名など識別子の上限（文字数）。
const NAME_WIDTH: usize = 80;

impl CodexFormatter {
    pub(super) fn render_item(&mut self, phase: Phase, item: &Value, now: Instant) -> Vec<String> {
        let id = item["id"].as_str().unwrap_or("");
        match item["type"].as_str().unwrap_or("") {
            "command_execution" => self.command_execution(phase, id, item, now),
            "file_change" => self.file_change(phase, item),
            "agent_message" => agent_message(phase, item),
            "reasoning" => reasoning(phase, item),
            "mcp_tool_call" => self.mcp_tool_call(phase, id, item, now),
            "web_search" => web_search(phase, item),
            "todo_list" => self.todo_list(id, item),
            "collab_tool_call" => self.collab_tool_call(phase, id, item, now),
            "error" => error_item(phase, item),
            other => unknown_item(phase, other),
        }
    }

    /// `command_execution`: 開始で 1 行、完了で 1 行（失敗なら出力の末尾を数行添える）。
    ///
    /// `item.updated` は途中までの `aggregated_output` を丸ごと載せて何度も届くため、
    /// 表示しない（人間向け出力で累積 diff を出し直していたのと同じ問題になる）。
    fn command_execution(
        &mut self,
        phase: Phase,
        id: &str,
        item: &Value,
        now: Instant,
    ) -> Vec<String> {
        let command = item["command"].as_str().unwrap_or("");
        match phase {
            Phase::Started => {
                let display = display_command(command);
                let line = cyan(&format!(
                    "\u{1f527} $ {}",
                    truncate_str(&display, STARTED_COMMAND_WIDTH)
                ));
                if !id.is_empty() {
                    // 同じ id の開始が重ねて届いても、最初の開始時刻を保つ。
                    self.commands
                        .entry(id.to_string())
                        .or_insert(RunningCommand {
                            display,
                            started: now,
                        });
                }
                vec![line]
            }
            Phase::Updated => Vec::new(),
            Phase::Completed => {
                let running = if id.is_empty() {
                    None
                } else {
                    self.commands.remove(id)
                };
                let (display, elapsed) = match running {
                    Some(running) => (
                        running.display,
                        format!(
                            " ({})",
                            format_elapsed(now.saturating_duration_since(running.started))
                        ),
                    ),
                    // 開始を見ていなければ経過時間は出さない（推測で埋めない）。
                    None => (display_command(command), String::new()),
                };
                let shown = truncate_str(&display, COMPLETED_COMMAND_WIDTH);
                let status = item["status"].as_str().unwrap_or("");
                let exit_code = item["exit_code"].as_i64();
                let output = item["aggregated_output"].as_str().unwrap_or("");

                if status == "declined" {
                    return vec![yellow(&format!("  \u{2298} declined $ {shown}"))];
                }
                if exit_code == Some(0) && status != "failed" {
                    let summary = output_tail_lines(output, 1)
                        .first()
                        .map(|last| {
                            format!(" \u{2192} {}", truncate_str(last, OUTPUT_SUMMARY_WIDTH))
                        })
                        .unwrap_or_default();
                    return vec![dim(&format!("  \u{2713} $ {shown}{elapsed}{summary}"))];
                }
                let code = exit_code.map_or_else(|| "?".to_string(), |code| code.to_string());
                let mut lines = vec![red(&format!("  \u{2717} exit {code} $ {shown}{elapsed}"))];
                lines.extend(
                    output_tail_lines(output, FAILED_OUTPUT_LINES)
                        .iter()
                        .map(|line| {
                            dim(&format!(
                                "    \u{2502} {}",
                                truncate_str(line, FAILED_OUTPUT_WIDTH)
                            ))
                        }),
                );
                lines
            }
        }
    }

    /// `file_change`: 完了時に変更したファイルを 1 件 1 行で出す。diff は出さない。
    fn file_change(&self, phase: Phase, item: &Value) -> Vec<String> {
        if phase != Phase::Completed {
            return Vec::new();
        }
        let changes: Vec<(String, String)> = item["changes"]
            .as_array()
            .map(|changes| {
                changes
                    .iter()
                    .map(|change| {
                        (
                            change_kind(change),
                            self.display_path(change["path"].as_str().unwrap_or("")),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        match item["status"].as_str().unwrap_or("") {
            "failed" => vec![red(&with_paths("  \u{2717} patch failed", &changes))],
            "declined" => vec![yellow(&with_paths("  \u{2298} patch declined", &changes))],
            _ => changes
                .into_iter()
                .map(|(kind, path)| cyan(&format!("\u{1f4dd} {kind} {path}")))
                .collect(),
        }
    }

    /// ファイルのパスを、カレントディレクトリ（＝対象リポジトリ）の中なら相対で、
    /// 外なら絶対パスのまま表示する。
    fn display_path(&self, path: &str) -> String {
        if path.is_empty() {
            return "?".to_string();
        }
        let relative = self
            .cwd
            .as_deref()
            .and_then(|cwd| Path::new(path).strip_prefix(cwd).ok())
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.to_string_lossy().into_owned());
        collapse(relative.as_deref().unwrap_or(path))
    }

    /// `mcp_tool_call`: 開始で `🔌 server.tool`、完了で経過時間か失敗理由。
    fn mcp_tool_call(&mut self, phase: Phase, id: &str, item: &Value, now: Instant) -> Vec<String> {
        let name = one_line(
            &format!(
                "{}.{}",
                non_empty(item["server"].as_str(), "mcp"),
                non_empty(item["tool"].as_str(), "?")
            ),
            NAME_WIDTH,
        );
        match phase {
            Phase::Started => {
                self.mark_started(id, now);
                vec![cyan(&format!("\u{1f50c} {name}"))]
            }
            Phase::Updated => Vec::new(),
            Phase::Completed => {
                let elapsed = self.take_elapsed(id, now);
                let status = item["status"].as_str().unwrap_or("");
                let error = truncate_str(&error_message(item), MCP_ERROR_WIDTH);
                if !error.is_empty() {
                    vec![red(&format!("  \u{2717} {name}: {error}"))]
                } else if status == "failed" || status == "declined" {
                    vec![red(&format!("  \u{2717} {name} ({status})"))]
                } else {
                    vec![dim(&format!("  \u{2713} {name}{elapsed}"))]
                }
            }
        }
    }

    /// `todo_list`: 計画の進捗。開始・更新・完了のどれでも、進捗か次の項目が変わったときだけ出す。
    fn todo_list(&mut self, id: &str, item: &Value) -> Vec<String> {
        let entries = item["items"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let total = entries.len();
        if total == 0 {
            return Vec::new();
        }
        let is_done = |entry: &Value| entry["completed"].as_bool() == Some(true);
        let done = entries.iter().filter(|entry| is_done(entry)).count();
        let next = entries
            .iter()
            .find(|entry| !is_done(entry))
            .map(|entry| one_line(entry["text"].as_str().unwrap_or(""), PLAN_ITEM_WIDTH));
        let progress = PlanProgress { done, total, next };
        if self.plans.get(id) == Some(&progress) {
            return Vec::new();
        }
        let line = match &progress.next {
            Some(next) if !next.is_empty() => {
                format!("\u{1f4cb} Plan {done}/{total} \u{2014} next: {next}")
            }
            Some(_) => format!("\u{1f4cb} Plan {done}/{total}"),
            None => format!("\u{1f4cb} Plan {done}/{total} \u{2014} all done"),
        };
        self.plans.insert(id.to_string(), progress);
        vec![cyan(&line)]
    }

    /// `collab_tool_call`（サブエージェントの起動・入力・待機・終了）。
    fn collab_tool_call(
        &mut self,
        phase: Phase,
        id: &str,
        item: &Value,
        now: Instant,
    ) -> Vec<String> {
        let tool = one_line(non_empty(item["tool"].as_str(), "collab"), NAME_WIDTH);
        match phase {
            Phase::Started => {
                self.mark_started(id, now);
                let prompt = one_line(item["prompt"].as_str().unwrap_or(""), PROMPT_WIDTH);
                let line = if prompt.is_empty() {
                    format!("\u{1f91d} {tool}")
                } else {
                    format!("\u{1f91d} {tool} {prompt}")
                };
                vec![cyan(&line)]
            }
            Phase::Updated => Vec::new(),
            Phase::Completed => {
                let elapsed = self.take_elapsed(id, now);
                match item["status"].as_str().unwrap_or("") {
                    "" | "completed" => vec![dim(&format!("  \u{2713} {tool}{elapsed}"))],
                    status => vec![red(&format!(
                        "  \u{2717} {tool} ({})",
                        one_line(status, NAME_WIDTH)
                    ))],
                }
            }
        }
    }

    fn mark_started(&mut self, id: &str, now: Instant) {
        if !id.is_empty() {
            self.started_at.entry(id.to_string()).or_insert(now);
        }
    }

    /// 開始時刻を取り出して ` (<経過時間>)` にする。開始を見ていなければ空文字。
    fn take_elapsed(&mut self, id: &str, now: Instant) -> String {
        if id.is_empty() {
            return String::new();
        }
        self.started_at
            .remove(id)
            .map(|started| {
                format!(
                    " ({})",
                    format_elapsed(now.saturating_duration_since(started))
                )
            })
            .unwrap_or_default()
    }
}

/// `agent_message`: エージェントの説明文。切り詰めず、2 行目以降は本文の位置へ字下げする。
fn agent_message(phase: Phase, item: &Value) -> Vec<String> {
    if phase != Phase::Completed {
        return Vec::new();
    }
    let text = sanitize(item["text"].as_str().unwrap_or(""));
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .enumerate()
        .map(|(index, line)| {
            let line = line.trim_end();
            if index == 0 {
                format!("\u{1f4ac} {line}")
            } else if line.is_empty() {
                String::new()
            } else {
                // `💬 ` の幅（絵文字 2 桁 + 空白）に揃える。
                format!("   {line}")
            }
        })
        .collect()
}

/// `reasoning`: 推論の要約を 1 行に畳んで出す。
fn reasoning(phase: Phase, item: &Value) -> Vec<String> {
    if phase != Phase::Completed {
        return Vec::new();
    }
    let text = one_line(item["text"].as_str().unwrap_or(""), REASONING_WIDTH);
    if text.is_empty() {
        return Vec::new();
    }
    vec![dim(&format!("\u{1f4ad} {text}"))]
}

/// `web_search`: 完了時に検索語を出す。
fn web_search(phase: Phase, item: &Value) -> Vec<String> {
    if phase != Phase::Completed {
        return Vec::new();
    }
    let query = one_line(item["query"].as_str().unwrap_or(""), QUERY_WIDTH);
    let line = if query.is_empty() {
        "\u{1f50e} web search".to_string()
    } else {
        format!("\u{1f50e} {query}")
    };
    vec![cyan(&line)]
}

/// `error` item（実行は続く種類のエラーや警告）。
fn error_item(phase: Phase, item: &Value) -> Vec<String> {
    if phase != Phase::Completed {
        return Vec::new();
    }
    let message = truncate_str(&error_message(item), MESSAGE_WIDTH);
    let line = if message.is_empty() {
        "  \u{26a0} error".to_string()
    } else {
        format!("  \u{26a0} {message}")
    };
    vec![yellow(&line)]
}

/// 知らない種類の item は、完了時に種類だけ出す（増えたことに気付けるように）。
fn unknown_item(phase: Phase, item_type: &str) -> Vec<String> {
    if phase != Phase::Completed {
        return Vec::new();
    }
    let label = one_line(non_empty(Some(item_type), "item"), NAME_WIDTH);
    vec![dim(&format!("  \u{b7} {label}"))]
}

/// 変更の種類（`add` / `delete` / `update`）。オブジェクトで届く版にも備える。
fn change_kind(change: &Value) -> String {
    let kind = change["kind"]
        .as_str()
        .or_else(|| change["kind"]["type"].as_str());
    one_line(non_empty(kind, "change"), 20)
}

/// `✗ patch failed: a.rs, b.rs` のようにパスを並べる。パスが無ければ見出しだけ。
fn with_paths(label: &str, changes: &[(String, String)]) -> String {
    if changes.is_empty() {
        return label.to_string();
    }
    let paths = changes
        .iter()
        .map(|(_, path)| path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{label}: {}", truncate_str(&paths, PATCH_PATHS_WIDTH))
}

/// 空・空白だけ・欠損なら既定値にする。
fn non_empty<'a>(value: Option<&'a str>, fallback: &'a str) -> &'a str {
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback)
}
