//! イベント種別ごとの表示（実データのサンプル・各 item・JSON 以外の行・欠けたフィールド）のテスト。

use super::*;

// --- 実データのサンプル（codex-cli 0.160.1 / 0.161.0 の `codex exec --json`） ---

const THREAD_STARTED: &str =
    r#"{"type":"thread.started","thread_id":"01a1192f-7c1e-7d32-9a51-3f2b8c4d5e6f"}"#;
const TURN_STARTED: &str = r#"{"type":"turn.started"}"#;
const FILE_CHANGE_STARTED: &str = r#"{"type":"item.started","item":{"id":"item_1","type":"file_change","changes":[{"path":"/abs/repo/hello.txt","kind":"add"}],"status":"in_progress"}}"#;
const FILE_CHANGE_COMPLETED: &str = r#"{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[{"path":"/abs/repo/hello.txt","kind":"add"}],"status":"completed"}}"#;
const COMMAND_STARTED: &str = r#"{"type":"item.started","item":{"id":"item_3","type":"command_execution","command":"/bin/zsh -lc 'cat hello.txt'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
const COMMAND_COMPLETED: &str = r#"{"type":"item.completed","item":{"id":"item_3","type":"command_execution","command":"/bin/zsh -lc 'cat hello.txt'","aggregated_output":"hello\nworld\n","exit_code":0,"status":"completed"}}"#;
const AGENT_MESSAGE: &str = r#"{"type":"item.completed","item":{"id":"item_4","type":"agent_message","text":"hello.txt を作成しました。\n内容は 2 行です。"}}"#;
const TURN_COMPLETED: &str = r#"{"type":"turn.completed","usage":{"input_tokens":97951,"cached_input_tokens":80000,"cache_write_input_tokens":0,"output_tokens":158,"reasoning_output_tokens":0}}"#;

#[test]
fn real_sample_session_renders_one_line_per_event() {
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    let shown: String = [
        (THREAD_STARTED, 0),
        (TURN_STARTED, 100),
        (FILE_CHANGE_STARTED, 1_000),
        (FILE_CHANGE_COMPLETED, 1_200),
        (COMMAND_STARTED, 2_000),
        (COMMAND_COMPLETED, 2_400),
        (AGENT_MESSAGE, 3_000),
        (TURN_COMPLETED, 3_100),
    ]
    .into_iter()
    .map(|(line, at)| feed(&mut formatter, line, start + millis(at)))
    .collect();

    assert_eq!(
        shown,
        "  \u{2139} Codex session 01a1192f-7c1e-7d32-9a51-3f2b8c4d5e6f\n\
         \u{1f4dd} add hello.txt\n\
         \u{1f527} $ cat hello.txt\n  \
         \u{2713} $ cat hello.txt (0.4s) \u{2192} world\n\
         \u{1f4ac} hello.txt を作成しました。\n   \
         内容は 2 行です。\n\
         \u{1f4ca} in:97,951 (cached:80,000) out:158 [12:34:56]\n"
    );
}

#[test]
fn every_line_resets_its_color() {
    // 色が次の行へ漏れると、以降の行（JSON 以外の行や codex の stderr）まで色が付く。
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    for line in [
        THREAD_STARTED,
        COMMAND_STARTED,
        COMMAND_COMPLETED,
        FILE_CHANGE_COMPLETED,
        TURN_COMPLETED,
    ] {
        let raw = feed_bytes(&mut formatter, format!("{line}\n").as_bytes(), start);
        for shown in raw.lines() {
            assert!(shown.ends_with("\x1b[0m"), "unterminated color: {shown:?}");
        }
    }
}

// --- ターン・セッション・エラー ---

#[test]
fn turn_started_prints_nothing() {
    assert_eq!(render(&[TURN_STARTED]), "");
}

#[test]
fn thread_started_without_id_still_announces_the_session() {
    assert_eq!(
        render(&[r#"{"type":"thread.started"}"#]),
        "  \u{2139} Codex session\n"
    );
}

#[test]
fn turn_completed_shows_reasoning_and_omits_zero_cached() {
    let line = r#"{"type":"turn.completed","usage":{"input_tokens":1200,"cached_input_tokens":0,"output_tokens":3400,"reasoning_output_tokens":2048}}"#;
    assert_eq!(
        render(&[line]),
        "\u{1f4ca} in:1,200 out:3,400 (reasoning:2,048) [12:34:56]\n"
    );
}

#[test]
fn turn_completed_omits_absent_breakdowns() {
    let line = r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}"#;
    assert_eq!(render(&[line]), "\u{1f4ca} in:10 out:2 [12:34:56]\n");
}

#[test]
fn turn_completed_without_usage() {
    assert_eq!(
        render(&[r#"{"type":"turn.completed"}"#]),
        "\u{1f4ca} turn completed [12:34:56]\n"
    );
    assert_eq!(
        render(&[r#"{"type":"turn.completed","usage":null}"#]),
        "\u{1f4ca} turn completed [12:34:56]\n"
    );
}

#[test]
fn turn_failed_collapses_newlines() {
    let line =
        r#"{"type":"turn.failed","error":{"message":"stream disconnected\nbefore completion"}}"#;
    assert_eq!(
        render(&[line]),
        "  \u{274c} Turn failed: stream disconnected before completion [12:34:56]\n"
    );
}

#[test]
fn turn_failed_truncates_long_messages_by_chars() {
    let message = "あ".repeat(1000);
    let line = format!(r#"{{"type":"turn.failed","error":{{"message":"{message}"}}}}"#);
    let shown = render(&[&line]);
    let body = shown
        .strip_prefix("  \u{274c} Turn failed: ")
        .and_then(|rest| rest.strip_suffix(" [12:34:56]\n"))
        .expect("turn failed line");
    assert_eq!(body.chars().count(), 400);
    assert!(body.ends_with("..."));
}

#[test]
fn turn_failed_without_message_and_with_string_error() {
    assert_eq!(
        render(&[r#"{"type":"turn.failed"}"#]),
        "  \u{274c} Turn failed [12:34:56]\n"
    );
    assert_eq!(
        render(&[r#"{"type":"turn.failed","error":"plain string"}"#]),
        "  \u{274c} Turn failed: plain string [12:34:56]\n"
    );
}

#[test]
fn top_level_error_event() {
    assert_eq!(
        render(&[r#"{"type":"error","message":"Reconnecting... 2/5"}"#]),
        "  \u{274c} Error: Reconnecting... 2/5\n"
    );
    assert_eq!(render(&[r#"{"type":"error"}"#]), "  \u{274c} Error\n");
}

#[test]
fn unknown_top_level_type_shows_its_name() {
    assert_eq!(
        render(&[r#"{"type":"session.configured","model":"gpt-5"}"#]),
        "  \u{b7} session.configured\n"
    );
}

// --- コマンド実行 ---

#[test]
fn command_updated_does_not_print_accumulated_output() {
    let updated = r#"{"type":"item.updated","item":{"id":"item_3","type":"command_execution","command":"/bin/zsh -lc 'cargo build'","aggregated_output":"Compiling a\nCompiling b\n","exit_code":null,"status":"in_progress"}}"#;
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    feed(&mut formatter, COMMAND_STARTED, start);
    assert_eq!(feed(&mut formatter, updated, start + secs(1)), "");
    assert_eq!(feed(&mut formatter, updated, start + secs(2)), "");
}

#[test]
fn failed_command_shows_exit_code_and_last_three_lines() {
    let started = r#"{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"/bin/bash -lc 'cargo test'","status":"in_progress"}}"#;
    let completed = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"/bin/bash -lc 'cargo test'","aggregated_output":"running 3 tests\nline2\n\nerror: a\nerror: b\nerror: c\n\n","exit_code":101,"status":"failed"}}"#;
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    assert_eq!(
        feed(&mut formatter, started, start),
        "\u{1f527} $ cargo test\n"
    );
    assert_eq!(
        feed(&mut formatter, completed, start + millis(12_345)),
        "  \u{2717} exit 101 $ cargo test (12.3s)\n    \u{2502} error: a\n    \u{2502} error: b\n    \u{2502} error: c\n"
    );
}

#[test]
fn failed_command_with_short_output_shows_what_there_is() {
    let completed = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"false","aggregated_output":"only line","exit_code":1,"status":"failed"}}"#;
    assert_eq!(
        render(&[completed]),
        "  \u{2717} exit 1 $ false\n    \u{2502} only line\n"
    );
}

#[test]
fn completed_command_with_null_exit_code_is_a_failure() {
    let completed = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"sleep 100","aggregated_output":"","exit_code":null,"status":"completed"}}"#;
    assert_eq!(render(&[completed]), "  \u{2717} exit ? $ sleep 100\n");
}

#[test]
fn failed_status_with_zero_exit_code_is_a_failure() {
    let completed = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"true","aggregated_output":"","exit_code":0,"status":"failed"}}"#;
    assert_eq!(render(&[completed]), "  \u{2717} exit 0 $ true\n");
}

#[test]
fn declined_command() {
    let completed = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"/bin/zsh -lc 'rm -rf build'","aggregated_output":"","exit_code":null,"status":"declined"}}"#;
    assert_eq!(render(&[completed]), "  \u{2298} declined $ rm -rf build\n");
}

#[test]
fn completed_command_without_a_start_omits_elapsed_time() {
    let completed = r#"{"type":"item.completed","item":{"id":"never-started","type":"command_execution","command":"/bin/zsh -lc ls","aggregated_output":"","exit_code":0,"status":"completed"}}"#;
    assert_eq!(render(&[completed]), "  \u{2713} $ ls\n");
}

#[test]
fn successful_command_summary_is_the_sanitized_last_output_line() {
    let completed = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"make","aggregated_output":"first\n\u001b[32mok\u001b[0m\n\n  \n","exit_code":0,"status":"completed"}}"#;
    assert_eq!(render(&[completed]), "  \u{2713} $ make \u{2192} ok\n");
}

#[test]
fn successful_command_summary_follows_carriage_return_overwrites() {
    let completed = r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"curl -O x","aggregated_output":"Downloading 10%\rDownloading 55%\rDownloading 100%\n","exit_code":0,"status":"completed"}}"#;
    assert_eq!(
        render(&[completed]),
        "  \u{2713} $ curl -O x \u{2192} Downloading 100%\n"
    );
}

#[test]
fn command_lines_are_truncated_to_their_widths() {
    let long = "x".repeat(300);
    let started = format!(
        r#"{{"type":"item.started","item":{{"id":"c1","type":"command_execution","command":"/bin/zsh -lc 'echo {long}'","status":"in_progress"}}}}"#
    );
    let completed = format!(
        r#"{{"type":"item.completed","item":{{"id":"c1","type":"command_execution","command":"/bin/zsh -lc 'echo {long}'","aggregated_output":"{long}\n","exit_code":0,"status":"completed"}}}}"#
    );
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    let started_line = feed(&mut formatter, &started, start);
    let command = started_line
        .strip_prefix("\u{1f527} $ ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .expect("started line");
    assert_eq!(command.chars().count(), 160);
    assert!(command.starts_with("echo xxx") && command.ends_with("..."));

    let completed_line = feed(&mut formatter, &completed, start + millis(400));
    let (head, summary) = completed_line
        .trim_end()
        .split_once(" (0.4s) \u{2192} ")
        .expect("completed line");
    let command = head
        .strip_prefix("  \u{2713} $ ")
        .expect("completed prefix");
    assert_eq!(command.chars().count(), 80);
    assert_eq!(summary.chars().count(), 100);
}

#[test]
fn heredoc_command_is_shown_on_one_line() {
    let started = r#"{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"/bin/zsh -lc \"cat > notes.md <<'EOF'\n# Title\n\nbody\nEOF\"","status":"in_progress"}}"#;
    assert_eq!(
        render(&[started]),
        "\u{1f527} $ cat > notes.md <<'EOF' # Title body EOF\n"
    );
}

// --- ファイル変更 ---

#[test]
fn file_change_lists_each_change_relative_inside_cwd_and_absolute_outside() {
    let completed = r#"{"type":"item.completed","item":{"id":"p1","type":"file_change","changes":[{"path":"/abs/repo/src/main.rs","kind":"update"},{"path":"/abs/repo/old.txt","kind":"delete"},{"path":"/elsewhere/notes.md","kind":"add"},{"path":"relative/given.rs","kind":"update"}],"status":"completed"}}"#;
    assert_eq!(
        render(&[completed]),
        "\u{1f4dd} update src/main.rs\n\
         \u{1f4dd} delete old.txt\n\
         \u{1f4dd} add /elsewhere/notes.md\n\
         \u{1f4dd} update relative/given.rs\n"
    );
}

#[test]
fn file_change_does_not_treat_a_sibling_directory_as_inside_cwd() {
    // `/abs/repository` は `/abs/repo` の配下ではない（文字列の前方一致で判定しない）。
    let completed = r#"{"type":"item.completed","item":{"id":"p1","type":"file_change","changes":[{"path":"/abs/repository/a.rs","kind":"add"}],"status":"completed"}}"#;
    assert_eq!(render(&[completed]), "\u{1f4dd} add /abs/repository/a.rs\n");
}

#[test]
fn file_change_prints_nothing_until_completed() {
    let updated = r#"{"type":"item.updated","item":{"id":"item_1","type":"file_change","changes":[{"path":"/abs/repo/hello.txt","kind":"add"}],"status":"in_progress"}}"#;
    assert_eq!(render(&[FILE_CHANGE_STARTED, updated]), "");
}

#[test]
fn failed_and_declined_patches() {
    let failed = r#"{"type":"item.completed","item":{"id":"p1","type":"file_change","changes":[{"path":"/abs/repo/src/a.rs","kind":"update"},{"path":"/elsewhere/b.rs","kind":"add"}],"status":"failed"}}"#;
    assert_eq!(
        render(&[failed]),
        "  \u{2717} patch failed: src/a.rs, /elsewhere/b.rs\n"
    );
    let declined = r#"{"type":"item.completed","item":{"id":"p2","type":"file_change","changes":[{"path":"/abs/repo/a.rs","kind":"add"}],"status":"declined"}}"#;
    assert_eq!(render(&[declined]), "  \u{2298} patch declined: a.rs\n");
    let failed_without_paths =
        r#"{"type":"item.completed","item":{"id":"p3","type":"file_change","status":"failed"}}"#;
    assert_eq!(render(&[failed_without_paths]), "  \u{2717} patch failed\n");
}

#[test]
fn file_change_never_prints_a_diff() {
    // 人間向け出力の累積 diff がログの 9 割を占めていた。diff らしいフィールドが
    // 付いていても表示しない。
    let completed = r#"{"type":"item.completed","item":{"id":"p1","type":"file_change","changes":[{"path":"/abs/repo/a.rs","kind":"update","unified_diff":"@@ -1 +1 @@\n-old\n+new\n"}],"status":"completed"}}"#;
    assert_eq!(render(&[completed]), "\u{1f4dd} update a.rs\n");
}

// --- 応答本文・推論要約 ---

#[test]
fn agent_message_indents_following_lines_and_keeps_blank_lines() {
    let line = r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"\nFirst line\n\nThird line  \n- bullet\n"}}"#;
    assert_eq!(
        render(&[line]),
        "\u{1f4ac} First line\n\n   Third line\n   - bullet\n"
    );
}

#[test]
fn agent_message_is_not_truncated() {
    let text = "長い説明".repeat(500);
    let line = format!(
        r#"{{"type":"item.completed","item":{{"id":"m1","type":"agent_message","text":"{text}"}}}}"#
    );
    assert_eq!(render(&[&line]), format!("\u{1f4ac} {text}\n"));
}

#[test]
fn agent_message_strips_escape_sequences_and_controls() {
    let line = r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"\u001b[31mred\u001b[0m and \u0007bell\r\nnext\u001b]0;title\u0007 line"}}"#;
    let raw = {
        let start = Instant::now();
        let mut formatter = new_formatter(start);
        feed_bytes(&mut formatter, format!("{line}\n").as_bytes(), start)
    };
    assert_eq!(raw, "\u{1f4ac} red and bell\n   next line\n");
}

#[test]
fn agent_message_prints_only_on_completion() {
    let started =
        r#"{"type":"item.started","item":{"id":"m1","type":"agent_message","text":"partial"}}"#;
    let updated = r#"{"type":"item.updated","item":{"id":"m1","type":"agent_message","text":"partial text"}}"#;
    let empty =
        r#"{"type":"item.completed","item":{"id":"m2","type":"agent_message","text":"  \n "}}"#;
    assert_eq!(render(&[started, updated, empty]), "");
}

#[test]
fn reasoning_is_collapsed_to_one_truncated_line() {
    let line = r#"{"type":"item.completed","item":{"id":"r1","type":"reasoning","text":"**Planning edits**\n\nI will read the files first."}}"#;
    assert_eq!(
        render(&[line]),
        "\u{1f4ad} **Planning edits** I will read the files first.\n"
    );

    let long = "考".repeat(500);
    let line = format!(
        r#"{{"type":"item.completed","item":{{"id":"r2","type":"reasoning","text":"{long}"}}}}"#
    );
    let shown = render(&[&line]);
    let body = shown
        .strip_prefix("\u{1f4ad} ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .expect("reasoning line");
    assert_eq!(body.chars().count(), 200);
}

#[test]
fn reasoning_without_text_or_before_completion_prints_nothing() {
    assert_eq!(
        render(&[
            r#"{"type":"item.completed","item":{"id":"r1","type":"reasoning","text":""}}"#,
            r#"{"type":"item.completed","item":{"id":"r2","type":"reasoning"}}"#,
            r#"{"type":"item.started","item":{"id":"r3","type":"reasoning","text":"thinking"}}"#,
        ]),
        ""
    );
}

// --- MCP・検索・計画・サブエージェント・エラー ---

#[test]
fn mcp_tool_call_started_and_completed() {
    let started = r#"{"type":"item.started","item":{"id":"t1","type":"mcp_tool_call","server":"context7","tool":"resolve-library-id","status":"in_progress"}}"#;
    let completed = r#"{"type":"item.completed","item":{"id":"t1","type":"mcp_tool_call","server":"context7","tool":"resolve-library-id","status":"completed"}}"#;
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    assert_eq!(
        feed(&mut formatter, started, start),
        "\u{1f50c} context7.resolve-library-id\n"
    );
    assert_eq!(
        feed(&mut formatter, completed, start + millis(1_500)),
        "  \u{2713} context7.resolve-library-id (1.5s)\n"
    );
}

#[test]
fn mcp_tool_call_failure_shows_the_error_message() {
    let failed = r#"{"type":"item.completed","item":{"id":"t1","type":"mcp_tool_call","server":"tavily","tool":"search","status":"failed","error":{"message":"request timed out\nafter 60s"}}}"#;
    assert_eq!(
        render(&[failed]),
        "  \u{2717} tavily.search: request timed out after 60s\n"
    );
    let failed_without_message = r#"{"type":"item.completed","item":{"id":"t2","type":"mcp_tool_call","server":"tavily","tool":"search","status":"failed"}}"#;
    assert_eq!(
        render(&[failed_without_message]),
        "  \u{2717} tavily.search (failed)\n"
    );
}

#[test]
fn web_search_with_and_without_query() {
    assert_eq!(
        render(&[
            r#"{"type":"item.started","item":{"id":"w1","type":"web_search","query":"rust mpsc"}}"#,
            r#"{"type":"item.completed","item":{"id":"w1","type":"web_search","query":"rust mpsc recv_timeout"}}"#,
            r#"{"type":"item.completed","item":{"id":"w2","type":"web_search"}}"#,
        ]),
        "\u{1f50e} rust mpsc recv_timeout\n\u{1f50e} web search\n"
    );
}

#[test]
fn todo_list_prints_only_when_progress_or_next_item_changes() {
    let plan = |phase: &str, done: [bool; 3]| {
        format!(
            r#"{{"type":"item.{phase}","item":{{"id":"plan","type":"todo_list","items":[{{"text":"Read the code","completed":{}}},{{"text":"Fix the bug","completed":{}}},{{"text":"Run the tests","completed":{}}}]}}}}"#,
            done[0], done[1], done[2]
        )
    };
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    assert_eq!(
        feed(&mut formatter, &plan("started", [false; 3]), start),
        "\u{1f4cb} Plan 0/3 \u{2014} next: Read the code\n"
    );
    assert_eq!(
        feed(&mut formatter, &plan("updated", [false; 3]), start),
        ""
    );
    assert_eq!(
        feed(
            &mut formatter,
            &plan("updated", [true, false, false]),
            start
        ),
        "\u{1f4cb} Plan 1/3 \u{2014} next: Fix the bug\n"
    );
    // 完了数が同じでも、次の項目が変われば出す。
    assert_eq!(
        feed(
            &mut formatter,
            &plan("updated", [false, true, false]),
            start
        ),
        "\u{1f4cb} Plan 1/3 \u{2014} next: Read the code\n"
    );
    assert_eq!(
        feed(&mut formatter, &plan("completed", [true; 3]), start),
        "\u{1f4cb} Plan 3/3 \u{2014} all done\n"
    );
    assert_eq!(
        feed(&mut formatter, &plan("completed", [true; 3]), start),
        ""
    );
}

#[test]
fn todo_list_progress_is_tracked_per_item_id() {
    let one = r#"{"type":"item.started","item":{"id":"a","type":"todo_list","items":[{"text":"Step","completed":false}]}}"#;
    let other = r#"{"type":"item.started","item":{"id":"b","type":"todo_list","items":[{"text":"Step","completed":false}]}}"#;
    let empty = r#"{"type":"item.started","item":{"id":"c","type":"todo_list","items":[]}}"#;
    assert_eq!(
        render(&[one, other, empty]),
        "\u{1f4cb} Plan 0/1 \u{2014} next: Step\n\u{1f4cb} Plan 0/1 \u{2014} next: Step\n"
    );
}

#[test]
fn collab_tool_call_started_completed_and_failed() {
    let started = r#"{"type":"item.started","item":{"id":"a1","type":"collab_tool_call","tool":"spawn_agent","prompt":"Investigate the failing test\nand report back","status":"in_progress","receiver_thread_ids":[]}}"#;
    let completed = r#"{"type":"item.completed","item":{"id":"a1","type":"collab_tool_call","tool":"spawn_agent","status":"completed","receiver_thread_ids":["019a"]}}"#;
    let wait_failed = r#"{"type":"item.completed","item":{"id":"a2","type":"collab_tool_call","tool":"wait","status":"failed"}}"#;
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    assert_eq!(
        feed(&mut formatter, started, start),
        "\u{1f91d} spawn_agent Investigate the failing test and report back\n"
    );
    assert_eq!(
        feed(&mut formatter, completed, start + secs(125)),
        "  \u{2713} spawn_agent (2m 05s)\n"
    );
    assert_eq!(
        feed(&mut formatter, wait_failed, start + secs(130)),
        "  \u{2717} wait (failed)\n"
    );
}

#[test]
fn error_item_is_a_warning_line() {
    assert_eq!(
        render(&[
            r#"{"type":"item.completed","item":{"id":"e1","type":"error","message":"model fell back to a smaller context"}}"#
        ]),
        "  \u{26a0} model fell back to a smaller context\n"
    );
}

#[test]
fn unknown_item_type_is_named_on_completion_only() {
    assert_eq!(
        render(&[
            r#"{"type":"item.started","item":{"id":"x1","type":"image_generation"}}"#,
            r#"{"type":"item.updated","item":{"id":"x1","type":"image_generation"}}"#,
            r#"{"type":"item.completed","item":{"id":"x1","type":"image_generation"}}"#,
        ]),
        "  \u{b7} image_generation\n"
    );
}

// --- JSON 以外の行・壊れた入力 ---

#[test]
fn non_json_lines_pass_through() {
    assert_eq!(
        render(&[
            "Reading additional input from stdin...",
            "WARNING: proceeding, even though we could not update PATH",
        ]),
        "Reading additional input from stdin...\n\
         WARNING: proceeding, even though we could not update PATH\n"
    );
}

#[test]
fn json_without_a_string_type_passes_through() {
    assert_eq!(
        render(&[r#"{"foo":1}"#, "[1,2]", r#"{"type":5}"#, "42"]),
        "{\"foo\":1}\n[1,2]\n{\"type\":5}\n42\n"
    );
}

#[test]
fn non_json_lines_are_sanitized() {
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    let raw = feed_bytes(
        &mut formatter,
        b"\x1b[1mWARN\x1b[0m: \x1b]0;pane title\x07disk\x01 low\x7f\t(90%)\n",
        start,
    );
    // 色も含めて制御文字が 1 つも残らない（整形器自身も JSON 以外の行には色を付けない）。
    assert_eq!(raw, "WARN: disk low\t(90%)\n");
}

#[test]
fn a_corrupted_giant_event_is_not_dumped_whole() {
    // 標準エラーの書き込みが巨大な JSON 行の途中に割り込むと、その行は JSON として
    // 読めなくなる。数百 KB をそのままペインへ流さない。
    let output = "z".repeat(300_000);
    let broken = format!(
        r#"{{"type":"item.completed","item":{{"id":"c1","type":"command_execution","aggregated_output":"{output}WARN stderr cut in"#
    );
    let shown = render(&[&broken]);
    let (kept, suffix) = shown.split_once('\u{2026}').expect("truncation marker");
    assert_eq!(kept.chars().count(), 2_000);
    assert!(kept.starts_with(r#"{"type":"item.completed""#));
    let omitted = broken.chars().count() - 2_000;
    assert_eq!(
        suffix,
        format!(
            " (+{} chars)\n",
            crate::format_stream::util::format_number(omitted as u64)
        )
    );
}

#[test]
fn ordinary_long_stderr_lines_are_not_truncated() {
    let line = format!(
        "ERROR: unexpected status 400 Bad Request: {}",
        "e".repeat(1_900)
    );
    assert_eq!(render(&[&line]), format!("{line}\n"));
}

#[test]
fn blank_lines_print_nothing() {
    assert_eq!(render(&["", "   ", "\t"]), "");
}

#[test]
fn invalid_utf8_is_shown_with_replacement_characters() {
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    assert_eq!(
        feed_bytes(&mut formatter, b"bad \xff\xfe bytes\n", start),
        "bad \u{fffd}\u{fffd} bytes\n"
    );
}

#[test]
fn crlf_terminated_events_are_parsed() {
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    let shown = strip_sgr(&feed_bytes(
        &mut formatter,
        format!("{TURN_COMPLETED}\r\n").as_bytes(),
        start,
    ));
    assert_eq!(
        shown,
        "\u{1f4ca} in:97,951 (cached:80,000) out:158 [12:34:56]\n"
    );
}

#[test]
fn missing_and_odd_fields_do_not_panic() {
    let shown = render(&[
        r#"{"type":"item.completed"}"#,
        r#"{"type":"item.started","item":null}"#,
        r#"{"type":"item.updated","item":"text"}"#,
        r#"{"type":"item.completed","item":{"type":"command_execution"}}"#,
        r#"{"type":"item.completed","item":{"type":"command_execution","id":"x","exit_code":"zero","aggregated_output":5}}"#,
        r#"{"type":"item.started","item":{"type":"command_execution","command":42}}"#,
        r#"{"type":"item.completed","item":{"type":"file_change","changes":"oops"}}"#,
        r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"kind":{"type":"update"}},7]}}"#,
        r#"{"type":"item.updated","item":{"type":"todo_list","items":[{"text":5},"x"]}}"#,
        r#"{"type":"item.completed","item":{"type":"mcp_tool_call","status":"failed","error":{}}}"#,
        r#"{"type":"item.completed","item":{"type":"collab_tool_call","status":7}}"#,
        r#"{"type":"item.completed","item":{"type":"web_search","query":["a"]}}"#,
        r#"{"type":"turn.completed","usage":"x"}"#,
        r#"{"type":"turn.completed","usage":{"input_tokens":-5,"output_tokens":1.5}}"#,
        r#"{"type":"turn.failed","error":{"message":null}}"#,
        r#"{"type":"thread.started","thread_id":{"nested":true}}"#,
    ]);
    // 種類が分かるものは種類なりの見出しで出る（欠けた値は ? や既定の文言で埋める）。
    assert_eq!(
        shown,
        "  \u{b7} item\n\
         \x20 \u{2717} exit ? $ ?\n\
         \x20 \u{2717} exit ? $ ?\n\
         \u{1f527} $ ?\n\
         \u{1f4dd} update ?\n\
         \u{1f4dd} change ?\n\
         \u{1f4cb} Plan 0/2\n\
         \x20 \u{2717} mcp.? (failed)\n\
         \x20 \u{2713} collab\n\
         \u{1f50e} web search\n\
         \u{1f4ca} turn completed [12:34:56]\n\
         \u{1f4ca} in:0 out:0 [12:34:56]\n\
         \x20 \u{274c} Turn failed [12:34:56]\n\
         \x20 \u{2139} Codex session\n"
    );
}
