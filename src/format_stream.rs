use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::Path;

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
                summary.update_from_result(v.as_object());
                finalize_open_blocks(&mut out, &mut blocks)?;
                handle_result(&v, &summary, &mut out)?;
            }
            "rate_limit_event" => {
                handle_rate_limit_event(&v, &mut out, stop_file, threshold)?;
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

/// system イベントのうち、サブエージェント進捗・通知・完了通知を表示する。
fn handle_system_event(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let subtype = v["subtype"].as_str().unwrap_or("");
    match subtype {
        "task_started" => {
            let desc = v["description"].as_str().unwrap_or("");
            let task_type = v["task_type"].as_str().unwrap_or("");
            if !desc.is_empty() {
                if !task_type.is_empty() {
                    writeln!(
                        out,
                        "\x1b[2m  \u{23f3} {} ({})\x1b[0m",
                        truncate_str(desc, 80),
                        task_type
                    )?;
                } else {
                    writeln!(out, "\x1b[2m  \u{23f3} {}\x1b[0m", truncate_str(desc, 80))?;
                }
            }
        }
        "task_progress" => {
            let desc = v["description"].as_str().unwrap_or("");
            let tool = v["last_tool_name"].as_str().unwrap_or("");
            if !desc.is_empty() {
                if !tool.is_empty() {
                    writeln!(
                        out,
                        "\x1b[2m  \u{1f504} {} ({})\x1b[0m",
                        truncate_str(desc, 80),
                        tool
                    )?;
                } else {
                    writeln!(out, "\x1b[2m  \u{1f504} {}\x1b[0m", truncate_str(desc, 80))?;
                }
            }
        }
        "task_notification" => {
            let status = v["status"].as_str().unwrap_or("");
            let summary = v["summary"].as_str().unwrap_or("");
            let tokens = v["usage"]["total_tokens"].as_u64().unwrap_or(0);
            let duration_ms = v["usage"]["duration_ms"].as_u64().unwrap_or(0);
            let dur_s = duration_ms / 1000;
            let m = dur_s / 60;
            let s = dur_s % 60;
            if status == "completed" {
                if !summary.is_empty() {
                    writeln!(
                        out,
                        "\x1b[32m  \u{2705} {} ({}m {}s, {} tokens)\x1b[0m",
                        truncate_str(summary, 60),
                        m,
                        s,
                        format_number(tokens)
                    )?;
                } else {
                    writeln!(
                        out,
                        "\x1b[32m  \u{2705} Task completed ({}m {}s)\x1b[0m",
                        m, s
                    )?;
                }
            } else if status == "failed" {
                writeln!(
                    out,
                    "\x1b[31m  \u{274c} Task {} ({}m {}s)\x1b[0m",
                    status, m, s
                )?;
            } else if status == "stopped" {
                // TaskStop で停止された場合
                if !summary.is_empty() {
                    writeln!(
                        out,
                        "\x1b[33m  \u{23f9} Task stopped: {} ({}m {}s)\x1b[0m",
                        truncate_str(summary, 60),
                        m,
                        s
                    )?;
                } else {
                    writeln!(
                        out,
                        "\x1b[33m  \u{23f9} Task stopped ({}m {}s)\x1b[0m",
                        m, s
                    )?;
                }
            }
        }
        "task_updated" => {
            let status = v["patch"]["status"].as_str().unwrap_or("");
            match status {
                "completed" => {
                    writeln!(out, "\x1b[32m  \u{2705} Task completed\x1b[0m")?;
                }
                "failed" | "cancelled" => {
                    writeln!(out, "\x1b[31m  \u{274c} Task {}\x1b[0m", status)?;
                }
                status if !status.is_empty() => {
                    writeln!(out, "\x1b[2m  \u{2139} Task {}\x1b[0m", status)?;
                }
                _ => {}
            }
        }
        "notification" => {
            let text = v["text"].as_str().unwrap_or("");
            if !text.is_empty() {
                let key = v["key"].as_str().unwrap_or("");
                let detail = if key.is_empty() {
                    truncate_str(text, 100)
                } else {
                    format!("{} ({})", truncate_str(text, 80), truncate_str(key, 40))
                };
                if v["priority"].as_str() == Some("immediate") {
                    writeln!(out, "\x1b[31m  \u{26a0} Notification: {}\x1b[0m", detail)?;
                } else {
                    writeln!(out, "\x1b[33m  \u{26a0} Notification: {}\x1b[0m", detail)?;
                }
            }
        }
        "api_retry" => {
            let attempt = v["attempt"].as_u64().unwrap_or(0);
            let max_retries = v["max_retries"].as_u64().unwrap_or(0);
            let error = v["error"].as_str().unwrap_or("unknown");
            let status = v["error_status"]
                .as_u64()
                .map(|s| format!(" ({})", s))
                .unwrap_or_default();
            writeln!(
                out,
                "\x1b[33m  \u{26a0} API retry {}/{}: {}{}\x1b[0m",
                attempt, max_retries, error, status
            )?;
        }
        _ => {} // init, hook_started, hook_response, hook_progress 等は無視
    }
    Ok(())
}

/// レート制限イベントを表示する。
/// `allowed_warning` は使用率の警告、`rejected` はリクエスト拒否を示す。
/// 使用率が閾値を超えた場合、stop_file を作成して後続タスクを停止する。
fn handle_rate_limit_event(
    v: &serde_json::Value,
    out: &mut impl Write,
    stop_file: Option<&Path>,
    threshold: u8,
) -> Result<()> {
    let info = &v["rate_limit_info"];
    let status = info["status"].as_str().unwrap_or("");
    let resets_at = format_resets_at(info);
    match status {
        "allowed" => {
            let limit_type = info["rateLimitType"].as_str().unwrap_or("");
            let extras = format_rate_limit_allowed_details(info);
            if !extras.is_empty() || !resets_at.is_empty() {
                let details = if limit_type.is_empty() {
                    extras
                } else if extras.is_empty() {
                    limit_type.to_string()
                } else {
                    format!("{limit_type} {extras}")
                };
                let details = truncate_str(&details, 80);
                if details.is_empty() {
                    writeln!(
                        out,
                        "\x1b[2m  \u{2139} Rate limit status: allowed{}\x1b[0m",
                        resets_at
                    )?;
                } else {
                    writeln!(
                        out,
                        "\x1b[2m  \u{2139} Rate limit status: allowed ({}){}\x1b[0m",
                        details, resets_at
                    )?;
                }
            }
        }
        "allowed_warning" => {
            let utilization = info["utilization"].as_f64().unwrap_or(0.0);
            let pct = utilization * 100.0;
            let limit_type = info["rateLimitType"].as_str().unwrap_or("");
            if pct >= threshold as f64 {
                touch_stop_file(stop_file);
                writeln!(
                    out,
                    "\x1b[31m  \u{26d4} Rate limit auto-stop: {:.0}% used ({}) >= threshold {}%{}\x1b[0m",
                    pct, limit_type, threshold, resets_at
                )?;
            } else {
                writeln!(
                    out,
                    "\x1b[33m  \u{26a0} Rate limit warning: {:.0}% used ({}){}\x1b[0m",
                    pct, limit_type, resets_at
                )?;
            }
        }
        "rejected" => {
            let limit_type = info["rateLimitType"].as_str().unwrap_or("");
            touch_stop_file(stop_file);
            writeln!(
                out,
                "\x1b[31m  \u{1f6ab} Rate limited: request rejected ({}){}\x1b[0m",
                limit_type, resets_at
            )?;
        }
        _ => {} // "allowed" は表示不要
    }
    Ok(())
}

/// `allowed` の補足情報を 1 行表示用に連結する。
fn format_rate_limit_allowed_details(info: &serde_json::Value) -> String {
    let mut parts = Vec::new();

    if let Some(overage_status) = info["overageStatus"].as_str()
        && !overage_status.is_empty()
    {
        parts.push(format!("overage:{overage_status}"));
    }
    if info["isUsingOverage"].as_bool() == Some(true) {
        parts.push("using_overage".to_string());
    }
    if let Some(reason) = info["overageDisabledReason"].as_str()
        && !reason.is_empty()
    {
        parts.push(format!("reason:{reason}"));
    }

    parts.join(" ")
}

/// `resetsAt` Unix タイムスタンプをローカル時刻の文字列に整形する。
/// フィールドが存在しない場合は空文字列を返す。
fn format_resets_at(info: &serde_json::Value) -> String {
    info["resetsAt"]
        .as_i64()
        .and_then(|ts| {
            chrono::DateTime::from_timestamp(ts, 0).map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format(" resets %H:%M")
                    .to_string()
            })
        })
        .unwrap_or_default()
}

/// stop_file が指定されていれば作成する（全ワーカーの後続タスクを停止するシグナル）。
fn touch_stop_file(stop_file: Option<&Path>) {
    if let Some(path) = stop_file {
        let _ = File::create(path);
    }
}

fn handle_result(
    v: &serde_json::Value,
    summary: &StreamSummary,
    out: &mut impl Write,
) -> Result<()> {
    if let Some(cost) = v["total_cost_usd"].as_f64() {
        writeln!(out, "\n\x1b[33m\u{1f4b0} ${:.4}\x1b[0m", cost)?;
    }
    if let Some(ms) = v["duration_ms"].as_u64() {
        let secs = ms / 1000;
        let m = secs / 60;
        let s = secs % 60;
        let api_part = if let Some(api_ms) = v["duration_api_ms"].as_u64() {
            let api_secs = api_ms / 1000;
            format!(" api:{}m {}s", api_secs / 60, api_secs % 60)
        } else {
            String::new()
        };
        if let Some(turns) = v["num_turns"].as_u64() {
            writeln!(
                out,
                "\x1b[33m\u{23f1}  {}m {}s ({} turns{})\x1b[0m",
                m, s, turns, api_part
            )?;
        } else {
            writeln!(out, "\x1b[33m\u{23f1}  {}m {}s\x1b[0m", m, s)?;
        }
    }
    let input = summary.usage.total_input_tokens();
    let output = summary.usage.output_tokens;
    if output > 0 {
        writeln!(
            out,
            "\x1b[33m\u{1f4ca} in:{} out:{}\x1b[0m",
            format_number(input),
            format_number(output)
        )?;
    }
    if summary.usage.has_cache_details() {
        let mut details = Vec::new();
        if summary.usage.cache_read_input_tokens > 0 {
            details.push(format!(
                "read:{}",
                format_number(summary.usage.cache_read_input_tokens)
            ));
        }
        if summary.usage.cache_write_5m_tokens() > 0 {
            details.push(format!(
                "write5m:{}",
                format_number(summary.usage.cache_write_5m_tokens())
            ));
        }
        if summary.usage.cache_creation_1h_input_tokens > 0 {
            details.push(format!(
                "write1h:{}",
                format_number(summary.usage.cache_creation_1h_input_tokens)
            ));
        }
        writeln!(out, "\x1b[2m   cache {}\x1b[0m", details.join(" "))?;
    }
    if let Some(model) = &summary.model
        && !model.is_empty()
    {
        writeln!(out, "\x1b[2m   model {}\x1b[0m", model)?;
    }
    if let Some(stop_reason) = &summary.stop_reason
        && !stop_reason.is_empty()
        && stop_reason != "end_turn"
    {
        writeln!(out, "\x1b[2m   stop {}\x1b[0m", stop_reason)?;
    }
    if summary.usage.web_search_requests > 0 || summary.usage.web_fetch_requests > 0 {
        let mut parts = Vec::new();
        if summary.usage.web_search_requests > 0 {
            parts.push(format!("search:{}", summary.usage.web_search_requests));
        }
        if summary.usage.web_fetch_requests > 0 {
            parts.push(format!("fetch:{}", summary.usage.web_fetch_requests));
        }
        writeln!(out, "\x1b[2m   web {}\x1b[0m", parts.join(" "))?;
    }
    // モデル別使用量（modelUsage）の表示
    if let Some(model_usage) = v["modelUsage"].as_object() {
        for (model_id, usage) in model_usage {
            let cost = usage["costUSD"].as_f64().unwrap_or(0.0);
            let input_tokens = usage["inputTokens"].as_u64().unwrap_or(0);
            let output_tokens = usage["outputTokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cacheReadInputTokens"].as_u64().unwrap_or(0);
            let cache_creation = usage["cacheCreationInputTokens"].as_u64().unwrap_or(0);
            let web_search = usage["webSearchRequests"].as_u64().unwrap_or(0);
            if cost > 0.0 || output_tokens > 0 {
                let mut extras = Vec::new();
                if cache_read > 0 {
                    extras.push(format!("cache_read:{}", format_number(cache_read)));
                }
                if cache_creation > 0 {
                    extras.push(format!("cache_write:{}", format_number(cache_creation)));
                }
                if web_search > 0 {
                    extras.push(format!("web:{}", web_search));
                }
                let extra_str = if extras.is_empty() {
                    String::new()
                } else {
                    format!(" {}", extras.join(" "))
                };
                writeln!(
                    out,
                    "\x1b[2m   {} ${:.4} (in:{} out:{}{})\x1b[0m",
                    model_id,
                    cost,
                    format_number(input_tokens),
                    format_number(output_tokens),
                    extra_str,
                )?;
            }
        }
    }
    // fast_mode の表示（off 以外の場合）
    if let Some(fast_mode) = v["fast_mode_state"].as_str()
        && fast_mode != "off"
    {
        writeln!(out, "\x1b[2m   fast_mode {}\x1b[0m", fast_mode)?;
    }
    // 異常終了時の終了理由（completed 以外）を表示
    if let Some(reason) = v["terminal_reason"].as_str()
        && !reason.is_empty()
        && reason != "completed"
    {
        writeln!(out, "\x1b[33m   terminal {}\x1b[0m", reason)?;
    }
    // 権限拒否されたツール呼び出しの件数を表示
    if let Some(denials) = v["permission_denials"].as_array()
        && !denials.is_empty()
    {
        writeln!(
            out,
            "\x1b[33m   permission_denials {}\x1b[0m",
            denials.len()
        )?;
    }
    Ok(())
}

struct StreamState<'a> {
    blocks: &'a mut HashMap<usize, ContentBlockState>,
    tool_id_map: &'a mut HashMap<String, String>,
    summary: &'a mut StreamSummary,
}

#[derive(Default)]
struct StreamSummary {
    session_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    message_id: Option<String>,
    stop_reason: Option<String>,
    usage: UsageSummary,
}

impl StreamSummary {
    fn update_from_system(&mut self, value: &serde_json::Value) {
        if self.session_id.is_none() {
            self.session_id = value["session_id"].as_str().map(ToOwned::to_owned);
        }
        if self.cwd.is_none() {
            self.cwd = value["cwd"].as_str().map(ToOwned::to_owned);
        }
    }

    fn update_from_message(&mut self, value: &serde_json::Value) {
        if let Some(model) = value["model"].as_str() {
            self.model = Some(model.to_string());
        }
        if let Some(id) = value["id"].as_str() {
            self.message_id = Some(id.to_string());
        }
        self.usage.merge_from_value(value.get("usage"));
    }

    fn update_from_message_delta(&mut self, event: &serde_json::Value) {
        if let Some(stop_reason) = event["delta"]["stop_reason"].as_str() {
            self.stop_reason = Some(stop_reason.to_string());
        }
        self.usage.merge_from_value(event.get("usage"));
    }

    fn update_from_result(&mut self, value: Option<&serde_json::Map<String, serde_json::Value>>) {
        let Some(value) = value else {
            return;
        };
        if let Some(model) = value.get("model").and_then(|v| v.as_str()) {
            self.model = Some(model.to_string());
        }
        if let Some(stop_reason) = value.get("stop_reason").and_then(|v| v.as_str()) {
            self.stop_reason = Some(stop_reason.to_string());
        }
        self.usage.merge_from_value(value.get("usage"));
    }
}

#[derive(Default)]
struct UsageSummary {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_creation_5m_input_tokens: u64,
    cache_creation_1h_input_tokens: u64,
    web_search_requests: u64,
    web_fetch_requests: u64,
}

impl UsageSummary {
    /// `usage` ペイロードからフィールドを取り込む。
    /// Claude Code の stream-json は各 message_start / message_delta が
    /// その API 呼び出し単独の usage を返し、`result` イベントに最終累計が入る。
    /// そのため最後に `update_from_result` が呼ばれた時点で正しい合計値となる。
    /// 各フィールドは累積ではなく上書き代入することで `result` の値を最終値として優先する。
    fn merge_from_value(&mut self, value: Option<&serde_json::Value>) {
        let Some(value) = value else {
            return;
        };

        if let Some(v) = value["input_tokens"].as_u64() {
            self.input_tokens = v;
        }
        if let Some(v) = value["output_tokens"].as_u64() {
            self.output_tokens = v;
        }
        if let Some(v) = value["cache_read_input_tokens"].as_u64() {
            self.cache_read_input_tokens = v;
        }
        if let Some(v) = value["cache_creation_input_tokens"].as_u64() {
            self.cache_creation_input_tokens = v;
        }
        if let Some(v) = value["cache_creation"]["ephemeral_5m_input_tokens"].as_u64() {
            self.cache_creation_5m_input_tokens = v;
        }
        if let Some(v) = value["cache_creation"]["ephemeral_1h_input_tokens"].as_u64() {
            self.cache_creation_1h_input_tokens = v;
        }
        if let Some(v) = value["server_tool_use"]["web_search_requests"].as_u64() {
            self.web_search_requests = v;
        }
        if let Some(v) = value["server_tool_use"]["web_fetch_requests"].as_u64() {
            self.web_fetch_requests = v;
        }
    }

    fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }

    fn cache_write_5m_tokens(&self) -> u64 {
        if self.cache_creation_5m_input_tokens > 0 {
            return self.cache_creation_5m_input_tokens;
        }
        // 1hの内訳が存在する場合、5mは本当に0
        if self.cache_creation_1h_input_tokens > 0 {
            return 0;
        }
        // 内訳が存在しない場合は合計値をフォールバック
        self.cache_creation_input_tokens
    }

    fn has_cache_details(&self) -> bool {
        self.cache_read_input_tokens > 0
            || self.cache_creation_input_tokens > 0
            || self.cache_creation_5m_input_tokens > 0
            || self.cache_creation_1h_input_tokens > 0
    }
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

fn extract_tool_detail(tool_name: &str, input_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    match tool_name {
        "Read" => {
            let file = first_string(&v, &["file_path", "path"]);
            let mut attrs = Vec::new();
            if let Some(offset) = v["offset"].as_u64() {
                attrs.push(format!("offset={offset}"));
            }
            if let Some(limit) = v["limit"].as_u64() {
                attrs.push(format!("limit={limit}"));
            }
            if !file.is_empty() && !attrs.is_empty() {
                return format!("{} ({})", truncate_str(file, 80), attrs.join(", "));
            }
            if !file.is_empty() {
                return truncate_str(file, 100).to_string();
            }
        }
        "Edit" => {
            let file = v["file_path"].as_str().unwrap_or("");
            let old = first_string(&v, &["old_string", "old_str"]);
            let new = first_string(&v, &["new_string", "new_str"]);
            let old_lines = old.lines().count();
            let new_lines = new.lines().count();
            let added = new_lines.saturating_sub(old_lines);
            let removed = old_lines.saturating_sub(new_lines);
            return format!("{} (+{}/-{})", truncate_str(file, 80), added, removed);
        }
        "Bash" => {
            let cmd = v["command"].as_str().unwrap_or("");
            let desc = v["description"].as_str().unwrap_or("");
            let mut attrs = Vec::new();
            if let Some(timeout) = v["timeout"].as_u64() {
                attrs.push(format!("timeout={}s", timeout / 1000));
            }
            if v["run_in_background"].as_bool() == Some(true) {
                attrs.push("background".to_string());
            }
            let attr_text = if attrs.is_empty() {
                String::new()
            } else {
                format!(" [{}]", attrs.join(", "))
            };
            if !desc.is_empty() {
                return format!(
                    "{}{} ({})",
                    truncate_str(cmd, 60),
                    attr_text,
                    truncate_str(desc, 40)
                );
            }
            return format!("{}{}", truncate_str(cmd, 100), attr_text);
        }
        "Grep" | "Glob" => {
            let pattern = v["pattern"].as_str().unwrap_or("");
            let path = v["path"].as_str().unwrap_or("");
            let glob = v["glob"].as_str().unwrap_or("");
            let mut attrs = Vec::new();
            if let Some(output_mode) = v["output_mode"].as_str()
                && !output_mode.is_empty()
            {
                attrs.push(format!("mode:{output_mode}"));
            }
            if !glob.is_empty() {
                attrs.push(format!("glob:{}", truncate_str(glob, 40)));
            }
            if let Some(head_limit) = v["head_limit"].as_u64() {
                attrs.push(format!("head:{head_limit}"));
            }
            if let Some(context) = v["context"].as_u64() {
                attrs.push(format!("ctx:{context}"));
            }
            for key in ["-A", "-B", "-C"] {
                if let Some(value) = v[key].as_u64() {
                    attrs.push(format!("{}:{value}", key.trim_start_matches('-')));
                }
            }
            if v["-n"].as_bool() == Some(true) {
                attrs.push("line".to_string());
            }
            let attr_text = if attrs.is_empty() {
                String::new()
            } else {
                format!(" ({})", attrs.join(", "))
            };
            if !pattern.is_empty() && !path.is_empty() {
                return format!(
                    "{} @ {}{}",
                    truncate_str(pattern, 60),
                    truncate_str(path, 50),
                    attr_text
                );
            }
            if !pattern.is_empty() {
                return format!("{}{}", truncate_str(pattern, 100), attr_text);
            }
            if !path.is_empty() {
                return format!("{}{}", truncate_str(path, 100), attr_text);
            }
        }
        "Task" | "Agent" => {
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
        "Write" => {
            let file = v["file_path"].as_str().unwrap_or("");
            let content = v["content"].as_str().unwrap_or("");
            let lines = content.lines().count();
            return format!("{} ({} lines)", truncate_str(file, 80), lines);
        }
        "Skill" => {
            let skill = v["skill"].as_str().unwrap_or("");
            let args = v["args"].as_str().unwrap_or("");
            if !args.is_empty() {
                return format!("{} ({})", skill, truncate_str(args, 60));
            }
            return skill.to_string();
        }
        "TodoWrite" => {
            if let Some(todos) = v["todos"].as_array() {
                let total = todos.len();
                let done = todos
                    .iter()
                    .filter(|t| t["status"].as_str() == Some("completed"))
                    .count();
                return format!("{}/{} completed", done, total);
            }
        }
        "ScheduleWakeup" => {
            let delay = v["delaySeconds"].as_u64();
            let reason = v["reason"].as_str().unwrap_or("");
            let prompt = v["prompt"].as_str().unwrap_or("");
            if let Some(delay) = delay {
                let note = if !reason.is_empty() { reason } else { prompt };
                if note.is_empty() {
                    return format!("{delay}s");
                }
                return format!("{delay}s ({})", truncate_str(note, 60));
            }
            if !reason.is_empty() {
                return truncate_str(reason, 80).to_string();
            }
            if !prompt.is_empty() {
                return truncate_str(prompt, 80).to_string();
            }
        }
        "WebFetch" => {
            let url = v["url"].as_str().unwrap_or("");
            let prompt = v["prompt"].as_str().unwrap_or("");
            if !url.is_empty() && !prompt.is_empty() {
                return format!("{} ({})", truncate_str(url, 70), truncate_str(prompt, 40));
            }
            if !url.is_empty() {
                return truncate_str(url, 100).to_string();
            }
        }
        "WebSearch" => {
            let query = v["query"].as_str().unwrap_or("");
            let allowed = v["allowed_domains"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            let blocked = v["blocked_domains"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            if !query.is_empty() {
                let mut detail = truncate_str(query, 80);
                if allowed > 0 || blocked > 0 {
                    let mut filters = Vec::new();
                    if allowed > 0 {
                        filters.push(format!("allow:{allowed}"));
                    }
                    if blocked > 0 {
                        filters.push(format!("block:{blocked}"));
                    }
                    detail = format!("{} ({})", detail, filters.join(", "));
                }
                return detail;
            }
        }
        "ToolSearch" => {
            let query = v["query"].as_str().unwrap_or("");
            let max_results = v["max_results"].as_u64();
            if !query.is_empty() {
                if let Some(n) = max_results {
                    return format!("{} (max={})", truncate_str(query, 80), n);
                }
                return truncate_str(query, 100).to_string();
            }
        }
        "Monitor" => {
            let desc = v["description"].as_str().unwrap_or("");
            let cmd = v["command"].as_str().unwrap_or("");
            let timeout_ms = v["timeout_ms"].as_u64();
            let persistent = v["persistent"].as_bool().unwrap_or(false);

            let mut detail = if !desc.is_empty() {
                truncate_str(desc, 80)
            } else if !cmd.is_empty() {
                truncate_str(cmd, 80)
            } else {
                String::new()
            };

            let mut attrs = Vec::new();
            if let Some(ms) = timeout_ms {
                attrs.push(format!("timeout={}s", ms / 1000));
            }
            if persistent {
                attrs.push("persistent".to_string());
            }
            if !detail.is_empty() && !attrs.is_empty() {
                detail = format!("{} ({})", detail, attrs.join(", "));
            }

            return detail;
        }
        "SendMessage" => {
            let to = first_string(&v, &["to", "recipient"]);
            let summary = v["summary"].as_str().unwrap_or("");
            let message = first_string(&v, &["message", "content"]);
            let label = if !summary.is_empty() {
                summary
            } else {
                message
            };
            if !to.is_empty() && !label.is_empty() {
                return format!(
                    "{} -> {}",
                    truncate_inline(label, 70),
                    truncate_inline(to, 40)
                );
            }
            if !label.is_empty() {
                return truncate_inline(label, 100);
            }
            if !to.is_empty() {
                return format!("to {}", truncate_inline(to, 80));
            }
        }
        "TaskStop" => {
            if let Some(task_id) = v["task_id"].as_str()
                && !task_id.is_empty()
            {
                return format!("task {}", truncate_str(task_id, 80));
            }
        }
        "TaskOutput" => {
            // TaskOutput はサブエージェント完了を待つために呼ばれる
            // input 例: {"task_id":"b9x7zeewd","block":true,"timeout":300000}
            let task_id = v["task_id"].as_str().unwrap_or("");
            let block = v["block"].as_bool().unwrap_or(false);
            let timeout_ms = v["timeout"].as_u64();
            let mut attrs = Vec::new();
            if block {
                attrs.push("block".to_string());
            }
            if let Some(ms) = timeout_ms {
                let secs = ms / 1000;
                attrs.push(format!("timeout={secs}s"));
            }
            if !task_id.is_empty() {
                if attrs.is_empty() {
                    return format!("task {}", truncate_str(task_id, 80));
                }
                return format!("task {} ({})", truncate_str(task_id, 60), attrs.join(", "));
            }
            if !attrs.is_empty() {
                return attrs.join(", ");
            }
        }
        "mcp__tavily__tavily-search" => {
            let query = v["query"].as_str().unwrap_or("");
            if !query.is_empty() {
                let mut attrs = Vec::new();
                if let Some(max_results) = v["max_results"].as_u64() {
                    attrs.push(format!("max={max_results}"));
                }
                if let Some(time_range) = v["time_range"].as_str()
                    && !time_range.is_empty()
                {
                    attrs.push(format!("range={time_range}"));
                }
                if let Some(search_depth) = v["search_depth"].as_str()
                    && !search_depth.is_empty()
                {
                    attrs.push(format!("depth={search_depth}"));
                }
                if attrs.is_empty() {
                    return truncate_inline(query, 100);
                }
                return format!("{} ({})", truncate_inline(query, 80), attrs.join(", "));
            }
        }
        "mcp__codex__codex" => {
            let prompt = v["prompt"].as_str().unwrap_or("");
            let cwd = v["cwd"].as_str().unwrap_or("");
            if !cwd.is_empty() && !prompt.is_empty() {
                return format!(
                    "{} ({})",
                    truncate_inline(prompt, 70),
                    truncate_inline(cwd, 50)
                );
            }
            if !prompt.is_empty() {
                return truncate_inline(prompt, 100);
            }
            if !cwd.is_empty() {
                return truncate_inline(cwd, 100);
            }
        }
        name if name.starts_with("mcp__context7__resolve-library-id") => {
            let library = v["libraryName"].as_str().unwrap_or("");
            let query = v["query"].as_str().unwrap_or("");
            if !library.is_empty() && !query.is_empty() {
                return format!(
                    "{} ({})",
                    truncate_str(library, 50),
                    truncate_str(query, 60)
                );
            }
            if !library.is_empty() {
                return truncate_str(library, 100).to_string();
            }
            if !query.is_empty() {
                return truncate_str(query, 100).to_string();
            }
        }
        name if name.starts_with("mcp__context7__query-docs") => {
            let library_id = v["libraryId"].as_str().unwrap_or("");
            let query = v["query"].as_str().unwrap_or("");
            if !library_id.is_empty() && !query.is_empty() {
                return format!(
                    "{} ({})",
                    truncate_str(library_id, 50),
                    truncate_str(query, 60)
                );
            }
            if !library_id.is_empty() {
                return truncate_str(library_id, 100).to_string();
            }
            if !query.is_empty() {
                return truncate_str(query, 100).to_string();
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
        "libraryName",
        "libraryId",
        "description",
        "prompt",
        "summary",
        "message",
        "name",
        "to",
        "task_id",
    ] {
        if let Some(val) = v[key].as_str() {
            return truncate_inline(val, 100);
        }
    }

    String::new()
}

fn first_string<'a>(value: &'a serde_json::Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|key| value[*key].as_str())
        .unwrap_or("")
}

fn truncate_inline(s: &str, max: usize) -> String {
    let normalized = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_str(&normalized, max)
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
            let old = first_string(&v, &["old_string", "old_str"]);
            let new = first_string(&v, &["new_string", "new_str"]);
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
        run_process_with_opts(input, None, None, 95)
    }

    fn run_process_with_raw_log(input: &str, raw_output: Option<&std::path::Path>) -> String {
        run_process_with_opts(input, raw_output, None, 95)
    }

    fn run_process_with_opts(
        input: &str,
        raw_output: Option<&std::path::Path>,
        stop_file: Option<&std::path::Path>,
        threshold: u8,
    ) -> String {
        let reader = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        process(reader, &mut output, raw_output, stop_file, threshold).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn extract_tool_detail_file_path() {
        let input = r#"{"file_path":"/src/main.rs"}"#;
        assert_eq!(extract_tool_detail("Read", input), "/src/main.rs");
    }

    #[test]
    fn extract_tool_detail_read_shows_offset_and_limit() {
        let input = r#"{"file_path":"/src/main.rs","offset":120,"limit":40}"#;
        assert_eq!(
            extract_tool_detail("Read", input),
            "/src/main.rs (offset=120, limit=40)"
        );
    }

    #[test]
    fn extract_tool_detail_command() {
        let input = r#"{"command":"cargo test"}"#;
        assert_eq!(extract_tool_detail("Bash", input), "cargo test");
    }

    #[test]
    fn extract_tool_detail_bash_shows_runtime_attrs() {
        let input = r#"{"command":"cargo test","description":"Run tests","timeout":300000,"run_in_background":true}"#;
        assert_eq!(
            extract_tool_detail("Bash", input),
            "cargo test [timeout=300s, background] (Run tests)"
        );
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
    fn extract_tool_detail_grep_shows_pattern_and_path() {
        let input = r#"{"pattern":"\"scripts\"","path":"/Users/owa/GitHub/vscode-git-smart-commit/package.json"}"#;
        let result = extract_tool_detail("Grep", input);
        assert!(result.starts_with("\"scripts\" @ "));
        assert!(result.contains("vscode-git-smart-commit"));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn extract_tool_detail_grep_shows_filters_and_limits() {
        let input = r#"{"pattern":"console\\.error","path":"/repo/src","output_mode":"content","glob":"*.ts","head_limit":20,"context":2,"-n":true}"#;
        assert_eq!(
            extract_tool_detail("Grep", input),
            "console\\.error @ /repo/src (mode:content, glob:*.ts, head:20, ctx:2, line)"
        );
    }

    #[test]
    fn extract_tool_detail_glob_pattern_only() {
        let input = r#"{"pattern":"{AGENTS.md,README.md,README.ja.md}"}"#;
        assert_eq!(
            extract_tool_detail("Glob", input),
            "{AGENTS.md,README.md,README.ja.md}"
        );
    }

    #[test]
    fn extract_tool_detail_schedule_wakeup_shows_delay_and_reason() {
        let input = r#"{"delaySeconds":90,"reason":"codex レビュー結果を待つため少し待機","prompt":"codex レビューの結果を確認して、テスト追加に進む"}"#;
        assert_eq!(
            extract_tool_detail("ScheduleWakeup", input),
            "90s (codex レビュー結果を待つため少し待機)"
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
    fn extract_tool_detail_edit_accepts_new_str_alias() {
        let input = r#"{"file_path":"/src/main.rs","old_string":"a","new_str":"a\nb"}"#;
        let result = extract_tool_detail("Edit", input);
        assert!(result.contains("(+1/-0)"), "got: {result}");
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
    fn extract_tool_detail_web_fetch_shows_url_and_prompt() {
        let input = r#"{"url":"https://example.com/docs","prompt":"Summarize the key points of this article"}"#;
        let result = extract_tool_detail("WebFetch", input);
        assert!(
            result.starts_with("https://example.com/docs"),
            "got: {result}"
        );
        assert!(result.contains("Summarize"), "got: {result}");
    }

    #[test]
    fn extract_tool_detail_web_fetch_url_only() {
        let input = r#"{"url":"https://example.com"}"#;
        assert_eq!(
            extract_tool_detail("WebFetch", input),
            "https://example.com"
        );
    }

    #[test]
    fn extract_tool_detail_web_search_with_filters() {
        let input = r#"{"query":"latest rust release","allowed_domains":["rust-lang.org","github.com"],"blocked_domains":["spam.example"]}"#;
        let result = extract_tool_detail("WebSearch", input);
        assert!(result.starts_with("latest rust release"), "got: {result}");
        assert!(result.contains("allow:2"), "got: {result}");
        assert!(result.contains("block:1"), "got: {result}");
    }

    #[test]
    fn extract_tool_detail_web_search_query_only() {
        let input = r#"{"query":"how to use tokio"}"#;
        assert_eq!(extract_tool_detail("WebSearch", input), "how to use tokio");
    }

    #[test]
    fn extract_tool_detail_tool_search_with_max_results() {
        let input = r#"{"query":"select:TodoWrite,WebFetch","max_results":3}"#;
        assert_eq!(
            extract_tool_detail("ToolSearch", input),
            "select:TodoWrite,WebFetch (max=3)"
        );
    }

    #[test]
    fn extract_tool_detail_tool_search_query_only() {
        let input = r#"{"query":"select:Monitor"}"#;
        assert_eq!(extract_tool_detail("ToolSearch", input), "select:Monitor");
    }

    #[test]
    fn extract_tool_detail_tavily_search_with_attrs() {
        let input = r#"{"query":"Android Gradle Plugin latest stable","max_results":5,"time_range":"month","search_depth":"advanced"}"#;
        assert_eq!(
            extract_tool_detail("mcp__tavily__tavily-search", input),
            "Android Gradle Plugin latest stable (max=5, range=month, depth=advanced)"
        );
    }

    #[test]
    fn extract_tool_detail_monitor_prefers_description() {
        let input = r#"{"description":"codexレビュー完了を待機","timeout_ms":600000,"persistent":false,"command":"until grep -q \"tokens used\" /tmp/codex-review-output.log; do sleep 5; done"}"#;
        assert_eq!(
            extract_tool_detail("Monitor", input),
            "codexレビュー完了を待機 (timeout=600s)"
        );
    }

    #[test]
    fn extract_tool_detail_monitor_falls_back_to_command() {
        let input = r#"{"command":"until test -s /tmp/output; do sleep 5; done","timeout_ms":300000,"persistent":true}"#;
        assert_eq!(
            extract_tool_detail("Monitor", input),
            "until test -s /tmp/output; do sleep 5; done (timeout=300s, persistent)"
        );
    }

    #[test]
    fn extract_tool_detail_task_stop_shows_task_id() {
        let input = r#"{"task_id":"b0mfly525"}"#;
        assert_eq!(extract_tool_detail("TaskStop", input), "task b0mfly525");
    }

    #[test]
    fn extract_tool_detail_task_output_shows_task_with_block_and_timeout() {
        // TaskOutput: task_id + block + timeout 全て指定
        let input = r#"{"task_id":"b9x7zeewd","block":true,"timeout":300000}"#;
        assert_eq!(
            extract_tool_detail("TaskOutput", input),
            "task b9x7zeewd (block, timeout=300s)"
        );
    }

    #[test]
    fn extract_tool_detail_task_output_with_only_task_id() {
        // TaskOutput: task_id のみ
        let input = r#"{"task_id":"abc123"}"#;
        assert_eq!(extract_tool_detail("TaskOutput", input), "task abc123");
    }

    #[test]
    fn extract_tool_detail_task_output_empty_input_returns_empty() {
        // TaskOutput: input が空の場合
        let input = r#"{}"#;
        assert_eq!(extract_tool_detail("TaskOutput", input), "");
    }

    #[test]
    fn extract_tool_detail_task_output_block_false_omits_block_attr() {
        // block:false の場合は属性表示しない
        let input = r#"{"task_id":"t1","block":false,"timeout":60000}"#;
        assert_eq!(
            extract_tool_detail("TaskOutput", input),
            "task t1 (timeout=60s)"
        );
    }

    #[test]
    fn extract_tool_detail_send_message_shows_summary_and_target() {
        let input = r#"{"to":"a15c8b054dbf603c9","message":"詳細を再送してください","summary":"Request full bug details"}"#;
        assert_eq!(
            extract_tool_detail("SendMessage", input),
            "Request full bug details -> a15c8b054dbf603c9"
        );
    }

    #[test]
    fn extract_tool_detail_send_message_accepts_recipient_alias() {
        let input = r#"{"recipient":"agent-1","content":"詳細な依頼本文"}"#;
        assert_eq!(
            extract_tool_detail("SendMessage", input),
            "詳細な依頼本文 -> agent-1"
        );
    }

    #[test]
    fn extract_tool_detail_codex_shows_prompt_and_cwd() {
        let input = r#"{"prompt":"レビューしてください\n詳細は git diff を確認してください","cwd":"/Users/owa/git/strategic-task-manager","sandbox":"read-only"}"#;
        let result = extract_tool_detail("mcp__codex__codex", input);
        assert!(
            result.starts_with("レビューしてください 詳細は git diff"),
            "got: {result}"
        );
        assert!(
            result.contains("/Users/owa/git/strategic-task-manager"),
            "got: {result}"
        );
    }

    #[test]
    fn extract_tool_detail_context7_resolve_library() {
        let input = r#"{"libraryName":"keyring","query":"keyring rust crate v4 migration breaking changes"}"#;
        assert_eq!(
            extract_tool_detail("mcp__context7__resolve-library-id", input),
            "keyring (keyring rust crate v4 migration breaking changes)"
        );
    }

    #[test]
    fn extract_tool_detail_context7_query_docs() {
        let input = r#"{"libraryId":"/open-source-cooperative/keyring-rs","query":"keyring v4 breaking changes migration guide"}"#;
        assert_eq!(
            extract_tool_detail("mcp__context7__query-docs", input),
            "/open-source-cooperative/keyring-rs (keyring v4 breaking changes migration guide)"
        );
    }

    #[test]
    fn process_result_shows_terminal_reason_when_not_completed() {
        // terminal_reason が "completed" 以外の場合だけ表示する
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.05,"duration_ms":5000,"usage":{"input_tokens":100,"output_tokens":50},"terminal_reason":"interrupted","permission_denials":[]}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("terminal interrupted"), "got: {clean}");
    }

    #[test]
    fn process_result_hides_terminal_reason_when_completed() {
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.05,"duration_ms":5000,"usage":{"input_tokens":100,"output_tokens":50},"terminal_reason":"completed","permission_denials":[]}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(!clean.contains("terminal"), "got: {clean}");
    }

    #[test]
    fn process_result_shows_permission_denials_count() {
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.05,"duration_ms":5000,"usage":{"input_tokens":100,"output_tokens":50},"terminal_reason":"completed","permission_denials":[{"tool_name":"Bash"},{"tool_name":"Edit"}]}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("permission_denials 2"), "got: {clean}");
    }

    #[test]
    fn process_result_hides_permission_denials_when_empty() {
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.05,"duration_ms":5000,"usage":{"input_tokens":100,"output_tokens":50},"terminal_reason":"completed","permission_denials":[]}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(!clean.contains("permission_denials"), "got: {clean}");
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
        assert!(
            clean.contains("cache read:372,099 write5m:54,926"),
            "expected cache breakdown in: {}",
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
    fn extract_tool_detail_generic_prompt_fallback_is_single_line() {
        let input = r#"{"prompt":"1行目\n2行目"}"#;
        assert_eq!(
            extract_tool_detail("UnknownPromptTool", input),
            "1行目 2行目"
        );
    }

    #[test]
    fn process_system_task_started_shows_description() {
        // 実データに出る task_started は開始通知として表示する
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

        assert!(clean.contains("implement feature"));
        assert!(clean.contains("in_process_teammate"));
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
    fn process_result_shows_model_and_web_search_usage() {
        let input = [
            r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"s1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","id":"msg_1","usage":{"input_tokens":12,"cache_creation_input_tokens":44,"cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":14}}}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":12,"output_tokens":7,"cache_read_input_tokens":20,"server_tool_use":{"web_search_requests":2}}}}"#,
            r#"{"type":"result","total_cost_usd":0.0101,"duration_ms":1000}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("model claude-opus-4-6"), "got: {}", clean);
        assert!(
            clean.contains("cache read:20 write5m:30 write1h:14"),
            "got: {}",
            clean
        );
        assert!(clean.contains("web search:2"), "got: {}", clean);
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
    fn format_tool_diff_edit_accepts_new_str_alias() {
        let input =
            r#"{"file_path":"/src/main.rs","old_string":"let x = 1;","new_str":"let x = 2;"}"#;
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

    #[test]
    fn process_task_progress_shows_subagent_progress() {
        let input = [
            r#"{"type":"system","subtype":"task_progress","task_id":"abc","tool_use_id":"tu1","description":"Running List all files","usage":{"total_tokens":5000,"tool_uses":1,"duration_ms":4823},"last_tool_name":"Bash"}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("\u{1f504} Running List all files (Bash)"),
            "expected task progress in: {}",
            clean
        );
    }

    #[test]
    fn process_task_notification_completed() {
        let input = [
            r#"{"type":"system","subtype":"task_notification","task_id":"abc","tool_use_id":"tu1","status":"completed","summary":"コードベースの徹底レビュー","usage":{"total_tokens":141902,"tool_uses":47,"duration_ms":158066}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("\u{2705}"),
            "expected completion mark in: {}",
            clean
        );
        assert!(
            clean.contains("コードベースの徹底レビュー"),
            "expected summary in: {}",
            clean
        );
        assert!(clean.contains("2m 38s"), "expected duration in: {}", clean);
        assert!(
            clean.contains("141,902 tokens"),
            "expected token count in: {}",
            clean
        );
    }

    #[test]
    fn process_task_notification_failed() {
        let input = [
            r#"{"type":"system","subtype":"task_notification","task_id":"abc","tool_use_id":"tu1","status":"failed","summary":"","usage":{"total_tokens":500,"tool_uses":1,"duration_ms":5000}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("\u{274c} Task failed"),
            "expected failure mark in: {}",
            clean
        );
    }

    #[test]
    fn process_task_notification_stopped_with_summary() {
        // TaskStop 経由で停止された場合: summary 付きで表示する
        let input = r#"{"type":"system","subtype":"task_notification","task_id":"bnrpvucd1","tool_use_id":"tu1","status":"stopped","output_file":"","summary":"Codex review completion (output file growth)","usage":{"total_tokens":1000,"tool_uses":2,"duration_ms":12000}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("\u{23f9} Task stopped:"),
            "expected stop mark in: {}",
            clean
        );
        assert!(
            clean.contains("Codex review completion"),
            "expected summary in: {}",
            clean
        );
        assert!(clean.contains("0m 12s"), "expected duration in: {}", clean);
    }

    #[test]
    fn process_task_notification_stopped_without_summary() {
        // summary が無くても停止イベントは表示する
        let input = r#"{"type":"system","subtype":"task_notification","task_id":"x","tool_use_id":"t","status":"stopped","summary":"","usage":{"total_tokens":0,"tool_uses":0,"duration_ms":3000}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("\u{23f9} Task stopped"),
            "expected stop mark in: {}",
            clean
        );
        assert!(clean.contains("0m 3s"), "expected duration in: {}", clean);
    }

    #[test]
    fn process_task_updated_completed_shows_status() {
        // 実データに出る task_updated は task_notification とは別の完了通知として表示する
        let input = r#"{"type":"system","subtype":"task_updated","task_id":"abc","patch":{"status":"completed","end_time":1776959941297}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("\u{2705} Task completed"),
            "expected task_updated completion in: {}",
            clean
        );
    }

    #[test]
    fn process_system_notification_shows_text_and_key() {
        // stop hook などの即時通知は無視せずエラーとして見えるようにする
        let input = r#"{"type":"system","subtype":"notification","key":"stop-hook-error","text":"Stop hook error occurred","priority":"immediate"}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("Notification: Stop hook error occurred"),
            "expected notification text in: {}",
            clean
        );
        assert!(
            clean.contains("stop-hook-error"),
            "expected notification key in: {}",
            clean
        );
    }

    #[test]
    fn extract_tool_detail_agent_with_description() {
        let input = r#"{"description":"research codebase","prompt":"Investigate..."}"#;
        assert_eq!(extract_tool_detail("Agent", input), "research codebase");
    }

    #[test]
    fn extract_tool_detail_agent_with_name_and_type() {
        let input = r#"{"description":"do stuff","name":"worker-1","subagent_type":"Explore"}"#;
        assert_eq!(extract_tool_detail("Agent", input), "worker-1 (Explore)");
    }

    #[test]
    fn process_result_with_web_fetch_requests() {
        let input = [
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5,"server_tool_use":{"web_search_requests":1,"web_fetch_requests":3}}}}"#,
            r#"{"type":"result","total_cost_usd":0.01,"duration_ms":1000}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(
            clean.contains("web search:1 fetch:3"),
            "expected web search and fetch in: {}",
            clean
        );
    }

    #[test]
    fn process_hook_events_are_silently_ignored() {
        let input = [
            r#"{"type":"system","subtype":"hook_started","hook_id":"h1","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"system","subtype":"hook_response","hook_id":"h1","hook_name":"SessionStart:startup","exit_code":0}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OK"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(!clean.contains("hook"), "hook events should be silent");
        assert!(clean.contains("OK"), "text should still appear");
    }

    #[test]
    fn process_schedule_wakeup_shows_detail_from_partial_json() {
        let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"ScheduleWakeup","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"delaySeconds\":90,\"reason\":\"codex レビュー結果を待つため少し待機\",\"prompt\":\"codex レビューの結果を確認して、テスト追加に進む\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

        let output = run_process(&input);
        let clean = strip_ansi(&output);

        assert!(clean.contains("ScheduleWakeup"));
        assert!(clean.contains("90s (codex レビュー結果を待つため少し待機)"));
    }

    #[test]
    fn extract_tool_detail_write_shows_file_and_line_count() {
        let input = r#"{"file_path":"/src/new.ts","content":"line1\nline2\nline3"}"#;
        let result = extract_tool_detail("Write", input);
        assert!(result.contains("/src/new.ts"));
        assert!(result.contains("3 lines"));
    }

    #[test]
    fn extract_tool_detail_skill_with_args() {
        let input = r#"{"skill":"codex","args":"コードレビューして"}"#;
        let result = extract_tool_detail("Skill", input);
        assert_eq!(result, "codex (コードレビューして)");
    }

    #[test]
    fn extract_tool_detail_skill_without_args() {
        let input = r#"{"skill":"commit"}"#;
        let result = extract_tool_detail("Skill", input);
        assert_eq!(result, "commit");
    }

    #[test]
    fn extract_tool_detail_todo_write_shows_progress() {
        let input = r#"{"todos":[{"content":"task1","status":"completed","activeForm":"t1"},{"content":"task2","status":"in_progress","activeForm":"t2"},{"content":"task3","status":"pending","activeForm":"t3"}]}"#;
        let result = extract_tool_detail("TodoWrite", input);
        assert_eq!(result, "1/3 completed");
    }

    #[test]
    fn process_result_with_model_usage_breakdown() {
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.5,"duration_ms":60000,"modelUsage":{"claude-opus-4-6[1m]":{"inputTokens":50000,"outputTokens":10000,"costUSD":1.2},"claude-haiku-4-5-20251001":{"inputTokens":5000,"outputTokens":2000,"costUSD":0.3}}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("claude-opus-4-6[1m]"),
            "モデル名が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("$1.2000"),
            "モデル別コストが表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("claude-haiku"),
            "Haikuモデルも表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_result_with_model_usage_cache_and_web() {
        // modelUsage に cacheCreationInputTokens と webSearchRequests が含まれる場合
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":2.0,"duration_ms":120000,"modelUsage":{"claude-opus-4-6[1m]":{"inputTokens":80000,"outputTokens":15000,"costUSD":2.0,"cacheCreationInputTokens":50000,"webSearchRequests":3}}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("cache_write:50,000"),
            "キャッシュ書き込みトークンが表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("web:3"),
            "Web検索回数が表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_result_with_fast_mode_on() {
        // fast_mode_state が "on" の場合は表示される
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"duration_ms":30000,"fast_mode_state":"on"}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("fast_mode on"),
            "fast_mode が表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_result_with_fast_mode_off() {
        // fast_mode_state が "off" の場合は表示されない
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"duration_ms":30000,"fast_mode_state":"off"}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            !clean.contains("fast_mode"),
            "fast_mode off は表示されるべきでない: {}",
            clean
        );
    }

    #[test]
    fn process_result_without_fast_mode() {
        // fast_mode_state フィールドがない場合も表示されない
        let input =
            r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"duration_ms":30000}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            !clean.contains("fast_mode"),
            "fast_mode フィールドがない場合は表示されるべきでない: {}",
            clean
        );
    }

    #[test]
    fn process_result_model_usage_without_extras() {
        // modelUsage に cacheCreationInputTokens や webSearchRequests がない場合
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.0,"duration_ms":60000,"modelUsage":{"claude-opus-4-6":{"inputTokens":30000,"outputTokens":5000,"costUSD":1.0}}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("claude-opus-4-6"),
            "モデル名が表示されるべき: {}",
            clean
        );
        assert!(
            !clean.contains("cache_write"),
            "キャッシュ情報がない場合は表示されるべきでない: {}",
            clean
        );
        assert!(
            !clean.contains("web:"),
            "Web検索情報がない場合は表示されるべきでない: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_event_ignored() {
        // rate_limit_event は無視される
        let input = r#"{"type":"rate_limit_event","limits":{"input_tokens":{"limit":100000,"remaining":50000}}}"#;
        let output = run_process(input);
        assert!(output.is_empty(), "rate_limit_event は出力されるべきでない");
    }

    #[test]
    fn cache_write_5m_returns_zero_when_only_1h_exists() {
        // 1hキャッシュのみ存在する場合、5mは0を返す（二重表示の防止）
        let usage = UsageSummary {
            cache_creation_input_tokens: 20000,
            cache_creation_1h_input_tokens: 20000,
            ..Default::default()
        };
        assert_eq!(usage.cache_write_5m_tokens(), 0);
    }

    #[test]
    fn cache_write_5m_returns_5m_when_both_exist() {
        // 5mと1hの両方が存在する場合、5mの値を返す
        let usage = UsageSummary {
            cache_creation_input_tokens: 15000,
            cache_creation_5m_input_tokens: 5000,
            cache_creation_1h_input_tokens: 10000,
            ..Default::default()
        };
        assert_eq!(usage.cache_write_5m_tokens(), 5000);
    }

    #[test]
    fn cache_write_5m_fallback_when_no_breakdown() {
        // 内訳が存在しない場合は合計値をフォールバック
        let usage = UsageSummary {
            cache_creation_input_tokens: 8000,
            ..Default::default()
        };
        assert_eq!(usage.cache_write_5m_tokens(), 8000);
    }

    #[test]
    fn process_result_with_model_usage_cache_read() {
        // modelUsage に cacheReadInputTokens が含まれる場合の表示
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":3.0,"duration_ms":180000,"modelUsage":{"claude-opus-4-6[1m]":{"inputTokens":100,"outputTokens":20000,"costUSD":3.0,"cacheReadInputTokens":5000000,"cacheCreationInputTokens":80000}}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("cache_read:5,000,000"),
            "cacheReadInputTokens が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("cache_write:80,000"),
            "cacheCreationInputTokens も表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_result_with_duration_api_ms() {
        // duration_api_ms が含まれる場合 api:Xm Ys が表示される
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.0,"duration_ms":600000,"duration_api_ms":900000,"num_turns":50}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("api:15m 0s"),
            "duration_api_ms が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("10m 0s"),
            "duration_ms も表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_result_without_duration_api_ms() {
        // duration_api_ms がない場合は api: が表示されない
        let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"duration_ms":120000,"num_turns":10}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            !clean.contains("api:"),
            "duration_api_ms がない場合は api: は表示されるべきでない: {}",
            clean
        );
    }

    #[test]
    fn process_result_only_1h_cache_no_write5m() {
        // 1hキャッシュのみの場合、write5m が表示されずに write1h のみ表示される
        let lines = [
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","id":"msg_01","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":5000,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2000}}}}}"#,
            r#"{"type":"result","subtype":"success","total_cost_usd":0.1,"duration_ms":5000}"#,
        ];
        let input = lines.join("\n");
        let output = run_process(&input);
        let clean = strip_ansi(&output);
        assert!(
            !clean.contains("write5m:"),
            "1hのみの場合 write5m は表示されるべきでない: {}",
            clean
        );
        assert!(
            clean.contains("write1h:2,000"),
            "write1h が表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_warning_shows_utilization() {
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.79}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("79%"), "使用率が表示されるべき: {}", clean);
        assert!(
            clean.contains("seven_day"),
            "制限タイプが表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_rejected_shows_error() {
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("rejected"),
            "拒否状態が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("five_hour"),
            "制限タイプが表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_allowed_is_silent() {
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"seven_day","utilization":0.5}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.is_empty(),
            "allowed は表示されるべきでない: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_allowed_with_details_is_shown() {
        // 実データにある overage 情報付き allowed は補足情報を表示する
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","resetsAt":1776009600,"overageStatus":"rejected","overageDisabledReason":"org_level_disabled_until","isUsingOverage":false}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("allowed"),
            "allowed 状態が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("five_hour"),
            "制限タイプが表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("overage:rejected"),
            "overage 状態が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("resets"),
            "リセット時刻が表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_auto_stop_touches_stop_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop_file = tmp.path().join("stop");
        // 95% >= threshold 95 → stop file が作成される
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.95}}"#;
        let output = run_process_with_opts(input, None, Some(&stop_file), 95);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("auto-stop"),
            "auto-stop メッセージが表示されるべき: {}",
            clean
        );
        assert!(clean.contains("95%"), "使用率が表示されるべき: {}", clean);
        assert!(stop_file.exists(), "stop file が作成されるべき");
    }

    #[test]
    fn process_rate_limit_below_threshold_no_stop_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop_file = tmp.path().join("stop");
        // 79% < threshold 95 → 通常の警告、stop file は作成されない
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.79}}"#;
        let output = run_process_with_opts(input, None, Some(&stop_file), 95);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("79%"),
            "通常の警告が表示されるべき: {}",
            clean
        );
        assert!(
            !clean.contains("auto-stop"),
            "auto-stop は表示されるべきでない: {}",
            clean
        );
        assert!(!stop_file.exists(), "stop file は作成されるべきでない");
    }

    #[test]
    fn process_rate_limit_rejected_touches_stop_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop_file = tmp.path().join("stop");
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#;
        let output = run_process_with_opts(input, None, Some(&stop_file), 95);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("rejected"),
            "拒否メッセージが表示されるべき: {}",
            clean
        );
        assert!(
            stop_file.exists(),
            "rejected 時に stop file が作成されるべき"
        );
    }

    #[test]
    fn process_rate_limit_custom_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop_file = tmp.path().join("stop");
        // 80% >= threshold 80 → auto-stop
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.80}}"#;
        let output = run_process_with_opts(input, None, Some(&stop_file), 80);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("auto-stop"),
            "カスタム閾値で auto-stop されるべき: {}",
            clean
        );
        assert!(clean.contains("80%"), "閾値が表示されるべき: {}", clean);
        assert!(stop_file.exists(), "stop file が作成されるべき");
    }

    #[test]
    fn process_rate_limit_no_stop_file_path_still_shows_message() {
        // stop_file が None でも閾値超過時のメッセージは表示される
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.96}}"#;
        let output = run_process_with_opts(input, None, None, 95);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("auto-stop"),
            "stop_file なしでも auto-stop メッセージは表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_warning_shows_resets_at() {
        // resetsAt タイムスタンプがローカル時刻で表示される
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.80,"resetsAt":1776009600}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("80%"), "使用率が表示されるべき: {}", clean);
        assert!(
            clean.contains("resets"),
            "リセット時刻が表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_rejected_shows_resets_at() {
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1776009600}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("rejected"),
            "拒否状態が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("resets"),
            "リセット時刻が表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_rate_limit_without_resets_at() {
        // resetsAt がない場合はリセット時刻を表示しない
        let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.79}}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(clean.contains("79%"), "使用率が表示されるべき: {}", clean);
        assert!(
            !clean.contains("resets"),
            "resetsAt がない場合はリセット時刻が表示されるべきでない: {}",
            clean
        );
    }

    #[test]
    fn process_api_retry_shows_attempt_info() {
        let input = r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":"server_error","error_status":503}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("1/10"),
            "試行回数が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("server_error"),
            "エラー種別が表示されるべき: {}",
            clean
        );
        assert!(
            clean.contains("503"),
            "HTTPステータスが表示されるべき: {}",
            clean
        );
    }

    #[test]
    fn process_api_retry_null_status() {
        let input = r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"error":"unknown","error_status":null}"#;
        let output = run_process(input);
        let clean = strip_ansi(&output);
        assert!(
            clean.contains("2/10"),
            "試行回数が表示されるべき: {}",
            clean
        );
        assert!(
            !clean.contains("("),
            "null ステータスは括弧なしで表示されるべき: {}",
            clean
        );
    }

    // --- format_resets_at の単体テスト ---

    #[test]
    fn format_resets_at_valid_timestamp() {
        let info: serde_json::Value = serde_json::from_str(r#"{"resetsAt":1776009600}"#).unwrap();
        let result = format_resets_at(&info);
        assert!(
            result.starts_with(" resets "),
            "有効なタイムスタンプはリセット時刻を返すべき: {}",
            result
        );
    }

    #[test]
    fn format_resets_at_missing_field() {
        let info: serde_json::Value = serde_json::from_str(r#"{"status":"allowed"}"#).unwrap();
        let result = format_resets_at(&info);
        assert_eq!(result, "", "resetsAt がない場合は空文字列を返すべき");
    }

    #[test]
    fn format_resets_at_null_value() {
        let info: serde_json::Value = serde_json::from_str(r#"{"resetsAt":null}"#).unwrap();
        let result = format_resets_at(&info);
        assert_eq!(result, "", "resetsAt が null の場合は空文字列を返すべき");
    }
}
