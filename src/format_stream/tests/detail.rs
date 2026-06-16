//! `extract_tool_detail` 系のツール詳細表示テスト。

use super::*;

#[test]
fn extract_tool_detail_file_path() {
    let input = r#"{"file_path":"/src/main.rs"}"#;
    assert_eq!(extract_tool_detail("Read", input), "/src/main.rs");
}

// 以下は fall-through 分類のリグレッションテスト。
// リファクタ前後で挙動が変わらないことを固定する。

#[test]
fn extract_tool_detail_read_falls_back_to_generic_prompt() {
    // Read はファイルパスが無い場合、汎用フォールバックに落ちて prompt を表示する
    assert_eq!(
        extract_tool_detail("Read", r#"{"prompt":"fallback prompt"}"#),
        "fallback prompt"
    );
}

#[test]
fn extract_tool_detail_bash_empty_does_not_fall_back() {
    // Bash は常に確定するため、command が無くても汎用フォールバックには落ちない
    assert_eq!(
        extract_tool_detail("Bash", r#"{"prompt":"fallback prompt"}"#),
        ""
    );
}

#[test]
fn extract_tool_detail_monitor_empty_does_not_fall_back() {
    // Monitor も常に確定するため、内容が無くても汎用フォールバックには落ちない
    assert_eq!(
        extract_tool_detail("Monitor", r#"{"prompt":"fallback prompt"}"#),
        ""
    );
}

#[test]
fn extract_tool_detail_generic_fallback_keeps_empty_string_priority() {
    // 汎用フォールバックは空文字フィールドでも as_str() が Some を返した時点で
    // 即 return するため、file_path:"" を優先して空文字を返す
    assert_eq!(
        extract_tool_detail("Unknown", r#"{"file_path":"","prompt":"fallback"}"#),
        ""
    );
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
fn extract_tool_detail_bash_shows_disabled_sandbox() {
    // 実 jsonl の Bash 入力では、サンドボックス無効化が明示されることがある。
    let input = r#"{"command":"make install","dangerouslyDisableSandbox":true}"#;
    assert_eq!(
        extract_tool_detail("Bash", input),
        "make install [sandbox:disabled]"
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
    let input = r#"{"pattern":"\"scripts\"","path":"/home/user/GitHub/vscode-git-smart-commit/package.json"}"#;
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
fn extract_tool_detail_grep_shows_offset_and_only_matching() {
    let input = r#"{"pattern":"TODO","path":"/repo","offset":10,"-o":true}"#;
    assert_eq!(
        extract_tool_detail("Grep", input),
        "TODO @ /repo (offset:10, only-matching)"
    );
}

#[test]
fn extract_tool_detail_grep_shows_ignore_case() {
    let input =
        r#"{"pattern":"gpt-5\\.4","path":"/repo/src","output_mode":"content","-n":true,"-i":true}"#;
    assert_eq!(
        extract_tool_detail("Grep", input),
        "gpt-5\\.4 @ /repo/src (mode:content, line, ignore-case)"
    );
}

#[test]
fn extract_tool_detail_grep_shows_search_type() {
    let input = r#"{"pattern":"token-burn","path":"/repo","output_mode":"files_with_matches","type":"regex"}"#;
    assert_eq!(
        extract_tool_detail("Grep", input),
        "token-burn @ /repo (mode:files_with_matches, type:regex)"
    );
}

#[test]
fn extract_tool_detail_grep_shows_multiline() {
    let input = r#"{"pattern":"foo[\\s\\S]+bar","path":"/repo","output_mode":"content","-n":true,"multiline":true}"#;
    assert_eq!(
        extract_tool_detail("Grep", input),
        "foo[\\s\\S]+bar @ /repo (mode:content, line, multiline)"
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
fn extract_tool_detail_edit_shows_replace_all() {
    let input =
        r#"{"file_path":"/src/main.rs","old_string":"a","new_string":"b","replace_all":true}"#;
    let result = extract_tool_detail("Edit", input);
    assert!(result.contains("+0/-0"), "got: {result}");
    assert!(result.contains("replace_all"), "got: {result}");
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
    let input =
        r#"{"url":"https://example.com/docs","prompt":"Summarize the key points of this article"}"#;
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
fn extract_tool_detail_tavily_extract_with_multiple_urls() {
    // urls は複数形・配列。先頭 URL + 件数(+N more) + depth を表示する
    let input =
        r#"{"urls":["https://example.com/a","https://example.com/b"],"extract_depth":"advanced"}"#;
    assert_eq!(
        extract_tool_detail("mcp__tavily__tavily-extract", input),
        "https://example.com/a (+1 more, depth:advanced)"
    );
}

#[test]
fn extract_tool_detail_tavily_extract_single_url() {
    // URL が1件のみなら "+N more" は付けない
    let input = r#"{"urls":["https://example.com/only"]}"#;
    assert_eq!(
        extract_tool_detail("mcp__tavily__tavily-extract", input),
        "https://example.com/only"
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
fn extract_tool_detail_monitor_shows_seconds_and_condition() {
    let input = r#"{"description":"Wait for review","condition":"tokens used","timeout_seconds":120,"persistent":true}"#;
    assert_eq!(
        extract_tool_detail("Monitor", input),
        "Wait for review (timeout=120s, persistent, condition:tokens used)"
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
fn extract_tool_detail_monitor_uses_condition_without_description_or_command() {
    let input = r#"{"condition":"test -s /tmp/output","timeout_seconds":30}"#;
    assert_eq!(
        extract_tool_detail("Monitor", input),
        "test -s /tmp/output (timeout=30s)"
    );
}

#[test]
fn extract_tool_detail_task_stop_shows_task_id() {
    let input = r#"{"task_id":"b0mfly525"}"#;
    assert_eq!(extract_tool_detail("TaskStop", input), "task b0mfly525");
}

#[test]
fn extract_tool_detail_task_stop_shows_task_ids_and_reason() {
    let input = r#"{"task_ids":["t1","t2","t3","t4"],"reason":"rate limit reached"}"#;
    assert_eq!(
        extract_tool_detail("TaskStop", input),
        "tasks t1,t2,t3 +1 more (rate limit reached)"
    );
}

#[test]
fn extract_tool_detail_task_list_shows_fixed_label() {
    assert_eq!(extract_tool_detail("TaskList", r#"{}"#), "tasks");
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
fn extract_tool_detail_ask_user_question_shows_question_and_options() {
    let input = r#"{"questions":[{"question":"Codexから3件の確実なバグ指摘を受けました。修正範囲はどうしますか？","header":"修正範囲","multiSelect":false,"options":[{"label":"確実なバグ3件のみ修正 (推奨)"},{"label":"Trusted origin allowlist方式まで実装"},{"label":"1と2のみ修正"}]}]}"#;
    assert_eq!(
        extract_tool_detail("AskUserQuestion", input),
        "修正範囲: Codexから3件の確実なバグ指摘を受けました。修正範囲はどうしますか？ (3 options)"
    );
}

#[test]
fn extract_tool_detail_ask_user_question_shows_multiple_and_multiselect() {
    let input = r#"{"questions":[{"question":"対象を選択","header":"Deploy","multiSelect":true,"options":[{"label":"staging"},{"label":"production"}]},{"question":"実行しますか？","header":"Confirm","options":[{"label":"はい"}]}]}"#;
    assert_eq!(
        extract_tool_detail("AskUserQuestion", input),
        "Deploy: 対象を選択 (2 questions, 2 options, multi-select)"
    );
}

#[test]
fn extract_tool_detail_ask_user_question_empty_questions() {
    let input = r#"{"questions":[]}"#;
    assert_eq!(extract_tool_detail("AskUserQuestion", input), "0 questions");
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
    let input = r#"{"prompt":"レビューしてください\n詳細は git diff を確認してください","cwd":"/workspace/strategic-task-manager","model":"gpt-5-codex","sandbox":"read-only","approval-policy":"never"}"#;
    let result = extract_tool_detail("mcp__codex__codex", input);
    assert!(
        result.starts_with("レビューしてください 詳細は git diff"),
        "got: {result}"
    );
    assert!(
        result.contains("/workspace/strategic-task-manager"),
        "got: {result}"
    );
    assert!(result.contains("model:gpt-5-codex"), "got: {result}");
    assert!(result.contains("sandbox:read-only"), "got: {result}");
    assert!(result.contains("approval:never"), "got: {result}");
}

#[test]
fn extract_tool_detail_context7_resolve_library() {
    let input =
        r#"{"libraryName":"keyring","query":"keyring rust crate v4 migration breaking changes"}"#;
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
fn extract_tool_detail_agent_shows_background_and_type() {
    let input = r#"{"description":"Code correctness review","subagent_type":"feature-dev:code-reviewer","prompt":"レビューしてください","run_in_background":true}"#;
    assert_eq!(
        extract_tool_detail("Agent", input),
        "Code correctness review (feature-dev:code-reviewer, background)"
    );
}

#[test]
fn extract_tool_detail_team_create() {
    let input = r#"{"team_name":"demo-team","description":"Working on feature X"}"#;
    assert_eq!(extract_tool_detail("TeamCreate", input), "demo-team");
}

#[test]
fn extract_tool_detail_task_create_shows_subject_and_description() {
    // 実データ例: {"subject":"レビューと改善","description":"...","activeForm":"..."}
    let input = r#"{"subject":"Run tests","description":"Execute test suite","activeForm":"Running tests"}"#;
    assert_eq!(
        extract_tool_detail("TaskCreate", input),
        "Run tests (Execute test suite) [active: Running tests]"
    );
}

#[test]
fn extract_tool_detail_task_create_subject_only() {
    let input = r#"{"subject":"Run tests"}"#;
    assert_eq!(extract_tool_detail("TaskCreate", input), "Run tests");
}

#[test]
fn extract_tool_detail_task_create_description_only_fallback() {
    // subject なしでも description があればそれを表示する
    let input = r#"{"description":"Execute test suite"}"#;
    assert_eq!(
        extract_tool_detail("TaskCreate", input),
        "Execute test suite"
    );
}

#[test]
fn extract_tool_detail_task_create_active_form_only_fallback() {
    let input = r#"{"activeForm":"Running tests"}"#;
    assert_eq!(extract_tool_detail("TaskCreate", input), "Running tests");
}

#[test]
fn extract_tool_detail_task_get_shows_task_id() {
    // 実データ例: {"taskId":"3"}
    let input = r#"{"taskId":"3"}"#;
    assert_eq!(extract_tool_detail("TaskGet", input), "task 3");
}

#[test]
fn extract_tool_detail_task_update_shows_id_and_status() {
    // 実データ例: {"taskId":"1","status":"in_progress"}
    let input = r#"{"taskId":"1","status":"in_progress"}"#;
    assert_eq!(
        extract_tool_detail("TaskUpdate", input),
        "task 1 status:in_progress"
    );
}

#[test]
fn extract_tool_detail_task_update_with_subject_and_owner() {
    let input =
        r#"{"taskId":"42","status":"completed","owner":"reviewer","subject":"Review and improve"}"#;
    assert_eq!(
        extract_tool_detail("TaskUpdate", input),
        "task 42 status:completed owner:reviewer (Review and improve)"
    );
}

#[test]
fn extract_tool_detail_task_update_subject_only() {
    let input = r#"{"subject":"Update doc"}"#;
    assert_eq!(extract_tool_detail("TaskUpdate", input), "Update doc");
}

#[test]
fn extract_tool_detail_task_update_description_fallback() {
    let input = r#"{"taskId":"7","status":"in_progress","description":"実データ由来の説明"}"#;
    assert_eq!(
        extract_tool_detail("TaskUpdate", input),
        "task 7 status:in_progress (実データ由来の説明)"
    );
}

#[test]
fn extract_tool_detail_task_update_empty_returns_empty() {
    let input = r#"{}"#;
    assert_eq!(extract_tool_detail("TaskUpdate", input), "");
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
