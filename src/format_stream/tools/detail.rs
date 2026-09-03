//! ツール使用イベントの 1 行詳細（`🔧 ToolName <detail>`）を生成するモジュール。
//! `tool_specific_detail` がツール名ごとに専用関数へディスパッチし、条件不足の
//! 場合は `generic_tool_detail` の汎用フォールバックへ委ねる。

use crate::format_stream::util::{first_string, format_timeout_ms, truncate_inline, truncate_str};

/// ツール固有の表示処理の結果。
/// `Handled` はそのツールが文字列を確定した状態（空文字を含む）、
/// `Fallback` は条件不足で汎用フォールバックに委ねる状態を表す。
/// 空文字を返すだけの arm（Bash/Monitor/Skill 等）と、汎用フォールバックに
/// 落ちる arm（Read/Grep 等）を型で明確に区別するために enum を採用する。
enum DetailResult {
    Handled(String),
    Fallback,
}

pub(crate) fn extract_tool_detail(tool_name: &str, input_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return unparseable_input_detail(input_json),
    };

    match tool_specific_detail(tool_name, &v) {
        DetailResult::Handled(detail) => detail,
        DetailResult::Fallback => generic_tool_detail(&v).unwrap_or_default(),
    }
}

/// ツール入力（ストリーミングで蓄積した `input_json_delta`）が JSON として
/// パースできなかった場合の詳細表示を生成する。
///
/// モデルが不正な JSON をツール入力として出力した場合（実データで
/// `InputValidationError: ... could not be parsed as JSON` として観測される）や、
/// レート制限・セッション切断によりツール呼び出しがストリーム途中で打ち切られた場合、
/// 蓄積した文字列はパースできない。空文字を返すと「ツール名だけで詳細なし」の行に
/// なり、malformed / truncated を判別できないため、生入力の文字数を
/// `unparsed:<n> chars` として補足表示する。
///
/// 引数なしツール（`TaskList` 等で `input_json` が空文字）は従来どおり空表示を維持する。
/// 有効な JSON 内に `__unparsedToolInput` を含む形（assistant メッセージ側の最終形）は
/// 各ツールの専用ハンドラ（例: `detail_read`）が別途処理する。
fn unparseable_input_detail(input_json: &str) -> String {
    if input_json.trim().is_empty() {
        return String::new();
    }
    format!("unparsed:{} chars", input_json.chars().count())
}

/// ツール名ごとに専用の表示関数へディスパッチする。
/// 無条件に文字列を確定する arm（Edit/Bash/Write/Skill/Monitor）は `-> String` を
/// 返す関数を `Handled` で包み、条件次第で汎用フォールバックへ落ちる arm は
/// `-> DetailResult` を返す関数へ委譲する。
fn tool_specific_detail(tool_name: &str, v: &serde_json::Value) -> DetailResult {
    match tool_name {
        "Read" => detail_read(v),
        "Edit" => detail_edit(v),
        "Bash" | "bash" => DetailResult::Handled(detail_bash(v)),
        "BashOutput" => detail_bash_output(v),
        "Grep" | "Glob" => detail_grep_or_glob(v),
        "Task" | "Agent" => detail_task_or_agent(v),
        "TeamCreate" => detail_team_create(v),
        "Write" => DetailResult::Handled(detail_write(v)),
        "Skill" => DetailResult::Handled(detail_skill(v)),
        "SlashCommand" => detail_slash_command(v),
        "TodoWrite" => detail_todo_write(v),
        "ScheduleWakeup" => detail_schedule_wakeup(v),
        "WebFetch" => detail_web_fetch(v),
        "WebSearch" => detail_web_search(v),
        "ToolSearch" => detail_tool_search(v),
        "Monitor" => DetailResult::Handled(detail_monitor(v)),
        "SendMessage" => detail_send_message(v),
        "TaskCreate" => detail_task_create(v),
        "TaskGet" => detail_task_get(v),
        "TaskList" => DetailResult::Handled(detail_task_list(v)),
        "TaskUpdate" => detail_task_update(v),
        "TaskStop" => detail_task_stop(v),
        "TaskOutput" => detail_task_output(v),
        "Workflow" => detail_workflow(v),
        "AskUserQuestion" => detail_ask_user_question(v),
        // tavily MCP は実データでハイフン版とアンダースコア版の両方が観測されるため両方サポートする
        "mcp__tavily__tavily-search" | "mcp__tavily__tavily_search" => detail_tavily_search(v),
        "mcp__tavily__tavily-extract" | "mcp__tavily__tavily_extract" => detail_tavily_extract(v),
        "mcp__codex__codex" => detail_codex(v),
        name if name.starts_with("mcp__context7__resolve-library-id") => {
            detail_context7_resolve_library(v)
        }
        name if name.starts_with("mcp__context7__query-docs") => detail_context7_query_docs(v),
        _ => DetailResult::Fallback,
    }
}

/// Read: ファイルパスと読み取り範囲を表示。パス未指定なら汎用フォールバックへ。
fn detail_read(v: &serde_json::Value) -> DetailResult {
    let file = first_string(v, &["file_path", "path"]);
    let mut attrs = Vec::new();
    if let Some(offset) = v["offset"].as_u64() {
        attrs.push(format!("offset={offset}"));
    }
    if let Some(limit) = v["limit"].as_u64() {
        attrs.push(format!("limit={limit}"));
    }
    if let Some(view_range) = format_view_range(&v["view_range"]) {
        attrs.push(format!("range={view_range}"));
    }
    if !file.is_empty() && !attrs.is_empty() {
        return DetailResult::Handled(format!("{} ({})", truncate_str(file, 80), attrs.join(", ")));
    }
    if !file.is_empty() {
        return DetailResult::Handled(truncate_str(file, 100).to_string());
    }
    if let Some(raw_len) = v
        .get("__unparsedToolInput")
        .and_then(|value| value.get("len"))
        .and_then(|value| value.as_u64())
    {
        return DetailResult::Handled(format!("unparsed:{raw_len} chars"));
    }
    DetailResult::Fallback
}

fn format_view_range(value: &serde_json::Value) -> Option<String> {
    if let Some(range) = value.as_str().filter(|range| !range.is_empty()) {
        return Some(truncate_inline(range, 40));
    }
    let range = value.as_array()?;
    let start = range.first()?.as_i64()?;
    let end = range.get(1)?.as_i64()?;
    Some(format!("{start}-{end}"))
}

/// Edit: ファイルパスと差分行数(+追加/-削除)、replace_all を表示。
/// `file_path` が無い不完全な入力では空表示を避けて汎用フォールバックへ委ねる。
/// 行数は行数差分（new - old）ではなく、共通プレフィックス/サフィックス除去後の
/// 実変更行数を数える。行数差分だと同一行数の in-place 置換が `(+0/-0)` になり、
/// 直下に実 diff が出ているのに「変更なし」に見えてしまう（実ログで確認）。
fn detail_edit(v: &serde_json::Value) -> DetailResult {
    let file = v["file_path"].as_str().unwrap_or("");
    let old = first_string(v, &["old_string", "old_str"]);
    let new = first_string(v, &["new_string", "new_str"]);
    if file.is_empty() {
        return DetailResult::Fallback;
    }
    let (added, removed) = crate::format_stream::diff::changed_line_counts(old, new);
    let mut attrs = vec![format!("+{added}/-{removed}")];
    if v["replace_all"].as_bool() == Some(true) {
        attrs.push("replace_all".to_string());
    }
    DetailResult::Handled(format!("{} ({})", truncate_str(file, 80), attrs.join(", ")))
}

/// Bash: コマンドと timeout/background 属性、description を表示。常に確定。
fn detail_bash(v: &serde_json::Value) -> String {
    let cmd = v["command"].as_str().unwrap_or("");
    let desc = v["description"].as_str().unwrap_or("");
    let mut attrs = Vec::new();
    if let Some(timeout) = v["timeout"].as_u64() {
        // ミリ秒未満で切り捨てて "0s" にならないよう、1000ms 未満はミリ秒表示にする。
        attrs.push(format!("timeout={}", format_timeout_ms(timeout)));
    }
    if v["run_in_background"].as_bool() == Some(true) {
        attrs.push("background".to_string());
    }
    if v["dangerouslyDisableSandbox"].as_bool() == Some(true) {
        attrs.push("sandbox:disabled".to_string());
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
    format!("{}{}", truncate_str(cmd, 100), attr_text)
}

/// Grep / Glob: 検索パターン・パス・各種フィルタ属性を表示。
/// pattern と path がともに空なら汎用フォールバックへ。
fn detail_grep_or_glob(v: &serde_json::Value) -> DetailResult {
    let pattern = v["pattern"].as_str().unwrap_or("");
    let path = v["path"].as_str().unwrap_or("");
    let glob = v["glob"].as_str().unwrap_or("");
    let mut attrs = Vec::new();
    if let Some(output_mode) = v["output_mode"].as_str()
        && !output_mode.is_empty()
    {
        attrs.push(format!("mode:{output_mode}"));
    }
    if let Some(search_type) = v["type"].as_str()
        && !search_type.is_empty()
    {
        attrs.push(format!("type:{search_type}"));
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
    if let Some(offset) = v["offset"].as_u64() {
        attrs.push(format!("offset:{offset}"));
    }
    for key in ["-A", "-B", "-C"] {
        if let Some(value) = v[key].as_u64() {
            attrs.push(format!("{}:{value}", key.trim_start_matches('-')));
        }
    }
    if v["-n"].as_bool() == Some(true) {
        attrs.push("line".to_string());
    }
    if v["-i"].as_bool() == Some(true) {
        attrs.push("ignore-case".to_string());
    }
    if v["-o"].as_bool() == Some(true) {
        attrs.push("only-matching".to_string());
    }
    if v["multiline"].as_bool() == Some(true) {
        attrs.push("multiline".to_string());
    }
    let attr_text = if attrs.is_empty() {
        String::new()
    } else {
        format!(" ({})", attrs.join(", "))
    };
    if !pattern.is_empty() && !path.is_empty() {
        return DetailResult::Handled(format!(
            "{} @ {}{}",
            truncate_str(pattern, 60),
            truncate_str(path, 50),
            attr_text
        ));
    }
    if !pattern.is_empty() {
        return DetailResult::Handled(format!("{}{}", truncate_str(pattern, 100), attr_text));
    }
    if !path.is_empty() {
        return DetailResult::Handled(format!("{}{}", truncate_str(path, 100), attr_text));
    }
    DetailResult::Fallback
}

/// Task / Agent: name または description/prompt と agent_type/background を表示。
/// detail も attrs もすべて空なら汎用フォールバックへ。
fn detail_task_or_agent(v: &serde_json::Value) -> DetailResult {
    let desc = v["description"].as_str().unwrap_or("");
    let name = v["name"].as_str().unwrap_or("");
    let agent_type = v["subagent_type"].as_str().unwrap_or("");
    let prompt = v["prompt"].as_str().unwrap_or("");
    let detail = if !name.is_empty() && !desc.is_empty() {
        // name は SendMessage で参照する識別子（例: review-core）で、それだけでは
        // 何をするエージェントか分からない。実データでは name 付き起動の大半が
        // description も持つため、両方あるときは併記する。
        format!("{} \u{2014} {}", name, truncate_str(desc, 70))
    } else if !name.is_empty() {
        name.to_string()
    } else if !desc.is_empty() {
        truncate_str(desc, 80)
    } else if !prompt.is_empty() {
        truncate_inline(prompt, 80)
    } else {
        String::new()
    };
    let mut attrs = Vec::new();
    if !agent_type.is_empty() {
        attrs.push(agent_type.to_string());
    }
    if v["run_in_background"].as_bool() == Some(true) {
        attrs.push("background".to_string());
    }
    // model 上書き・isolation はサブエージェントの起動条件（コスト・分離環境）を左右する
    // ため、指定時のみ短く併記する。
    if let Some(model) = v["model"].as_str()
        && !model.is_empty()
    {
        attrs.push(format!("model:{model}"));
    }
    if let Some(isolation) = v["isolation"].as_str()
        && !isolation.is_empty()
    {
        attrs.push(format!("isolation:{isolation}"));
    }
    if !detail.is_empty() && !attrs.is_empty() {
        return DetailResult::Handled(format!("{} ({})", detail, attrs.join(", ")));
    }
    if !detail.is_empty() {
        return DetailResult::Handled(detail);
    }
    if !attrs.is_empty() {
        return DetailResult::Handled(attrs.join(", "));
    }
    DetailResult::Fallback
}

/// TeamCreate: team_name を表示。空なら汎用フォールバックへ。
fn detail_team_create(v: &serde_json::Value) -> DetailResult {
    if let Some(team) = v["team_name"].as_str()
        && !team.is_empty()
    {
        return DetailResult::Handled(team.to_string());
    }
    DetailResult::Fallback
}

/// Write: ファイルパスと書き込み行数を表示。常に確定。
fn detail_write(v: &serde_json::Value) -> String {
    let file = v["file_path"].as_str().unwrap_or("");
    let content = v["content"].as_str().unwrap_or("");
    let lines = content.lines().count();
    format!("{} ({} lines)", truncate_str(file, 80), lines)
}

/// Skill: スキル名と引数を表示。常に確定。
fn detail_skill(v: &serde_json::Value) -> String {
    let skill = v["skill"].as_str().unwrap_or("");
    let args = v["args"].as_str().unwrap_or("");
    if !args.is_empty() {
        return format!("{} ({})", skill, truncate_str(args, 60));
    }
    skill.to_string()
}

/// TodoWrite: 完了済み/全体の件数を表示。todos が配列でなければ汎用フォールバックへ。
fn detail_todo_write(v: &serde_json::Value) -> DetailResult {
    if let Some(todos) = v["todos"].as_array() {
        let total = todos.len();
        let done = todos
            .iter()
            .filter(|t| t["status"].as_str() == Some("completed"))
            .count();
        return DetailResult::Handled(format!("{}/{} completed", done, total));
    }
    DetailResult::Fallback
}

/// ScheduleWakeup: 待機秒数と reason/prompt を表示。
/// delay も reason も prompt も無ければ汎用フォールバックへ。
fn detail_schedule_wakeup(v: &serde_json::Value) -> DetailResult {
    let delay = v["delaySeconds"].as_u64();
    let reason = v["reason"].as_str().unwrap_or("");
    let prompt = v["prompt"].as_str().unwrap_or("");
    if let Some(delay) = delay {
        let note = if !reason.is_empty() { reason } else { prompt };
        if note.is_empty() {
            return DetailResult::Handled(format!("{delay}s"));
        }
        return DetailResult::Handled(format!("{delay}s ({})", truncate_str(note, 60)));
    }
    if !reason.is_empty() {
        return DetailResult::Handled(truncate_str(reason, 80).to_string());
    }
    if !prompt.is_empty() {
        return DetailResult::Handled(truncate_str(prompt, 80).to_string());
    }
    DetailResult::Fallback
}

/// WebFetch: URL と prompt 要約を表示。URL が空なら汎用フォールバックへ。
fn detail_web_fetch(v: &serde_json::Value) -> DetailResult {
    let url = v["url"].as_str().unwrap_or("");
    let prompt = v["prompt"].as_str().unwrap_or("");
    if !url.is_empty() && !prompt.is_empty() {
        return DetailResult::Handled(format!(
            "{} ({})",
            truncate_str(url, 70),
            truncate_str(prompt, 40)
        ));
    }
    if !url.is_empty() {
        return DetailResult::Handled(truncate_str(url, 100).to_string());
    }
    DetailResult::Fallback
}

/// WebSearch: クエリと allow/block ドメイン件数を表示。クエリが空なら汎用フォールバックへ。
fn detail_web_search(v: &serde_json::Value) -> DetailResult {
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
        return DetailResult::Handled(detail);
    }
    DetailResult::Fallback
}

/// ToolSearch: クエリと max_results を表示。クエリが空なら汎用フォールバックへ。
fn detail_tool_search(v: &serde_json::Value) -> DetailResult {
    let query = v["query"].as_str().unwrap_or("");
    let max_results = v["max_results"].as_u64();
    if !query.is_empty() {
        if let Some(n) = max_results {
            return DetailResult::Handled(format!("{} (max={})", truncate_str(query, 80), n));
        }
        return DetailResult::Handled(truncate_str(query, 100).to_string());
    }
    DetailResult::Fallback
}

/// Monitor: description/command と timeout/persistent 属性を表示。常に確定。
fn detail_monitor(v: &serde_json::Value) -> String {
    let desc = v["description"].as_str().unwrap_or("");
    let cmd = v["command"].as_str().unwrap_or("");
    let condition = v["condition"].as_str().unwrap_or("");
    // timeout_seconds（秒）を優先し、無ければ timeout_ms（ミリ秒）を整形する。
    // ms を秒へ整数除算すると 1〜999ms が "0s" になるため format_timeout_ms を使う。
    let timeout_text = v["timeout_seconds"]
        .as_u64()
        .map(|s| format!("{s}s"))
        .or_else(|| v["timeout_ms"].as_u64().map(format_timeout_ms));
    let persistent = v["persistent"].as_bool().unwrap_or(false);

    let mut detail = if !desc.is_empty() {
        truncate_str(desc, 80)
    } else if !cmd.is_empty() {
        truncate_str(cmd, 80)
    } else if !condition.is_empty() {
        truncate_inline(condition, 80)
    } else {
        String::new()
    };

    let mut attrs = Vec::new();
    if let Some(timeout) = timeout_text {
        attrs.push(format!("timeout={timeout}"));
    }
    if persistent {
        attrs.push("persistent".to_string());
    }
    if !condition.is_empty() && (!desc.is_empty() || !cmd.is_empty()) {
        attrs.push(format!("condition:{}", truncate_inline(condition, 40)));
    }
    if !detail.is_empty() && !attrs.is_empty() {
        detail = format!("{} ({})", detail, attrs.join(", "));
    }

    detail
}

/// SendMessage: 送信先と summary/message を表示。to も label も空なら汎用フォールバックへ。
fn detail_send_message(v: &serde_json::Value) -> DetailResult {
    let to = first_string(v, &["to", "recipient"]);
    let summary = v["summary"].as_str().unwrap_or("");
    let message = first_string(v, &["message", "content"]);
    let label = if !summary.is_empty() {
        summary
    } else {
        message
    };
    if !to.is_empty() && !label.is_empty() {
        return DetailResult::Handled(format!(
            "{} -> {}",
            truncate_inline(label, 70),
            truncate_inline(to, 40)
        ));
    }
    if !label.is_empty() {
        return DetailResult::Handled(truncate_inline(label, 100));
    }
    if !to.is_empty() {
        return DetailResult::Handled(format!("to {}", truncate_inline(to, 80)));
    }
    DetailResult::Fallback
}

/// TaskCreate: subject を最優先で表示し、description / activeForm を補足する。
/// すべて空なら汎用フォールバックへ。
fn detail_task_create(v: &serde_json::Value) -> DetailResult {
    // 実データ例: {"subject":"レビューと改善","description":"...","activeForm":"レビューと改善中"}
    // subject はタスクの主目的を簡潔に示すため最優先で表示する
    let subject = v["subject"].as_str().unwrap_or("");
    let description = v["description"].as_str().unwrap_or("");
    let active_form = v["activeForm"].as_str().unwrap_or("");
    let append_active = |detail: String| {
        if active_form.is_empty() {
            detail
        } else {
            format!("{detail} [active: {}]", truncate_inline(active_form, 40))
        }
    };
    if !subject.is_empty() && !description.is_empty() {
        return DetailResult::Handled(append_active(format!(
            "{} ({})",
            truncate_inline(subject, 60),
            truncate_inline(description, 60)
        )));
    }
    if !subject.is_empty() {
        return DetailResult::Handled(append_active(truncate_inline(subject, 100)));
    }
    if !description.is_empty() {
        return DetailResult::Handled(append_active(truncate_inline(description, 100)));
    }
    if !active_form.is_empty() {
        return DetailResult::Handled(truncate_inline(active_form, 100));
    }
    DetailResult::Fallback
}

/// TaskList: 入力が空の実データでも、ツールの意図が分かるよう固定ラベルを表示する。
fn detail_task_list(_v: &serde_json::Value) -> String {
    "tasks".to_string()
}

/// TaskGet: 取得対象の task id を表示。空なら汎用フォールバックへ。
fn detail_task_get(v: &serde_json::Value) -> DetailResult {
    let task_id = first_string(v, &["taskId", "task_id", "id"]);
    if !task_id.is_empty() {
        return DetailResult::Handled(format!("task {}", truncate_inline(task_id, 80)));
    }
    DetailResult::Fallback
}

/// TaskUpdate: taskId / status / owner と subject/description を組み合わせて表示。
/// すべて空なら汎用フォールバックへ。
fn detail_task_update(v: &serde_json::Value) -> DetailResult {
    // 実データ例: {"taskId":"1","status":"in_progress"}
    // taskId 単独では文脈に乏しいため status と組み合わせて表示する
    let task_id = v["taskId"].as_str().unwrap_or("");
    let status = v["status"].as_str().unwrap_or("");
    let subject = v["subject"].as_str().unwrap_or("");
    let description = v["description"].as_str().unwrap_or("");
    let owner = v["owner"].as_str().unwrap_or("");
    let mut parts = Vec::new();
    if !task_id.is_empty() {
        parts.push(format!("task {task_id}"));
    }
    if !status.is_empty() {
        parts.push(format!("status:{status}"));
    }
    if !owner.is_empty() {
        parts.push(format!("owner:{owner}"));
    }
    if !subject.is_empty() {
        let label = truncate_inline(subject, 60);
        if parts.is_empty() {
            return DetailResult::Handled(label);
        }
        return DetailResult::Handled(format!("{} ({})", parts.join(" "), label));
    }
    if !description.is_empty() {
        let label = truncate_inline(description, 60);
        if parts.is_empty() {
            return DetailResult::Handled(label);
        }
        return DetailResult::Handled(format!("{} ({})", parts.join(" "), label));
    }
    if !parts.is_empty() {
        return DetailResult::Handled(parts.join(" "));
    }
    DetailResult::Fallback
}

/// TaskStop: 停止対象の task id を表示。空なら汎用フォールバックへ。
fn detail_task_stop(v: &serde_json::Value) -> DetailResult {
    let mut task_ids = Vec::new();
    if let Some(task_id) = v["task_id"].as_str()
        && !task_id.is_empty()
    {
        task_ids.push(task_id);
    }
    if let Some(ids) = v["task_ids"].as_array() {
        for id in ids {
            if let Some(id) = id.as_str()
                && !id.is_empty()
            {
                task_ids.push(id);
            }
        }
    }
    let reason = v["reason"].as_str().unwrap_or("");

    let detail = if task_ids.is_empty() {
        String::new()
    } else if task_ids.len() == 1 {
        format!("task {}", truncate_str(task_ids[0], 80))
    } else {
        let shown = task_ids
            .iter()
            .take(3)
            .map(|id| truncate_str(id, 24))
            .collect::<Vec<_>>();
        let suffix = if task_ids.len() > 3 {
            format!(" +{} more", task_ids.len() - 3)
        } else {
            String::new()
        };
        format!("tasks {}{}", shown.join(","), suffix)
    };

    if !detail.is_empty() && !reason.is_empty() {
        return DetailResult::Handled(format!("{} ({})", detail, truncate_inline(reason, 60)));
    }
    if !detail.is_empty() {
        return DetailResult::Handled(detail);
    }
    if !reason.is_empty() {
        return DetailResult::Handled(truncate_inline(reason, 100));
    }
    DetailResult::Fallback
}

/// TaskOutput: 待機対象の task id と block/timeout 属性を表示。
/// task_id も attrs も空なら汎用フォールバックへ。
fn detail_task_output(v: &serde_json::Value) -> DetailResult {
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
        attrs.push(format!("timeout={}", format_timeout_ms(ms)));
    }
    if !task_id.is_empty() {
        if attrs.is_empty() {
            return DetailResult::Handled(format!("task {}", truncate_str(task_id, 80)));
        }
        return DetailResult::Handled(format!(
            "task {} ({})",
            truncate_str(task_id, 60),
            attrs.join(", ")
        ));
    }
    if !attrs.is_empty() {
        return DetailResult::Handled(attrs.join(", "));
    }
    DetailResult::Fallback
}

/// AskUserQuestion: 先頭の質問内容と質問数/選択肢数を表示。
/// questions が配列でなければ汎用フォールバックへ。
fn detail_ask_user_question(v: &serde_json::Value) -> DetailResult {
    if let Some(questions) = v["questions"].as_array() {
        let total = questions.len();
        let Some(first) = questions.first() else {
            return DetailResult::Handled("0 questions".to_string());
        };

        let header = first["header"].as_str().unwrap_or("");
        let question = first["question"].as_str().unwrap_or("");
        let options = first["options"]
            .as_array()
            .map(|options| options.len())
            .unwrap_or(0);
        let mut detail = if !header.is_empty() && !question.is_empty() {
            format!(
                "{}: {}",
                truncate_inline(header, 24),
                truncate_inline(question, 70)
            )
        } else if !question.is_empty() {
            truncate_inline(question, 90)
        } else if !header.is_empty() {
            truncate_inline(header, 90)
        } else {
            String::new()
        };

        let mut attrs = Vec::new();
        if total > 1 {
            attrs.push(format!("{total} questions"));
        }
        if options > 0 {
            attrs.push(format!("{options} options"));
        }
        if first["multiSelect"].as_bool() == Some(true) {
            attrs.push("multi-select".to_string());
        }

        if detail.is_empty() {
            detail = format!("{total} question{}", if total == 1 { "" } else { "s" });
        }
        if attrs.is_empty() {
            return DetailResult::Handled(detail);
        }
        return DetailResult::Handled(format!("{} ({})", detail, attrs.join(", ")));
    }
    DetailResult::Fallback
}

/// ドメイン絞り込み配列を `site=<domain>` / `site=<n> domains` の属性へ整形する。
/// 空配列・非配列なら何も返さない（属性を増やさない）。
fn domain_filter_attr(value: &serde_json::Value, label: &str) -> Option<String> {
    let domains: Vec<&str> = value
        .as_array()?
        .iter()
        .filter_map(|d| d.as_str())
        .filter(|d| !d.is_empty())
        .collect();
    match domains.len() {
        0 => None,
        1 => Some(format!("{label}={}", truncate_inline(domains[0], 40))),
        n => Some(format!("{label}={n} domains")),
    }
}

/// Tavily 検索: クエリと max/range/depth/日付/ドメイン属性を表示。
/// クエリが空なら汎用フォールバックへ。
fn detail_tavily_search(v: &serde_json::Value) -> DetailResult {
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
        // topic=news はニュース索引への切り替え、days はその遡及日数で、いずれも
        // 検索対象そのものを変える。実データの `topic:news days:8` は time_range を
        // 伴わないため、これらを落とすと通常の Web 検索と区別できなくなる。
        if let Some(topic) = v["topic"].as_str()
            && !topic.is_empty()
        {
            attrs.push(format!("topic={topic}"));
        }
        if let Some(days) = v["days"].as_u64() {
            attrs.push(format!("days={days}"));
        }
        // start_date / end_date は time_range / days と同じ「検索対象期間の絞り込み」で、
        // 実データでは time_range(2 件)より start_date(13 件)の方が主流。落とすと日付で
        // 絞った検索が期間無制限の検索と区別できない。
        if let Some(start) = v["start_date"].as_str()
            && !start.is_empty()
        {
            attrs.push(format!("start={}", truncate_inline(start, 20)));
        }
        if let Some(end) = v["end_date"].as_str()
            && !end.is_empty()
        {
            attrs.push(format!("end={}", truncate_inline(end, 20)));
        }
        // ドメイン絞り込みは検索対象そのものを置き換える強いフィルタで、実データでは
        // `["ast-grep.github.io"]` のように単一ドメインへ限定した検索が 13 件ある。
        // WebSearch では allowed/blocked の件数を既に出しているのに、Tavily 側だけ
        // 落ちていた。件数だけだと「どのドメインか」が分からないので、1 件なら
        // ドメイン名そのものを出す。
        attrs.extend(domain_filter_attr(&v["include_domains"], "site"));
        attrs.extend(domain_filter_attr(&v["exclude_domains"], "-site"));
        if attrs.is_empty() {
            return DetailResult::Handled(truncate_inline(query, 100));
        }
        return DetailResult::Handled(format!(
            "{} ({})",
            truncate_inline(query, 80),
            attrs.join(", ")
        ));
    }
    DetailResult::Fallback
}

/// Tavily 抽出: 先頭 URL と件数(+N more)・extract_depth を表示。urls が空なら汎用フォールバックへ。
/// urls は複数形・配列のため、汎用フォールバックの単数 `url` ではマッチせず詳細が空になる。
fn detail_tavily_extract(v: &serde_json::Value) -> DetailResult {
    let Some(urls) = v["urls"].as_array() else {
        return DetailResult::Fallback;
    };
    let first = urls.first().and_then(|url| url.as_str()).unwrap_or("");
    if first.is_empty() {
        return DetailResult::Fallback;
    }
    let mut attrs = Vec::new();
    if urls.len() > 1 {
        attrs.push(format!("+{} more", urls.len() - 1));
    }
    if let Some(depth) = v["extract_depth"].as_str()
        && !depth.is_empty()
    {
        attrs.push(format!("depth:{depth}"));
    }
    if attrs.is_empty() {
        DetailResult::Handled(truncate_inline(first, 100))
    } else {
        DetailResult::Handled(format!(
            "{} ({})",
            truncate_inline(first, 80),
            attrs.join(", ")
        ))
    }
}

/// Codex MCP: prompt と cwd/model/sandbox/approval 属性を表示。
/// prompt も attrs も空なら汎用フォールバックへ。
fn detail_codex(v: &serde_json::Value) -> DetailResult {
    let prompt = v["prompt"].as_str().unwrap_or("");
    let cwd = v["cwd"].as_str().unwrap_or("");
    let model = v["model"].as_str().unwrap_or("");
    let sandbox = v["sandbox"].as_str().unwrap_or("");
    let approval_policy = v["approval-policy"].as_str().unwrap_or("");
    let mut attrs = Vec::new();
    if !cwd.is_empty() {
        attrs.push(truncate_inline(cwd, 50));
    }
    if !model.is_empty() {
        attrs.push(format!("model:{model}"));
    }
    if !sandbox.is_empty() {
        attrs.push(format!("sandbox:{sandbox}"));
    }
    if !approval_policy.is_empty() {
        attrs.push(format!("approval:{approval_policy}"));
    }
    if !attrs.is_empty() && !prompt.is_empty() {
        return DetailResult::Handled(format!(
            "{} ({})",
            truncate_inline(prompt, 70),
            attrs.join(", ")
        ));
    }
    if !prompt.is_empty() {
        return DetailResult::Handled(truncate_inline(prompt, 100));
    }
    if !attrs.is_empty() {
        return DetailResult::Handled(attrs.join(", "));
    }
    DetailResult::Fallback
}

/// Context7 resolve-library-id: libraryName と query を表示。
/// 両方空なら汎用フォールバックへ。
fn detail_context7_resolve_library(v: &serde_json::Value) -> DetailResult {
    let library = v["libraryName"].as_str().unwrap_or("");
    let query = v["query"].as_str().unwrap_or("");
    if !library.is_empty() && !query.is_empty() {
        return DetailResult::Handled(format!(
            "{} ({})",
            truncate_str(library, 50),
            truncate_str(query, 60)
        ));
    }
    if !library.is_empty() {
        return DetailResult::Handled(truncate_str(library, 100).to_string());
    }
    if !query.is_empty() {
        return DetailResult::Handled(truncate_str(query, 100).to_string());
    }
    DetailResult::Fallback
}

/// Context7 query-docs: libraryId と query を表示。両方空なら汎用フォールバックへ。
fn detail_context7_query_docs(v: &serde_json::Value) -> DetailResult {
    let library_id = v["libraryId"].as_str().unwrap_or("");
    let query = v["query"].as_str().unwrap_or("");
    if !library_id.is_empty() && !query.is_empty() {
        return DetailResult::Handled(format!(
            "{} ({})",
            truncate_str(library_id, 50),
            truncate_str(query, 60)
        ));
    }
    if !library_id.is_empty() {
        return DetailResult::Handled(truncate_str(library_id, 100).to_string());
    }
    if !query.is_empty() {
        return DetailResult::Handled(truncate_str(query, 100).to_string());
    }
    DetailResult::Fallback
}

/// 汎用フォールバック: よくあるフィールド名を優先順に試行する。
/// `as_str()` が `Some` を返した時点で（空文字でも）即 return する現状挙動を保持する。
/// BashOutput: 出力を取得する対象の background bash の id を表示する。
/// 入力は `{"bash_id":"..."}`（任意で `filter`）。bash_id は generic_tool_detail の
/// 候補キーに無く、専用化しないと空表示になるため個別ハンドラを設ける。
fn detail_bash_output(v: &serde_json::Value) -> DetailResult {
    let bash_id = v["bash_id"].as_str().unwrap_or("");
    if bash_id.is_empty() {
        return DetailResult::Fallback;
    }
    let mut detail = format!("bash:{}", truncate_str(bash_id, 60));
    // 出力フィルタ（正規表現）が指定されていれば併記する。
    if let Some(filter) = v["filter"].as_str().filter(|s| !s.is_empty()) {
        detail = format!("{} (filter:{})", detail, truncate_inline(filter, 40));
    }
    DetailResult::Handled(detail)
}

/// Workflow: 起動するワークフローの識別情報を表示する。
/// - named workflow（保存済みを名前で実行）は `name`
/// - インライン script は `meta.name` を best-effort 抽出（不能ならスクリプト文字数）
/// - `scriptPath` 指定（保存済みスクリプトの再実行・resume）はファイル名
///
/// いずれも取れなければ汎用フォールバックへ委ねる。
fn detail_workflow(v: &serde_json::Value) -> DetailResult {
    // named workflow を名前で実行するケース。
    if let Some(name) = v["name"].as_str().filter(|s| !s.is_empty()) {
        return DetailResult::Handled(truncate_str(name, 80).to_string());
    }
    // インライン script: meta.name を抽出できれば名前とスクリプト規模を併記する。
    if let Some(script) = v["script"].as_str().filter(|s| !s.is_empty()) {
        let chars = script.chars().count();
        return DetailResult::Handled(match extract_workflow_meta_name(script) {
            Some(name) => format!("{} (script:{chars} chars)", truncate_str(&name, 60)),
            None => format!("script:{chars} chars"),
        });
    }
    // scriptPath 指定（再実行・resume）。
    if let Some(path) = v["scriptPath"].as_str().filter(|s| !s.is_empty()) {
        let base = path.rsplit('/').next().unwrap_or(path);
        return DetailResult::Handled(truncate_str(base, 80).to_string());
    }
    DetailResult::Fallback
}

/// Workflow の script 文字列から `meta.name` を best-effort で抽出する。
/// `export const meta = { name: 'xxx', ... }` の最初の `name:` を対象にする
/// （meta は先頭が name のため、phases 内の name 等を誤検出しにくい）。抽出に
/// 失敗しても呼び出し側がスクリプト文字数へフォールバックするため None を返してよい。
fn extract_workflow_meta_name(script: &str) -> Option<String> {
    // meta ブロック以降に絞る（先頭の `export const meta = {` を基準にする）。
    let after_meta = &script[script.find("meta")?..];
    let name_pos = after_meta.find("name")?;
    let after_name = &after_meta[name_pos + "name".len()..];
    let colon = after_name.find(':')?;
    // name と : の間は空白のみ許容（`namespace:` 等の別トークン誤検出を防ぐ）。
    if !after_name[..colon].trim().is_empty() {
        return None;
    }
    // : の直後（空白除去後）は文字列リテラルの引用符でなければならない
    // （遠くの description 内の引用符を誤って拾わないようにする）。
    let after_colon = after_name[colon + 1..].trim_start();
    let mut chars = after_colon.chars();
    let quote = match chars.next() {
        Some(q @ ('\'' | '"' | '`')) => q,
        _ => return None,
    };
    let value = chars.as_str();
    let end = value.find(quote)?;
    let name = value[..end].trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// SlashCommand: 実行するスラッシュコマンド文字列（例: `/get-md ...`）を表示する。
/// generic_tool_detail でも `command` は拾えるが、専用化して表示とテストを固定する。
fn detail_slash_command(v: &serde_json::Value) -> DetailResult {
    let command = v["command"].as_str().unwrap_or("");
    if command.is_empty() {
        return DetailResult::Fallback;
    }
    DetailResult::Handled(truncate_inline(command, 100))
}

fn generic_tool_detail(v: &serde_json::Value) -> Option<String> {
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
            return Some(truncate_inline(val, 100));
        }
    }

    None
}
