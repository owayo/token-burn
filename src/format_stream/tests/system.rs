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
fn hook_response_with_empty_output_falls_back_to_stderr() {
    // 実データの hook_response は output / stdout / stderr を常に持ち、
    // 失敗時は stderr にだけ内容が入る。旧実装（first_string）は「文字列であれば
    // 空文字でも確定」する仕様のため output:"" を採用してしまい、最も診断が
    // 欲しい失敗時に本文が丸ごと落ちて "no output" にしかならなかった。
    let out = render(
        r#"{"type":"system","subtype":"hook_response","hook_name":"Stop:git-sc","output":"","stdout":"","stderr":"fatal: could not read Username","exit_code":1,"outcome":"error"}"#,
    );
    assert!(
        out.contains("fatal: could not read Username"),
        "stderr の内容が表示されるはず: {out:?}"
    );
    assert!(out.contains("Stop:git-sc"), "{out:?}");
    assert!(out.contains("outcome:error"), "{out:?}");
    assert!(out.contains("exit:1"), "{out:?}");
    assert!(
        !out.contains("no output"),
        "stderr があるのに no output 表示になってはいけない: {out:?}"
    );
}

#[test]
fn hook_response_with_empty_output_and_stderr_falls_back_to_stdout() {
    // output / stderr の両方が空でも stdout に内容があればそこまで辿る。
    let out = render(
        r#"{"type":"system","subtype":"hook_response","hook_name":"PostToolUse","output":"","stderr":"","stdout":"formatted 3 files","outcome":"success","exit_code":0}"#,
    );
    assert!(out.contains("formatted 3 files"), "{out:?}");
}

#[test]
fn hook_output_takes_priority_over_stderr() {
    // 空文字スキップ後もキー順（output → stderr → stdout）の優先度は変わらない。
    let out = render(
        r#"{"type":"system","subtype":"hook_response","hook_name":"Stop","output":"primary","stderr":"secondary","stdout":"tertiary","outcome":"success","exit_code":0}"#,
    );
    assert!(out.contains("primary"), "{out:?}");
    assert!(!out.contains("secondary"), "{out:?}");
    assert!(!out.contains("tertiary"), "{out:?}");
}

#[test]
fn hook_name_empty_falls_back_to_hook_event() {
    // hook 名も同じ空文字スキップが必要。hook_name:"" が常設される実データで
    // フック種別が空欄になるのを防ぐ。
    let out = render(
        r#"{"type":"system","subtype":"hook_response","hook_name":"","hook_event":"SessionStart","output":"","stderr":"boom","outcome":"error","exit_code":1}"#,
    );
    assert!(out.contains("SessionStart"), "{out:?}");
    assert!(out.contains("boom"), "{out:?}");
}

#[test]
fn hook_started_is_ignored() {
    // hook_started は開始通知で出力を伴わないため無視する。
    let out = render(r#"{"type":"system","subtype":"hook_started","hook_name":"PostToolUse"}"#);
    assert!(out.is_empty(), "hook_started は無視されるはず: {out:?}");
}

#[test]
fn api_retry_with_error_shows_error_text() {
    let out = render(
        r#"{"type":"system","subtype":"api_retry","attempt":2,"max_retries":10,"error":"Overloaded","error_status":529}"#,
    );
    assert!(out.contains("API retry 2/10: Overloaded (529)"), "{out:?}");
}

#[test]
fn api_retry_without_error_omits_unknown_placeholder() {
    // 実データには error フィールドの無い api_retry がある。
    // 旧実装は "API retry 1/10: unknown" と表示し、"unknown" が
    // エラー名のように見えていた（実ログで確認）。
    let out = render(r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10}"#);
    assert!(out.contains("API retry 1/10"), "{out:?}");
    assert!(!out.contains("unknown"), "{out:?}");
    assert!(!out.contains("1/10:"), "{out:?}");
}

#[test]
fn model_refusal_fallback_shows_models_and_category_without_content() {
    let out = render(
        r#"{"type":"system","subtype":"model_refusal_fallback","original_model":"claude-fable-5","fallback_model":"claude-opus-4-8[1m]","api_refusal_category":"cyber","content":"拒否された本文","api_refusal_explanation":"長い説明"}"#,
    );
    assert!(out.contains("Model refusal fallback"));
    assert!(out.contains("claude-fable-5"));
    assert!(out.contains("claude-opus-4-8"));
    assert!(!out.contains("[1m]"), "{out:?}");
    assert!(out.contains("category:cyber"));
    assert!(!out.contains("拒否された本文"));
    assert!(!out.contains("長い説明"));
}

/// `init` はモデル・CLI バージョン・権限モードを 1 行で表示する。
/// これらは他のどのイベントにも現れず、以前は `init` を丸ごと無視していたため
/// CLI バージョンと `bypassPermissions` で走ったかどうかが完全に失われていた。
#[test]
fn init_shows_model_version_and_permission_mode() {
    let out = render(
        r#"{"type":"system","subtype":"init","model":"claude-opus-5[1m]","claude_code_version":"2.1.220","permissionMode":"bypassPermissions","cwd":"/repo","session_id":"s1"}"#,
    );
    assert!(out.contains("Session claude-opus-5"), "{out:?}");
    assert!(!out.contains("[1m]"), "{out:?}");
    assert!(out.contains("v2.1.220"), "{out:?}");
    assert!(out.contains("bypassPermissions"), "{out:?}");
}

/// version / permissionMode が無くてもモデルだけで表示する（属性の括弧は付けない）。
#[test]
fn init_with_model_only_omits_attribute_parentheses() {
    let out = render(r#"{"type":"system","subtype":"init","model":"claude-fable-5"}"#);
    assert!(out.contains("Session claude-fable-5"), "{out:?}");
    assert!(!out.contains('('), "{out:?}");
}

/// モデルが無く属性だけある場合は "?" で埋めて属性を表示する。
#[test]
fn init_without_model_falls_back_to_question_mark() {
    let out = render(
        r#"{"type":"system","subtype":"init","claude_code_version":"2.1.218","permissionMode":"default"}"#,
    );
    assert!(out.contains("Session ? (v2.1.218, default)"), "{out:?}");
}

/// 表示材料が何も無い init は行を出さない（session_id / cwd だけの形）。
#[test]
fn init_without_display_fields_writes_nothing() {
    let out = render(r#"{"type":"system","subtype":"init","session_id":"s1","cwd":"/repo"}"#);
    assert!(out.is_empty(), "{out:?}");
}

// --- task_updated: 失敗理由とバックグラウンド移行 ---

/// 失敗 patch の error（実データの主要な失敗原因）を表示する。
#[test]
fn task_updated_failed_shows_patch_error() {
    let out = render(
        r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"status":"failed","end_time":1785433765926,"error":"Agent terminated early due to an API error: API Error: Connection closed mid-response."}}"#,
    );
    assert!(out.contains("Task failed"), "{out:?}");
    assert!(
        out.contains("Connection closed mid-response"),
        "失敗理由を表示すべき: {out:?}"
    );
}

/// killed も失敗系として error を併記する。
#[test]
fn task_updated_killed_shows_patch_error() {
    let out = render(
        r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"status":"killed","error":"stopped by user"}}"#,
    );
    assert!(out.contains("Task killed"), "{out:?}");
    assert!(out.contains("stopped by user"), "{out:?}");
}

/// error が無い失敗 patch は従来どおり状態のみを表示する。
#[test]
fn task_updated_failed_without_error_keeps_short_form() {
    let out = render(
        r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"status":"failed","end_time":1785433765926}}"#,
    );
    assert!(out.contains("Task failed"), "{out:?}");
    assert!(!out.contains(':'), "余計な区切りを付けないべき: {out:?}");
}

/// 空文字の error は付けない。
#[test]
fn task_updated_failed_with_empty_error_keeps_short_form() {
    let out = render(
        r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"status":"failed","error":""}}"#,
    );
    assert!(out.contains("Task failed"), "{out:?}");
    assert!(!out.contains(':'), "{out:?}");
}

/// status を伴わない is_backgrounded patch は「バックグラウンド移行」として表示する。
/// これ以降タスクの出力がインラインに出なくなる理由そのものになる。
#[test]
fn task_updated_backgrounded_is_reported() {
    let out = render(
        r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"is_backgrounded":true}}"#,
    );
    assert!(out.contains("Task backgrounded"), "{out:?}");
}

/// is_backgrounded:false や未知フィールドのみの patch は何も出さない。
#[test]
fn task_updated_unknown_patch_writes_nothing() {
    let out = render(
        r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"is_backgrounded":false,"end_time":1785433765926}}"#,
    );
    assert!(out.is_empty(), "{out:?}");
}

/// status がある場合は is_backgrounded より status を優先する。
#[test]
fn task_updated_status_takes_precedence_over_backgrounded() {
    let out = render(
        r#"{"type":"system","subtype":"task_updated","task_id":"a1","patch":{"status":"completed","is_backgrounded":true}}"#,
    );
    assert!(out.contains("Task completed"), "{out:?}");
    assert!(!out.contains("backgrounded"), "{out:?}");
}
