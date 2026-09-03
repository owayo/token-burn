//! `blocks` モジュールのブロック種別分類（`block_kind` /
//! `infer_block_kind_from_delta`）の単体テスト。
//! 各 stream-json の type / delta type に対する分類を直接固定する。

use crate::format_stream::blocks::{
    BlockKind, ContentBlockState, ThinkingProgress, block_kind, break_open_line, finalize_block,
    infer_block_kind_from_delta,
};
use std::collections::HashMap;

// --- content_block.type から BlockKind への変換 ---

#[test]
fn block_kind_maps_known_block_types() {
    assert_eq!(block_kind("text"), BlockKind::Text);
    assert_eq!(block_kind("thinking"), BlockKind::Thinking);
    assert_eq!(block_kind("tool_use"), BlockKind::ToolUse);
    assert_eq!(block_kind("server_tool_use"), BlockKind::ServerToolUse);
}

#[test]
fn block_kind_unknown_type_is_unknown() {
    assert_eq!(block_kind("redacted_thinking"), BlockKind::Unknown);
    assert_eq!(block_kind("image"), BlockKind::Unknown);
}

#[test]
fn block_kind_empty_string_is_unknown() {
    // content_block.type が欠落して空文字フォールバックした場合
    assert_eq!(block_kind(""), BlockKind::Unknown);
}

#[test]
fn block_kind_is_case_sensitive() {
    // 完全一致のみ。大文字違いは Unknown
    assert_eq!(block_kind("Text"), BlockKind::Unknown);
    assert_eq!(block_kind("TOOL_USE"), BlockKind::Unknown);
}

// --- delta.type から BlockKind への推論 ---
// delta が content_block_start より先に届いた場合の復旧分類に使われる。

#[test]
fn infer_block_kind_from_delta_text() {
    assert_eq!(infer_block_kind_from_delta("text_delta"), BlockKind::Text);
}

#[test]
fn infer_block_kind_from_delta_thinking_variants_map_to_thinking() {
    // thinking_delta と signature_delta はどちらも思考ブロックに属する
    assert_eq!(
        infer_block_kind_from_delta("thinking_delta"),
        BlockKind::Thinking
    );
    assert_eq!(
        infer_block_kind_from_delta("signature_delta"),
        BlockKind::Thinking
    );
}

#[test]
fn infer_block_kind_from_delta_input_json_is_tool_use() {
    // input_json_delta はツール入力の断片なので ToolUse に分類する
    assert_eq!(
        infer_block_kind_from_delta("input_json_delta"),
        BlockKind::ToolUse
    );
}

#[test]
fn infer_block_kind_from_delta_unknown_delta_is_unknown() {
    assert_eq!(
        infer_block_kind_from_delta("citations_delta"),
        BlockKind::Unknown
    );
    assert_eq!(infer_block_kind_from_delta(""), BlockKind::Unknown);
}

#[test]
fn infer_block_kind_does_not_infer_server_tool_use() {
    // server_tool_use はデルタからは判別できず（input_json_delta は通常の ToolUse 扱い）、
    // server 種別は content_block_start からのみ確定する。デルタ単体では ServerToolUse にならない。
    assert_ne!(
        infer_block_kind_from_delta("input_json_delta"),
        BlockKind::ServerToolUse
    );
}

// --- break_open_line: 開きっぱなしの行を閉じてから通知を書くための前処理 ---
// 思考ブロックは "\x1b[2m💭 " を改行なしで書き続けるため、通知をそのまま
// 書くと同じ行に連結され、通知末尾の \x1b[0m が dim を打ち消していた。

/// 指定した種別・状態のブロックを 1 つだけ持つマップを組み立てる。
fn one_block(
    kind: BlockKind,
    text: &str,
    thinking_started: bool,
) -> HashMap<usize, ContentBlockState> {
    let mut block = ContentBlockState::new(kind);
    block.text.push_str(text);
    block.thinking_started = thinking_started;
    let mut blocks = HashMap::new();
    blocks.insert(0, block);
    blocks
}

fn render_break(blocks: &mut HashMap<usize, ContentBlockState>) -> String {
    let mut buf = Vec::new();
    break_open_line(&mut buf, blocks).unwrap();
    String::from_utf8(buf).unwrap()
}

#[test]
fn break_open_line_closes_started_thinking_line() {
    // 💭 を書き出し済みの思考ブロックは行を閉じる（リセット + 改行）。
    let mut blocks = one_block(BlockKind::Thinking, "", true);
    assert_eq!(render_break(&mut blocks), "\x1b[0m\n");
}

#[test]
fn break_open_line_resets_thinking_started_for_redraw() {
    // 行を閉じたら次の thinking_delta で 💭 プレフィックスから描き直させる。
    let mut blocks = one_block(BlockKind::Thinking, "", true);
    render_break(&mut blocks);
    assert!(!blocks[&0].thinking_started);
}

#[test]
fn break_open_line_thinking_not_started_writes_nothing() {
    // まだ 💭 を書いていない思考ブロックは行を開いていないので何も出さない。
    // ここで空行を出すと、通知のたびに無駄な改行が挟まる。
    let mut blocks = one_block(BlockKind::Thinking, "", false);
    assert_eq!(render_break(&mut blocks), "");
}

#[test]
fn break_open_line_text_ending_with_newline_writes_nothing() {
    // テキストが既に改行で終わっていれば行は閉じているので何もしない。
    let mut blocks = one_block(BlockKind::Text, "line1\n", false);
    assert_eq!(render_break(&mut blocks), "");
}

#[test]
fn break_open_line_empty_text_writes_nothing() {
    // まだ 1 文字も書いていないテキストブロックも行を開いていない。
    let mut blocks = one_block(BlockKind::Text, "", false);
    assert_eq!(render_break(&mut blocks), "");
}

#[test]
fn break_open_line_open_text_line_is_closed() {
    let mut blocks = one_block(BlockKind::Text, "途中まで出力", false);
    assert_eq!(render_break(&mut blocks), "\n");
}

#[test]
fn break_open_line_then_finalize_block_does_not_double_newline() {
    // break_open_line が書いた改行を text に記録しないと、後続の finalize_block が
    // もう 1 つ改行を足して通知の直後に空行が入る。
    let mut blocks = one_block(BlockKind::Text, "途中まで出力", false);
    let mut buf = Vec::new();
    break_open_line(&mut buf, &mut blocks).unwrap();
    let block = blocks.remove(&0).expect("block should remain");
    finalize_block(&mut buf, block).unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "\n");
}

#[test]
fn break_open_line_then_finalize_thinking_does_not_emit_second_reset() {
    // 思考ブロックも同様に、閉じ済みなら finalize_block は追加出力しない。
    let mut blocks = one_block(BlockKind::Thinking, "", true);
    let mut buf = Vec::new();
    break_open_line(&mut buf, &mut blocks).unwrap();
    let block = blocks.remove(&0).expect("block should remain");
    finalize_block(&mut buf, block).unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "\x1b[0m\n");
}

#[test]
fn break_open_line_ignores_tool_use_blocks() {
    // ツール使用ブロックは確定時にまとめて 1 行で書くため、途中に行を開かない。
    let mut blocks = one_block(BlockKind::ToolUse, "", false);
    assert_eq!(render_break(&mut blocks), "");
}

#[test]
fn break_open_line_on_empty_map_writes_nothing() {
    let mut blocks: HashMap<usize, ContentBlockState> = HashMap::new();
    assert_eq!(render_break(&mut blocks), "");
}

// --- 思考進捗（ThinkingProgress）の情報源とドット数 ---

/// Claude Code の実データは `thinking` が空文字で、進捗は `estimated_tokens`（増分）に
/// だけ入る。50 トークンごとに 1 ドット出す。
#[test]
fn thinking_progress_counts_dots_from_estimated_tokens() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("", Some(50)), 1);
    assert_eq!(progress.advance("", Some(100)), 2);
    assert_eq!(progress.advance("", Some(150)), 3);
}

/// 50 未満の増分は端数として持ち越し、累積が 50 に届いた時点で 1 ドット出す。
#[test]
fn thinking_progress_carries_over_sub_dot_token_increments() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("", Some(30)), 0);
    assert_eq!(progress.advance("", Some(30)), 1, "累積 60 で 1 ドット");
    assert_eq!(progress.advance("", Some(40)), 1, "累積 100 で 2 ドット目");
}

/// ブロック終端の `estimated_tokens: null`（実データで 1,284 件）は進捗を増やさない。
#[test]
fn thinking_progress_ignores_null_estimated_tokens() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("", Some(100)), 2);
    assert_eq!(progress.advance("", None), 0);
    assert_eq!(
        progress.advance("", Some(50)),
        1,
        "null の後も累積は保たれる"
    );
}

/// 本文も推定トークンも無いデルタでは情報源を確定させない。
/// ここで Bytes / Tokens のどちらかへ倒すと、後続の本来の情報源が使えなくなる。
#[test]
fn thinking_progress_stays_unknown_without_any_signal() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("", None), 0);
    assert_eq!(
        progress.advance("", Some(100)),
        2,
        "空デルタの後でもトークン情報源を選べるべき"
    );
}

/// 本文が返る形式では 100 バイトごとに 1 ドット出す。
#[test]
fn thinking_progress_counts_dots_from_body_bytes() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance(&"a".repeat(250), None), 2);
    assert_eq!(
        progress.advance(&"a".repeat(50), None),
        1,
        "累積 300 で 3 ドット目"
    );
}

/// 情報源はブロック単位で固定する。両方を加算するとドットが二重計上される。
#[test]
fn thinking_progress_does_not_switch_source_midway() {
    let mut tokens_first = ThinkingProgress::default();
    assert_eq!(tokens_first.advance("", Some(100)), 2);
    assert_eq!(
        tokens_first.advance(&"a".repeat(1000), None),
        0,
        "トークン情報源に固定した後は本文バイトを数えない"
    );

    let mut bytes_first = ThinkingProgress::default();
    assert_eq!(bytes_first.advance(&"a".repeat(100), None), 1);
    assert_eq!(
        bytes_first.advance("", Some(1000)),
        0,
        "バイト情報源に固定した後は推定トークンを数えない"
    );
}

/// 壊れた巨大値でも飽和加算で panic させない（debug build のオーバーフロー回避）。
#[test]
fn thinking_progress_saturates_on_absurd_token_values() {
    let mut progress = ThinkingProgress::default();
    progress.advance("", Some(u64::MAX));
    assert_eq!(
        progress.advance("", Some(u64::MAX)),
        0,
        "飽和後は増分ゼロになるだけで panic しない"
    );
}
