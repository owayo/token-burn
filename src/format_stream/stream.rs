//! `stream_event` 種別のイベント（message_start / content_block_* / message_delta /
//! message_stop）をディスパッチし、逐次表示を行うモジュール。

use anyhow::Result;
use std::io::Write;

use crate::format_stream::blocks::{
    BlockKind, ContentBlockState, finalize_block, finalize_open_blocks, infer_block_kind_from_delta,
};
use crate::format_stream::state::StreamState;

pub(crate) fn handle_stream_event(
    event: &serde_json::Value,
    out: &mut impl Write,
    state: &mut StreamState,
) -> Result<()> {
    let event_type = event["type"].as_str().unwrap_or("");

    match event_type {
        "message_start" => {
            finalize_open_blocks(out, state.blocks)?;
            state.summary.update_from_message(&event["message"]);
        }
        "content_block_start" => {
            let block = &event["content_block"];
            let index = event["index"]
                .as_u64()
                .and_then(|idx| usize::try_from(idx).ok())
                .unwrap_or(0);
            let incoming = ContentBlockState::from_start_block(block);
            let current = state
                .blocks
                .entry(index)
                .or_insert_with(|| ContentBlockState::new(incoming.kind));

            if current.kind == BlockKind::Unknown {
                current.kind = incoming.kind;
            }
            if current.tool_name.is_empty() && !incoming.tool_name.is_empty() {
                current.tool_name = incoming.tool_name.clone();
            }
            if current.tool_input.is_empty() && !incoming.tool_input.is_empty() {
                current.tool_input = incoming.tool_input;
            }
            if current.text.is_empty() && !incoming.text.is_empty() {
                current.text = incoming.text.clone();
                write!(out, "{}", incoming.text)?;
                out.flush()?;
            }

            if matches!(current.kind, BlockKind::ToolUse | BlockKind::ServerToolUse)
                && let Some(id) = block["id"].as_str()
            {
                let name = if current.tool_name.is_empty() {
                    "?"
                } else {
                    current.tool_name.as_str()
                };
                state.tool_id_map.insert(id.to_string(), name.to_string());
            }

            if current.kind == BlockKind::Thinking {
                current.ensure_thinking_started(out)?;
            }
        }
        "content_block_delta" => {
            let delta = &event["delta"];
            let dt = delta["type"].as_str().unwrap_or("");
            let index = event["index"]
                .as_u64()
                .and_then(|idx| usize::try_from(idx).ok())
                .unwrap_or(0);
            let block = state
                .blocks
                .entry(index)
                .or_insert_with(|| ContentBlockState::new(infer_block_kind_from_delta(dt)));

            if block.kind == BlockKind::Unknown {
                block.kind = infer_block_kind_from_delta(dt);
            }

            match dt {
                "thinking_delta" => {
                    if let Some(text) = delta["thinking"].as_str() {
                        block.ensure_thinking_started(out)?;
                        let prev = block.thinking_chars / 100;
                        block.thinking_chars += text.len();
                        let curr = block.thinking_chars / 100;
                        for _ in prev..curr {
                            write!(out, ".")?;
                        }
                        out.flush()?;
                    }
                }
                "text_delta" => {
                    if let Some(text) = delta["text"].as_str() {
                        block.text.push_str(text);
                        write!(out, "{}", text)?;
                        out.flush()?;
                    }
                }
                "input_json_delta" => {
                    if let Some(json) = delta["partial_json"].as_str() {
                        block.tool_input.push_str(json);
                    }
                }
                _ => {} // signature_delta etc
            }
        }
        "content_block_stop" => {
            let index = event["index"]
                .as_u64()
                .and_then(|idx| usize::try_from(idx).ok())
                .unwrap_or(0);
            if let Some(block) = state.blocks.remove(&index) {
                finalize_block(out, block)?;
            }
        }
        "message_delta" => {
            state.summary.update_from_message_delta(event);
        }
        "message_stop" => finalize_open_blocks(out, state.blocks)?,
        _ => {} // message_start, message_delta
    }

    Ok(())
}
