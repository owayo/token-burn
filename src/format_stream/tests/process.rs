//! `process` のストリーム処理フロー全般（テキスト/思考/ツール使用/通知/フック等）のテスト。

use super::*;

#[test]
fn process_text_only_response() {
    // 思考ブロックなしの単純なテキスト応答（例: "say hello"）
    let input = [
            r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"s1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","id":"msg_1"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world!"}}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hello world!"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            r#"{"type":"result","subtype":"success","total_cost_usd":0.2148,"duration_ms":5191,"usage":{"input_tokens":3,"cache_read_input_tokens":14726,"output_tokens":45}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("Hello world!"));
    assert!(clean.contains("$0.2148"));
    assert!(clean.contains("0m 5s"));
    assert!(clean.contains("in:14,729 out:45"));
    // system/assistant/rate_limit は表示しない
    assert!(!clean.contains("init"));
    assert!(!clean.contains("rate_limit"));
}

#[test]
fn process_thinking_then_tool_use() {
    // 思考ブロック → ツール使用 → ツール結果 → テキスト応答
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me analyze this code..."}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"/src/main.rs\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"fn main() {}"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Found the file."}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    // 思考インジケーター
    assert!(clean.contains("\u{1f4ad}"));
    // ツール名とファイルパス
    assert!(clean.contains("\u{1f527} Read"));
    assert!(clean.contains("/src/main.rs"));
    // ツール結果にツール名が表示される
    assert!(
        clean.contains("\u{2713} Read"),
        "expected '✓ Read' in: {}",
        clean
    );
    // テキスト応答
    assert!(clean.contains("Found the file."));
}

#[test]
fn process_non_json_passthrough() {
    let input = "plain text line\nanother line\n";
    let output = run_process(input);
    assert!(output.contains("plain text line"));
    assert!(output.contains("another line"));
}

#[test]
fn process_text_then_tool_use_inserts_newline() {
    // テキストブロックが改行なしで終わった直後にツール使用が来ても、
    // 同じ行に "...します。🔧 WebFetch" のように連結されないこと。
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"公式ドキュメントを並列で取得します。"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t_wf","name":"WebFetch","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"url\":\"https://example.com\",\"prompt\":\"summarize\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        !clean.contains("します。\u{1f527} WebFetch"),
        "text and tool emoji must not be on the same line: {}",
        clean
    );
    assert!(
        clean.contains("します。\n"),
        "expected newline after text block: {}",
        clean
    );
    assert!(clean.contains("\u{1f527} WebFetch"));
}

#[test]
fn process_text_already_ending_with_newline_no_double_newline() {
    // テキスト自体が改行で終わっている場合は改行を二重に出さない。
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"line1\n"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/x.rs\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.contains("line1\n\n\u{1f527}"),
        "double newline detected: {clean:?}"
    );
    assert!(clean.contains("line1\n\u{1f527} Read"));
}

#[test]
fn process_bash_tool_with_description() {
    // 実際の Bash ツールと同じ入力形式
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t_bash","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pnpm install\",\"description\":\"Install dependencies\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_bash","type":"tool_result","content":"+ typescript 5.9.3"}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{1f527} Bash"),
        "expected Bash tool icon in: {}",
        clean
    );
    assert!(
        clean.contains("pnpm install"),
        "expected command in: {}",
        clean
    );
    assert!(
        clean.contains("(Install dependencies)"),
        "expected description in: {}",
        clean
    );
}

#[test]
fn process_multi_turn_read_edit_bash() {
    // Read → Edit → Bash の複数ツール連続実行
    let input = [
            // 1ターン目: Read
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/index.ts\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"export function add(a, b) { return a + b; }"}]}}"#,
            // 2ターン目: Edit
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t2","name":"Edit","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/index.test.ts\",\"old_string\":\"test1\",\"new_string\":\"test1\\ntest2\\ntest3\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t2","type":"tool_result","content":"Updated successfully."}]}}"#,
            // 3ターン目: Bash
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t3","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"pnpm exec tsc --noEmit\",\"description\":\"Type check\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t3","type":"tool_result","content":""}]}}"#,
            // 最終テキスト
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            // 結果
            r#"{"type":"result","total_cost_usd":0.55,"duration_ms":41000,"num_turns":4,"usage":{"input_tokens":10,"cache_read_input_tokens":1000,"output_tokens":100}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    // Read
    assert!(clean.contains("\u{1f527} Read"));
    assert!(clean.contains("/src/index.ts"));
    assert!(clean.contains("\u{2713} Read"));
    // Edit
    assert!(clean.contains("\u{1f527} Edit"));
    assert!(clean.contains("/src/index.test.ts"));
    assert!(clean.contains("(+2/-0)"));
    assert!(clean.contains("\u{2713} Edit"));
    // Bash
    assert!(clean.contains("\u{1f527} Bash"));
    assert!(clean.contains("pnpm exec tsc --noEmit"));
    assert!(clean.contains("(Type check)"));
    assert!(clean.contains("\u{2713} Bash"));
    // テキスト
    assert!(clean.contains("Done."));
    // 結果
    assert!(clean.contains("(4 turns)"));
}

#[test]
fn process_system_task_started_shows_description() {
    // 実データに出る task_started は開始通知として表示する
    let input = [
            r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"s1"}"#,
            r#"{"type":"system","subtype":"task_started","task_type":"in_process_teammate","task_id":"abc-123","tool_use_id":"tu_1","description":"implement feature"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("implement feature"));
    assert!(clean.contains("in_process_teammate"));
    // テキストは表示される
    assert!(clean.contains("Hello"));
}

#[test]
fn process_team_create_then_task_spawn() {
    // TeamCreate → Task の順でチームエージェントを起動する流れ
    let input = [
            // チームを作成
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tc1","name":"TeamCreate","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"team_name\":\"demo-team\",\"description\":\"Build demo project\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tc1","type":"tool_result","content":"Team created"}]}}"#,
            // Task 起動
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"ts1","name":"Task","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"implement utils\",\"name\":\"worker-1\",\"subagent_type\":\"general-purpose\",\"team_name\":\"demo-team\",\"prompt\":\"Create utility module\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"ts1","type":"tool_result","content":"{\"status\":\"teammate_spawned\"}"}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    // チーム作成ツール
    assert!(
        clean.contains("\u{1f527} TeamCreate"),
        "expected TeamCreate tool in: {}",
        clean
    );
    assert!(
        clean.contains("demo-team"),
        "expected team name in: {}",
        clean
    );
    assert!(
        clean.contains("\u{2713} TeamCreate"),
        "expected checkmark for TeamCreate in: {}",
        clean
    );
    // Task
    assert!(
        clean.contains("\u{1f527} Task"),
        "expected Task tool in: {}",
        clean
    );
    assert!(
        clean.contains("worker-1 (general-purpose)"),
        "expected agent name and type in: {}",
        clean
    );
    assert!(
        clean.contains("\u{2713} Task"),
        "expected checkmark for Task in: {}",
        clean
    );
}

#[test]
fn process_subagent_tool_uses_from_assistant_message() {
    // サブエージェントの tool_use は stream_event ではなく assistant message として届く。
    // assistant message 内の tool_use に parent_tool_use_id が入り、
    // 後続の user message の tool_result がその ID を参照する。
    let input = [
            r#"{"type":"system","subtype":"init"}"#,
            // tool_use を 2 つ含む assistant message
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"sub_read_1","name":"Read","input":{"file_path":"/src/lib.rs"},"parent_tool_use_id":"task_1"},{"type":"tool_use","id":"sub_glob_2","name":"Glob","input":{"pattern":"**/*.rs"},"parent_tool_use_id":"task_1"}]}}"#,
            // 2 つの tool_result を返す user message
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"sub_read_1","content":"file contents"},{"type":"tool_result","tool_use_id":"sub_glob_2","content":"src/lib.rs\nsrc/main.rs"}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    // どちらの結果も "?" ではなく正しいツール名を表示する
    assert!(
        clean.contains("\u{2713} Read"),
        "expected '✓ Read' but got: {}",
        clean
    );
    assert!(
        clean.contains("\u{2713} Glob"),
        "expected '✓ Glob' but got: {}",
        clean
    );
    assert!(
        !clean.contains("\u{2713} ?"),
        "should not contain '✓ ?' fallback: {}",
        clean
    );
}

#[test]
fn process_input_json_delta_before_block_start() {
    // 実装によっては delta が start より先に見えることがあるため、
    // index 単位で入力断片を保持して後続 start と結合する。
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"cargo test\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("\u{1f527} Bash"), "got: {}", clean);
    assert!(clean.contains("cargo test"), "got: {}", clean);
    assert!(clean.contains("\u{2713} Bash"), "got: {}", clean);
}

#[test]
fn process_message_stop_flushes_open_tool_use() {
    // content_block_stop が欠けても message_stop で未完了 block を確定する。
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/tmp/demo.txt\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("\u{1f527} Read"), "got: {}", clean);
    assert!(clean.contains("/tmp/demo.txt"), "got: {}", clean);
}

#[test]
fn process_server_tool_use() {
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv1","name":"WebFetch","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"url\":\"https://example.com\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"srv1","content":"ok"}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("\u{1f527} WebFetch"), "got: {}", clean);
    assert!(clean.contains("https://example.com"), "got: {}", clean);
    assert!(clean.contains("\u{2713} WebFetch"), "got: {}", clean);
}

#[test]
fn process_writes_raw_stream_log() {
    let dir = tempfile::tempdir().unwrap();
    let raw_path = dir.path().join("raw.jsonl");
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            "",
            "plain text line",
        ]
        .join("\n");

    let _ = run_process_with_raw_log(&input, Some(&raw_path));
    let raw = std::fs::read_to_string(&raw_path).unwrap();

    assert!(raw.contains("\"content_block_start\""), "got: {}", raw);
    assert!(raw.contains("\"text_delta\""), "got: {}", raw);
    assert!(raw.contains("plain text line"), "got: {}", raw);
    assert!(raw.lines().count() >= 4, "got: {}", raw);
}

// --- truncate_str の単体テスト ---

#[test]
fn truncate_str_short_string_unchanged() {
    assert_eq!(truncate_str("hello", 10), "hello");
}

#[test]
fn truncate_str_exact_length_unchanged() {
    assert_eq!(truncate_str("abcde", 5), "abcde");
}

#[test]
fn truncate_str_long_string_truncated() {
    assert_eq!(truncate_str("abcdefghij", 7), "abcd...");
}

#[test]
fn truncate_str_multibyte_counts_chars() {
    // 日本語 5 文字は 15 バイトでも、文字数としては 5 として扱う
    let s = "あいうえお";
    assert_eq!(truncate_str(s, 5), "あいうえお");
    assert_eq!(truncate_str(s, 4), "あ...");
}

#[test]
fn process_empty_tool_input_on_block_stop() {
    // tool_input が空（partial_json なし）で content_block_stop が来た場合
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Read","id":"t1"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop"}}"#,
        ]
        .join("\n");
    let output = run_process(&input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("Read"), "ツール名が表示されるべき");
}

#[test]
fn process_unknown_tool_result_id() {
    // tool_id_map に存在しない ID の tool_result
    let input = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"unknown-id","content":"ok"}]}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("?"), "不明なツールは '?' と表示されるべき");
}

#[test]
fn truncate_str_empty_string() {
    assert_eq!(truncate_str("", 10), "");
}

#[test]
fn truncate_str_max_three() {
    // max=3 の場合、3文字以下は変化なし、4文字以上は "..." のみ
    assert_eq!(truncate_str("abc", 3), "abc");
    assert_eq!(truncate_str("abcd", 3), "...");
}

#[test]
fn truncate_str_max_zero() {
    // max=0: 空文字なら空、非空なら即 "..."（kept は take(0) で空）
    assert_eq!(truncate_str("", 0), "");
    assert_eq!(truncate_str("a", 0), "...");
    assert_eq!(truncate_str("abc", 0), "...");
}

#[test]
fn truncate_str_max_one_and_two_saturating_sub() {
    // max < 3 で切り詰めが発生すると saturating_sub(3) が 0 になり kept が空 → "..."
    // ただし「ちょうど max 文字」や「max 文字以下」は変化しない
    assert_eq!(truncate_str("a", 1), "a"); // ちょうど 1 文字
    assert_eq!(truncate_str("ab", 1), "..."); // 1 文字超過
    assert_eq!(truncate_str("ab", 2), "ab"); // ちょうど 2 文字
    assert_eq!(truncate_str("abc", 2), "..."); // 2 文字超過
}

#[test]
fn truncate_str_multibyte_at_truncation_boundary() {
    // マルチバイト文字でも切り詰めは「文字数」基準。max=4 で 5 文字なら 1 文字 + "..."
    assert_eq!(truncate_str("あいうえお", 4), "あ...");
    // max=3 ちょうどで 3 文字なら変化なし、4 文字なら "..."（kept take(0)）
    assert_eq!(truncate_str("あいう", 3), "あいう");
    assert_eq!(truncate_str("あいうえ", 3), "...");
}

#[test]
fn truncate_str_one_char_over_max_keeps_max_minus_three() {
    // max=10 で 11 文字 → 先頭 7 文字 + "..."
    assert_eq!(truncate_str("abcdefghijk", 10), "abcdefg...");
}

#[test]
fn process_task_progress_shows_subagent_progress() {
    let input = [
            r#"{"type":"system","subtype":"task_progress","task_id":"abc","tool_use_id":"tu1","description":"Running List all files","usage":{"total_tokens":5000,"tool_uses":1,"duration_ms":4823},"last_tool_name":"Bash"}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{1f504} Running List all files (Bash)"),
        "expected task progress in: {}",
        clean
    );
}

#[test]
fn process_task_notification_completed() {
    let input = [
            r#"{"type":"system","subtype":"task_notification","task_id":"abc","tool_use_id":"tu1","status":"completed","summary":"コードベースの徹底レビュー","usage":{"total_tokens":141902,"tool_uses":47,"duration_ms":158066}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{2705}"),
        "expected completion mark in: {}",
        clean
    );
    assert!(
        clean.contains("コードベースの徹底レビュー"),
        "expected summary in: {}",
        clean
    );
    assert!(clean.contains("2m 38s"), "expected duration in: {}", clean);
    assert!(
        clean.contains("141,902 tokens"),
        "expected token count in: {}",
        clean
    );
}

#[test]
fn process_task_notification_completed_without_usage_omits_zero_values() {
    // 実データでは usage が無い完了通知が多いため、未提供値を 0 として表示しない。
    let input = r#"{"type":"system","subtype":"task_notification","task_id":"abc","tool_use_id":"tu1","status":"completed","summary":"Fetch latest from remote"}"#;

    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{2705} Fetch latest from remote"),
        "expected completion summary in: {}",
        clean
    );
    assert!(
        !clean.contains("0m 0s"),
        "missing duration must not be shown as zero: {}",
        clean
    );
    assert!(
        !clean.contains("0 tokens"),
        "missing token count must not be shown as zero: {}",
        clean
    );
}

#[test]
fn process_task_notification_failed() {
    let input = [
            r#"{"type":"system","subtype":"task_notification","task_id":"abc","tool_use_id":"tu1","status":"failed","summary":"","usage":{"total_tokens":500,"tool_uses":1,"duration_ms":5000}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{274c} Task failed"),
        "expected failure mark in: {}",
        clean
    );
}

#[test]
fn process_task_notification_stopped_with_summary() {
    // TaskStop 経由で停止された場合: summary 付きで表示する
    let input = r#"{"type":"system","subtype":"task_notification","task_id":"bnrpvucd1","tool_use_id":"tu1","status":"stopped","output_file":"","summary":"Codex review completion (output file growth)","usage":{"total_tokens":1000,"tool_uses":2,"duration_ms":12000}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{23f9} Task stopped:"),
        "expected stop mark in: {}",
        clean
    );
    assert!(
        clean.contains("Codex review completion"),
        "expected summary in: {}",
        clean
    );
    assert!(clean.contains("0m 12s"), "expected duration in: {}", clean);
}

#[test]
fn process_task_notification_stopped_without_summary() {
    // summary が無くても停止イベントは表示する
    let input = r#"{"type":"system","subtype":"task_notification","task_id":"x","tool_use_id":"t","status":"stopped","summary":"","usage":{"total_tokens":0,"tool_uses":0,"duration_ms":3000}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{23f9} Task stopped"),
        "expected stop mark in: {}",
        clean
    );
    assert!(clean.contains("0m 3s"), "expected duration in: {}", clean);
}

#[test]
fn process_task_notification_stopped_without_usage_omits_zero_duration() {
    let input = r#"{"type":"system","subtype":"task_notification","task_id":"x","tool_use_id":"t","status":"stopped","summary":"Manual stop"}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("Task stopped: Manual stop"),
        "expected stop summary in: {}",
        clean
    );
    assert!(
        !clean.contains("0m 0s"),
        "missing duration must not be shown as zero: {}",
        clean
    );
}

#[test]
fn process_task_updated_completed_shows_status() {
    // 実データに出る task_updated は task_notification とは別の完了通知として表示する
    let input = r#"{"type":"system","subtype":"task_updated","task_id":"abc","patch":{"status":"completed","end_time":1776959941297}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{2705} Task completed"),
        "expected task_updated completion in: {}",
        clean
    );
}

#[test]
fn process_system_notification_shows_text_and_key() {
    // stop hook などの即時通知は無視せずエラーとして見えるようにする
    let input = r#"{"type":"system","subtype":"notification","key":"stop-hook-error","text":"Stop hook error occurred","priority":"immediate"}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("Notification: Stop hook error occurred"),
        "expected notification text in: {}",
        clean
    );
    assert!(
        clean.contains("stop-hook-error"),
        "expected notification key in: {}",
        clean
    );
}

#[test]
fn process_hook_progress_shows_output() {
    // 実データでは hook_progress にタイムアウトなどの stderr が入る
    let input = r#"{"type":"system","subtype":"hook_progress","hook_name":"SessionStart:startup","hook_event":"SessionStart","stderr":"[ai-analytics-hook] request timeout\n","output":"[ai-analytics-hook] request timeout\n"}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("Hook SessionStart:startup"),
        "expected hook name in: {}",
        clean
    );
    assert!(
        clean.contains("[ai-analytics-hook] request timeout"),
        "expected hook output in: {}",
        clean
    );
}

#[test]
fn process_hook_response_shows_success_with_stderr() {
    // exit_code 0 でも stderr/output がある場合は診断情報として表示する
    let input = r#"{"type":"system","subtype":"hook_response","hook_name":"SessionStart:startup","hook_event":"SessionStart","outcome":"success","exit_code":0,"stderr":"[ai-analytics-hook] Error: socket hang up\n","output":"[ai-analytics-hook] Error: socket hang up\n"}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("outcome:success"),
        "expected hook outcome in: {}",
        clean
    );
    assert!(
        clean.contains("exit:0"),
        "expected hook exit code in: {}",
        clean
    );
    assert!(
        clean.contains("socket hang up"),
        "expected hook stderr in: {}",
        clean
    );
}

#[test]
fn process_hook_response_success_without_output_is_silent() {
    let input = r#"{"type":"system","subtype":"hook_response","hook_name":"SessionStart:startup","hook_event":"SessionStart","outcome":"success","exit_code":0,"stdout":"","stderr":"","output":""}"#;
    let output = run_process(input);

    assert!(
        output.is_empty(),
        "expected silent hook response: {}",
        output
    );
}

#[test]
fn process_hook_events_are_silently_ignored() {
    let input = [
            r#"{"type":"system","subtype":"hook_started","hook_id":"h1","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"system","subtype":"hook_response","hook_id":"h1","hook_name":"SessionStart:startup","exit_code":0}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OK"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(!clean.contains("hook"), "hook events should be silent");
    assert!(clean.contains("OK"), "text should still appear");
}

#[test]
fn process_schedule_wakeup_shows_detail_from_partial_json() {
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"ScheduleWakeup","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"delaySeconds\":90,\"reason\":\"codex レビュー結果を待つため少し待機\",\"prompt\":\"codex レビューの結果を確認して、テスト追加に進む\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("ScheduleWakeup"));
    assert!(clean.contains("90s (codex レビュー結果を待つため少し待機)"));
}

#[test]
fn cache_write_5m_returns_zero_when_only_1h_exists() {
    // 1hキャッシュのみ存在する場合、5mは0を返す（二重表示の防止）
    let usage = UsageSummary {
        cache_creation_input_tokens: 20000,
        cache_creation_1h_input_tokens: 20000,
        ..Default::default()
    };
    assert_eq!(usage.cache_write_5m_tokens(), 0);
}

#[test]
fn cache_write_5m_returns_5m_when_both_exist() {
    // 5mと1hの両方が存在する場合、5mの値を返す
    let usage = UsageSummary {
        cache_creation_input_tokens: 15000,
        cache_creation_5m_input_tokens: 5000,
        cache_creation_1h_input_tokens: 10000,
        ..Default::default()
    };
    assert_eq!(usage.cache_write_5m_tokens(), 5000);
}

#[test]
fn cache_write_5m_fallback_when_no_breakdown() {
    // 内訳が存在しない場合は合計値をフォールバック
    let usage = UsageSummary {
        cache_creation_input_tokens: 8000,
        ..Default::default()
    };
    assert_eq!(usage.cache_write_5m_tokens(), 8000);
}

#[test]
fn process_api_retry_shows_attempt_info() {
    let input = r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":"server_error","error_status":503}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("1/10"),
        "試行回数が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("server_error"),
        "エラー種別が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("503"),
        "HTTPステータスが表示されるべき: {}",
        clean
    );
}

#[test]
fn process_api_retry_null_status() {
    let input = r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"error":"unknown","error_status":null}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("2/10"),
        "試行回数が表示されるべき: {}",
        clean
    );
    assert!(
        !clean.contains("("),
        "null ステータスは括弧なしで表示されるべき: {}",
        clean
    );
}

#[test]
fn process_stream_event_ping_is_silent() {
    // 実データに頻出する {"type":"stream_event","event":{"type":"ping"}} は
    // 静かに無視され、後続イベントの出力に影響しないことを確認する。
    let input = [
            r#"{"type":"stream_event","event":{"type":"ping"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OK"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");
    let output = run_process(&input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.to_lowercase().contains("ping"),
        "ping イベントは表示されるべきでない: {}",
        clean
    );
    assert!(clean.contains("OK"), "後続テキストは表示されるべき");
}

#[test]
fn process_async_agent_launch_shows_async_marker() {
    // Agent を run_in_background=true で起動した async-launched 応答が
    // tool 完了行で [async, output-file:readable] として表示されることを確認。
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_async","name":"Agent","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"bg task\",\"run_in_background\":true}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tu_async","type":"tool_result","content":"Async agent launched successfully."}]},"tool_use_result":{"isAsync":true,"status":"async_launched","agentId":"a32bc162897eb706d","outputFile":"/tmp/agent.output","canReadOutputFile":true}}"#,
        ]
        .join("\n");
    let output = run_process(&input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("\u{2713} Agent"), "{clean}");
    assert!(clean.contains("async"), "{clean}");
    assert!(clean.contains("output-file:readable"), "{clean}");
    // agentId そのものは表示しない
    assert!(!clean.contains("a32bc162"), "{clean}");
}

#[test]
fn process_task_update_status_only_omits_updated_field() {
    // TaskUpdate で status のみ変更したケースは statusChange のみ表示され、updated: は出ない。
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_up","name":"TaskUpdate","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"taskId\":\"1\",\"status\":\"completed\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"tu_up","type":"tool_result","content":"updated"}]},"tool_use_result":{"updatedFields":["status"],"statusChange":{"from":"pending","to":"completed"}}}"#,
        ]
        .join("\n");
    let output = run_process(&input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("status:pending->completed"), "{clean}");
    assert!(!clean.contains("updated:"), "{clean}");
}

#[test]
fn process_system_status_subtype_is_silent() {
    // 実データに頻出する {"type":"system","subtype":"status","status":"requesting"} は
    // 表示するとノイズになるため、サイレントに無視されること。
    let input = r#"{"type":"system","subtype":"status","status":"requesting","uuid":"abc","session_id":"s1"}"#;
    let output = run_process(input);
    assert!(
        output.is_empty(),
        "system.subtype=status は表示されるべきでない: {}",
        output
    );
}

#[test]
fn process_system_thinking_tokens_subtype_is_silent() {
    // 実データに高頻度（1 セッションで数千件）で出る thinking_tokens は、
    // thinking_delta のドット進捗表示と result.usage の最終集計と重複するため、
    // サイレントに無視されること。estimated_tokens はセッション累積の推定値で、
    // 個別の thinking ブロックには紐付けられない。
    let input = r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50,"estimated_tokens_delta":50,"uuid":"abc","session_id":"s1"}"#;
    let output = run_process(input);
    assert!(
        output.is_empty(),
        "system.subtype=thinking_tokens は表示されるべきでない: {}",
        output
    );
}

#[test]
fn usage_summary_merge_from_none_is_noop() {
    // None 値を渡しても既存の値が変わらないことを確認する
    let mut usage = UsageSummary {
        input_tokens: 100,
        output_tokens: 50,
        ..Default::default()
    };
    usage.merge_from_value(None);
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 50);
}

#[test]
fn usage_summary_merge_overrides_with_latest() {
    // result イベントの値が最終累計値として優先される（累積ではなく上書き）。
    let v_first: serde_json::Value =
        serde_json::from_str(r#"{"input_tokens":10,"output_tokens":5}"#).unwrap();
    let v_last: serde_json::Value =
        serde_json::from_str(r#"{"input_tokens":100,"output_tokens":50}"#).unwrap();
    let mut usage = UsageSummary::default();
    usage.merge_from_value(Some(&v_first));
    usage.merge_from_value(Some(&v_last));
    assert_eq!(usage.input_tokens, 100, "最終値で上書きされるべき");
    assert_eq!(usage.output_tokens, 50);
}
