//! ストリーミングされるコンテンツブロック（テキスト/思考/ツール使用）の状態管理と
//! ブロック確定時の表示を担うモジュール。

use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;

use crate::format_stream::diff::format_tool_diff;
use crate::format_stream::tools::detail::extract_tool_detail;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    ServerToolUse,
    Unknown,
}

pub(crate) struct ContentBlockState {
    pub(crate) kind: BlockKind,
    pub(crate) tool_name: String,
    pub(crate) tool_input: String,
    pub(crate) text: String,
    /// 思考デルタの累積バイト数。100 バイトごとに進捗ドットを 1 つ出すために使う。
    pub(crate) thinking_bytes: usize,
    pub(crate) thinking_started: bool,
}

impl ContentBlockState {
    pub(crate) fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            tool_name: String::new(),
            tool_input: String::new(),
            text: String::new(),
            thinking_bytes: 0,
            thinking_started: false,
        }
    }

    pub(crate) fn from_start_block(block: &serde_json::Value) -> Self {
        let kind = block_kind(block["type"].as_str().unwrap_or(""));
        let mut state = Self::new(kind);

        if matches!(kind, BlockKind::ToolUse | BlockKind::ServerToolUse) {
            state.tool_name = block["name"].as_str().unwrap_or("?").to_string();
            if let Some(input) = block.get("input")
                && !input.is_null()
                && input.as_object().map(|obj| !obj.is_empty()).unwrap_or(true)
                && let Ok(serialized) = serde_json::to_string(input)
            {
                state.tool_input = serialized;
            }
        }

        if kind == BlockKind::Text {
            state.text = block["text"].as_str().unwrap_or("").to_string();
        }

        state
    }

    pub(crate) fn ensure_thinking_started(&mut self, out: &mut impl Write) -> Result<()> {
        if !self.thinking_started {
            write!(out, "\x1b[2m\u{1f4ad} ")?;
            out.flush()?;
            self.thinking_started = true;
        }
        Ok(())
    }
}

pub(crate) fn block_kind(block_type: &str) -> BlockKind {
    match block_type {
        "text" => BlockKind::Text,
        "thinking" => BlockKind::Thinking,
        "tool_use" => BlockKind::ToolUse,
        "server_tool_use" => BlockKind::ServerToolUse,
        _ => BlockKind::Unknown,
    }
}

pub(crate) fn infer_block_kind_from_delta(delta_type: &str) -> BlockKind {
    match delta_type {
        "text_delta" => BlockKind::Text,
        "thinking_delta" | "signature_delta" => BlockKind::Thinking,
        "input_json_delta" => BlockKind::ToolUse,
        _ => BlockKind::Unknown,
    }
}

pub(crate) fn finalize_block(out: &mut impl Write, block: ContentBlockState) -> Result<()> {
    match block.kind {
        BlockKind::Thinking => {
            if block.thinking_started {
                writeln!(out, "\x1b[0m")?;
            }
        }
        BlockKind::ToolUse | BlockKind::ServerToolUse => {
            let tool_name = if block.tool_name.is_empty() {
                "?"
            } else {
                block.tool_name.as_str()
            };
            let detail = extract_tool_detail(tool_name, &block.tool_input);
            if detail.is_empty() {
                writeln!(out, "\x1b[36m\u{1f527} {}\x1b[0m", tool_name)?;
            } else {
                writeln!(
                    out,
                    "\x1b[36m\u{1f527} {}\x1b[0m \x1b[2m{}\x1b[0m",
                    tool_name, detail
                )?;
            }
            if let Some(diff) = format_tool_diff(tool_name, &block.tool_input) {
                write!(out, "{}", diff)?;
            }
        }
        BlockKind::Text => {
            // テキストデルタは改行なしで逐次書き出すため、ブロック終了時に
            // 末尾が改行で終わっていなければ改行を補う。次に続くツール使用や
            // 思考ブロックが同じ行に連結されるのを防ぐ。
            if !block.text.is_empty() && !block.text.ends_with('\n') {
                writeln!(out)?;
            }
        }
        BlockKind::Unknown => {}
    }

    Ok(())
}

/// 未確定のブロックが行を開いたままなら、その行を閉じる。
///
/// 思考ブロックは `\x1b[2m💭 ` を改行なしで書き、以後ドットを追記していく。
/// `--include-partial-messages` では同一メッセージの `assistant` イベントが
/// 思考の途中で届くため、通知行をそのまま書くと思考行の途中に連結され、
/// 通知末尾の `\x1b[0m` が思考ブロックの dim を打ち消してしまう。
/// 通知を出す直前にこれを呼んで、独立した行から書き始められるようにする。
pub(crate) fn break_open_line(
    out: &mut impl Write,
    blocks: &mut HashMap<usize, ContentBlockState>,
) -> Result<()> {
    for block in blocks.values_mut() {
        match block.kind {
            BlockKind::Thinking if block.thinking_started => {
                writeln!(out, "\x1b[0m")?;
                // 次の thinking_delta で 💭 プレフィックスから描き直させる。
                block.thinking_started = false;
            }
            BlockKind::Text if !block.text.is_empty() && !block.text.ends_with('\n') => {
                writeln!(out)?;
                // finalize_block が改行を二重に足さないよう、書き出した分を記録する。
                block.text.push('\n');
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn finalize_open_blocks(
    out: &mut impl Write,
    blocks: &mut HashMap<usize, ContentBlockState>,
) -> Result<()> {
    let mut indices: Vec<_> = blocks.keys().copied().collect();
    indices.sort_unstable();
    for index in indices {
        if let Some(block) = blocks.remove(&index) {
            finalize_block(out, block)?;
        }
    }
    Ok(())
}
