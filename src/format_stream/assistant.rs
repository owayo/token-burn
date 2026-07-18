//! `assistant` メッセージにだけ現れるツール ID と診断通知を処理するモジュール。

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::format_stream::util::{format_number, truncate_inline};

/// ツール ID の対応表を更新し、stream_event に現れないモデル切替と
/// キャッシュミス診断を重複排除して表示する。
pub(crate) fn handle_assistant_event(
    value: &serde_json::Value,
    out: &mut impl Write,
    tool_id_map: &mut HashMap<String, String>,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    let message = &value["message"];
    let message_id = message["id"].as_str().unwrap_or("");

    if let Some(content) = message["content"].as_array() {
        for item in content {
            if matches!(item["type"].as_str(), Some("tool_use" | "server_tool_use"))
                && let (Some(id), Some(name)) = (item["id"].as_str(), item["name"].as_str())
            {
                tool_id_map.insert(id.to_string(), name.to_string());
            }

            if item["type"].as_str() == Some("fallback") {
                write_model_fallback(item, message_id, out, shown_notices)?;
            }
        }
    }

    write_cache_miss(message, message_id, out, shown_notices)
}

fn write_model_fallback(
    item: &serde_json::Value,
    message_id: &str,
    out: &mut impl Write,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    let from = model_name(&item["from"]);
    let to = model_name(&item["to"]);
    if from.is_empty() && to.is_empty() {
        return Ok(());
    }

    let notice_key = format!("fallback:{message_id}:{from}:{to}");
    if !shown_notices.insert(notice_key) {
        return Ok(());
    }

    let from = if from.is_empty() { "?" } else { from };
    let to = if to.is_empty() { "?" } else { to };
    writeln!(
        out,
        "\x1b[33m  \u{21aa} Model fallback: {} \u{2192} {}\x1b[0m",
        truncate_inline(from, 40),
        truncate_inline(to, 40)
    )?;
    Ok(())
}

fn write_cache_miss(
    message: &serde_json::Value,
    message_id: &str,
    out: &mut impl Write,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    let reason = &message["diagnostics"]["cache_miss_reason"];
    let reason_type = reason["type"].as_str().unwrap_or("");
    let missed_tokens = reason["cache_missed_input_tokens"].as_u64();
    if reason_type.is_empty() && missed_tokens.is_none() {
        return Ok(());
    }

    let notice_key = format!("cache-miss:{message_id}:{reason_type}:{missed_tokens:?}");
    if !shown_notices.insert(notice_key) {
        return Ok(());
    }

    let reason_type = if reason_type.is_empty() {
        "unknown"
    } else {
        reason_type
    };
    if let Some(tokens) = missed_tokens {
        writeln!(
            out,
            "\x1b[33m  \u{26a0} Cache miss: {} ({} input tokens)\x1b[0m",
            truncate_inline(reason_type, 40),
            format_number(tokens)
        )?;
    } else {
        writeln!(
            out,
            "\x1b[33m  \u{26a0} Cache miss: {}\x1b[0m",
            truncate_inline(reason_type, 40)
        )?;
    }
    Ok(())
}

fn model_name(value: &serde_json::Value) -> &str {
    value["model"]
        .as_str()
        .or_else(|| value.as_str())
        .unwrap_or("")
}
