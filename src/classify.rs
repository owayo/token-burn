use std::path::Path;

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
/// ファイルが存在しない場合は Success（result イベントなしと等価）として扱うが、
/// 権限エラーや I/O エラーで読めない場合は Failed として返す（成功扱いで
/// state.json に誤記録するのを防ぐ）。
pub fn classify_jsonl(path: &Path) -> ResultClass {
    match std::fs::read_to_string(path) {
        Ok(content) => classify_content(&content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ResultClass::Success,
        Err(e) => ResultClass::Failed(format!("failed to read jsonl {}: {}", path.display(), e)),
    }
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
    if lower.contains("usage limit reached") {
        return true;
    }
    // 実ログで観測される上限到達メッセージ群:
    //   "You've hit your session limit · resets 3am (Asia/Tokyo)"
    //   "You've hit your org's monthly spend limit · run /usage-credits to raise it, ..."
    // いずれも再試行では回復せず、api_error_status=429 のため Retryable へ落ちると
    // 残りのターゲット全件にエラー行が出続ける。上限到達として扱う。
    if let Some(idx) = lower.find("hit your")
        && lower[idx..].contains("limit")
    {
        return true;
    }
    // "resets <時刻><am|pm>" のパターンに厳密にマッチする。
    // `after.contains("am")` のような緩い判定だと、数字と "am"/"pm" が離れた位置にある
    // 一般のエラーメッセージを誤って RateLimited 扱いして state.json への記録を欠落させる。
    if let Some(idx) = lower.find("resets ") {
        return starts_with_clock_time(&lower[idx + "resets ".len()..]);
    }
    false
}

/// `"3am"` / `"12pm"` / `"2:30am"` のような時刻表記で始まるか判定する。
///
/// 分を含む `"resets 2:30am (Asia/Tokyo)"` は実ログで観測される表記だが、`":"` を
/// 単純な非数字として打ち切ると `":30am"` が残って am/pm 判定に失敗し、上限到達を
/// 取りこぼしていた。任意の `":<分>"` を明示的に読み飛ばす。
fn starts_with_clock_time(s: &str) -> bool {
    let bytes = s.as_bytes();
    let digits_from = |start: usize| {
        let mut i = start;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        i
    };

    // 時（1 桁以上の数字）
    let mut pos = digits_from(0);
    if pos == 0 {
        return false;
    }
    // 任意の ":<分>"。":" の後に数字が無ければ時刻表記ではない。
    if bytes.get(pos) == Some(&b':') {
        let minutes_end = digits_from(pos + 1);
        if minutes_end == pos + 1 {
            return false;
        }
        pos = minutes_end;
    }
    s[pos..].starts_with("am") || s[pos..].starts_with("pm")
}

/// リトライ可能な HTTP ステータス（5xx および 408/429）か判定する。
fn is_retryable_status(status: u64) -> bool {
    matches!(status, 408 | 429) || (500..600).contains(&status)
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

    /// 権限エラーや I/O エラーでファイルが読めない場合は Failed として扱う。
    /// （NotFound 以外のエラーを Success と誤分類すると state.json に誤記録されてしまう）
    #[test]
    fn classify_jsonl_io_error_is_failed() {
        // 既存のディレクトリをパスとして渡すと EISDIR で読み込み失敗するため、
        // NotFound 以外の I/O エラー経路を確認できる。
        let dir = tempfile::tempdir().unwrap();
        match classify_jsonl(dir.path()) {
            ResultClass::Failed(msg) => assert!(msg.contains("failed to read jsonl")),
            other => panic!("expected Failed for I/O error, got {other:?}"),
        }
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
    fn is_rate_limit_message_does_not_match_distant_am_pm() {
        // "resets <数字>" の後に直接 am/pm が続かない場合は誤検知しない。
        // 緩いマッチング（after.contains("am")）だと "resets 5 times max ... at 8am" のような
        // 一般エラーメッセージを RateLimited 扱いして state.json の記録を欠落させていた。
        assert!(!is_rate_limit_message(
            "API error: resets 5 times max. Please retry tomorrow at 8am"
        ));
        assert!(!is_rate_limit_message(
            "resets 100 entries before sample.pm processing"
        ));
    }

    #[test]
    fn is_rate_limit_message_matches_multi_digit_clock() {
        // "resets 12pm" / "resets 09am" のような複数桁の時刻表記を検出する
        assert!(is_rate_limit_message("resets 12pm"));
        assert!(is_rate_limit_message("resets 09am tomorrow"));
    }

    /// 実ログに 6 件存在した `resets 2:30am` 表記。`":"` を単なる非数字として
    /// 打ち切ると `":30am"` が残って am/pm 判定に失敗し、api_error_status=429 の
    /// Retryable へ落ちて残りのターゲット全件にエラー行が出続けていた。
    #[test]
    fn classify_session_limit_with_minutes_is_rate_limited() {
        let input = r#"{"type":"result","is_error":true,"api_error_status":429,"result":"You've hit your session limit · resets 2:30am (Asia/Tokyo)"}"#;
        assert_eq!(classify_content(input), ResultClass::RateLimited);
    }

    /// 実ログの `resets 3am` / `resets 12pm` 表記（分なし）も従来どおり検出する。
    #[test]
    fn classify_session_limit_without_minutes_is_rate_limited() {
        for msg in [
            "You've hit your session limit \u{b7} resets 3am (Asia/Tokyo)",
            "You've hit your session limit \u{b7} resets 12pm (Asia/Tokyo)",
        ] {
            assert!(is_rate_limit_message(msg), "should detect: {msg}");
        }
    }

    /// 月次の支出上限。リセット時刻表記が無いため `resets` 判定では拾えないが、
    /// リトライしても回復しないので Retryable にしてはいけない。
    #[test]
    fn classify_monthly_spend_limit_is_rate_limited() {
        let input = r#"{"type":"result","is_error":true,"api_error_status":429,"result":"You've hit your org's monthly spend limit · run /usage-credits to raise it, or visit claude.ai/admin-settings/usage"}"#;
        assert_eq!(classify_content(input), ResultClass::RateLimited);
    }

    #[test]
    fn is_rate_limit_message_matches_clock_with_minutes() {
        assert!(is_rate_limit_message("resets 2:30am"));
        assert!(is_rate_limit_message("resets 11:05pm tomorrow"));
        assert!(is_rate_limit_message("resets 09:00AM"));
    }

    /// `":"` の後に数字が無い表記は時刻ではないので拾わない。
    #[test]
    fn is_rate_limit_message_rejects_colon_without_minutes() {
        assert!(!is_rate_limit_message("resets 2:am"));
        assert!(!is_rate_limit_message("resets 3: pm"));
        assert!(!is_rate_limit_message("resets 5:"));
    }

    /// 「上限に達した」系の文言でも、`hit your` と `limit` が揃わなければ拾わない。
    #[test]
    fn is_rate_limit_message_requires_both_hit_your_and_limit() {
        assert!(!is_rate_limit_message("could not hit your endpoint"));
        assert!(!is_rate_limit_message("limit exceeded for this request"));
        assert!(is_rate_limit_message("You've hit your weekly limit"));
    }

    /// 「上限」系の文言を追加しても、既存の誤検知ガードは崩れていない。
    #[test]
    fn is_rate_limit_message_still_rejects_unrelated_errors() {
        assert!(!is_rate_limit_message(
            "API error: resets 5 times max. Please retry tomorrow at 8am"
        ));
        assert!(!is_rate_limit_message("API Error: 529 Overloaded"));
        assert!(!is_rate_limit_message(""));
    }

    #[test]
    fn starts_with_clock_time_boundaries() {
        assert!(starts_with_clock_time("1am"));
        assert!(starts_with_clock_time("0:00pm rest"));
        // 数字で始まらない / am・pm が続かない / 空文字はいずれも時刻ではない
        assert!(!starts_with_clock_time("am"));
        assert!(!starts_with_clock_time("12"));
        assert!(!starts_with_clock_time("12 am"));
        assert!(!starts_with_clock_time(""));
    }

    #[test]
    fn is_retryable_status_excludes_other_4xx() {
        // 408/429 以外の 4xx は再試行対象外
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
    }
}
