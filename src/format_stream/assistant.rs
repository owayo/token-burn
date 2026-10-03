//! `assistant` メッセージにだけ現れるツール ID と診断通知を処理するモジュール。

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::format_stream::blocks::{ContentBlockState, break_open_line, write_tool_use};
use crate::format_stream::util::{
    format_number, normalize_model_name, subagent_attribution, truncate_inline, truncate_str,
};

/// ツール ID の対応表を更新し、stream_event に現れないモデル切替と
/// キャッシュミス診断を重複排除して表示する。
pub(crate) fn handle_assistant_event(
    value: &serde_json::Value,
    out: &mut impl Write,
    tool_id_map: &mut HashMap<String, String>,
    shown_notices: &mut HashSet<String>,
    blocks: &mut HashMap<usize, ContentBlockState>,
    streamed_message_ids: &HashSet<String>,
) -> Result<()> {
    let message = &value["message"];
    let message_id = message["id"].as_str().unwrap_or("");

    // 通知はいったんバッファへ書く。assistant イベントは 1 セッションで数千件届くが
    // 大半は通知を伴わないため、実際に出力がある場合だけ開きっぱなしの
    // 思考/テキスト行を閉じる（毎回閉じると思考の進捗ドット表示が壊れる）。
    let mut notices: Vec<u8> = Vec::new();

    // 通常のメインループは `stream_event` の再送なので表示を重複させない。
    // API リトライ後には stream を伴わないメッセージも実在するため、その ID だけ補う。
    let owner = subagent_attribution(value);
    let from_subagent = !owner.is_empty() && !value["parent_tool_use_id"].is_null();
    let unstreamed_main = !from_subagent
        && value["parent_tool_use_id"].is_null()
        && blocks.is_empty()
        && !message_id.is_empty()
        && !streamed_message_ids.contains(message_id)
        && value["is_api_error_message"].as_bool() != Some(true);

    if let Some(content) = message["content"].as_array() {
        for (index, item) in content.iter().enumerate() {
            if matches!(item["type"].as_str(), Some("tool_use" | "server_tool_use"))
                && let (Some(id), Some(name)) = (item["id"].as_str(), item["name"].as_str())
            {
                let already_streamed = tool_id_map.contains_key(id);
                tool_id_map.insert(id.to_string(), name.to_string());
                if from_subagent || (unstreamed_main && !already_streamed) {
                    write_unstreamed_tool_use(item, id, name, &owner, &mut notices, shown_notices)?;
                }
            }

            if item["type"].as_str() == Some("text") {
                if value["is_api_error_message"].as_bool() == Some(true) {
                    write_api_error_message(
                        item,
                        message_id,
                        value["uuid"].as_str().unwrap_or(""),
                        index,
                        &owner,
                        &mut notices,
                        shown_notices,
                    )?;
                } else if from_subagent {
                    write_subagent_text(item, message_id, &owner, &mut notices, shown_notices)?;
                } else if unstreamed_main {
                    write_unstreamed_text(item, message_id, &mut notices, shown_notices)?;
                }
            }

            if item["type"].as_str() == Some("fallback") {
                write_model_fallback(item, message_id, &mut notices, shown_notices)?;
            }
        }
    }

    write_cache_miss(message, message_id, &mut notices, shown_notices)?;

    if !notices.is_empty() {
        break_open_line(out, blocks)?;
        out.write_all(&notices)?;
    }
    Ok(())
}

/// API エラーの本文は `assistant` にしか届かない場合がある。通常の本文は
/// `stream_event` と重複するため表示しないが、この印がある本文だけ独立行に出す。
fn write_api_error_message(
    item: &serde_json::Value,
    message_id: &str,
    event_id: &str,
    index: usize,
    owner: &str,
    out: &mut impl Write,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    let text = item["text"].as_str().unwrap_or("").trim();
    if text.is_empty() {
        return Ok(());
    }
    let id = if message_id.is_empty() {
        event_id
    } else {
        message_id
    };
    if !shown_notices.insert(format!("api-error:{id}:{index}")) {
        return Ok(());
    }
    writeln!(
        out,
        "\x1b[31m  \u{26a0} API error{}: {}\x1b[0m",
        owner,
        truncate_inline(text, 160)
    )?;
    Ok(())
}

/// サブエージェントとストリーム欠落時のツールを `🔧` 行として書く。
///
/// `stream_event` はメインループ専用で、実データ 98,061 件すべてが
/// `parent_tool_use_id: null` だった。サブエージェント内部のツール使用は
/// `assistant` イベントにしか現れないため、ここを素通りしていた頃は
/// ツール完了行の 44%（1,178 / 2,695 件）が `✓ Bash @<タスク名>` だけになり、
/// 何のコマンドを打ったのかがログから完全に消えていた。`tool_id_map` にはここで
/// 登録しているのでツール名だけは解決できていたが、入力（`input`）は
/// `finalize_block` → `extract_tool_detail` を通るストリーム経路にしか繋がって
/// いなかった。
///
/// `--include-partial-messages` は同一 message id の assistant イベントを複数回
/// 送るため、ツール使用 id で重複排除する（実データでは 1 回ずつしか現れないが、
/// 二重表示は完了行との対応を崩すので防いでおく）。
fn write_unstreamed_tool_use(
    item: &serde_json::Value,
    id: &str,
    name: &str,
    owner: &str,
    out: &mut impl Write,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    if !shown_notices.insert(format!("unstreamed-tool:{id}")) {
        return Ok(());
    }
    let input_json = item
        .get("input")
        .filter(|input| !input.is_null())
        .and_then(|input| serde_json::to_string(input).ok())
        .unwrap_or_default();
    write_tool_use(out, name, &input_json, owner)?;
    Ok(())
}

/// API リトライ後などにストリームが欠けた本文を、通常の本文と同じ形で補う。
fn write_unstreamed_text(
    item: &serde_json::Value,
    message_id: &str,
    out: &mut impl Write,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    let text = item["text"].as_str().unwrap_or("");
    if text.is_empty() || !shown_notices.insert(format!("unstreamed-text:{message_id}:{text}")) {
        return Ok(());
    }
    write!(out, "{text}")?;
    if !text.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

/// サブエージェントのテキスト出力を 1 行の要約として書く。
///
/// サブエージェントの最終レポート（実データで 179 ブロック / 363KB）はここにしか
/// 現れない。`task_notification.summary` は 60 文字に切られ、`✓ Agent` の完了行は
/// メタデータを持つため `tool_result_string_summary` のフォールバックにも乗らない。
/// 全文を流すと本文だけで 363KB になるので、ブロックごとに先頭 1 行へ畳む。
fn write_subagent_text(
    item: &serde_json::Value,
    message_id: &str,
    owner: &str,
    out: &mut impl Write,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    let text = item["text"].as_str().unwrap_or("").trim();
    if text.is_empty() {
        return Ok(());
    }
    let summary = truncate_str(text, 100);
    if !shown_notices.insert(format!("subagent-text:{message_id}:{summary}")) {
        return Ok(());
    }
    writeln!(out, "\x1b[2m  \u{1f4ac}{} {}\x1b[0m", owner, summary)?;
    Ok(())
}

fn write_model_fallback(
    item: &serde_json::Value,
    message_id: &str,
    out: &mut impl Write,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    let from = normalize_model_name(model_name(&item["from"]));
    let to = normalize_model_name(model_name(&item["to"]));
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
