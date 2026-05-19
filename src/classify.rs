use std::path::Path;

/// Claude Code の hook ペイロード `error` 値（StopFailure 時のみ）。
/// 公式ドキュメント: https://code.claude.com/docs/en/hooks
const RATE_LIMIT_ERRORS: &[&str] = &["rate_limit"];
const RETRYABLE_ERRORS: &[&str] = &["server_error"];
const FAILED_ERRORS: &[&str] = &[
    "authentication_failed",
    "oauth_org_not_allowed",
    "billing_error",
    "invalid_request",
    "max_output_tokens",
];

/// Claude stream-json の result イベントから導出されるタスク結果分類。
#[derive(Debug, PartialEq, Eq)]
pub enum ResultClass {
    /// 成功、または分類に使える result イベントが無い（他要因で判定すべき）。
    Success,
    /// 週次/日次リセットまでレート制限に達した。
    RateLimited,
    /// プロバイダ側のリトライ可能なエラー（5xx など）。次回実行で再試行可能。
    Retryable(String),
    /// プロバイダ側で恒久的にエラーとなった。
    Failed(String),
}

impl ResultClass {
    /// `token-burn classify-result` の終了コード表現。
    pub fn exit_code(&self) -> i32 {
        match self {
            ResultClass::Success => 0,
            ResultClass::Failed(_) => 1,
            ResultClass::RateLimited => 2,
            ResultClass::Retryable(_) => 3,
        }
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            ResultClass::Success | ResultClass::RateLimited => None,
            ResultClass::Failed(m) | ResultClass::Retryable(m) => Some(m.as_str()),
        }
    }
}

/// jsonl ファイルから最後の `"type":"result"` イベントを取り出して分類する。
pub fn classify_jsonl(path: &Path) -> ResultClass {
    let Ok(content) = std::fs::read_to_string(path) else {
        return ResultClass::Success;
    };
    classify_content(&content)
}

pub fn classify_content(content: &str) -> ResultClass {
    let mut last_result: Option<serde_json::Value> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("result") {
            last_result = Some(v);
        }
    }

    let Some(v) = last_result else {
        return ResultClass::Success;
    };

    let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
    if !is_error {
        return ResultClass::Success;
    }

    let message = v
        .get("result")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();

    if is_rate_limit_message(&message) {
        return ResultClass::RateLimited;
    }

    if let Some(status) = v.get("api_error_status").and_then(|s| s.as_u64())
        && is_retryable_status(status)
    {
        return ResultClass::Retryable(message);
    }

    ResultClass::Failed(message)
}

/// `"Claude AI usage limit reached|<timestamp>"` のようなレート制限メッセージを検出する。
fn is_rate_limit_message(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    // format_stream 側と同様のヒューリスティック: "resets <digit><am|pm>" もしくは "usage limit reached" を含む
    if lower.contains("usage limit reached") {
        return true;
    }
    if let Some(idx) = lower.find("resets ") {
        let after = &lower[idx + "resets ".len()..];
        let has_digit = after.chars().next().is_some_and(|c| c.is_ascii_digit());
        if has_digit && (after.contains("am") || after.contains("pm")) {
            return true;
        }
    }
    false
}

/// リトライ可能な HTTP ステータス（5xx および 408/429）か判定する。
fn is_retryable_status(status: u64) -> bool {
    matches!(status, 408 | 429) || (500..600).contains(&status)
}

/// `claude-hook` が書き出した outcome JSON（Stop / StopFailure hook ペイロード）を読んで分類する。
/// ファイルが存在しない場合は Success を返す（呼び出し側で hook 不発火として別途扱う想定）。
pub fn classify_outcome_json(path: &Path) -> ResultClass {
    let Ok(content) = std::fs::read_to_string(path) else {
        return ResultClass::Success;
    };
    classify_outcome_content(&content)
}

pub fn classify_outcome_content(content: &str) -> ResultClass {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return ResultClass::Failed("empty hook outcome".to_string());
    }

    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return ResultClass::Failed("invalid hook outcome JSON".to_string());
    };

    let event = v
        .get("hook_event_name")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let last_message = v
        .get("last_assistant_message")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let error_details = v
        .get("error_details")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let message = if !error_details.is_empty() {
        error_details.to_string()
    } else {
        last_message.to_string()
    };

    match event {
        "Stop" => ResultClass::Success,
        "StopFailure" => {
            let err = v.get("error").and_then(|s| s.as_str()).unwrap_or("");
            if RATE_LIMIT_ERRORS.contains(&err) {
                return ResultClass::RateLimited;
            }
            if RETRYABLE_ERRORS.contains(&err) {
                return ResultClass::Retryable(message);
            }
            if FAILED_ERRORS.contains(&err) {
                return ResultClass::Failed(message);
            }
            // `unknown` などはメッセージ判定で fallback
            if is_rate_limit_message(&message) {
                return ResultClass::RateLimited;
            }
            ResultClass::Failed(message)
        }
        // 想定外イベント
        _ => ResultClass::Failed(format!("unexpected hook_event_name: {event}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_empty_content_is_success() {
        assert_eq!(classify_content(""), ResultClass::Success);
    }

    #[test]
    fn classify_without_result_is_success() {
        let input = r#"{"type":"system","subtype":"init"}"#;
        assert_eq!(classify_content(input), ResultClass::Success);
    }

    #[test]
    fn classify_successful_result_is_success() {
        let input = r#"{"type":"result","is_error":false,"result":"done"}"#;
        assert_eq!(classify_content(input), ResultClass::Success);
    }

    #[test]
    fn classify_api_error_529_is_retryable() {
        let input = r#"{"type":"result","is_error":true,"api_error_status":529,"result":"API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment."}"#;
        match classify_content(input) {
            ResultClass::Retryable(msg) => {
                assert!(msg.contains("529 Overloaded"));
            }
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn classify_api_error_500_is_retryable() {
        let input = r#"{"type":"result","is_error":true,"api_error_status":500,"result":"Internal server error"}"#;
        match classify_content(input) {
            ResultClass::Retryable(_) => {}
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn classify_api_error_429_is_retryable() {
        let input = r#"{"type":"result","is_error":true,"api_error_status":429,"result":"Too many requests"}"#;
        match classify_content(input) {
            ResultClass::Retryable(_) => {}
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn classify_api_error_400_is_failed() {
        let input =
            r#"{"type":"result","is_error":true,"api_error_status":400,"result":"Bad request"}"#;
        match classify_content(input) {
            ResultClass::Failed(msg) => {
                assert_eq!(msg, "Bad request");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_rate_limit_result_is_rate_limited() {
        let input = r#"{"type":"result","is_error":true,"result":"Claude AI usage limit reached. Rate limit resets 9pm."}"#;
        assert_eq!(classify_content(input), ResultClass::RateLimited);
    }

    #[test]
    fn classify_rate_limit_with_am_marker() {
        let input = r#"{"type":"result","is_error":true,"result":"limit reached - resets 3am"}"#;
        assert_eq!(classify_content(input), ResultClass::RateLimited);
    }

    #[test]
    fn classify_uses_last_result_event() {
        // 最後の result イベントのみが分類対象になる
        let input = "\
{\"type\":\"result\",\"is_error\":false,\"result\":\"first ok\"}\n\
{\"type\":\"result\",\"is_error\":true,\"api_error_status\":529,\"result\":\"529 Overloaded\"}\n";
        match classify_content(input) {
            ResultClass::Retryable(msg) => assert!(msg.contains("529")),
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn classify_ignores_invalid_json() {
        let input = "not json\n{\"type\":\"result\",\"is_error\":true,\"result\":\"err\"}\n";
        match classify_content(input) {
            ResultClass::Failed(msg) => assert_eq!(msg, "err"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_jsonl_missing_file_is_success() {
        let path = std::path::Path::new("/nonexistent/token-burn-missing.jsonl");
        assert_eq!(classify_jsonl(path), ResultClass::Success);
    }

    #[test]
    fn classify_jsonl_reads_file_content() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"{"type":"result","is_error":true,"api_error_status":503,"result":"Service Unavailable"}"#,
        )
        .unwrap();
        match classify_jsonl(tmp.path()) {
            ResultClass::Retryable(msg) => assert_eq!(msg, "Service Unavailable"),
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn exit_code_mapping() {
        assert_eq!(ResultClass::Success.exit_code(), 0);
        assert_eq!(ResultClass::Failed("x".to_string()).exit_code(), 1);
        assert_eq!(ResultClass::RateLimited.exit_code(), 2);
        assert_eq!(ResultClass::Retryable("x".to_string()).exit_code(), 3);
    }

    #[test]
    fn message_for_failed_and_retryable() {
        assert_eq!(ResultClass::Failed("f".to_string()).message(), Some("f"));
        assert_eq!(ResultClass::Retryable("r".to_string()).message(), Some("r"));
        assert_eq!(ResultClass::Success.message(), None);
        assert_eq!(ResultClass::RateLimited.message(), None);
    }

    #[test]
    fn is_retryable_status_covers_5xx() {
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(529));
        assert!(is_retryable_status(599));
        assert!(!is_retryable_status(499));
        assert!(!is_retryable_status(600));
    }

    #[test]
    fn is_retryable_status_covers_429_and_408() {
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(429));
    }

    #[test]
    fn classify_rate_limit_takes_precedence_over_retryable_status() {
        // usage limit reached メッセージは api_error_status=429 より優先される
        let input = r#"{"type":"result","is_error":true,"api_error_status":429,"result":"Claude AI usage limit reached"}"#;
        assert_eq!(classify_content(input), ResultClass::RateLimited);
    }

    #[test]
    fn classify_is_error_without_status_falls_to_failed() {
        // api_error_status がなく、レート制限でもないエラーは Failed
        let input = r#"{"type":"result","is_error":true,"result":"unexpected error"}"#;
        match classify_content(input) {
            ResultClass::Failed(msg) => assert_eq!(msg, "unexpected error"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_is_error_without_message_uses_empty_string() {
        // result フィールドが欠けている場合は空文字列の Failed メッセージ
        let input = r#"{"type":"result","is_error":true}"#;
        match classify_content(input) {
            ResultClass::Failed(msg) => assert_eq!(msg, ""),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn is_rate_limit_message_requires_digit_after_resets() {
        // "resets " の直後が数字でない場合は誤検知しない
        assert!(!is_rate_limit_message("resets soon — please retry pm"));
    }

    #[test]
    fn is_rate_limit_message_requires_am_or_pm_marker() {
        // "resets <digit>" だけでは時刻表記とみなされない
        assert!(!is_rate_limit_message("resets 5 minutes from now"));
    }

    #[test]
    fn is_rate_limit_message_case_insensitive() {
        // 大文字メッセージでも検出する
        assert!(is_rate_limit_message("CLAUDE AI USAGE LIMIT REACHED"));
        assert!(is_rate_limit_message("Resets 11PM"));
    }

    #[test]
    fn is_retryable_status_excludes_other_4xx() {
        // 408/429 以外の 4xx は再試行対象外
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
    }

    // -------- classify_outcome_* tests (interactive 経路) --------

    #[test]
    fn classify_outcome_stop_is_success() {
        let input = r#"{"hook_event_name":"Stop","session_id":"abc"}"#;
        assert_eq!(classify_outcome_content(input), ResultClass::Success);
    }

    #[test]
    fn classify_outcome_stop_failure_rate_limit() {
        let input = r#"{"hook_event_name":"StopFailure","error":"rate_limit","error_details":"limit reached"}"#;
        assert_eq!(classify_outcome_content(input), ResultClass::RateLimited);
    }

    #[test]
    fn classify_outcome_stop_failure_server_error_is_retryable() {
        let input = r#"{"hook_event_name":"StopFailure","error":"server_error","error_details":"503 unavailable"}"#;
        match classify_outcome_content(input) {
            ResultClass::Retryable(m) => assert_eq!(m, "503 unavailable"),
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_stop_failure_billing_is_failed() {
        let input = r#"{"hook_event_name":"StopFailure","error":"billing_error","error_details":"plan exhausted"}"#;
        match classify_outcome_content(input) {
            ResultClass::Failed(m) => assert_eq!(m, "plan exhausted"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_stop_failure_auth_is_failed() {
        let input = r#"{"hook_event_name":"StopFailure","error":"authentication_failed","error_details":"token expired"}"#;
        match classify_outcome_content(input) {
            ResultClass::Failed(m) => assert_eq!(m, "token expired"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_stop_failure_unknown_with_rate_limit_message() {
        // unknown でメッセージが rate-limit シグネチャを含む場合は RateLimited
        let input = r#"{"hook_event_name":"StopFailure","error":"unknown","error_details":"Claude AI usage limit reached"}"#;
        assert_eq!(classify_outcome_content(input), ResultClass::RateLimited);
    }

    #[test]
    fn classify_outcome_stop_failure_unknown_without_signature_is_failed() {
        let input =
            r#"{"hook_event_name":"StopFailure","error":"unknown","error_details":"weird stuff"}"#;
        match classify_outcome_content(input) {
            ResultClass::Failed(m) => assert_eq!(m, "weird stuff"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_falls_back_to_last_assistant_message() {
        // error_details が空でも last_assistant_message を採用する
        let input = r#"{"hook_event_name":"StopFailure","error":"server_error","last_assistant_message":"working on it"}"#;
        match classify_outcome_content(input) {
            ResultClass::Retryable(m) => assert_eq!(m, "working on it"),
            other => panic!("expected Retryable, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_empty_content_is_failed() {
        // 空ファイル = hook が発火したが何も書けなかった、として Failed
        match classify_outcome_content("") {
            ResultClass::Failed(m) => assert!(m.contains("empty")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_invalid_json_is_failed() {
        match classify_outcome_content("not json") {
            ResultClass::Failed(m) => assert!(m.contains("invalid")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_unexpected_event_is_failed() {
        let input = r#"{"hook_event_name":"PreToolUse"}"#;
        match classify_outcome_content(input) {
            ResultClass::Failed(m) => assert!(m.contains("unexpected")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn classify_outcome_json_missing_file_is_success() {
        let path = std::path::Path::new("/nonexistent/token-burn-no-outcome.json");
        assert_eq!(classify_outcome_json(path), ResultClass::Success);
    }

    #[test]
    fn classify_outcome_json_reads_file_content() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"{"hook_event_name":"StopFailure","error":"rate_limit"}"#,
        )
        .unwrap();
        assert_eq!(classify_outcome_json(tmp.path()), ResultClass::RateLimited);
    }
}
