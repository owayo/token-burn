use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::Path;

mod assistant;
mod blocks;
mod diff;
mod rate_limit;
mod result;
mod state;
mod stream;
pub(crate) mod system;
mod tool_result;
mod tools;
mod util;

use assistant::handle_assistant_event;
use blocks::{ContentBlockState, finalize_open_blocks};
use rate_limit::handle_rate_limit_event;
use result::handle_result;
use state::{StreamState, StreamSummary};
use stream::handle_stream_event;
use system::handle_system_event;
use tool_result::handle_tool_result_event;
use tools::progress::handle_tool_progress;

/// `claude -p` の stream-json 出力を読みやすいテキストに変換する。
/// JSON以外の行はそのまま出力（任意のエージェントで動作）。
pub fn run(raw_output: Option<&Path>, stop_file: Option<&Path>, threshold: u8) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let out = stdout.lock();
    process(stdin.lock(), out, raw_output, stop_file, threshold)
}

fn process(
    reader: impl BufRead,
    mut out: impl Write,
    raw_output: Option<&Path>,
    stop_file: Option<&Path>,
    threshold: u8,
) -> Result<()> {
    let mut tool_id_map: HashMap<String, String> = HashMap::new();
    let mut shown_notices = std::collections::HashSet::new();
    let mut blocks: HashMap<usize, ContentBlockState> = HashMap::new();
    let mut summary = StreamSummary::default();
    let mut raw_writer = match raw_output {
        Some(path) => Some(io::BufWriter::new(File::create(path)?)),
        None => None,
    };

    for line in reader.lines() {
        let line = line?;
        if let Some(writer) = raw_writer.as_mut() {
            writeln!(writer, "{}", line)?;
        }
        if line.is_empty() {
            continue;
        }

        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                // JSON以外 — そのまま出力（例: codex のプレーンテキスト出力）
                writeln!(out, "{}", line)?;
                out.flush()?;
                continue;
            }
        };

        let msg_type = v["type"].as_str().unwrap_or("");

        match msg_type {
            "system" => {
                summary.update_from_system(&v);
                handle_system_event(&v, &mut out)?;
            }
            "stream_event" => {
                handle_stream_event(
                    &v["event"],
                    &mut out,
                    &mut StreamState {
                        blocks: &mut blocks,
                        tool_id_map: &mut tool_id_map,
                        summary: &mut summary,
                    },
                )?;
            }
            "assistant" => {
                handle_assistant_event(
                    &v,
                    &mut out,
                    &mut tool_id_map,
                    &mut shown_notices,
                    &mut blocks,
                )?;
            }
            "user" => {
                // ツール結果 — 完了したツール名を表示
                handle_tool_result_event(&v, &mut out, &tool_id_map)?;
            }
            "result" => {
                summary.update_from_result(v.as_object());
                finalize_open_blocks(&mut out, &mut blocks)?;
                handle_result(&v, &summary, &mut out)?;
            }
            "rate_limit_event" => {
                handle_rate_limit_event(&v, &mut out, stop_file, threshold)?;
            }
            "tool_progress" => {
                handle_tool_progress(&v, &mut out)?;
            }
            _ => {} // message_stop 等
        }
    }

    finalize_open_blocks(&mut out, &mut blocks)?;
    out.flush()?;
    if let Some(writer) = raw_writer.as_mut() {
        writer.flush()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests;
