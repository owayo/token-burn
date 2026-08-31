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
        !clean.contains("Rate limit auto-stop"),
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
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.96,"surpassedThreshold":0.95,"isUsingOverage":true}}"#;
    let clean = strip_ansi(&run_process(input));
    assert!(clean.contains("auto-stop"), "{clean}");
    assert!(clean.contains("using_overage"), "{clean}");
}

// --- 停止判定の基準枠（unifiedWindows）のテスト ---
// 実データ（~/Documents/token-burn の jsonl）では、月次の追加課金枠が 103% に
// なると `rateLimitType:"overage"` / `utilization:1.03` の警告が繰り返し届く
// （13 セッションで 188 件）。同じイベントの `unifiedWindows.five_hour` は 0.13 で
// リクエストは通っており、この % で全タスクを止めてはいけない。

#[test]
fn overage_warning_does_not_stop_when_gating_windows_are_low() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // 実ログそのままの overage 警告。5 時間枠 13% / 7 日枠 54% で余裕がある。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"overage","utilization":1.03,"isUsingOverage":false,"surpassedThreshold":1,"unifiedWindows":{"five_hour":{"utilization":0.13},"seven_day":{"utilization":0.54},"seven_day_overage_included":{"utilization":0.02}}}}"#;
    let clean = strip_ansi(&run_process_with_opts(input, None, Some(&stop_file), 90));
    assert!(
        !clean.contains("Rate limit auto-stop"),
        "追加課金枠の使用率で停止してはいけない: {clean}"
    );
    assert!(
        !stop_file.exists(),
        "5 時間枠 13% で stop file が作られてはいけない"
    );
    assert!(clean.contains("103%"), "警告自体は表示する: {clean}");
    assert!(
        clean.contains("no auto-stop"),
        "停止判定に使わない枠であることを明示すべき: {clean}"
    );
    assert!(
        clean.contains("5h 13%") && clean.contains("7d 54%"),
        "実際に判定へ使う枠の使用率を併記すべき: {clean}"
    );
}

#[test]
fn overage_warning_still_stops_when_five_hour_is_exhausted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // overage の警告でも、同じイベントの 5 時間枠が閾値を超えていれば停止する。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"overage","utilization":1.03,"surpassedThreshold":1,"unifiedWindows":{"five_hour":{"utilization":0.95},"seven_day":{"utilization":0.54}}}}"#;
    let clean = strip_ansi(&run_process_with_opts(input, None, Some(&stop_file), 90));
    assert!(clean.contains("auto-stop"), "{clean}");
    assert!(
        clean.contains("95% used (five_hour)"),
        "停止理由は追加課金枠でなく 5 時間枠であるべき: {clean}"
    );
    assert!(
        !clean.contains("warning at"),
        "追加課金枠に対する警告閾値を 5 時間枠の停止行へ持ち込まない: {clean}"
    );
    assert!(stop_file.exists(), "stop file が作成されるべき");
}

#[test]
fn stop_uses_worst_gating_window_and_shows_both() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // 5 時間枠と 7 日枠の両方が閾値超過なら、高い方を主理由にして両方を残す。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.92,"unifiedWindows":{"five_hour":{"utilization":0.92},"seven_day":{"utilization":0.97}}}}"#;
    let clean = strip_ansi(&run_process_with_opts(input, None, Some(&stop_file), 90));
    assert!(
        clean.contains("97% used (seven_day)"),
        "使用率が高い枠を主理由にすべき: {clean}"
    );
    assert!(
        clean.contains("5h 92%") && clean.contains("7d 97%"),
        "両方の枠を併記すべき: {clean}"
    );
    assert!(stop_file.exists());
}

#[test]
fn stop_shows_reset_of_the_triggering_window() {
    // 停止した枠のリセット時刻を出す。top-level の `resetsAt` は `rateLimitType` が
    // 指す枠（実データでは overage）のもので、5 時間枠の復帰時刻ではない。
    let today_noon = chrono::Local::now()
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .expect("valid time")
        .and_local_timezone(chrono::Local)
        .earliest()
        .expect("resolvable local time")
        .timestamp();
    let input = format!(
        r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"allowed_warning","rateLimitType":"overage","utilization":1.03,"resetsAt":{},"unifiedWindows":{{"five_hour":{{"utilization":0.95,"resetsAt":{}}}}}}}}}"#,
        today_noon + 7200,
        today_noon
    );
    let clean = strip_ansi(&run_process_with_opts(&input, None, None, 90));
    assert!(clean.contains("auto-stop"), "{clean}");
    assert!(
        clean.contains("resets 12:00"),
        "停止した枠自身のリセット時刻を出すべき: {clean}"
    );
    assert!(
        !clean.contains("14:00"),
        "追加課金枠のリセット時刻を出してはいけない: {clean}"
    );
}

#[test]
fn allowed_status_stops_when_gating_window_exceeds_threshold() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // 閾値を警告閾値より低く設定した場合、`allowed` のままでも超過し得る。
    // ここで判定しないと停止が漏れる。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.85},"seven_day":{"utilization":0.3}}}}"#;
    let clean = strip_ansi(&run_process_with_opts(input, None, Some(&stop_file), 80));
    assert!(
        clean.contains("85% used (five_hour)"),
        "allowed でも閾値超過なら停止すべき: {clean}"
    );
    assert!(stop_file.exists(), "stop file が作成されるべき");
}

#[test]
fn allowed_status_below_threshold_stays_silent() {
    // 閾値未満の `allowed` は従来どおり黙る（480 件/13 セッションの高頻度イベント）。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.5},"seven_day":{"utilization":0.3}}}}"#;
    assert_eq!(strip_ansi(&run_process(input)), "");
}

#[test]
fn legacy_overage_warning_without_windows_does_not_stop() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // `unifiedWindows` を持たない形式でも、追加課金枠の使用率では停止しない。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"overage","utilization":1.03}}"#;
    let clean = strip_ansi(&run_process_with_opts(input, None, Some(&stop_file), 90));
    assert!(!clean.contains("Rate limit auto-stop"), "{clean}");
    assert!(!stop_file.exists());
    assert!(clean.contains("103%"), "警告自体は表示する: {clean}");
}

#[test]
fn legacy_gating_warning_without_windows_still_stops() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // `unifiedWindows` を持たない形式の 5 時間枠警告は従来どおり停止する。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.95}}"#;
    let clean = strip_ansi(&run_process_with_opts(input, None, Some(&stop_file), 90));
    assert!(clean.contains("95% used (five_hour)"), "{clean}");
    assert!(stop_file.exists());
}

#[test]
fn rejected_stops_regardless_of_limit_type() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // `rejected` はリクエストが実際に拒否された結果なので、枠の種類に依らず停止する。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"overage","unifiedWindows":{"five_hour":{"utilization":0.1}}}}"#;
    let clean = strip_ansi(&run_process_with_opts(input, None, Some(&stop_file), 90));
    assert!(clean.contains("rejected"), "{clean}");
    assert!(
        stop_file.exists(),
        "5 時間枠が空いていても拒否されたら停止すべき"
    );
}

#[test]
fn malformed_window_utilization_is_ignored() {
    let tmp = tempfile::TempDir::new().unwrap();
    let stop_file = tmp.path().join("stop");
    // 使用率が読めない枠（文字列・負値）は判定に使えないので無視し、読める枠だけで
    // 判定する。ここでは 7 日枠だけが有効で閾値未満なので停止しない。
    let input = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","rateLimitType":"five_hour","utilization":0.99,"unifiedWindows":{"five_hour":{"utilization":"broken"},"seven_day":{"utilization":0.2}}}}"#;
    let clean = strip_ansi(&run_process_with_opts(input, None, Some(&stop_file), 90));
    assert!(
        !clean.contains("Rate limit auto-stop"),
        "壊れた枠を理由に停止してはいけない: {clean}"
    );
    assert!(!stop_file.exists());
    assert!(clean.contains("7d 20%"), "読める枠だけ併記する: {clean}");
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
