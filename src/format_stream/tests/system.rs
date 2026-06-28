//! `handle_system_event` の system イベント表示テスト。
//! 特にフック診断（`hook_response` / `hook_progress` / `hook_started`）の
//! 表示・非表示の分岐を、実データ構造に即して固定する。

use crate::format_stream::system::handle_system_event;

/// system イベント JSON を `handle_system_event` に通し、出力文字列を返す。
fn render(json: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    let mut buf = Vec::new();
    handle_system_event(&v, &mut buf).unwrap();
    String::from_utf8(buf).unwrap()
}

#[test]
fn hook_success_without_output_is_silent() {
    // 成功し出力も無いフックはノイズになるため非表示。
    // 実データ例: {"subtype":"hook_response","hook_name":"PostToolUse",
    //              "output":"","outcome":"success","exit_code":0}
    let out = render(
        r#"{"type":"system","subtype":"hook_response","hook_name":"PostToolUse","output":"","outcome":"success","exit_code":0}"#,
    );
    assert!(
        out.is_empty(),
        "成功・出力なしフックは非表示のはず: {out:?}"
    );
}

#[test]
fn hook_failure_is_shown_even_without_output() {
    // outcome が success 以外なら、出力が無くても失敗として表示する。
    let out = render(
        r#"{"type":"system","subtype":"hook_response","hook_name":"PreToolUse","output":"","outcome":"blocked","exit_code":2}"#,
    );
    assert!(out.contains("Hook"), "失敗フックは表示されるはず: {out:?}");
    assert!(out.contains("PreToolUse"));
    assert!(out.contains("blocked"));
}

#[test]
fn hook_nonzero_exit_is_shown() {
    // outcome が success でも exit_code が非0なら失敗として表示する。
    let out = render(
        r#"{"type":"system","subtype":"hook_progress","hook_name":"Stop","output":"","outcome":"success","exit_code":1}"#,
    );
    assert!(out.contains("Hook"));
    assert!(out.contains("exit:1"));
}

#[test]
fn hook_with_stderr_is_shown() {
    // 出力（stderr）があるフックは成功でも表示する。
    let out = render(
        r#"{"type":"system","subtype":"hook_progress","hook_name":"Stop","stderr":"deprecated API","outcome":"success","exit_code":0}"#,
    );
    assert!(out.contains("Hook"));
    assert!(out.contains("deprecated API"));
}

#[test]
fn hook_started_is_ignored() {
    // hook_started は開始通知で出力を伴わないため無視する。
    let out = render(r#"{"type":"system","subtype":"hook_started","hook_name":"PostToolUse"}"#);
    assert!(out.is_empty(), "hook_started は無視されるはず: {out:?}");
}
