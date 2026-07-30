//! `system` イベント（サブエージェント進捗・通知・完了通知・API リトライ・モデル
//! フォールバック・フック診断）の表示を担うモジュール。`handle_system_event` は
//! subtype ごとに `write_*` ヘルパーへディスパッチする薄い入口。

use anyhow::Result;
use std::io::Write;

use crate::format_stream::util::{
    first_non_empty_string, format_number, truncate_inline, truncate_str,
};

/// system イベントのうち、サブエージェント進捗・通知・完了通知を表示する。
pub(crate) fn handle_system_event(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let subtype = v["subtype"].as_str().unwrap_or("");
    match subtype {
        "init" => write_session_init(v, out)?,
        "task_started" => write_task_started(v, out)?,
        "task_progress" => write_task_progress(v, out)?,
        "task_notification" => write_task_notification(v, out)?,
        "task_updated" => write_task_updated(v, out)?,
        "notification" => write_notification(v, out)?,
        "api_retry" => write_api_retry(v, out)?,
        "model_refusal_fallback" => write_model_refusal_fallback(v, out)?,
        "hook_progress" | "hook_response" => {
            handle_hook_output(v, out)?;
        }
        "status" | "thinking_tokens" => {
            // 実データで高頻度に出る内部ステータス系イベントは表示しない。
            // status:requesting はリクエスト状態の通知、thinking_tokens はセッション
            // 累積の推定トークン数（estimated_tokens / estimated_tokens_delta）であり、
            // いずれも表示するとノイズになる。思考中の進捗は thinking_delta のドット
            // 表示、正確なトークン総数は result.usage の集計表示に委ねる。
        }
        "background_tasks_changed" => {
            // 実行中バックグラウンドタスク一覧のスナップショット通知。タスクの増減の
            // たびに発火する（実データで 1 セッション十数回）が、個々の開始・完了は
            // task_started / task_notification で既に表示しており、重複表示は
            // ノイズになるため明示的に無視する。
        }
        _ => {} // hook_started 等は無視
    }
    Ok(())
}

/// `init`: セッション開始時のモデル・CLI バージョン・権限モードを 1 行で表示する。
///
/// これらはログ中の他のどのイベントにも現れない。`result.modelUsage` からは
/// 実際に課金されたモデルしか分からず、CLI のバージョンと権限モード
/// （`bypassPermissions` で走ったのか）は完全に失われていた。セッションにつき
/// 1 行だけなのでノイズにならない。
fn write_session_init(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let model = v["model"].as_str().unwrap_or("");
    let mut attrs = Vec::new();
    if let Some(version) = v["claude_code_version"]
        .as_str()
        .filter(|version| !version.is_empty())
    {
        attrs.push(format!("v{}", truncate_inline(version, 20)));
    }
    if let Some(mode) = v["permissionMode"].as_str().filter(|mode| !mode.is_empty()) {
        attrs.push(truncate_inline(mode, 24));
    }
    if model.is_empty() && attrs.is_empty() {
        return Ok(());
    }

    let model = if model.is_empty() {
        "?".to_string()
    } else {
        truncate_inline(model, 40)
    };
    if attrs.is_empty() {
        writeln!(out, "\x1b[2m  \u{2139} Session {}\x1b[0m", model)?;
    } else {
        writeln!(
            out,
            "\x1b[2m  \u{2139} Session {} ({})\x1b[0m",
            model,
            attrs.join(", ")
        )?;
    }
    Ok(())
}

/// `model_refusal_fallback`: 拒否後に再試行するモデルと拒否カテゴリを表示する。
///
/// `content` と `api_refusal_explanation` は拒否された本文や長いポリシー説明を含むため、
/// 出力せず、モデル名と構造化カテゴリだけを表示する。
fn write_model_refusal_fallback(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let original = v["original_model"].as_str().unwrap_or("");
    let fallback = v["fallback_model"].as_str().unwrap_or("");
    if original.is_empty() && fallback.is_empty() {
        return Ok(());
    }

    let original = if original.is_empty() { "?" } else { original };
    let fallback = if fallback.is_empty() { "?" } else { fallback };
    let category = v["api_refusal_category"].as_str().unwrap_or("");
    let category = if category.is_empty() {
        String::new()
    } else {
        format!(" (category:{})", truncate_inline(category, 30))
    };
    writeln!(
        out,
        "\x1b[33m  \u{21aa} Model refusal fallback: {} \u{2192} {}{}\x1b[0m",
        truncate_inline(original, 40),
        truncate_inline(fallback, 40),
        category
    )?;
    Ok(())
}

/// `task_started`: サブエージェント開始の説明と具体的なエージェント種別を表示する。
fn write_task_started(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let desc = v["description"].as_str().unwrap_or("");
    let task_type = v["task_type"].as_str().unwrap_or("");
    let subagent_type = v["subagent_type"].as_str().unwrap_or("");
    // local_agent のような実行方式より general-purpose / Explore などの具体的な
    // サブエージェント種別を優先する。local_bash には subagent_type が無いため、
    // 従来どおり task_type を表示する。
    let display_type = if subagent_type.is_empty() {
        task_type
    } else {
        subagent_type
    };
    if !desc.is_empty() {
        if !display_type.is_empty() {
            writeln!(
                out,
                "\x1b[2m  \u{23f3} {} ({})\x1b[0m",
                truncate_str(desc, 80),
                display_type
            )?;
        } else {
            writeln!(out, "\x1b[2m  \u{23f3} {}\x1b[0m", truncate_str(desc, 80))?;
        }
    }
    Ok(())
}

/// `task_progress`: サブエージェント進捗の説明と最後のツール名を表示する。
fn write_task_progress(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
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
    Ok(())
}

/// `task_notification`: completed / failed / stopped を usage 付きで表示する。
fn write_task_notification(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let status = v["status"].as_str().unwrap_or("");
    let summary = v["summary"].as_str().unwrap_or("");
    let usage_attrs = task_notification_usage_attrs(&v["usage"]);
    let usage_text = if usage_attrs.is_empty() {
        String::new()
    } else {
        format!(" ({})", usage_attrs.join(", "))
    };
    if status == "completed" {
        if !summary.is_empty() {
            writeln!(
                out,
                "\x1b[32m  \u{2705} {}{}\x1b[0m",
                truncate_str(summary, 60),
                usage_text
            )?;
        } else {
            writeln!(
                out,
                "\x1b[32m  \u{2705} Task completed{}\x1b[0m",
                usage_text
            )?;
        }
    } else if status == "failed" {
        if !summary.is_empty() {
            writeln!(
                out,
                "\x1b[31m  \u{274c} Task failed: {}{}\x1b[0m",
                truncate_str(summary, 100),
                usage_text
            )?;
        } else {
            writeln!(out, "\x1b[31m  \u{274c} Task failed{}\x1b[0m", usage_text)?;
        }
    } else if status == "stopped" {
        // TaskStop で停止された場合
        if !summary.is_empty() {
            writeln!(
                out,
                "\x1b[33m  \u{23f9} Task stopped: {}{}\x1b[0m",
                truncate_str(summary, 60),
                usage_text
            )?;
        } else {
            writeln!(out, "\x1b[33m  \u{23f9} Task stopped{}\x1b[0m", usage_text)?;
        }
    }
    Ok(())
}

/// `task_updated`: patch.status の状態遷移を表示する。
fn write_task_updated(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let status = v["patch"]["status"].as_str().unwrap_or("");
    match status {
        "completed" => {
            writeln!(out, "\x1b[32m  \u{2705} Task completed\x1b[0m")?;
        }
        "failed" | "cancelled" | "killed" => {
            writeln!(out, "\x1b[31m  \u{274c} Task {}\x1b[0m", status)?;
        }
        status if !status.is_empty() => {
            writeln!(out, "\x1b[2m  \u{2139} Task {}\x1b[0m", status)?;
        }
        _ => {}
    }
    Ok(())
}

/// `notification`: Claude Code のシステム通知（stop hook エラー等）を表示する。
fn write_notification(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
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
    Ok(())
}

/// `api_retry`: API リトライの試行回数とエラー情報を表示する。
fn write_api_retry(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let attempt = v["attempt"].as_u64().unwrap_or(0);
    let max_retries = v["max_retries"].as_u64().unwrap_or(0);
    let error = v["error"].as_str().unwrap_or("");
    let status = v["error_status"]
        .as_u64()
        .map(|s| format!(" ({})", s))
        .unwrap_or_default();
    // 実データには error フィールドの無い api_retry がある。"unknown" を補うと
    // それ自体がエラー名のように見えて紛らわしいため、試行回数と status だけ出す。
    if error.is_empty() || error == "unknown" {
        writeln!(
            out,
            "\x1b[33m  \u{26a0} API retry {}/{}{}\x1b[0m",
            attempt, max_retries, status
        )?;
    } else {
        writeln!(
            out,
            "\x1b[33m  \u{26a0} API retry {}/{}: {}{}\x1b[0m",
            attempt, max_retries, error, status
        )?;
    }
    Ok(())
}

/// `task_notification.usage` が実データに無い場合は、未提供の値を 0 として表示しない。
fn task_notification_usage_attrs(usage: &serde_json::Value) -> Vec<String> {
    let mut attrs = Vec::new();
    if let Some(duration_ms) = usage["duration_ms"].as_u64() {
        let dur_s = duration_ms / 1000;
        attrs.push(format!("{}m {}s", dur_s / 60, dur_s % 60));
    }
    if let Some(tokens) = usage["total_tokens"].as_u64() {
        attrs.push(format!("{} tokens", format_number(tokens)));
    }
    attrs
}

/// フックの stderr/output がある場合だけ表示する。
/// 成功して何も出力していない通常フックはノイズになるため表示しない。
///
/// 実データの `hook_response` は `output` / `stdout` / `stderr` を常に持ち、
/// 失敗時は stderr にだけ内容が入る。空文字を飛ばして次の候補へ進まないと
/// フック失敗の診断が最も欲しい場面で "no output" にしかならない。
fn handle_hook_output(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let detail = first_non_empty_string(v, &["output", "stderr", "stdout"]);
    let outcome = v["outcome"].as_str().unwrap_or("");
    let exit_code = v["exit_code"].as_i64();
    let has_failure =
        outcome != "success" && !outcome.is_empty() || exit_code.is_some_and(|code| code != 0);

    if detail.is_empty() && !has_failure {
        return Ok(());
    }

    let hook = first_non_empty_string(v, &["hook_name", "hook_event"]);
    let hook = if hook.is_empty() { "hook" } else { hook };
    let mut attrs = Vec::new();
    if !outcome.is_empty() {
        attrs.push(format!("outcome:{outcome}"));
    }
    if let Some(code) = exit_code {
        attrs.push(format!("exit:{code}"));
    }
    let attr_text = if attrs.is_empty() {
        String::new()
    } else {
        format!(" ({})", attrs.join(", "))
    };
    let message = if detail.is_empty() {
        "no output".to_string()
    } else {
        truncate_inline(detail, 100)
    };
    let color = if has_failure { "\x1b[31m" } else { "\x1b[33m" };
    writeln!(
        out,
        "{}  \u{26a0} Hook {}{}: {}\x1b[0m",
        color, hook, attr_text, message
    )?;
    Ok(())
}
