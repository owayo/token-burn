//! `assistant` メッセージにだけ現れるツール ID と診断通知を処理するモジュール。

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::format_stream::blocks::{ContentBlockState, break_open_line};
use crate::format_stream::diff::format_tool_diff;
use crate::format_stream::tools::detail::extract_tool_detail;
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
) -> Result<()> {
    let message = &value["message"];
    let message_id = message["id"].as_str().unwrap_or("");

    // 通知はいったんバッファへ書く。assistant イベントは 1 セッションで数千件届くが
    // 大半は通知を伴わないため、実際に出力がある場合だけ開きっぱなしの
    // 思考/テキスト行を閉じる（毎回閉じると思考の進捗ドット表示が壊れる）。
    let mut notices: Vec<u8> = Vec::new();

    // サブエージェント発のイベントかどうか。メインループの assistant は
    // `stream_event` と同じ内容を再送するだけなので、本文・ツール使用を書くと
    // 1 ツールにつき 2 行出てしまう。
    let owner = subagent_attribution(value);
    let from_subagent = !owner.is_empty() && !value["parent_tool_use_id"].is_null();

    if let Some(content) = message["content"].as_array() {
        for item in content {
            if matches!(item["type"].as_str(), Some("tool_use" | "server_tool_use"))
                && let (Some(id), Some(name)) = (item["id"].as_str(), item["name"].as_str())
            {
                tool_id_map.insert(id.to_string(), name.to_string());
                if from_subagent {
                    write_subagent_tool_use(item, id, name, &owner, &mut notices, shown_notices)?;
                }
            }

            if from_subagent && item["type"].as_str() == Some("text") {
                write_subagent_text(item, message_id, &owner, &mut notices, shown_notices)?;
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

/// サブエージェントが使ったツールを、メインループと同じ `🔧` 行として書く。
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
fn write_subagent_tool_use(
    item: &serde_json::Value,
    id: &str,
    name: &str,
    owner: &str,
    out: &mut impl Write,
    shown_notices: &mut HashSet<String>,
) -> Result<()> {
    if !shown_notices.insert(format!("subagent-tool:{id}")) {
        return Ok(());
    }
    let input_json = item
        .get("input")
        .filter(|input| !input.is_null())
        .and_then(|input| serde_json::to_string(input).ok())
        .unwrap_or_default();
    let detail = extract_tool_detail(name, &input_json);
    if detail.is_empty() {
        writeln!(out, "\x1b[36m\u{1f527} {}{}\x1b[0m", name, owner)?;
    } else {
        writeln!(
            out,
            "\x1b[36m\u{1f527} {}{}\x1b[0m \x1b[2m{}\x1b[0m",
            name, owner, detail
        )?;
    }
    // Edit の差分もメインループと同じく出す。実データのサブエージェント Edit は
    // 7 セッションで 3 件しかなく、出力量は増えない一方、変更内容そのものは
    // 他のどの行にも現れない。
    if let Some(diff) = format_tool_diff(name, &input_json) {
        write!(out, "{}", diff)?;
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
