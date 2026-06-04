//! Edit ツールの差分表示（`format_diff_lines` / `format_tool_diff`）と差分出力のテスト。

use super::*;

#[test]
fn process_edit_tool_shows_diff_stats() {
    // 実際の Edit ツールと同じ入力形式
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t_edit","name":"Edit","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/index.test.ts\",\"old_string\":\"line1\\nline2\",\"new_string\":\"line1\\nline2\\nline3\\nline4\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":1}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_edit","type":"tool_result","content":"The file has been updated successfully."}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{1f527} Edit"),
        "expected Edit tool icon in: {}",
        clean
    );
    assert!(
        clean.contains("/src/index.test.ts"),
        "expected file path in: {}",
        clean
    );
    assert!(
        clean.contains("(+2/-0)"),
        "expected diff stats in: {}",
        clean
    );
    assert!(
        clean.contains("\u{2713} Edit"),
        "expected checkmark in: {}",
        clean
    );
}

#[test]
fn diff_pure_deletion() {
    let diff = format_diff_lines("line1\nline2\nline3", "");
    let clean = strip_ansi(&diff);
    assert!(clean.contains("- line1"), "got: {}", clean);
    assert!(clean.contains("- line2"), "got: {}", clean);
    assert!(clean.contains("- line3"), "got: {}", clean);
    assert!(!clean.contains("+"), "should have no additions: {}", clean);
}

#[test]
fn diff_pure_addition() {
    let diff = format_diff_lines("", "new1\nnew2");
    let clean = strip_ansi(&diff);
    assert!(clean.contains("+ new1"), "got: {}", clean);
    assert!(clean.contains("+ new2"), "got: {}", clean);
    assert!(!clean.contains("-"), "should have no removals: {}", clean);
}

#[test]
fn diff_with_context() {
    // 共通プレフィックス "aaa" と共通サフィックス "zzz" の間だけが変化
    let old = "aaa\nbbb\nzzz";
    let new = "aaa\nccc\nddd\nzzz";
    let diff = format_diff_lines(old, new);
    let clean = strip_ansi(&diff);
    // コンテキスト行（前後）
    assert!(
        clean.contains("    aaa"),
        "expected prefix context: {}",
        clean
    );
    assert!(
        clean.contains("    zzz"),
        "expected suffix context: {}",
        clean
    );
    // 変更行
    assert!(clean.contains("- bbb"), "expected removal: {}", clean);
    assert!(clean.contains("+ ccc"), "expected addition: {}", clean);
    assert!(clean.contains("+ ddd"), "expected addition: {}", clean);
}

#[test]
fn diff_identical_returns_empty() {
    let diff = format_diff_lines("same\nlines", "same\nlines");
    assert!(
        diff.is_empty(),
        "identical strings should produce empty diff"
    );
}

#[test]
fn diff_truncates_long_changes() {
    // 20行の削除は省略表示される
    let old_lines: Vec<&str> = (0..20).map(|_| "old").collect();
    let old = old_lines.join("\n");
    let diff = format_diff_lines(&old, "new");
    let clean = strip_ansi(&diff);
    assert!(
        clean.contains("... (8 more)"),
        "expected truncation indicator: {}",
        clean
    );
}

#[test]
fn diff_single_line_change() {
    let diff = format_diff_lines("old line", "new line");
    let clean = strip_ansi(&diff);
    assert!(clean.contains("- old line"), "got: {}", clean);
    assert!(clean.contains("+ new line"), "got: {}", clean);
}

// --- format_tool_diff の単体テスト ---

#[test]
fn format_tool_diff_edit() {
    let input =
        r#"{"file_path":"/src/main.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#;
    let diff = format_tool_diff("Edit", input).unwrap();
    let clean = strip_ansi(&diff);
    assert!(clean.contains("- let x = 1;"));
    assert!(clean.contains("+ let x = 2;"));
}

#[test]
fn format_tool_diff_edit_accepts_new_str_alias() {
    let input = r#"{"file_path":"/src/main.rs","old_string":"let x = 1;","new_str":"let x = 2;"}"#;
    let diff = format_tool_diff("Edit", input).unwrap();
    let clean = strip_ansi(&diff);
    assert!(clean.contains("- let x = 1;"));
    assert!(clean.contains("+ let x = 2;"));
}

#[test]
fn format_tool_diff_edit_empty_strings() {
    let input = r#"{"file_path":"/src/main.rs","old_string":"","new_string":""}"#;
    assert!(format_tool_diff("Edit", input).is_none());
}

#[test]
fn format_tool_diff_non_edit() {
    let input = r#"{"command":"ls"}"#;
    assert!(format_tool_diff("Bash", input).is_none());
}

// --- process パイプラインで Edit 差分が出る統合テスト ---

#[test]
fn process_edit_shows_diff_output() {
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Edit","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/lib.rs\",\"old_string\":\"fn old() {}\\nfn keep() {}\",\"new_string\":\"fn new() {}\\nfn also_new() {}\\nfn keep() {}\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    // ツールヘッダー
    assert!(
        clean.contains("\u{1f527} Edit"),
        "expected Edit header: {}",
        clean
    );
    assert!(
        clean.contains("/src/lib.rs"),
        "expected file path: {}",
        clean
    );
    // 差分内容
    assert!(
        clean.contains("- fn old() {}"),
        "expected removed line: {}",
        clean
    );
    assert!(
        clean.contains("+ fn new() {}"),
        "expected added line: {}",
        clean
    );
    assert!(
        clean.contains("+ fn also_new() {}"),
        "expected added line: {}",
        clean
    );
    // コンテキスト（共通サフィックス）
    assert!(
        clean.contains("    fn keep() {}"),
        "expected context line: {}",
        clean
    );
}

#[test]
fn process_edit_pure_deletion_shows_diff() {
    // ログに現れる純削除パターン
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Edit","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/repo.rs\",\"old_string\":\"    if found {\\n        break;\\n    }\",\"new_string\":\"\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("(+0/-3)"), "expected diff stats: {}", clean);
    assert!(clean.contains("- "), "expected removed lines: {}", clean);
    assert!(
        !clean.contains("+ "),
        "should have no added lines: {}",
        clean
    );
}
