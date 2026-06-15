//! `tool_use_result` の top-level メタデータから、見落とすと判断材料を失うものだけ
//! 短い補足文字列として組み立てるモジュール。

use crate::format_stream::util::{
    format_byte_size, format_epoch_millis_clock, format_millis_as_seconds, format_number,
    truncate_inline,
};

/// `tool_use_result` の補足情報から、見落とすと判断材料を失うものだけ短く表示する。
pub(crate) fn tool_result_metadata(result: &serde_json::Value) -> String {
    let Some(obj) = result.as_object() else {
        return String::new();
    };

    let mut attrs = Vec::new();
    if obj
        .get("truncated")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        attrs.push("truncated".to_string());
    }
    if obj
        .get("interrupted")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        attrs.push("interrupted".to_string());
    }
    if obj
        .get("success")
        .and_then(|value| value.as_bool())
        .is_some_and(|success| !success)
    {
        attrs.push("failed".to_string());
    }
    // Bash 経由の git commit など、git 操作が完了した事実は進捗上の重要なマイルストーン。
    // sha は短縮済み(7桁)、kind は committed / amended 等。
    if let Some(commit) = obj
        .get("gitOperation")
        .and_then(|value| value.get("commit"))
        .and_then(|value| value.as_object())
    {
        let sha = commit
            .get("sha")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let kind = commit
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !sha.is_empty() {
            if kind.is_empty() {
                attrs.push(format!("commit:{sha}"));
            } else {
                attrs.push(format!("commit:{sha} {kind}"));
            }
        }
    }
    if let Some(limit) = obj.get("appliedLimit").and_then(|value| value.as_u64()) {
        attrs.push(format!("limit:{limit}"));
    }
    if let Some(files) = obj.get("numFiles").and_then(|value| value.as_u64()) {
        attrs.push(format!("files:{files}"));
    }
    if let Some(lines) = obj.get("numLines").and_then(|value| value.as_u64()) {
        attrs.push(format!("lines:{lines}"));
    }
    // Read は読み取り結果を file オブジェクトに格納する。部分読み取り（limit 指定や
    // ファイル途中までの読み取り）では numLines < totalLines となり、切り詰めの判断材料になる。
    if let Some(file) = obj.get("file").and_then(|value| value.as_object())
        && let Some(num_lines) = file.get("numLines").and_then(|value| value.as_u64())
        && let Some(total_lines) = file.get("totalLines").and_then(|value| value.as_u64())
        && total_lines > num_lines
    {
        attrs.push(format!("lines:{num_lines}/{total_lines}"));
    }
    if let Some(matches) = obj.get("matches").and_then(|value| value.as_array()) {
        // ToolSearch は matches 配列で結果を返す
        attrs.push(format!("matches:{}", matches.len()));
    } else if let Some(matches) = obj.get("numMatches").and_then(|value| value.as_u64()) {
        // Grep の count モードは matches 配列ではなく numMatches 整数で件数を返す
        attrs.push(format!("matches:{matches}"));
    }
    // --- Web ツール（WebSearch / WebFetch）の結果メタデータ ---
    // WebSearch: 検索結果件数 / 検索回数（複数時のみ）/ 所要時間（秒の float）
    if let Some(results) = obj.get("results").and_then(|value| value.as_array()) {
        attrs.push(format!("results:{}", results.len()));
    }
    if let Some(searches) = obj
        .get("searchCount")
        .and_then(|value| value.as_u64())
        .filter(|count| *count > 1)
    {
        attrs.push(format!("searches:{searches}"));
    }
    if let Some(seconds) = obj
        .get("durationSeconds")
        .and_then(|value| value.as_f64())
        .filter(|seconds| *seconds > 0.0)
    {
        // 既存の ms ベース duration 表示と表記を揃えるためミリ秒へ換算する
        attrs.push(format!(
            "duration:{}",
            format_millis_as_seconds((seconds * 1000.0) as u64)
        ));
    }
    // WebFetch: HTTP ステータスコード（+ codeText）と応答サイズ
    if let Some(code) = obj.get("code").and_then(|value| value.as_u64()) {
        if let Some(code_text) = obj
            .get("codeText")
            .and_then(|value| value.as_str())
            .filter(|text| !text.is_empty())
        {
            attrs.push(format!("http:{code} {code_text}"));
        } else {
            attrs.push(format!("http:{code}"));
        }
    }
    if let Some(bytes) = obj.get("bytes").and_then(|value| value.as_u64()) {
        attrs.push(format!("bytes:{}", format_byte_size(bytes)));
    }
    if let Some(mode) = obj
        .get("mode")
        .and_then(|value| value.as_str())
        .filter(|mode| !mode.is_empty())
    {
        attrs.push(format!("mode:{mode}"));
    }
    if let Some(deferred) = obj
        .get("total_deferred_tools")
        .and_then(|value| value.as_u64())
    {
        attrs.push(format!("deferred:{deferred}"));
    }
    if let Some(command_name) = obj
        .get("commandName")
        .and_then(|value| value.as_str())
        .filter(|name| !name.is_empty())
    {
        attrs.push(format!("command:{command_name}"));
    }
    if let Some(change) = obj.get("statusChange").and_then(|value| value.as_object()) {
        let from = change
            .get("from")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let to = change
            .get("to")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !from.is_empty() && !to.is_empty() {
            attrs.push(format!("status:{from}->{to}"));
        }
    }
    // Agent: 起動したサブエージェント種別（どのエージェントが実行したかの文脈）。
    if let Some(agent_type) = obj
        .get("agentType")
        .and_then(|value| value.as_str())
        .filter(|agent_type| !agent_type.is_empty())
    {
        attrs.push(format!("agent:{agent_type}"));
    }
    // Skill / Agent が解決したモデル名（モデルによりトークン消費が変わるため判断材料になる）。
    if let Some(model) = obj
        .get("resolvedModel")
        .and_then(|value| value.as_str())
        .filter(|model| !model.is_empty())
    {
        attrs.push(format!("model:{}", truncate_inline(model, 30)));
    }
    if let Some(ms) = obj.get("totalDurationMs").and_then(|value| value.as_u64()) {
        attrs.push(format!("duration:{}", format_millis_as_seconds(ms)));
    } else if let Some(ms) = obj.get("durationMs").and_then(|value| value.as_u64()) {
        attrs.push(format!("duration:{}", format_millis_as_seconds(ms)));
    }
    if let Some(tokens) = obj.get("totalTokens").and_then(|value| value.as_u64()) {
        attrs.push(format!("tokens:{}", format_number(tokens)));
    }
    if let Some(count) = obj
        .get("totalToolUseCount")
        .and_then(|value| value.as_u64())
    {
        attrs.push(format!("tools:{count}"));
    }
    // Agent/Task の toolStats からサブエージェントの編集行数を表示する。
    // 他のメタデータには無い固有情報のため、加除いずれかが非ゼロのときだけ出す。
    if let Some(stats) = obj.get("toolStats").and_then(|value| value.as_object()) {
        let added = stats
            .get("linesAdded")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let removed = stats
            .get("linesRemoved")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        if added > 0 || removed > 0 {
            attrs.push(format!("edits:+{added}/-{removed}"));
        }
    }
    if let Some(tasks) = obj.get("tasks").and_then(|value| value.as_array()) {
        attrs.push(format!("tasks:{}", tasks.len()));
    }
    if let Some(task) = obj.get("task").and_then(|value| value.as_object()) {
        let id = task
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let subject = task
            .get("subject")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !id.is_empty() && !subject.is_empty() {
            attrs.push(format!("task:{id} {}", truncate_inline(subject, 40)));
        } else if !id.is_empty() {
            attrs.push(format!("task:{id}"));
        }
    }
    if let Some(status) = obj
        .get("retrieval_status")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
    {
        attrs.push(format!("retrieval:{status}"));
    }
    if obj
        .get("outputFile")
        .and_then(|value| value.as_str())
        .is_some_and(|path| !path.is_empty())
    {
        if obj
            .get("canReadOutputFile")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            attrs.push("output-file:readable".to_string());
        } else {
            attrs.push("output-file".to_string());
        }
    }
    if let Some(timeout) = obj.get("timeoutMs").and_then(|value| value.as_u64()) {
        attrs.push(format!("timeout:{}", format_millis_as_seconds(timeout)));
    }
    if obj
        .get("persistent")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        attrs.push("persistent".to_string());
    }
    if obj
        .get("userModified")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        attrs.push("user-modified".to_string());
    }
    if let Some(hint) = obj
        .get("staleReadFileStateHint")
        .and_then(|value| value.as_str())
        .filter(|hint| !hint.is_empty())
    {
        attrs.push(format!("stale-read:{}", truncate_inline(hint, 70)));
    }
    if obj
        .get("assistantAutoBackgrounded")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        attrs.push("auto-backgrounded".to_string());
    }
    if let Some(task_id) = obj
        .get("backgroundTaskId")
        .and_then(|value| value.as_str())
        .filter(|task_id| !task_id.is_empty())
    {
        attrs.push(format!("background:{}", truncate_inline(task_id, 40)));
    }
    if obj
        .get("wasClamped")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        if let Some(delay) = obj
            .get("clampedDelaySeconds")
            .and_then(|value| value.as_u64())
        {
            attrs.push(format!("clamped:{delay}s"));
        } else {
            attrs.push("clamped".to_string());
        }
    }
    if obj
        .get("persistedOutputPath")
        .and_then(|value| value.as_str())
        .is_some_and(|path| !path.is_empty())
    {
        if let Some(size) = obj
            .get("persistedOutputSize")
            .and_then(|value| value.as_u64())
        {
            attrs.push(format!("persisted-output:{}", format_byte_size(size)));
        } else {
            attrs.push("persisted-output".to_string());
        }
    } else if let Some(size) = obj
        .get("persistedOutputSize")
        .and_then(|value| value.as_u64())
    {
        attrs.push(format!("output:{}", format_byte_size(size)));
    }
    if let Some(scheduled) = obj
        .get("scheduledFor")
        .and_then(|value| value.as_i64())
        .and_then(format_epoch_millis_clock)
    {
        attrs.push(format!("scheduled:{scheduled}"));
    }
    if let Some(interpretation) = obj
        .get("returnCodeInterpretation")
        .and_then(|value| value.as_str())
        .filter(|interpretation| !interpretation.is_empty())
    {
        attrs.push(format!("return:{}", truncate_inline(interpretation, 50)));
    }

    attrs.join(", ")
}
