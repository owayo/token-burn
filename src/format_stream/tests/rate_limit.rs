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
fn process_rate_limit_event_breaks_open_text_line() {
    // 実ログではテキスト delta の単語途中に rate_limit_event が到着する。通知を直接
    // 書くと `I<通知>'ll` のように本文と同じ行へ混入するため、独立した行へ分離する。
    let input = [
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I"}}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","resetsAt":1776009600}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"'ll continue"}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
    ]
    .join("\n");

    let clean = strip_ansi(&run_process(&input));
    assert!(
        clean.contains("I\n  ℹ Rate limit status: allowed (five_hour)"),
        "rate-limit 通知は本文と独立した行へ出るべき: {clean:?}"
    );
    assert!(
        clean.contains("\n'll continue\n"),
        "通知後の text delta も独立した本文行へ続くべき: {clean:?}"
    );
}

#[test]
fn process_silent_rate_limit_event_keeps_open_text_line() {
    // 詳細の無い allowed は表示しないため、前後の本文 delta に改行を挟まない。
    let input = [
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"前半"}}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"後半"}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
    ]
    .join("\n");

    assert_eq!(strip_ansi(&run_process(&input)), "前半後半\n");
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
fn process_rate_limit_stop_file_is_idempotent_and_preserves_existing_content() {
    // 既存の stop_file が存在する場合、`create_new` により上書きされず内容を保持する。
    // 別ワーカーが先に書き込んだ理由を後続イベントが消してしまうのを防ぐため。
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    let existing = "preexisting reason\n";
    std::fs::write(&stop_file, existing).unwrap();

    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#;
    let _ = run_process_with_opts(input, None, Some(&stop_file), 95);

    let after = std::fs::read_to_string(&stop_file).unwrap();
    assert_eq!(
        after, existing,
        "既存の stop file 内容は維持されるべき (create_new で上書きしない)"
    );
}

#[test]
fn process_rate_limit_stop_file_creation_failure_is_surfaced() {
    // stop file の親ディレクトリが存在しない等で作成に失敗した場合、握り潰さずに
    // 警告を出力へ明示する。後続タスクの自動停止シグナル（stop file）が生成されない
    // ことを可視化するため（format-stream はパイプ中段で exit code が観測されない）。
    let tmp = tempfile::TempDir::new().unwrap();
    // 存在しないサブディレクトリ配下を指定 → create_new が NotFound で失敗する。
    let stop_file = tmp.path().join("missing-dir").join("stop");
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"weekly"}}"#;
    let output = run_process_with_opts(input, None, Some(&stop_file), 95);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("rejected"),
        "拒否メッセージは表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("作成に失敗"),
        "stop file 作成失敗の警告が表示されるべき: {}",
        clean
    );
    assert!(
        !stop_file.exists(),
        "作成に失敗したので stop file は存在しない"
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

// --- overage（超過枠）補足と日付付きリセット時刻のテスト ---
// いずれも ~/Documents/token-burn の実 jsonl に現れた rate_limit_info を再現する。

#[test]
fn rejected_shows_overage_details() {
    // 実データの rejected は overageStatus / overageResetsAt / isUsingOverage を伴う。
    // これらを落とすと 5 時間枠の resets だけが残り、実際は超過枠まで使い切っていて
    // 復旧が数週間先でも「その時刻まで待てば再開できる」と誤読される。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1785772200,"rateLimitType":"five_hour","overageStatus":"allowed","overageResetsAt":1788220800,"isUsingOverage":true,"overageInUse":true}}"#;
    let clean = strip_ansi(&run_process(input));
    assert!(clean.contains("rejected"), "{clean}");
    assert!(clean.contains("five_hour"), "{clean}");
    assert!(
        clean.contains("overage:allowed"),
        "overage の可否が表示されるべき: {clean}"
    );
    assert!(
        clean.contains("using_overage"),
        "超過枠を消費中である旨が表示されるべき: {clean}"
    );
    assert!(
        clean.contains("overage_resets:"),
        "超過枠のリセット時刻が表示されるべき: {clean}"
    );
}

#[test]
fn rejected_without_overage_keeps_previous_format() {
    // overage 情報が無い rejected は従来どおり括弧を増やさない。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#;
    let clean = strip_ansi(&run_process(input));
    assert!(
        clean.trim_end().ends_with("rejected (five_hour)"),
        "余計な括弧を付けないべき: {clean}"
    );
}

#[test]
fn warning_shows_using_overage() {
    // 警告イベントは実データで isUsingOverage を常に持つ。通常枠の警告か
    // 超過枠の警告かを区別できないと停止判断を誤る。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.79,"isUsingOverage":true}}"#;
    let clean = strip_ansi(&run_process(input));
    assert!(clean.contains("79%"), "{clean}");
    assert!(
        clean.contains("using_overage"),
        "超過枠の消費中表示が必要: {clean}"
    );
}

#[test]
fn warning_without_overage_has_no_overage_suffix() {
    // isUsingOverage:false のときは補足を付けない（過剰表示の防止）。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"seven_day","utilization":0.79,"isUsingOverage":false}}"#;
    let clean = strip_ansi(&run_process(input));
    assert!(!clean.contains("using_overage"), "{clean}");
}

#[test]
fn overage_in_use_alias_is_recognized() {
    // isUsingOverage が無く overageInUse だけの形でも超過枠消費として扱う。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","overageInUse":true}}"#;
    let clean = strip_ansi(&run_process(input));
    assert!(clean.contains("using_overage"), "{clean}");
}

#[test]
fn auto_stop_line_includes_overage_flag() {
    // 閾値超過で停止する行にも超過枠の消費有無を残す。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"overage","utilization":1.03,"surpassedThreshold":1,"isUsingOverage":true}}"#;
    let clean = strip_ansi(&run_process(input));
    assert!(clean.contains("auto-stop"), "{clean}");
    assert!(clean.contains("using_overage"), "{clean}");
}

#[test]
fn resets_today_shows_time_only() {
    // 当日中のリセットは従来どおり HH:MM のみ（日付でノイズを増やさない）。
    let today_noon = chrono::Local::now()
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .expect("valid time");
    let ts = today_noon
        .and_local_timezone(chrono::Local)
        .earliest()
        .expect("resolvable local time")
        .timestamp();
    let info: serde_json::Value = serde_json::json!({ "resetsAt": ts });
    let result = format_resets_at(&info);
    assert_eq!(result, " resets 12:00", "当日は時刻のみのはず: {result}");
}

#[test]
fn resets_on_another_day_includes_date() {
    // seven_day 枠や overage 枠のリセットは最大 1 か月先になる。時刻だけだと
    // 「今日のその時刻に回復する」と誤読されるため日付を添える。
    let ts = (chrono::Local::now() + chrono::Duration::days(28)).timestamp();
    let info: serde_json::Value = serde_json::json!({ "resetsAt": ts });
    let result = format_resets_at(&info);
    assert!(
        result.starts_with(" resets ") && result.contains('/'),
        "翌日以降は日付付きで表示すべき: {result}"
    );
}
