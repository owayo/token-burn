//! `user` イベント（`tool_result` のツール完了行と、Claude Code が合成する
//! `isSynthetic` メッセージ）の表示を担うモジュール。
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
use crate::format_stream::util::{extract_tool_result_summary, truncate_inline};

/// 合成 user メッセージのうち、フック差し戻しを示す第 1 行の目印。
/// 実データは `"Stop hook feedback:\n⏱ Stop hook timed out after 120s: cargo"`。
/// claude 本体も `"<Event> hook feedback:"` の形（`Stop` 以外の hook_event も同形）で
/// 組み立てるため、接頭辞ではなくこのマーカーで判定する。
const HOOK_FEEDBACK_MARKER: &str = "hook feedback:";

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
        let owner = subagent_attribution(value);
        let metadata = build_metadata(value, id, is_error);

        if is_error {
            // エラー内容のサマリーがある場合は併記する
            let summary = extract_tool_result_summary(&item["content"]);
            if summary.is_empty() {
                writeln!(
                    out,
                    "\x1b[31m  \u{2717} {}{}{}\x1b[0m",
                    name, owner, metadata
                )?;
            } else {
                writeln!(
                    out,
                    "\x1b[31m  \u{2717} {}{} \u{2014} {}{}\x1b[0m",
                    name, owner, summary, metadata
                )?;
            }
        } else {
            writeln!(
                out,
                "\x1b[2m  \u{2713} {}{}{}\x1b[0m",
                name, owner, metadata
            )?;
        }
    }
    Ok(())
}

/// `isSynthetic` な user メッセージのうち、フックの差し戻し（hook feedback）を表示する。
///
/// Claude Code は Stop フックが exit 2 / タイムアウトで終わると、その内容を合成 user
/// メッセージとしてモデルへ差し戻す。実データの
/// `"Stop hook feedback:\n⏱ Stop hook timed out after 120s: cargo"` がこれで、
/// `system` の `hook_response` にも `notification` にも現れないため、従来は完全に
/// 表示から落ちていた。無人実行では Stop フックが自動コミット / push を担うので、
/// これが落ちると「仕事は終わったのにコミットされていない」理由をログから追えない。
///
/// 同じ `isSynthetic` でもスキル本文の注入（`"Base directory for this skill: ..."`）は
/// SKILL.md 全文を含んで巨大になり、内容も `Skill` ツール行と重複するため表示しない。
pub(crate) fn handle_synthetic_user_event(
    value: &serde_json::Value,
    out: &mut impl Write,
) -> Result<()> {
    if value["isSynthetic"].as_bool() != Some(true) {
        return Ok(());
    }
    let text = synthetic_text(&value["message"]["content"]);
    let Some((label, detail)) = split_hook_feedback(&text) else {
        return Ok(());
    };
    let label = if label.is_empty() {
        String::new()
    } else {
        format!(" ({})", truncate_inline(&label, 40))
    };
    writeln!(
        out,
        "\x1b[33m  \u{26a0} Hook feedback{}: {}\x1b[0m",
        label,
        truncate_inline(&detail, 100)
    )?;
    Ok(())
}

/// 合成 user メッセージの本文を取り出す。`content` は文字列と text ブロック配列の
/// 両形式が現れるため、どちらも連結して扱う。
fn synthetic_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter(|item| item["type"].as_str() != Some("tool_result"))
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// ASCII の大文字小文字だけ無視して `needle` の位置を `haystack` 自身のバイト位置で返す。
///
/// `str::to_lowercase()` の結果へ `find` した位置を元文字列のスライスに流用しては
/// いけない。小文字化はバイト長を保存せず、`İ`(U+0130, 2 バイト) は `i` + U+0307
/// (3 バイト) へ伸び、`K`(U+212A, 3 バイト) は `k` (1 バイト) へ縮む。伸びた側では
/// 範囲外スライス、縮んだ側では非文字境界スライスとなり、どちらも panic する。
/// `format-stream` はパイプの中段なので、panic はパイプを閉じて上流の `claude` を
/// SIGPIPE で道連れにし、数時間の実行を巻き添えにする。
///
/// `needle` は ASCII のみ（`HOOK_FEEDBACK_MARKER`）を前提とする。UTF-8 では ASCII
/// バイトが多バイト列の内部に現れないため、返す位置は必ず文字境界になる。
fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    debug_assert!(needle.is_ascii(), "needle は ASCII のみを前提とする");
    if needle.is_empty() {
        return Some(0);
    }
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// hook feedback 本文を「どのフックか」と「差し戻された内容」に分ける。
/// 第 1 行が `<Event> hook feedback:` でなければ `None`（＝表示対象外）。
/// 第 2 行以降が無い場合はラベルだけが情報なので、内容もラベルで埋める。
fn split_hook_feedback(text: &str) -> Option<(String, String)> {
    let mut lines = text.lines();
    let head = lines.next()?.trim();
    let marker_at = find_ascii_case_insensitive(head, HOOK_FEEDBACK_MARKER)?;
    // マーカー前がフック名（`Stop` / `PostToolUse:Bash` 等）。前置きが無い形式も許す。
    let label = head[..marker_at].trim().to_string();
    // マーカー直後に同じ行で内容が続く形式（`... feedback: <内容>`）も拾う。
    let inline = head[marker_at + HOOK_FEEDBACK_MARKER.len()..].trim();
    let detail = if inline.is_empty() {
        lines
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .to_string()
    } else {
        inline.to_string()
    };
    let detail = if detail.is_empty() {
        head.to_string()
    } else {
        detail
    };
    Some((label, detail))
}

/// サブエージェント由来のツール完了行に付ける帰属表示（` @<タスク名>`）を組み立てる。
///
/// サブエージェント内で走ったツールの結果は、メインループの出力と同じストリームへ
/// user イベントとしてインラインに混ざる。実ログでは 22 個のサブエージェントが並列で
/// 動き、ツール完了行の 69%（2,309 / 3,350 件）がサブエージェント由来だった。帰属が
/// 無いと画面が `✓ Bash` の羅列になり、どの `✓ Bash` が誰の作業かを後から追えない。
///
/// `task_description` は Agent 起動時の description（実ログの「PHP 版比較のバグ修正」
/// 等）で、並列タスクを一意に識別できる唯一の手掛かりのため優先する。サブエージェント
/// 内部からの結果でも description を持たない形式に備えて `subagent_type` へ落とす。
/// どちらも無いメインループの結果では空文字を返し、従来どおりの表示を保つ。
fn subagent_attribution(value: &serde_json::Value) -> String {
    for key in ["task_description", "subagent_type"] {
        let owner = value[key].as_str().unwrap_or("").trim();
        if !owner.is_empty() {
            return format!(" @{}", truncate_inline(owner, 40));
        }
    }
    String::new()
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
