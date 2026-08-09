//! `user` イベントに含まれる `tool_result`（ツール完了行）の表示を担うモジュール。
//!
//! 他のイベント種別（`system` / `stream_event` / `assistant` / `result` /
//! `rate_limit_event` / `tool_progress`）と同じく、1 イベント 1 モジュールの
//! 構成に揃えるため `mod.rs` の `process` から切り出している。

use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;

use crate::format_stream::tools::metadata::{
    tool_result_meta_metadata, tool_result_metadata, tool_result_string_summary,
};
use crate::format_stream::util::extract_tool_result_summary;

/// ツール結果を受け取り、完了したツール名とメタデータを 1 行で表示する。
///
/// `tool_use_result` / `tool_result_meta` は `tool_result` ブロックではなく
/// user イベントの top-level に入るため、イベント全体 (`value`) を受け取る。
pub(crate) fn handle_tool_result_event(
    value: &serde_json::Value,
    out: &mut impl Write,
    tool_id_map: &HashMap<String, String>,
) -> Result<()> {
    let Some(content) = value["message"]["content"].as_array() else {
        return Ok(());
    };

    for item in content {
        if item["type"].as_str() != Some("tool_result") {
            continue;
        }
        let id = item["tool_use_id"].as_str().unwrap_or("");
        let name = tool_id_map.get(id).map(|s| s.as_str()).unwrap_or("?");
        let is_error = item["is_error"].as_bool().unwrap_or(false);
        let metadata = build_metadata(value, id, is_error);

        if is_error {
            // エラー内容のサマリーがある場合は併記する
            let summary = extract_tool_result_summary(&item["content"]);
            if summary.is_empty() {
                writeln!(out, "\x1b[31m  \u{2717} {}{}\x1b[0m", name, metadata)?;
            } else {
                writeln!(
                    out,
                    "\x1b[31m  \u{2717} {} \u{2014} {}{}\x1b[0m",
                    name, summary, metadata
                )?;
            }
        } else {
            writeln!(out, "\x1b[2m  \u{2713} {}{}\x1b[0m", name, metadata)?;
        }
    }
    Ok(())
}

/// 完了行に付ける `[...]` 形式の補足を組み立てる。補足が無ければ空文字を返す。
fn build_metadata(value: &serde_json::Value, tool_use_id: &str, is_error: bool) -> String {
    let mut metadata = tool_result_metadata(&value["tool_use_result"]);
    let result_meta = tool_result_meta_metadata(&value["tool_result_meta"], tool_use_id);
    if !result_meta.is_empty() {
        if !metadata.is_empty() {
            metadata.push_str(", ");
        }
        metadata.push_str(&result_meta);
    }
    // tool_use_result が文字列や text ブロック配列の応答（MCP ツール等）には
    // object メタデータが無い。
    // エラー時は content 側のサマリーと同文になるため、成功時のみ result: として
    // 先頭行を補足する。
    if metadata.is_empty()
        && !is_error
        && let Some(text_summary) = tool_result_string_summary(&value["tool_use_result"])
    {
        metadata = text_summary;
    }
    if metadata.is_empty() {
        String::new()
    } else {
        format!(" [{}]", metadata)
    }
}
