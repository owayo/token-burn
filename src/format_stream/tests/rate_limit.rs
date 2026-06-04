//! レート制限イベント表示・stop file 作成・時刻整形のテスト。

use super::*;

#[test]
fn process_rate_limit_event_ignored() {
    // rate_limit_event は無視される
    let input = r#"{"type":"rate_limit_event","limits":{"input_tokens":{"limit":100000,"remaining":50000}}}"#;
    let output = run_process(input);
    assert!(output.is_empty(), "rate_limit_event は出力されるべきでない");
}

#[test]
fn process_rate_limit_warning_shows_utilization() {
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.79}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("79%"), "使用率が表示されるべき: {}", clean);
    assert!(
        clean.contains("seven_day"),
        "制限タイプが表示されるべき: {}",
        clean
    );
}

#[test]
fn process_rate_limit_rejected_shows_error() {
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("rejected"),
        "拒否状態が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("five_hour"),
        "制限タイプが表示されるべき: {}",
        clean
    );
}

#[test]
fn process_rate_limit_allowed_is_silent() {
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"seven_day","utilization":0.5}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.is_empty(),
        "allowed は表示されるべきでない: {}",
        clean
    );
}

#[test]
fn process_rate_limit_allowed_with_details_is_shown() {
    // 実データにある overage 情報付き allowed は補足情報を表示する
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","resetsAt":1776009600,"overageStatus":"rejected","overageResetsAt":1776006000,"overageDisabledReason":"org_level_disabled_until","isUsingOverage":false}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("allowed"),
        "allowed 状態が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("five_hour"),
        "制限タイプが表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("overage:rejected"),
        "overage 状態が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("overage_resets"),
        "overage のリセット時刻が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("resets"),
        "リセット時刻が表示されるべき: {}",
        clean
    );
}

#[test]
fn process_rate_limit_auto_stop_touches_stop_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // 95% >= threshold 95 → stop file が作成される
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.95}}"#;
    let output = run_process_with_opts(input, None, Some(&stop_file), 95);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("auto-stop"),
        "auto-stop メッセージが表示されるべき: {}",
        clean
    );
    assert!(clean.contains("95%"), "使用率が表示されるべき: {}", clean);
    assert!(stop_file.exists(), "stop file が作成されるべき");
}

#[test]
fn process_rate_limit_below_threshold_no_stop_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // 79% < threshold 95 → 通常の警告、stop file は作成されない
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.79}}"#;
    let output = run_process_with_opts(input, None, Some(&stop_file), 95);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("79%"),
        "通常の警告が表示されるべき: {}",
        clean
    );
    assert!(
        !clean.contains("auto-stop"),
        "auto-stop は表示されるべきでない: {}",
        clean
    );
    assert!(!stop_file.exists(), "stop file は作成されるべきでない");
}

#[test]
fn process_rate_limit_rejected_touches_stop_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#;
    let output = run_process_with_opts(input, None, Some(&stop_file), 95);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("rejected"),
        "拒否メッセージが表示されるべき: {}",
        clean
    );
    assert!(
        stop_file.exists(),
        "rejected 時に stop file が作成されるべき"
    );
}

#[test]
fn process_rate_limit_custom_threshold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // 80% が閾値 80% 以上なので自動停止する
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.80}}"#;
    let output = run_process_with_opts(input, None, Some(&stop_file), 80);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("auto-stop"),
        "カスタム閾値で auto-stop されるべき: {}",
        clean
    );
    assert!(clean.contains("80%"), "閾値が表示されるべき: {}", clean);
    assert!(stop_file.exists(), "stop file が作成されるべき");
}

#[test]
fn process_rate_limit_no_stop_file_path_still_shows_message() {
    // stop_file が None でも閾値超過時のメッセージは表示される
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.96}}"#;
    let output = run_process_with_opts(input, None, None, 95);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("auto-stop"),
        "stop_file なしでも auto-stop メッセージは表示されるべき: {}",
        clean
    );
}

#[test]
fn process_rate_limit_warning_shows_resets_at() {
    // resetsAt タイムスタンプがローカル時刻で表示される
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.80,"resetsAt":1776009600}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("80%"), "使用率が表示されるべき: {}", clean);
    assert!(
        clean.contains("resets"),
        "リセット時刻が表示されるべき: {}",
        clean
    );
}

#[test]
fn process_rate_limit_rejected_shows_resets_at() {
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1776009600}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("rejected"),
        "拒否状態が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("resets"),
        "リセット時刻が表示されるべき: {}",
        clean
    );
}

#[test]
fn process_rate_limit_without_resets_at() {
    // resetsAt がない場合はリセット時刻を表示しない
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.79}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("79%"), "使用率が表示されるべき: {}", clean);
    assert!(
        !clean.contains("resets"),
        "resetsAt がない場合はリセット時刻が表示されるべきでない: {}",
        clean
    );
}

#[test]
fn process_rate_limit_warning_shows_surpassed_threshold() {
    // surpassedThreshold (0.9) が含まれる場合は警告閾値が併記される
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.91,"surpassedThreshold":0.9}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("91%"), "使用率が表示されるべき: {}", clean);
    assert!(
        clean.contains("warning at 90%"),
        "通過済み警告閾値が表示されるべき: {}",
        clean
    );
}

#[test]
fn process_rate_limit_auto_stop_shows_surpassed_threshold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.96,"surpassedThreshold":0.95}}"#;
    let output = run_process_with_opts(input, None, Some(&stop_file), 95);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("auto-stop"),
        "auto-stop メッセージが表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("warning at 95%"),
        "auto-stop 時も通過済み警告閾値が表示されるべき: {}",
        clean
    );
}

#[test]
fn process_rate_limit_warning_without_surpassed_threshold() {
    // surpassedThreshold がない場合は併記されない
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.80}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.contains("warning at"),
        "surpassedThreshold がない場合は表示されるべきでない: {}",
        clean
    );
}

// --- format_token_size の単体テスト ---

#[test]
fn format_token_size_million() {
    assert_eq!(format_token_size(1_000_000), "1M");
    assert_eq!(format_token_size(2_000_000), "2M");
}

#[test]
fn format_token_size_thousands() {
    assert_eq!(format_token_size(64_000), "64K");
    assert_eq!(format_token_size(200_000), "200K");
}

#[test]
fn format_token_size_non_round_falls_back_to_number() {
    // ちょうど割り切れない値はカンマ区切りの数値表示にフォールバック
    assert_eq!(format_token_size(64_500), "64,500");
    assert_eq!(format_token_size(1_500_500), "1,500,500");
}

#[test]
fn format_token_size_small_value() {
    assert_eq!(format_token_size(500), "500");
    assert_eq!(format_token_size(0), "0");
}

#[test]
fn format_token_size_exactly_one_thousand_is_k() {
    // ちょうど 1000 は K 表記の下限境界
    assert_eq!(format_token_size(1_000), "1K");
    // 999 は K 未満なので数値表示
    assert_eq!(format_token_size(999), "999");
}

#[test]
fn format_token_size_million_takes_precedence_over_thousand() {
    // 1_000_000 は 1000 の倍数でもあるが、M 判定が先なので "1000K" ではなく "1M"
    assert_eq!(format_token_size(1_000_000), "1M");
}

#[test]
fn format_token_size_thousand_multiple_above_million_uses_k() {
    // 1_001_000 は 1M の倍数ではないが 1000 の倍数なので K 表記（"1001K"）
    assert_eq!(format_token_size(1_001_000), "1001K");
}

#[test]
fn format_token_size_near_million_non_multiples_fall_back_to_number() {
    // 100 万近傍で割り切れない値は数値表示にフォールバック
    assert_eq!(format_token_size(999_999), "999,999");
    assert_eq!(format_token_size(1_000_001), "1,000,001");
}

// --- format_resets_at の単体テスト ---

#[test]
fn format_resets_at_valid_timestamp() {
    let info: serde_json::Value = serde_json::from_str(r#"{"resetsAt":1776009600}"#).unwrap();
    let result = format_resets_at(&info);
    assert!(
        result.starts_with(" resets "),
        "有効なタイムスタンプはリセット時刻を返すべき: {}",
        result
    );
}

#[test]
fn format_resets_at_missing_field() {
    let info: serde_json::Value = serde_json::from_str(r#"{"status":"allowed"}"#).unwrap();
    let result = format_resets_at(&info);
    assert_eq!(result, "", "resetsAt がない場合は空文字列を返すべき");
}

#[test]
fn format_resets_at_null_value() {
    let info: serde_json::Value = serde_json::from_str(r#"{"resetsAt":null}"#).unwrap();
    let result = format_resets_at(&info);
    assert_eq!(result, "", "resetsAt が null の場合は空文字列を返すべき");
}
