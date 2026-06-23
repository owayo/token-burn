//! `blocks` モジュールのブロック種別分類（`block_kind` /
//! `infer_block_kind_from_delta`）の単体テスト。
//! 各 stream-json の type / delta type に対する分類を直接固定する。

use crate::format_stream::blocks::{BlockKind, block_kind, infer_block_kind_from_delta};

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
