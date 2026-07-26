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
mod tools;
mod util;

use assistant::handle_assistant_event;
use blocks::{ContentBlockState, finalize_open_blocks};
use rate_limit::handle_rate_limit_event;
use result::handle_result;
use state::{StreamState, StreamSummary};
use stream::handle_stream_event;
use system::handle_system_event;
use tools::metadata::{
    tool_result_meta_metadata, tool_result_metadata, tool_result_string_summary,
};
use tools::progress::handle_tool_progress;
use util::extract_tool_result_summary;

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
                handle_assistant_event(&v, &mut out, &mut tool_id_map, &mut shown_notices)?;
            }
            "user" => {
                // ツール結果 — 完了したツール名を表示
                if let Some(content) = v["message"]["content"].as_array() {
                    for item in content {
                        if item["type"].as_str() == Some("tool_result") {
                            let id = item["tool_use_id"].as_str().unwrap_or("");
                            let name = tool_id_map.get(id).map(|s| s.as_str()).unwrap_or("?");
                            let is_error = item["is_error"].as_bool().unwrap_or(false);
                            let mut metadata = tool_result_metadata(&v["tool_use_result"]);
                            let result_meta = tool_result_meta_metadata(&v["tool_result_meta"], id);
                            if !result_meta.is_empty() {
                                if !metadata.is_empty() {
                                    metadata.push_str(", ");
                                }
                                metadata.push_str(&result_meta);
                            }
                            // tool_use_result が文字列の応答（MCP ツール等）には object
                            // メタデータが無い。エラー時は content 側のサマリーと同文に
                            // なるため、成功時のみ result: として先頭行を補足する。
                            if metadata.is_empty()
                                && !is_error
                                && let Some(text_summary) =
                                    tool_result_string_summary(&v["tool_use_result"])
                            {
                                metadata = text_summary;
                            }
                            let metadata = if metadata.is_empty() {
                                String::new()
                            } else {
                                format!(" [{}]", metadata)
                            };
                            if is_error {
                                // エラー内容のサマリーがある場合は併記する
                                let summary = extract_tool_result_summary(&item["content"]);
                                if summary.is_empty() {
                                    writeln!(
                                        out,
                                        "\x1b[31m  \u{2717} {}{}\x1b[0m",
                                        name, metadata
                                    )?;
                                } else {
                                    writeln!(
                                        out,
                                        "\x1b[31m  \u{2717} {} — {}{}\x1b[0m",
                                        name, summary, metadata
                                    )?;
                                }
                            } else {
                                writeln!(out, "\x1b[2m  \u{2713} {}{}\x1b[0m", name, metadata)?;
                            }
                        }
                    }
                }
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
