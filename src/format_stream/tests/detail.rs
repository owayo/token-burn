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
fn extract_tool_detail_read_shows_view_range_string() {
    let input = r#"{"file_path":"/src/main.rs","view_range":"[120, 200]"}"#;
    assert_eq!(
        extract_tool_detail("Read", input),
        "/src/main.rs (range=[120, 200])"
    );
}

#[test]
fn extract_tool_detail_read_shows_view_range_array() {
    let input = r#"{"file_path":"/src/main.rs","view_range":[120,200]}"#;
    assert_eq!(
        extract_tool_detail("Read", input),
        "/src/main.rs (range=120-200)"
    );
}

#[test]
fn extract_tool_detail_read_shows_unparsed_input_len() {
    let input =
        r#"{"__unparsedToolInput":{"raw":"{\"file_path\":\"/tmp/a\",\"offset\":1, 2}","len":37}}"#;
    assert_eq!(extract_tool_detail("Read", input), "unparsed:37 chars");
}

#[test]
fn extract_tool_detail_command() {
    let input = r#"{"command":"cargo test"}"#;
    assert_eq!(extract_tool_detail("Bash", input), "cargo test");
}

#[test]
fn extract_tool_detail_lowercase_bash_matches_bash() {
    // 実 jsonl では一部の Bash 互換ツールが小文字 `bash` として出る。
    let input = r#"{"command":"grep -A 5 \"needle\" file"}"#;
    assert_eq!(
        extract_tool_detail("bash", input),
        "grep -A 5 \"needle\" file"
    );
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
fn extract_tool_detail_bash_timeout_under_one_second_uses_millis() {
    // 1000ms 未満の timeout を秒で除算すると "timeout=0s" になってしまうため
    // ミリ秒表記でフォールバックすることを確認する。
    let input = r#"{"command":"echo hi","timeout":500}"#;
    assert_eq!(
        extract_tool_detail("Bash", input),
        "echo hi [timeout=500ms]"
    );
}

#[test]
fn extract_tool_detail_bash_timeout_exactly_one_second_uses_seconds() {
    // 境界値: 1000ms ちょうどは "timeout=1s" として表示する。
    let input = r#"{"command":"echo hi","timeout":1000}"#;
    assert_eq!(extract_tool_detail("Bash", input), "echo hi [timeout=1s]");
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
    assert!(result.contains("+1/-1"), "got: {result}");
    assert!(result.contains("replace_all"), "got: {result}");
}

#[test]
fn extract_tool_detail_edit_inplace_replacement_counts_changed_lines() {
    // 同一行数の in-place 置換。行数差分方式では (+0/-0) となり「変更なし」に
    // 見えていた実ログの再発防止（表示 diff の -/+ 行数と一致させる）。
    let input = r#"{"file_path":"/docs/AGENTS.md","old_string":"a\nb\nc","new_string":"a\nX\nc"}"#;
    let result = extract_tool_detail("Edit", input);
    assert!(result.contains("(+1/-1)"), "got: {result}");
}

#[test]
fn extract_tool_detail_truncates_long_values() {
    let long_path = format!(r#"{{"file_path":"{}"}}"#, "a".repeat(200));
    let result = extract_tool_detail("Read", &long_path);
    assert!(result.len() <= 103); // 100 + "..."
    assert!(result.ends_with("..."));
}

#[test]
fn extract_tool_detail_edit_without_file_path_falls_back() {
    // partial_json の確定タイミング等で file_path が欠落した不完全な入力は、
    // 「 (+0/-0)」のような中身の無い表示を避けて汎用フォールバックへ委ねる。
    let input = r#"{"old_string":"a","new_string":"a"}"#;
    let result = extract_tool_detail("Edit", input);
    assert!(
        !result.starts_with(' '),
        "file_path 空時は先頭空白付きの不格好な表示を残さない: {result:?}"
    );
    assert!(
        !result.contains("(+0/-0)"),
        "差分行数の体裁が無意味に残らない: {result:?}"
    );
}

#[test]
fn extract_tool_detail_invalid_json() {
    // パースできない非空入力は空表示ではなく unparsed:<n> chars を返す
    // （malformed / truncated なツール入力を可視化する）。"not json" は 8 文字。
    assert_eq!(extract_tool_detail("Read", "not json"), "unparsed:8 chars");
}

#[test]
fn extract_tool_detail_streaming_malformed_input_shows_unparsed() {
    // 実データで観測したケース: モデルが不正な JSON をツール入力として出力すると
    // ストリーミングで蓄積した partial_json がパースできず、従来は詳細空だった。
    // 修正後は生入力の文字数を unparsed として補足する。
    let input = r#"{"file_path": "/tmp/a.ts", "offset": 1, 110, "limit": 40}"#;
    assert_eq!(
        extract_tool_detail("Read", input),
        format!("unparsed:{} chars", input.chars().count())
    );
}

#[test]
fn extract_tool_detail_truncated_input_shows_unparsed() {
    // レート制限・セッション切断でツール呼び出しがストリーム途中で切れた場合、
    // 蓄積した JSON は途中までしか無くパースできない。これも可視化する。
    let input = r#"{"command": "cargo te"#;
    assert_eq!(
        extract_tool_detail("Bash", input),
        format!("unparsed:{} chars", input.chars().count())
    );
}

#[test]
fn extract_tool_detail_empty_input_stays_empty() {
    // 引数なしツール（入力が空文字／空白のみ）は従来どおり空表示を維持し、
    // unparsed 補足を出さない。
    assert_eq!(extract_tool_detail("TaskList", ""), "");
    assert_eq!(extract_tool_detail("Read", "   "), "");
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
fn extract_tool_detail_tavily_search_underscore_variant() {
    // 実データでハイフン版とアンダースコア版の両方が観測されたため、
    // mcp__tavily__tavily_search（アンダースコア版）でも同じ詳細を抽出すること。
    let input = r#"{"query":"Bevy 0.18 release notes","max_results":3}"#;
    assert_eq!(
        extract_tool_detail("mcp__tavily__tavily_search", input),
        "Bevy 0.18 release notes (max=3)"
    );
}

#[test]
fn extract_tool_detail_tavily_extract_underscore_variant() {
    // tavily-extract のアンダースコア版も同じく対応する
    let input = r#"{"urls":["https://example.com/spec"],"extract_depth":"basic"}"#;
    assert_eq!(
        extract_tool_detail("mcp__tavily__tavily_extract", input),
        "https://example.com/spec (depth:basic)"
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
fn extract_tool_detail_agent_shows_model_and_isolation() {
    let input = r#"{"description":"Review","model":"opus","isolation":"worktree"}"#;
    assert_eq!(
        extract_tool_detail("Agent", input),
        "Review (model:opus, isolation:worktree)"
    );
}

#[test]
fn extract_tool_detail_agent_omits_empty_model_and_isolation() {
    let input = r#"{"description":"Review","model":"","isolation":""}"#;
    assert_eq!(extract_tool_detail("Agent", input), "Review");
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

#[test]
fn bash_output_shows_bash_id() {
    // BashOutput は background bash の id を表示する（旧実装では generic
    // フォールバックの候補キーに bash_id が無く空表示になっていた）。
    let detail = extract_tool_detail("BashOutput", r#"{"bash_id":"bbb497"}"#);
    assert_eq!(detail, "bash:bbb497");
}

#[test]
fn bash_output_includes_filter() {
    let detail = extract_tool_detail("BashOutput", r#"{"bash_id":"da3d96","filter":"error"}"#);
    assert_eq!(detail, "bash:da3d96 (filter:error)");
}

#[test]
fn bash_output_without_bash_id_is_empty() {
    // bash_id が無い入力は generic フォールバックへ委ねられ、候補キーに
    // bash_id が無いため空表示になる。
    let detail = extract_tool_detail("BashOutput", r#"{}"#);
    assert_eq!(detail, "");
}

#[test]
fn slash_command_shows_command() {
    let detail = extract_tool_detail("SlashCommand", r#"{"command":"/get-md is:pr 123"}"#);
    assert_eq!(detail, "/get-md is:pr 123");
}

#[test]
fn slash_command_without_command_is_empty() {
    let detail = extract_tool_detail("SlashCommand", r#"{}"#);
    assert_eq!(detail, "");
}

#[test]
fn workflow_named_shows_name() {
    // 保存済みワークフローを名前で実行するケースは name を表示する。
    let detail = extract_tool_detail("Workflow", r#"{"name":"find-flaky-tests"}"#);
    assert_eq!(detail, "find-flaky-tests");
}

#[test]
fn workflow_inline_script_extracts_meta_name() {
    // インライン script は meta.name を抽出し、スクリプト規模も併記する。
    let input = r#"{"script":"export const meta = {\n  name: 'review-changes',\n  description: 'x',\n}\nphase('Review')"}"#;
    let detail = extract_tool_detail("Workflow", input);
    assert!(
        detail.starts_with("review-changes (script:"),
        "got: {detail}"
    );
    assert!(detail.ends_with("chars)"), "got: {detail}");
}

#[test]
fn workflow_inline_script_double_quoted_meta_name() {
    // meta.name がダブルクォートでも抽出できる。
    let input = r#"{"script":"export const meta = { name: \"audit\", description: \"d\" }"}"#;
    let detail = extract_tool_detail("Workflow", input);
    assert!(detail.starts_with("audit (script:"), "got: {detail}");
}

#[test]
fn workflow_inline_script_without_meta_name_shows_char_count() {
    // meta.name を抽出できない script はスクリプト文字数のみ表示する（空表示にしない）。
    let input = r#"{"script":"const x = 1; phase('go')"}"#;
    let detail = extract_tool_detail("Workflow", input);
    assert!(detail.starts_with("script:"), "got: {detail}");
    assert!(detail.ends_with("chars"), "got: {detail}");
}

#[test]
fn workflow_script_path_shows_basename() {
    // scriptPath 指定（再実行・resume）はファイル名を表示する。
    let input = r#"{"scriptPath":"/tmp/session/workflows/scripts/my-wf-wf_abc.js"}"#;
    let detail = extract_tool_detail("Workflow", input);
    assert_eq!(detail, "my-wf-wf_abc.js");
}

#[test]
fn workflow_empty_falls_back() {
    // script/name/scriptPath いずれも無ければ汎用フォールバックへ。
    let detail = extract_tool_detail("Workflow", r#"{"description":"fallback desc"}"#);
    assert_eq!(detail, "fallback desc");
}

/// Tavily 検索の topic / days は検索対象そのものを変えるため表示する。
/// 実データ: {"query":"CodeRabbit code review","topic":"news","days":8,"max_results":10}
#[test]
fn tavily_search_shows_topic_and_days() {
    let input = r#"{"query":"CodeRabbit code review","topic":"news","days":8,"max_results":10}"#;
    let detail = extract_tool_detail("mcp__tavily__tavily-search", input);
    assert!(detail.contains("topic=news"), "got: {detail}");
    assert!(detail.contains("days=8"), "got: {detail}");
    assert!(detail.contains("max=10"), "got: {detail}");
}

/// topic / days が無い通常検索では属性を増やさない。
#[test]
fn tavily_search_without_topic_omits_attrs() {
    let input = r#"{"query":"rust async","max_results":5}"#;
    let detail = extract_tool_detail("mcp__tavily__tavily-search", input);
    assert!(!detail.contains("topic="), "got: {detail}");
    assert!(!detail.contains("days="), "got: {detail}");
}

/// アンダースコア版のツール名でも同じ属性を表示する。
#[test]
fn tavily_search_underscore_variant_shows_topic() {
    let input = r#"{"query":"q","topic":"general","days":3}"#;
    let detail = extract_tool_detail("mcp__tavily__tavily_search", input);
    assert!(detail.contains("topic=general"), "got: {detail}");
    assert!(detail.contains("days=3"), "got: {detail}");
}
