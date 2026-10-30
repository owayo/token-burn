//! `blocks` モジュールのブロック種別分類（`block_kind` /
//! `infer_block_kind_from_delta`）の単体テスト。
//! 各 stream-json の type / delta type に対する分類を直接固定する。

use crate::format_stream::blocks::{
    BlockKind, ContentBlockState, NarrationText, ThinkingProgress, block_kind, break_open_line,
    finalize_block, infer_block_kind_from_delta,
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

// --- 思考進捗（ThinkingProgress）の情報源と書き出す断片 ---

fn dots(count: usize) -> Option<String> {
    Some(".".repeat(count))
}

/// 伏せられた思考は `thinking` が空文字で、進捗は `estimated_tokens`（増分）に
/// だけ入る。50 トークンごとに 1 ドット出す。
#[test]
fn thinking_progress_counts_dots_from_estimated_tokens() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("", Some(50)), dots(1));
    assert_eq!(progress.advance("", Some(100)), dots(2));
    assert_eq!(progress.advance("", Some(150)), dots(3));
}

/// 50 未満の増分は端数として持ち越し、累積が 50 に届いた時点で 1 ドット出す。
/// 1 ドットに満たなくても進捗はあるので、空の断片を返して行だけを開かせる。
#[test]
fn thinking_progress_carries_over_sub_dot_token_increments() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("", Some(30)), dots(0));
    assert_eq!(
        progress.advance("", Some(30)),
        dots(1),
        "累積 60 で 1 ドット"
    );
    assert_eq!(
        progress.advance("", Some(40)),
        dots(1),
        "累積 100 で 2 ドット目"
    );
}

/// ブロック終端の `estimated_tokens: null`（実データで 1,284 件）は進捗を報せないので、
/// 行を開かない（`None`）。
#[test]
fn thinking_progress_ignores_null_estimated_tokens() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("", Some(100)), dots(2));
    assert_eq!(progress.advance("", None), None);
    assert_eq!(
        progress.advance("", Some(50)),
        dots(1),
        "null の後も累積は保たれる"
    );
}

/// 本文も推定トークンも無いデルタでは情報源を確定させない。
/// ここで Text / Tokens のどちらかへ倒すと、後続の本来の情報源が使えなくなる。
#[test]
fn thinking_progress_stays_unknown_without_any_signal() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("", None), None);
    assert_eq!(
        progress.advance("", Some(100)),
        dots(2),
        "空デルタの後でもトークン情報源を選べるべき"
    );
}

/// 本文が届く思考（ナレーション）は、ドットではなく本文そのものを書く。
/// 実データの本文は全件が末尾に `\n\n` を持つが、空行は作らない。
#[test]
fn thinking_progress_writes_narration_text() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(
        progress.advance("リモート同期は", None).as_deref(),
        Some("リモート同期は")
    );
    assert_eq!(
        progress.advance("完了済みです。\n\n", None).as_deref(),
        Some("完了済みです。")
    );
}

/// 情報源はブロック単位で固定する。両方を出すと同じ進捗を二重に示す。
#[test]
fn thinking_progress_does_not_switch_source_midway() {
    let mut tokens_first = ThinkingProgress::default();
    assert_eq!(tokens_first.advance("", Some(100)), dots(2));
    assert_eq!(
        tokens_first.advance("後から届いた本文", None),
        None,
        "トークン情報源に固定した後は本文を書かない"
    );

    let mut text_first = ThinkingProgress::default();
    assert_eq!(text_first.advance("本文", None).as_deref(), Some("本文"));
    assert_eq!(
        text_first.advance("", Some(1000)),
        None,
        "本文に固定した後は推定トークンをドットにしない"
    );
}

/// 本文と推定トークンが同じデルタに入っていたら、本文を優先する。
/// 本文は送る側が見せると決めたものなので、ドットに置き換えると読めなくなる。
#[test]
fn thinking_progress_prefers_text_when_both_signals_arrive_together() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(progress.advance("進捗", Some(100)).as_deref(), Some("進捗"));
}

/// 壊れた巨大値でも飽和加算で panic させない（debug build のオーバーフロー回避）。
#[test]
fn thinking_progress_saturates_on_absurd_token_values() {
    let mut progress = ThinkingProgress::default();
    progress.advance("", Some(u64::MAX));
    assert_eq!(
        progress.advance("", Some(u64::MAX)),
        dots(0),
        "飽和後は増分ゼロになるだけで panic しない"
    );
}

/// 壊れた巨大値をそのままドット数にすると、何京個ものドットを書こうとして整形器が
/// 止まらなくなる（または確保で panic する）。1 デルタあたりの上限で切る。
#[test]
fn thinking_progress_bounds_dots_per_delta() {
    let mut progress = ThinkingProgress::default();
    let fragment = progress.advance("", Some(u64::MAX)).unwrap();
    assert!(fragment.len() <= 64, "len={}", fragment.len());
    assert!(fragment.chars().all(|ch| ch == '.'));
}

// --- ナレーション本文（NarrationText）の 1 行化・上限・続き ---

/// 改行・タブを含む空白の連なりは空白 1 つにし、先頭と末尾の空白は書かない。
#[test]
fn narration_collapses_whitespace_runs_into_single_spaces() {
    assert_eq!(
        NarrationText::collapse("  \n先に同期します。\n\n次に\t更新します。\n\n", 500),
        "先に同期します。 次に 更新します。"
    );
}

/// デルタの区切り方（1 文字ずつ、CR と LF が別のデルタ、末尾の空白だけのデルタ）に
/// よらず、全体を一度に畳んだ結果と同じになる。
#[test]
fn narration_is_independent_of_how_deltas_are_split() {
    let full = "前半の確認が終わりました。\r\n\r\n次は後半を確認します。\n\n";
    let expected = NarrationText::collapse(full, 500);

    let mut per_char = NarrationText::new(500);
    let joined: String = full
        .chars()
        .map(|ch| per_char.push(&ch.to_string()))
        .collect();
    assert_eq!(joined, expected);

    let mut split = NarrationText::new(500);
    let pieces = [
        "前半の確認が終わりました。\r",
        "\n",
        "\r\n次は後半を",
        "確認します。",
        "\n\n",
    ];
    let joined: String = pieces.iter().map(|piece| split.push(piece)).collect();
    assert_eq!(joined, expected);
    assert_eq!(split.push("\n"), "", "末尾の空白だけのデルタは何も書かない");
}

/// ESC などの制御文字は端末へ出さない（エスケープシーケンスを成立させない）。
#[test]
fn narration_drops_control_characters() {
    let out = NarrationText::collapse("赤\x1b[31m字\x07と\u{9b}C1\x00", 500);
    assert!(!out.chars().any(char::is_control), "{out:?}");
    assert!(out.starts_with('赤'));
}

/// 上限ちょうどなら省略記号を付けない。末尾の空白（`\n\n`）だけが上限を超えても同じ。
#[test]
fn narration_at_the_limit_is_not_marked_truncated() {
    let exact = "あ".repeat(500);
    assert_eq!(NarrationText::collapse(&exact, 500), exact);
    assert_eq!(NarrationText::collapse(&format!("{exact}\n\n"), 500), exact);
}

/// 上限を超える文字が届いたら `…` を 1 回だけ書き、以後は何も書かない（文字数で数える）。
#[test]
fn narration_over_the_limit_writes_one_ellipsis_and_stops() {
    let mut narration = NarrationText::new(500);
    let first = narration.push(&"あ".repeat(499));
    assert_eq!(first.chars().count(), 499);
    assert_eq!(narration.push("いう"), "い\u{2026}");
    assert_eq!(narration.push("えお"), "", "省略した後は何も書かない");
}

/// 上限を超える位置に空白を挟む場合も、空白 1 つ分を数えて上限を超えない。
#[test]
fn narration_counts_the_collapsed_space_toward_the_limit() {
    let out = NarrationText::collapse(&format!("{} い", "あ".repeat(499)), 500);
    assert_eq!(out, format!("{}\u{2026}", "あ".repeat(499)));
}

/// 割り込みの通知で行が閉じられた後の続きは `↳ ` から書き始め、保留していた空白は
/// 新しい行の先頭に付けない。
#[test]
fn narration_continues_after_the_line_was_closed() {
    let mut progress = ThinkingProgress::default();
    assert_eq!(
        progress.advance("前半です。 ", None).as_deref(),
        Some("前半です。")
    );
    let mut block = ContentBlockState::new(BlockKind::Thinking);
    block.thinking_progress = progress;
    block.thinking_started = true;
    let mut blocks = HashMap::from([(0, block)]);
    assert_eq!(render_break(&mut blocks), "\x1b[0m\n");
    let block = blocks.get_mut(&0).unwrap();
    assert_eq!(
        block
            .thinking_progress
            .advance("後半です。\n", None)
            .as_deref(),
        Some("\u{21b3} 後半です。")
    );
    assert_eq!(
        block
            .thinking_progress
            .advance("続きです。", None)
            .as_deref(),
        Some(" 続きです。"),
        "同じ行の中では ↳ を繰り返さない"
    );
}

/// 本文を書く前に行が閉じられた場合（まだ何も出していない）は `↳ ` を付けない。
#[test]
fn narration_without_text_before_the_break_does_not_mark_continuation() {
    let mut narration = NarrationText::new(500);
    assert_eq!(narration.push("\n"), "");
    let mut progress = ThinkingProgress::Text(narration);
    let mut block = ContentBlockState::new(BlockKind::Thinking);
    std::mem::swap(&mut block.thinking_progress, &mut progress);
    block.thinking_started = true;
    let mut blocks = HashMap::from([(0, block)]);
    render_break(&mut blocks);
    let block = blocks.get_mut(&0).unwrap();
    assert_eq!(
        block.thinking_progress.advance("本文", None).as_deref(),
        Some("本文")
    );
}
