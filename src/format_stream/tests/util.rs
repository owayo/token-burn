//! `util` モジュールの純粋ヘルパー（数値/バイト/時刻整形・文字列切り詰め）の
//! 境界値・異常系・マルチバイト境界テスト。
//! `process` 経由では踏みにくい端数・オーバーフロー境界を直接検証する。

use crate::format_stream::util::{
    first_non_empty_string, first_string, format_byte_size, format_epoch_millis_clock,
    format_millis_as_seconds, truncate_inline,
};

// --- format_byte_size: B / KB / MB の境界 ---

#[test]
fn format_byte_size_zero_is_bytes() {
    assert_eq!(format_byte_size(0), "0B");
}

#[test]
fn format_byte_size_one_byte() {
    assert_eq!(format_byte_size(1), "1B");
}

#[test]
fn format_byte_size_just_below_kb_stays_bytes() {
    // 1023B はちょうど KB 未満なので B 表記のまま
    assert_eq!(format_byte_size(1023), "1023B");
}

#[test]
fn format_byte_size_exactly_kb_switches_unit() {
    // 1024B はちょうど 1.0KB に切り替わる境界
    assert_eq!(format_byte_size(1024), "1.0KB");
}

#[test]
fn format_byte_size_just_below_mb_stays_kb() {
    // 1MB - 1 = 1048575B は MB 未満なので KB 表記（四捨五入で 1024.0KB）
    assert_eq!(format_byte_size(1024 * 1024 - 1), "1024.0KB");
}

#[test]
fn format_byte_size_exactly_mb_switches_unit() {
    assert_eq!(format_byte_size(1024 * 1024), "1.0MB");
}

#[test]
fn format_byte_size_rounds_to_one_decimal() {
    // 1536B = 1.5KB（端数 1 桁表示）
    assert_eq!(format_byte_size(1536), "1.5KB");
    // 41119B ≈ 40.2KB（実 jsonl 由来の値）
    assert_eq!(format_byte_size(41119), "40.2KB");
}

#[test]
fn format_byte_size_large_value_does_not_overflow() {
    // u64 上限近傍でも f64 演算でパニックせず MB 表記になる
    let result = format_byte_size(u64::MAX);
    assert!(result.ends_with("MB"), "got: {result}");
}

// --- format_millis_as_seconds: 1000 倍数 vs 端数 ---

#[test]
fn format_millis_as_seconds_zero() {
    assert_eq!(format_millis_as_seconds(0), "0s");
}

#[test]
fn format_millis_as_seconds_exact_second_has_no_decimal() {
    // ちょうど 1000ms 倍数は整数秒（小数点なし）
    assert_eq!(format_millis_as_seconds(1000), "1s");
    assert_eq!(format_millis_as_seconds(60000), "60s");
}

#[test]
fn format_millis_as_seconds_sub_second_shows_decimal() {
    // 1000ms 未満は端数 1 桁表示
    assert_eq!(format_millis_as_seconds(500), "0.5s");
    assert_eq!(format_millis_as_seconds(1), "0.0s");
}

#[test]
fn format_millis_as_seconds_fractional_rounds_one_decimal() {
    // 14837ms → 14.8s（CLAUDE.md の例）
    assert_eq!(format_millis_as_seconds(14837), "14.8s");
    // 1500 ミリ秒は 1.5 秒として表示する
    assert_eq!(format_millis_as_seconds(1500), "1.5s");
}

// --- first_string: キー優先順・欠落・非文字列 ---

#[test]
fn first_string_returns_first_present_key() {
    let v = serde_json::json!({"a": "alpha", "b": "beta"});
    // 探索順の先頭にマッチしたキーを返す
    assert_eq!(first_string(&v, &["a", "b"]), "alpha");
}

#[test]
fn first_string_skips_missing_keys_until_match() {
    let v = serde_json::json!({"b": "beta"});
    // 先頭キーが欠落していても次のキーにフォールバックする
    assert_eq!(first_string(&v, &["a", "b"]), "beta");
}

#[test]
fn first_string_respects_key_order_when_both_present() {
    let v = serde_json::json!({"a": "alpha", "b": "beta"});
    // キー配列の順序が優先される（b が先なら b を返す）
    assert_eq!(first_string(&v, &["b", "a"]), "beta");
}

#[test]
fn first_string_skips_non_string_values() {
    // 数値・真偽値・配列・null は文字列ではないのでスキップする
    let v = serde_json::json!({"a": 42, "b": true, "c": "found"});
    assert_eq!(first_string(&v, &["a", "b", "c"]), "found");
}

#[test]
fn first_string_returns_empty_when_no_key_matches() {
    let v = serde_json::json!({"x": "y"});
    assert_eq!(first_string(&v, &["a", "b"]), "");
}

#[test]
fn first_string_empty_keys_returns_empty() {
    let v = serde_json::json!({"a": "alpha"});
    assert_eq!(first_string(&v, &[]), "");
}

#[test]
fn first_string_returns_empty_string_value_when_key_present() {
    // 値が空文字でも as_str() は Some を返すため、その空文字を採用する
    let v = serde_json::json!({"a": "", "b": "beta"});
    assert_eq!(first_string(&v, &["a", "b"]), "");
}

// --- first_non_empty_string: 空文字をスキップして次の候補へ進む ---
// first_string との差分（空文字を確定しない）が本体。hook イベントのように
// 候補キーが全て常設される payload で、内容のあるキーまで辿れることを固定する。

#[test]
fn first_non_empty_string_skips_empty_value_and_takes_next() {
    // これが効かないと hook_response の output:"" が採用され、stderr の
    // 失敗診断が丸ごと落ちる（旧実装の実バグ）。
    let v = serde_json::json!({"output": "", "stderr": "boom", "stdout": ""});
    assert_eq!(
        first_non_empty_string(&v, &["output", "stderr", "stdout"]),
        "boom"
    );
}

#[test]
fn first_non_empty_string_all_empty_returns_empty() {
    let v = serde_json::json!({"output": "", "stderr": "", "stdout": ""});
    assert_eq!(
        first_non_empty_string(&v, &["output", "stderr", "stdout"]),
        ""
    );
}

#[test]
fn first_non_empty_string_takes_middle_key_only() {
    // 前後が空でも中間の非空キーだけを拾う
    let v = serde_json::json!({"a": "", "b": "found", "c": ""});
    assert_eq!(first_non_empty_string(&v, &["a", "b", "c"]), "found");
}

#[test]
fn first_non_empty_string_skips_whitespace_only_values() {
    // 空白・改行のみの値も「内容なし」として次の候補へ進む
    let v = serde_json::json!({"a": "   ", "b": "\n\t", "c": "real"});
    assert_eq!(first_non_empty_string(&v, &["a", "b", "c"]), "real");
}

#[test]
fn first_non_empty_string_returns_value_verbatim_without_trimming() {
    // 空判定にのみ trim を使い、返す値自体は加工しない
    let v = serde_json::json!({"a": "  padded  "});
    assert_eq!(first_non_empty_string(&v, &["a"]), "  padded  ");
}

#[test]
fn first_non_empty_string_returns_empty_when_keys_absent() {
    let v = serde_json::json!({"x": "y"});
    assert_eq!(first_non_empty_string(&v, &["a", "b"]), "");
    assert_eq!(first_non_empty_string(&v, &[]), "");
}

#[test]
fn first_non_empty_string_skips_non_string_values() {
    // 数値・真偽値・null は文字列ではないのでスキップする
    let v = serde_json::json!({"a": 42, "b": null, "c": "found"});
    assert_eq!(first_non_empty_string(&v, &["a", "b", "c"]), "found");
}

#[test]
fn first_non_empty_string_respects_key_order_when_both_non_empty() {
    // 空文字スキップを入れてもキー配列の優先順は変わらない
    let v = serde_json::json!({"a": "alpha", "b": "beta"});
    assert_eq!(first_non_empty_string(&v, &["a", "b"]), "alpha");
    assert_eq!(first_non_empty_string(&v, &["b", "a"]), "beta");
}

// --- truncate_inline: 空白正規化 + 切り詰め ---

#[test]
fn truncate_inline_collapses_internal_whitespace() {
    // 連続する空白・タブ・改行は単一スペースに正規化される
    assert_eq!(truncate_inline("a\t b\n\nc", 100), "a b c");
}

#[test]
fn truncate_inline_trims_leading_and_trailing_whitespace() {
    assert_eq!(truncate_inline("  hello  world  ", 100), "hello world");
}

#[test]
fn truncate_inline_truncates_after_normalization() {
    // 正規化後に文字数で切り詰める（"a b c d e" は 9 文字）
    assert_eq!(truncate_inline("a   b   c   d   e", 5), "a ...");
}

#[test]
fn truncate_inline_empty_and_whitespace_only() {
    assert_eq!(truncate_inline("", 10), "");
    // 空白のみは正規化で空文字になる
    assert_eq!(truncate_inline("   \n\t  ", 10), "");
}

#[test]
fn truncate_inline_collapses_unicode_whitespace() {
    // 全角スペース(U+3000)も split_whitespace の空白扱いで ASCII スペースに正規化される
    assert_eq!(truncate_inline("あ　い", 100), "あ い");
}

#[test]
fn truncate_inline_multibyte_counts_chars_not_bytes() {
    // 日本語は 1 文字 3 バイトだが、文字数でカウントして切り詰める
    // "あ い う え お" は正規化後 9 文字。max=4 で "あ..." になる
    assert_eq!(truncate_inline("あ い う え お", 4), "あ...");
}

// --- format_epoch_millis_clock: epoch 秒/ミリ秒の閾値・負値・境界 ---

#[test]
fn format_epoch_millis_clock_seconds_and_millis_agree() {
    // 同じ瞬間を「秒」と「ミリ秒」で渡したら同じ時刻文字列になる
    // (閾値 1_000_000_000_000 でミリ秒判定される)
    let seconds = 1_700_000_000_i64;
    let millis = seconds * 1000;
    let from_seconds = format_epoch_millis_clock(seconds);
    let from_millis = format_epoch_millis_clock(millis);
    assert!(from_seconds.is_some());
    assert_eq!(from_seconds, from_millis);
}

#[test]
fn format_epoch_millis_clock_formats_as_hh_mm() {
    // 出力はローカルタイムゾーン依存だが必ず HH:MM 形式（コロン区切り 5 文字）
    let result = format_epoch_millis_clock(1_700_000_000_000).expect("valid epoch");
    assert_eq!(result.len(), 5, "got: {result}");
    assert_eq!(result.as_bytes()[2], b':', "got: {result}");
    assert!(
        result[..2].chars().all(|c| c.is_ascii_digit()),
        "hour digits, got: {result}"
    );
    assert!(
        result[3..].chars().all(|c| c.is_ascii_digit()),
        "minute digits, got: {result}"
    );
}

#[test]
fn format_epoch_millis_clock_epoch_zero_is_valid() {
    // epoch 0（1970-01-01）も有効な時刻として整形できる
    assert!(format_epoch_millis_clock(0).is_some());
}

#[test]
fn format_epoch_millis_clock_negative_epoch_is_valid() {
    // 負の epoch（1970 より前）も from_timestamp で有効
    assert!(format_epoch_millis_clock(-1).is_some());
}

#[test]
fn format_epoch_millis_clock_above_threshold_divides_by_1000() {
    // 「> 閾値」の値はミリ秒とみなして /1000 される。
    // よって 1_700_000_000_000(ms) は 1_700_000_000(s) と同じ瞬間 = 同じ表示になる。
    // (ローカルタイムゾーンに依存せず、同一 instant の比較なので決定的)
    let as_millis = format_epoch_millis_clock(1_700_000_000_000);
    let as_seconds = format_epoch_millis_clock(1_700_000_000);
    assert!(as_millis.is_some());
    assert_eq!(as_millis, as_seconds);
}

#[test]
fn format_epoch_millis_clock_threshold_boundary_is_seconds_not_millis() {
    // 閾値はちょうど 1_000_000_000_000 で「> 閾値」のみミリ秒扱い。
    // 閾値ちょうどは秒として扱われる（=西暦 33658 年相当）。
    // ミリ秒扱いされていれば 1_000_000_000(s)=2001 年と同じ表示になるはずだが、
    // 実装は秒扱いなので別 instant として整形される（None ではなく Some）。
    let at_threshold = format_epoch_millis_clock(1_000_000_000_000);
    assert!(
        at_threshold.is_some(),
        "boundary value should still be formattable"
    );
    // 1 多いだけの値はミリ秒扱い(/1000=1_000_000_000s)になり、秒扱いの閾値とは
    // 約 3 万年離れた別 instant になる。両者が同じ instant でないことを
    // タイムゾーン非依存に確認するため、秒解釈の基準値と突き合わせる。
    let above_as_millis = format_epoch_millis_clock(1_000_000_000_001);
    assert_eq!(
        above_as_millis,
        format_epoch_millis_clock(1_000_000_000),
        "above-threshold value must be divided by 1000 (millis path)"
    );
}

#[test]
fn format_epoch_millis_clock_out_of_range_returns_none() {
    // i64 最大値を秒として渡すと chrono の表現可能範囲外で None
    assert_eq!(format_epoch_millis_clock(i64::MAX), None);
}
