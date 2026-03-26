use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::Path;

/// `claude -p` の stream-json 出力を読みやすいテキストに変換する。
/// JSON以外の行はそのまま出力（任意のエージェントで動作）。
pub fn run(raw_output: Option<&Path>) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let out = stdout.lock();
    process(stdin.lock(), out, raw_output)
}

fn process(reader: impl BufRead, mut out: impl Write, raw_output: Option<&Path>) -> Result<()> {
    let mut tool_id_map: HashMap<String, String> = HashMap::new();
    let mut blocks: HashMap<usize, ContentBlockState> = HashMap::new();
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
            "system" => {} // 初期化メッセージをスキップ
            "stream_event" => {
                handle_stream_event(
                    &v["event"],
                    &mut out,
                    &mut StreamState {
                        blocks: &mut blocks,
                        tool_id_map: &mut tool_id_map,
                    },
                )?;
            }
            "assistant" => {
                // assistant メッセージから tool_use ID を抽出（サブエージェントのツール結果に必要）
                if let Some(content) = v["message"]["content"].as_array() {
                    for item in content {
                        if matches!(item["type"].as_str(), Some("tool_use" | "server_tool_use"))
                            && let (Some(id), Some(name)) =
                                (item["id"].as_str(), item["name"].as_str())
                        {
                            tool_id_map.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }
            "user" => {
                // ツール結果 — 完了したツール名を表示
                if let Some(content) = v["message"]["content"].as_array() {
                    for item in content {
                        if item["type"].as_str() == Some("tool_result") {
                            let id = item["tool_use_id"].as_str().unwrap_or("");
                            let name = tool_id_map.get(id).map(|s| s.as_str()).unwrap_or("?");
                            let is_error = item["is_error"].as_bool().unwrap_or(false);
                            if is_error {
                                writeln!(out, "\x1b[31m  \u{2717} {}\x1b[0m", name)?;
                            } else {
                                writeln!(out, "\x1b[2m  \u{2713} {}\x1b[0m", name)?;
                            }
                        }
                    }
                }
            }
            "result" => {
                finalize_open_blocks(&mut out, &mut blocks)?;
                handle_result(&v, &mut out)?;
            }
            _ => {} // rate_limit_event, message_stop 等
        }
    }

    finalize_open_blocks(&mut out, &mut blocks)?;

    Ok(())
}

fn handle_result(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    if let Some(cost) = v["total_cost_usd"].as_f64() {
        writeln!(out, "\n\x1b[33m\u{1f4b0} ${:.4}\x1b[0m", cost)?;
    }
    if let Some(ms) = v["duration_ms"].as_u64() {
        let secs = ms / 1000;
        let m = secs / 60;
        let s = secs % 60;
        if let Some(turns) = v["num_turns"].as_u64() {
            writeln!(
                out,
                "\x1b[33m\u{23f1}  {}m {}s ({} turns)\x1b[0m",
                m, s, turns
            )?;
        } else {
            writeln!(out, "\x1b[33m\u{23f1}  {}m {}s\x1b[0m", m, s)?;
        }
    }
    let input = v["usage"]["input_tokens"].as_u64().unwrap_or(0)
        + v["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0)
        + v["usage"]["cache_creation_input_tokens"]
            .as_u64()
            .unwrap_or(0);
    let output = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
    if output > 0 {
        writeln!(
            out,
            "\x1b[33m\u{1f4ca} in:{} out:{}\x1b[0m",
            format_number(input),
            format_number(output)
        )?;
    }
    Ok(())
}

struct StreamState<'a> {
    blocks: &'a mut HashMap<usize, ContentBlockState>,
    tool_id_map: &'a mut HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    ServerToolUse,
    Unknown,
}

struct ContentBlockState {
    kind: BlockKind,
    tool_name: String,
    tool_input: String,
    text: String,
    thinking_chars: usize,
    thinking_started: bool,
}

impl ContentBlockState {
    fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            tool_name: String::new(),
            tool_input: String::new(),
            text: String::new(),
            thinking_chars: 0,
            thinking_started: false,
        }
    }

    fn from_start_block(block: &serde_json::Value) -> Self {
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

    fn ensure_thinking_started(&mut self, out: &mut impl Write) -> Result<()> {
        if !self.thinking_started {
            write!(out, "\x1b[2m\u{1f4ad} ")?;
            out.flush()?;
            self.thinking_started = true;
        }
        Ok(())
    }
}

fn block_kind(block_type: &str) -> BlockKind {
    match block_type {
        "text" => BlockKind::Text,
        "thinking" => BlockKind::Thinking,
        "tool_use" => BlockKind::ToolUse,
        "server_tool_use" => BlockKind::ServerToolUse,
        _ => BlockKind::Unknown,
    }
}

fn infer_block_kind_from_delta(delta_type: &str) -> BlockKind {
    match delta_type {
        "text_delta" => BlockKind::Text,
        "thinking_delta" | "signature_delta" => BlockKind::Thinking,
        "input_json_delta" => BlockKind::ToolUse,
        _ => BlockKind::Unknown,
    }
}

fn finalize_block(out: &mut impl Write, block: ContentBlockState) -> Result<()> {
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
        BlockKind::Text | BlockKind::Unknown => {}
    }

    Ok(())
}

fn finalize_open_blocks(
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

fn handle_stream_event(
    event: &serde_json::Value,
    out: &mut impl Write,
    state: &mut StreamState,
) -> Result<()> {
    let event_type = event["type"].as_str().unwrap_or("");

    match event_type {
        "message_start" => {
            finalize_open_blocks(out, state.blocks)?;
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
        "message_stop" => finalize_open_blocks(out, state.blocks)?,
        _ => {} // message_start, message_delta
    }

    Ok(())
}

fn extract_tool_detail(tool_name: &str, input_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    match tool_name {
        "Edit" => {
            let file = v["file_path"].as_str().unwrap_or("");
            let old = v["old_string"].as_str().unwrap_or("");
            let new = v["new_string"].as_str().unwrap_or("");
            let old_lines = old.lines().count();
            let new_lines = new.lines().count();
            let added = new_lines.saturating_sub(old_lines);
            let removed = old_lines.saturating_sub(new_lines);
            return format!("{} (+{}/-{})", truncate_str(file, 80), added, removed);
        }
        "Bash" => {
            let cmd = v["command"].as_str().unwrap_or("");
            let desc = v["description"].as_str().unwrap_or("");
            if !desc.is_empty() {
                return format!("{} ({})", truncate_str(cmd, 60), truncate_str(desc, 40));
            }
            return truncate_str(cmd, 100).to_string();
        }
        "Task" => {
            let desc = v["description"].as_str().unwrap_or("");
            let name = v["name"].as_str().unwrap_or("");
            let agent_type = v["subagent_type"].as_str().unwrap_or("");
            if !name.is_empty() && !agent_type.is_empty() {
                return format!("{} ({})", name, agent_type);
            } else if !desc.is_empty() {
                return truncate_str(desc, 80).to_string();
            } else if !name.is_empty() {
                return name.to_string();
            }
        }
        "TeamCreate" => {
            if let Some(team) = v["team_name"].as_str()
                && !team.is_empty()
            {
                return team.to_string();
            }
        }
        _ => {}
    }

    // 汎用: よくあるフィールド名を優先順に試行
    for key in [
        "file_path",
        "path",
        "pattern",
        "command",
        "query",
        "url",
        "description",
        "name",
    ] {
        if let Some(val) = v[key].as_str() {
            return truncate_str(val, 100).to_string();
        }
    }

    String::new()
}

fn truncate_str(s: &str, max: usize) -> String {
    let mut iter = s.chars();
    let mut prefix = String::new();
    for _ in 0..max {
        match iter.next() {
            Some(ch) => prefix.push(ch),
            None => return prefix,
        }
    }
    if iter.next().is_none() {
        return prefix;
    }
    let kept: String = prefix.chars().take(max.saturating_sub(3)).collect();
    format!("{kept}...")
}

fn format_tool_diff(tool_name: &str, input_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input_json).ok()?;

    match tool_name {
        "Edit" => {
            let old = v["old_string"].as_str().unwrap_or("");
            let new = v["new_string"].as_str().unwrap_or("");
            if old.is_empty() && new.is_empty() {
                return None;
            }
            let diff = format_diff_lines(old, new);
            if diff.is_empty() { None } else { Some(diff) }
        }
        _ => None,
    }
}

/// 新旧テキスト間のカラー差分を生成する。
/// 共通のプレフィックス/サフィックス行を検出し、変更部分のみ表示する。
fn format_diff_lines(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        old.lines().collect()
    };
    let new_lines: Vec<&str> = if new.is_empty() {
        Vec::new()
    } else {
        new.lines().collect()
    };

    // 共通プレフィックス行を検出
    let prefix_len = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();

    // 共通サフィックス行を検出（プレフィックス以降）
    let old_rest = &old_lines[prefix_len..];
    let new_rest = &new_lines[prefix_len..];
    let suffix_len = old_rest
        .iter()
        .rev()
        .zip(new_rest.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let old_changed = &old_lines[prefix_len..old_lines.len() - suffix_len];
    let new_changed = &new_lines[prefix_len..new_lines.len() - suffix_len];

    if old_changed.is_empty() && new_changed.is_empty() {
        return String::new();
    }

    let max_context = 2;
    let max_changed = 12;
    let mut result = String::new();

    // プレフィックスからのコンテキスト（末尾N行）
    let ctx_start = prefix_len.saturating_sub(max_context);
    for line in &old_lines[ctx_start..prefix_len] {
        result.push_str(&format!("\x1b[2m    {}\x1b[0m\n", line));
    }

    // 削除行
    for (i, line) in old_changed.iter().enumerate() {
        if i >= max_changed {
            result.push_str(&format!(
                "\x1b[31m  ... ({} more)\x1b[0m\n",
                old_changed.len() - max_changed
            ));
            break;
        }
        result.push_str(&format!("\x1b[31m  - {}\x1b[0m\n", line));
    }

    // 追加行
    for (i, line) in new_changed.iter().enumerate() {
        if i >= max_changed {
            result.push_str(&format!(
                "\x1b[32m  ... ({} more)\x1b[0m\n",
                new_changed.len() - max_changed
            ));
            break;
        }
        result.push_str(&format!("\x1b[32m  + {}\x1b[0m\n", line));
    }

    // サフィックスからのコンテキスト（先頭N行）
    let suffix_start = old_lines.len() - suffix_len;
    let suffix_show = std::cmp::min(suffix_len, max_context);
    for line in &old_lines[suffix_start..suffix_start + suffix_show] {
        result.push_str(&format!("\x1b[2m    {}\x1b[0m\n", line));
    }

    result
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn strip_ansi(s: &str) -> String {
        let mut result = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                result.push(c);
            }
        }
        result
    }

    fn run_process(input: &str) -> String {
        run_process_with_raw_log(input, None)
    }

    fn run_process_with_raw_log(input: &str, raw_output: Option<&std::path::Path>) -> String {
        let reader = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        process(reader, &mut output, raw_output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn extract_tool_detail_file_path() {
        let input = r#"{"file_path":"/src/main.rs"}"#;
        assert_eq!(extract_tool_detail("Read", input), "/src/main.rs");
    }

    #[test]
    fn extract_tool_detail_command() {
        let input = r#"{"command":"cargo test"}"#;
        assert_eq!(extract_tool_detail("Bash", input), "cargo test");
    }

    #[test]
    fn extract_tool_detail_bash_with_description() {
        let input = r#"{"command":"pnpm install","description":"Install deps"}"#;
        assert_eq!(
            extract_tool_detail("Bash", input),
            "pnpm install (Install deps)"
        );
    }

    #[test]
    fn extract_tool_detail_edit_shows_diff_stats() {
        let input = r#"{"file_path":"/src/index.test.ts","old_string":"line1\nline2\nline3","new_string":"line1\nline2\nline3\nline4\nline5"}"#;
        let result = extract_tool_detail("Edit", input);
        assert!(result.contains("/src/index.test.ts"));
        assert!(result.contains("(+2/-0)"));
    }

    #[test]
    fn extract_tool_detail_edit_removal() {
        let input = r#"{"file_path":"/src/main.rs","old_string":"a\nb\nc","new_string":"a"}"#;
        let result = extract_tool_detail("Edit", input);
        assert!(result.contains("(+0/-2)"));
    }

    #[test]
    fn extract_tool_detail_truncates_long_values() {
        let long_path = format!(r#"{{"file_path":"{}"}}"#, "a".repeat(200));
        let result = extract_tool_detail("Read", &long_path);
        assert!(result.len() <= 103); // 100 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn extract_tool_detail_invalid_json() {
        assert_eq!(extract_tool_detail("Read", "not json"), "");
    }

    #[test]
    fn extract_tool_detail_no_known_fields() {
        assert_eq!(extract_tool_detail("Unknown", r#"{"foo":"bar"}"#), "");
    }

    #[test]
    fn format_number_with_commas() {
        assert_eq!(format_number(1234567), "1,234,567");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
    }

    #[test]
    fn process_text_only_response() {
        // 思考ブロックなしの単純なテキスト応答（例: "say hello"）
        let input = [
            r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"s1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","id":"msg_1"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world!"}}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hello world!"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            r#"{"type":"result","subtype":"success","total_cost_usd":0.2148,"duration_ms":5191,"usage":{"input_tokens":3,"cache_read_input_tokens":14726,"output_tokens":45}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("Hello world!"));
        assert!(clean.contains("$0.2148"));
        assert!(clean.contains("0m 5s"));
        assert!(clean.contains("in:14,729 out:45"));
        // system/assistant/rate_limit は表示しない
        assert!(!clean.contains("init"));
        assert!(!clean.contains("rate_limit"));
    }

    #[test]
    fn process_thinking_then_tool_use() {
        // 思考ブロック → ツール使用 → ツール結果 → テキスト応答
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me analyze this code..."}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"/src/main.rs\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"fn main() {}"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Found the file."}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        // 思考インジケーター
        assert!(clean.contains("\u{1f4ad}"));
        // ツール名とファイルパス
        assert!(clean.contains("\u{1f527} Read"));
        assert!(clean.contains("/src/main.rs"));
        // ツール結果にツール名が表示される
        assert!(
            clean.contains("\u{2713} Read"),
            "expected '✓ Read' in: {}",
            clean
        );
        // テキスト応答
        assert!(clean.contains("Found the file."));
    }

    #[test]
    fn process_non_json_passthrough() {
        let input = "plain text line\nanother line\n";
        let output = run_process(input);
        assert!(output.contains("plain text line"));
        assert!(output.contains("another line"));
    }

    #[test]
    fn process_result_without_cache() {
        let input = r#"{"type":"result","total_cost_usd":0.05,"duration_ms":1234,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("$0.0500"));
        assert!(clean.contains("in:100 out:50"));
    }

    #[test]
    fn process_edit_tool_shows_diff_stats() {
        // 実際の Edit ツールと同じ入力形式
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t_edit","name":"Edit","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/index.test.ts\",\"old_string\":\"line1\\nline2\",\"new_string\":\"line1\\nline2\\nline3\\nline4\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_edit","type":"tool_result","content":"The file has been updated successfully."}]}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("\u{1f527} Edit"),
            "expected Edit tool icon in: {}",
            clean
        );
        assert!(
            clean.contains("/src/index.test.ts"),
            "expected file path in: {}",
            clean
        );
        assert!(
            clean.contains("(+2/-0)"),
            "expected diff stats in: {}",
            clean
        );
        assert!(
            clean.contains("\u{2713} Edit"),
            "expected checkmark in: {}",
            clean
        );
    }

    #[test]
    fn process_bash_tool_with_description() {
        // 実際の Bash ツールと同じ入力形式
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t_bash","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pnpm install\",\"description\":\"Install dependencies\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_bash","type":"tool_result","content":"+ typescript 5.9.3"}]}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("\u{1f527} Bash"),
            "expected Bash tool icon in: {}",
            clean
        );
        assert!(
            clean.contains("pnpm install"),
            "expected command in: {}",
            clean
        );
        assert!(
            clean.contains("(Install dependencies)"),
            "expected description in: {}",
            clean
        );
    }

    #[test]
    fn process_tool_result_error() {
        // is_error=true の tool_result
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t_err","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"exit 1\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_err","type":"tool_result","is_error":true,"content":"Command failed"}]}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        // エラー時は ✓ ではなく ✗ を表示する
        assert!(
            clean.contains("\u{2717} Bash"),
            "expected error mark in: {}",
            clean
        );
        assert!(
            !clean.contains("\u{2713}"),
            "should not have checkmark on error: {}",
            clean
        );
    }

    #[test]
    fn process_result_with_full_stats() {
        // 実際の claude 実行結果と同じく num_turns と cache_creation_input_tokens を含む
        let input = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":41712,"num_turns":9,"total_cost_usd":0.5565,"usage":{"input_tokens":14,"cache_creation_input_tokens":54926,"cache_read_input_tokens":372099,"output_tokens":987}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("$0.5565"), "expected cost in: {}", clean);
        assert!(clean.contains("0m 41s"), "expected duration in: {}", clean);
        assert!(clean.contains("(9 turns)"), "expected turns in: {}", clean);
        // input = 14 + 54926 + 372099 = 427039
        assert!(
            clean.contains("in:427,039"),
            "expected input tokens with cache creation in: {}",
            clean
        );
        assert!(
            clean.contains("out:987"),
            "expected output tokens in: {}",
            clean
        );
    }

    #[test]
    fn process_multi_turn_read_edit_bash() {
        // Read → Edit → Bash の複数ツール連続実行
        let input = [
            // 1ターン目: Read
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/index.ts\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"export function add(a, b) { return a + b; }"}]}}"#,
            // 2ターン目: Edit
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t2","name":"Edit","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/index.test.ts\",\"old_string\":\"test1\",\"new_string\":\"test1\\ntest2\\ntest3\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t2","type":"tool_result","content":"Updated successfully."}]}}"#,
            // 3ターン目: Bash
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t3","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pnpm exec tsc --noEmit\",\"description\":\"Type check\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t3","type":"tool_result","content":""}]}}"#,
            // 最終テキスト
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            // 結果
            r#"{"type":"result","total_cost_usd":0.55,"duration_ms":41000,"num_turns":4,"usage":{"input_tokens":10,"cache_read_input_tokens":1000,"output_tokens":100}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        // Read
        assert!(clean.contains("\u{1f527} Read"));
        assert!(clean.contains("/src/index.ts"));
        assert!(clean.contains("\u{2713} Read"));
        // Edit
        assert!(clean.contains("\u{1f527} Edit"));
        assert!(clean.contains("/src/index.test.ts"));
        assert!(clean.contains("(+2/-0)"));
        assert!(clean.contains("\u{2713} Edit"));
        // Bash
        assert!(clean.contains("\u{1f527} Bash"));
        assert!(clean.contains("pnpm exec tsc --noEmit"));
        assert!(clean.contains("(Type check)"));
        assert!(clean.contains("\u{2713} Bash"));
        // テキスト
        assert!(clean.contains("Done."));
        // 結果
        assert!(clean.contains("(4 turns)"));
    }

    #[test]
    fn extract_tool_detail_task_with_name_and_type() {
        let input = r#"{"description":"implement feature","name":"worker-1","subagent_type":"general-purpose","prompt":"Do stuff","team_name":"my-team"}"#;
        assert_eq!(
            extract_tool_detail("Task", input),
            "worker-1 (general-purpose)"
        );
    }

    #[test]
    fn extract_tool_detail_task_description_only() {
        let input = r#"{"description":"research codebase","prompt":"Investigate..."}"#;
        assert_eq!(extract_tool_detail("Task", input), "research codebase");
    }

    #[test]
    fn extract_tool_detail_task_name_only() {
        let input = r#"{"name":"explorer","prompt":"Find files"}"#;
        assert_eq!(extract_tool_detail("Task", input), "explorer");
    }

    #[test]
    fn extract_tool_detail_team_create() {
        let input = r#"{"team_name":"demo-team","description":"Working on feature X"}"#;
        assert_eq!(extract_tool_detail("TeamCreate", input), "demo-team");
    }

    #[test]
    fn extract_tool_detail_generic_description_fallback() {
        // TaskCreate のように description はあるが専用ハンドラがないツール
        let input = r#"{"subject":"Run tests","description":"Execute test suite","activeForm":"Running tests"}"#;
        assert_eq!(
            extract_tool_detail("TaskCreate", input),
            "Execute test suite"
        );
    }

    #[test]
    fn extract_tool_detail_generic_name_fallback() {
        // 優先フィールドがなく name だけを持つツール
        let input = r#"{"name":"my-worktree"}"#;
        assert_eq!(extract_tool_detail("EnterWorktree", input), "my-worktree");
    }

    #[test]
    fn process_system_task_started_skipped() {
        // subtype が task_started の system イベントは黙って無視する
        let input = [
            r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"s1"}"#,
            r#"{"type":"system","subtype":"task_started","task_type":"in_process_teammate","task_id":"abc-123","tool_use_id":"tu_1","description":"implement feature"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        // system イベントは表示しない
        assert!(!clean.contains("task_started"));
        assert!(!clean.contains("in_process_teammate"));
        // テキストは表示される
        assert!(clean.contains("Hello"));
    }

    #[test]
    fn process_team_create_then_task_spawn() {
        // TeamCreate → Task の順でチームエージェントを起動する流れ
        let input = [
            // TeamCreate
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tc1","name":"TeamCreate","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"team_name\":\"demo-team\",\"description\":\"Build demo project\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tc1","type":"tool_result","content":"Team created"}]}}"#,
            // Task 起動
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"ts1","name":"Task","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"implement utils\",\"name\":\"worker-1\",\"subagent_type\":\"general-purpose\",\"team_name\":\"demo-team\",\"prompt\":\"Create utility module\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"ts1","type":"tool_result","content":"{\"status\":\"teammate_spawned\"}"}]}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        // TeamCreate
        assert!(
            clean.contains("\u{1f527} TeamCreate"),
            "expected TeamCreate tool in: {}",
            clean
        );
        assert!(
            clean.contains("demo-team"),
            "expected team name in: {}",
            clean
        );
        assert!(
            clean.contains("\u{2713} TeamCreate"),
            "expected checkmark for TeamCreate in: {}",
            clean
        );
        // Task
        assert!(
            clean.contains("\u{1f527} Task"),
            "expected Task tool in: {}",
            clean
        );
        assert!(
            clean.contains("worker-1 (general-purpose)"),
            "expected agent name and type in: {}",
            clean
        );
        assert!(
            clean.contains("\u{2713} Task"),
            "expected checkmark for Task in: {}",
            clean
        );
    }

    #[test]
    fn process_result_with_model_usage() {
        // modelUsage のモデル別内訳があっても崩れないことを確認
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.234,"duration_ms":120000,"num_turns":15,"usage":{"input_tokens":500,"cache_read_input_tokens":50000,"output_tokens":2000},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":200,"outputTokens":1500,"cacheReadInputTokens":40000,"cost":0.234},"claude-opus-4-6":{"inputTokens":300,"outputTokens":500,"cacheReadInputTokens":10000,"cost":1.0}},"stop_reason":null}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("$1.2340"), "expected cost in: {}", clean);
        assert!(clean.contains("2m 0s"), "expected duration in: {}", clean);
        assert!(clean.contains("(15 turns)"), "expected turns in: {}", clean);
        // input = 500 + 50000 = 50500
        assert!(
            clean.contains("in:50,500"),
            "expected input tokens in: {}",
            clean
        );
        assert!(
            clean.contains("out:2,000"),
            "expected output tokens in: {}",
            clean
        );
    }

    #[test]
    fn process_result_with_stop_reason_null() {
        // team session では stop_reason が null でも処理できる
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.01,"duration_ms":3000,"usage":{"input_tokens":10,"output_tokens":5},"stop_reason":null}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("$0.0100"));
        assert!(clean.contains("0m 3s"));
    }

    // --- format_diff_lines の単体テスト ---

    #[test]
    fn diff_pure_deletion() {
        let diff = format_diff_lines("line1\nline2\nline3", "");
        let clean = strip_ansi(&diff);
        assert!(clean.contains("- line1"), "got: {}", clean);
        assert!(clean.contains("- line2"), "got: {}", clean);
        assert!(clean.contains("- line3"), "got: {}", clean);
        assert!(!clean.contains("+"), "should have no additions: {}", clean);
    }

    #[test]
    fn diff_pure_addition() {
        let diff = format_diff_lines("", "new1\nnew2");
        let clean = strip_ansi(&diff);
        assert!(clean.contains("+ new1"), "got: {}", clean);
        assert!(clean.contains("+ new2"), "got: {}", clean);
        assert!(!clean.contains("-"), "should have no removals: {}", clean);
    }

    #[test]
    fn diff_with_context() {
        // 共通プレフィックス "aaa" と共通サフィックス "zzz" の間だけが変化
        let old = "aaa\nbbb\nzzz";
        let new = "aaa\nccc\nddd\nzzz";
        let diff = format_diff_lines(old, new);
        let clean = strip_ansi(&diff);
        // コンテキスト行（前後）
        assert!(
            clean.contains("    aaa"),
            "expected prefix context: {}",
            clean
        );
        assert!(
            clean.contains("    zzz"),
            "expected suffix context: {}",
            clean
        );
        // 変更行
        assert!(clean.contains("- bbb"), "expected removal: {}", clean);
        assert!(clean.contains("+ ccc"), "expected addition: {}", clean);
        assert!(clean.contains("+ ddd"), "expected addition: {}", clean);
    }

    #[test]
    fn diff_identical_returns_empty() {
        let diff = format_diff_lines("same\nlines", "same\nlines");
        assert!(
            diff.is_empty(),
            "identical strings should produce empty diff"
        );
    }

    #[test]
    fn diff_truncates_long_changes() {
        // 20行の削除は省略表示される
        let old_lines: Vec<&str> = (0..20).map(|_| "old").collect();
        let old = old_lines.join("\n");
        let diff = format_diff_lines(&old, "new");
        let clean = strip_ansi(&diff);
        assert!(
            clean.contains("... (8 more)"),
            "expected truncation indicator: {}",
            clean
        );
    }

    #[test]
    fn diff_single_line_change() {
        let diff = format_diff_lines("old line", "new line");
        let clean = strip_ansi(&diff);
        assert!(clean.contains("- old line"), "got: {}", clean);
        assert!(clean.contains("+ new line"), "got: {}", clean);
    }

    // --- format_tool_diff の単体テスト ---

    #[test]
    fn format_tool_diff_edit() {
        let input =
            r#"{"file_path":"/src/main.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#;
        let diff = format_tool_diff("Edit", input).unwrap();
        let clean = strip_ansi(&diff);
        assert!(clean.contains("- let x = 1;"));
        assert!(clean.contains("+ let x = 2;"));
    }

    #[test]
    fn format_tool_diff_edit_empty_strings() {
        let input = r#"{"file_path":"/src/main.rs","old_string":"","new_string":""}"#;
        assert!(format_tool_diff("Edit", input).is_none());
    }

    #[test]
    fn format_tool_diff_non_edit() {
        let input = r#"{"command":"ls"}"#;
        assert!(format_tool_diff("Bash", input).is_none());
    }

    // --- process パイプラインで Edit 差分が出る統合テスト ---

    #[test]
    fn process_edit_shows_diff_output() {
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Edit","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/lib.rs\",\"old_string\":\"fn old() {}\\nfn keep() {}\",\"new_string\":\"fn new() {}\\nfn also_new() {}\\nfn keep() {}\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        // ツールヘッダー
        assert!(
            clean.contains("\u{1f527} Edit"),
            "expected Edit header: {}",
            clean
        );
        assert!(
            clean.contains("/src/lib.rs"),
            "expected file path: {}",
            clean
        );
        // 差分内容
        assert!(
            clean.contains("- fn old() {}"),
            "expected removed line: {}",
            clean
        );
        assert!(
            clean.contains("+ fn new() {}"),
            "expected added line: {}",
            clean
        );
        assert!(
            clean.contains("+ fn also_new() {}"),
            "expected added line: {}",
            clean
        );
        // コンテキスト（共通サフィックス）
        assert!(
            clean.contains("    fn keep() {}"),
            "expected context line: {}",
            clean
        );
    }

    #[test]
    fn process_edit_pure_deletion_shows_diff() {
        // ログに現れる純削除パターン
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Edit","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/repo.rs\",\"old_string\":\"    if found {\\n        break;\\n    }\",\"new_string\":\"\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("(+0/-3)"), "expected diff stats: {}", clean);
        assert!(clean.contains("- "), "expected removed lines: {}", clean);
        assert!(
            !clean.contains("+ "),
            "should have no added lines: {}",
            clean
        );
    }

    #[test]
    fn process_subagent_tool_uses_from_assistant_message() {
        // サブエージェントの tool_use は stream_event ではなく assistant message として届く。
        // assistant message 内の tool_use に parent_tool_use_id が入り、
        // 後続の user message の tool_result がその ID を参照する。
        let input = [
            r#"{"type":"system","subtype":"init"}"#,
            // tool_use を 2 つ含む assistant message
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"sub_read_1","name":"Read","input":{"file_path":"/src/lib.rs"},"parent_tool_use_id":"task_1"},{"type":"tool_use","id":"sub_glob_2","name":"Glob","input":{"pattern":"**/*.rs"},"parent_tool_use_id":"task_1"}]}}"#,
            // 2 つの tool_result を返す user message
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"sub_read_1","content":"file contents"},{"type":"tool_result","tool_use_id":"sub_glob_2","content":"src/lib.rs\nsrc/main.rs"}]}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        // どちらの結果も "?" ではなく正しいツール名を表示する
        assert!(
            clean.contains("\u{2713} Read"),
            "expected '✓ Read' but got: {}",
            clean
        );
        assert!(
            clean.contains("\u{2713} Glob"),
            "expected '✓ Glob' but got: {}",
            clean
        );
        assert!(
            !clean.contains("\u{2713} ?"),
            "should not contain '✓ ?' fallback: {}",
            clean
        );
    }

    #[test]
    fn process_input_json_delta_before_block_start() {
        // 実装によっては delta が start より先に見えることがあるため、
        // index 単位で入力断片を保持して後続 start と結合する。
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"cargo test\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("\u{1f527} Bash"), "got: {}", clean);
        assert!(clean.contains("cargo test"), "got: {}", clean);
        assert!(clean.contains("\u{2713} Bash"), "got: {}", clean);
    }

    #[test]
    fn process_message_stop_flushes_open_tool_use() {
        // content_block_stop が欠けても message_stop で未完了 block を確定する。
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/tmp/demo.txt\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("\u{1f527} Read"), "got: {}", clean);
        assert!(clean.contains("/tmp/demo.txt"), "got: {}", clean);
    }

    #[test]
    fn process_server_tool_use() {
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv1","name":"WebFetch","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"url\":\"https://example.com\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"srv1","content":"ok"}]}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("\u{1f527} WebFetch"), "got: {}", clean);
        assert!(clean.contains("https://example.com"), "got: {}", clean);
        assert!(clean.contains("\u{2713} WebFetch"), "got: {}", clean);
    }

    #[test]
    fn process_writes_raw_stream_log() {
        let dir = tempfile::tempdir().unwrap();
        let raw_path = dir.path().join("raw.jsonl");
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            "",
            "plain text line",
        ]
        .join("\n");

        let _ = run_process_with_raw_log(&input, Some(&raw_path));
        let raw = std::fs::read_to_string(&raw_path).unwrap();

        assert!(raw.contains("\"content_block_start\""), "got: {}", raw);
        assert!(raw.contains("\"text_delta\""), "got: {}", raw);
        assert!(raw.contains("plain text line"), "got: {}", raw);
        assert!(raw.lines().count() >= 4, "got: {}", raw);
    }

    // --- truncate_str の単体テスト ---

    #[test]
    fn process_result_duration_only() {
        // コストなし、duration_ms のみ
        let input =
            r#"{"type":"result","duration_ms":65000,"usage":{"input_tokens":0,"output_tokens":0}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("1m 5s"), "expected duration in: {}", clean);
        // output=0 の場合はトークン行を出力しない
        assert!(
            !clean.contains("in:"),
            "output=0 ではトークン行を出力しない: {}",
            clean
        );
    }

    #[test]
    fn process_result_no_duration() {
        // duration_ms がない場合
        let input = r#"{"type":"result","total_cost_usd":0.01,"usage":{"input_tokens":10,"output_tokens":5}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("$0.0100"));
        assert!(!clean.contains("m "), "duration なしでは時間を表示しない");
    }

    #[test]
    fn format_number_zero() {
        assert_eq!(format_number(0), "0");
    }

    #[test]
    fn format_number_large() {
        assert_eq!(format_number(1_000_000), "1,000,000");
    }

    #[test]
    fn truncate_str_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_exact_length_unchanged() {
        assert_eq!(truncate_str("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_str_long_string_truncated() {
        assert_eq!(truncate_str("abcdefghij", 7), "abcd...");
    }

    #[test]
    fn truncate_str_multibyte_counts_chars() {
        // 日本語 5 文字は 15 バイトでも、文字数としては 5 として扱う
        let s = "あいうえお";
        assert_eq!(truncate_str(s, 5), "あいうえお");
        assert_eq!(truncate_str(s, 4), "あ...");
    }

    #[test]
    fn process_empty_tool_input_on_block_stop() {
        // tool_input が空（partial_json なし）で content_block_stop が来た場合
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Read","id":"t1"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#,
        ]
        .join("\n");
        let output = run_process(&input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("Read"), "ツール名が表示されるべき");
    }

    #[test]
    fn process_unknown_tool_result_id() {
        // tool_id_map に存在しない ID の tool_result
        let input = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"unknown-id","content":"ok"}]}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("?"), "不明なツールは '?' と表示されるべき");
    }

    #[test]
    fn process_tool_result_error_flag() {
        // is_error: true の tool_result は異なるマーカーで表示
        let input = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"command failed"}]}}"#,
        ]
        .join("\n");
        let output = run_process(&input);
        let clean = strip_ansi(&output);
        // エラーマーカー "✗" が表示される
        assert!(clean.contains("\u{2717}"), "エラーマーカーが表示されるべき");
    }

    #[test]
    fn format_number_single_digit() {
        assert_eq!(format_number(5), "5");
    }

    #[test]
    fn format_number_three_digits() {
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn format_number_four_digits() {
        assert_eq!(format_number(1000), "1,000");
    }

    #[test]
    fn truncate_str_empty_string() {
        assert_eq!(truncate_str("", 10), "");
    }

    #[test]
    fn truncate_str_max_three() {
        // max=3 の場合、3文字以下は変化なし、4文字以上は "..." のみ
        assert_eq!(truncate_str("abc", 3), "abc");
        assert_eq!(truncate_str("abcd", 3), "...");
    }
}
