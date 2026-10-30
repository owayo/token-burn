//! ストリーミングされるコンテンツブロック（テキスト/思考/ツール使用）の状態管理と
//! ブロック確定時の表示を担うモジュール。

use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;

use crate::format_stream::diff::format_tool_diff;
use crate::format_stream::tools::detail::extract_tool_detail;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    ServerToolUse,
    Unknown,
}

/// 進捗ドットを 1 つ出すのに必要な思考トークン数。
///
/// 実データの `thinking_delta` は 50 / 100 / 150 トークン単位で飛び飛びに届き、
/// 1 ブロックの合計は 100〜500 トークン程度。50 刻みなら 1 ブロック 2〜10 ドットに
/// 収まり、進捗は読み取れてログはうるさくならない。
const THINKING_TOKENS_PER_DOT: u64 = 50;

/// 1 つの `thinking_delta` から書くドットの上限。
///
/// 実データの増分は 1 デルタ 50〜150 トークン（1〜3 ドット）。壊れた巨大値
/// （`u64::MAX` など）をそのままドット数にすると、何京個ものドットを書こうとして
/// 整形器が止まらなくなる（パイプが詰まり、上流の claude も書き込みで止まる）か、
/// 文字列の確保で panic する。上限で切っても、進捗を示すという目的は損なわない。
const MAX_DOTS_PER_DELTA: u64 = 64;

/// 1 つの思考ブロックから表示するナレーション本文の上限（文字数）。
///
/// 実データ（`thinking_display: "updates"`）のナレーションは最大 376 文字・中央値 107 文字
/// なので、実在する長さはすべて収まる。上限は、思考の全文が届く表示形式でログを
/// 思考本文で埋めないためのもの。
pub(crate) const NARRATION_MAX_CHARS: usize = 500;

/// サブエージェントのナレーションを 1 行に畳むときの上限（文字数）。
///
/// サブエージェントの本文（`💬`）は 100 文字に切るが、ナレーションは進捗の要約そのもので
/// 情報密度が高く、中央値（107 文字）で切れると半分が読めない。並列で動くサブエージェントの
/// 行がペインを埋めないよう、メインループより短くする。
pub(crate) const SUBAGENT_NARRATION_MAX_CHARS: usize = 200;

/// 思考ブロックの進捗をどの情報源で表しているか。
///
/// Claude Code（`thinking_display: "updates"`）は、利用者に見せない思考の本文を伏せて
/// `thinking` を空文字で送り、進捗を `estimated_tokens`（増分トークン数）にだけ入れる。
/// 一方、利用者に見せるナレーション（「リモート同期は完了済みで、次はライブラリを更新します」
/// のような進捗の要約）は `thinking` に本文が入る（実データで思考ブロックの約 35%）。
/// 伏せるかどうかは送る側が決めているので、本文が届いたブロックは本文を表示し、
/// 伏せられたブロックはドットで進捗だけを示す。本文をドットに数えていた頃は、ナレーションが
/// 整形済みログから 1 件も残っていなかった。
///
/// 情報源はブロックごとに最初に観測できた方へ固定し、途中で切り替えない（両方を
/// 表示すると、同じ進捗を二重に示すことになる）。
#[derive(Debug, Default)]
pub(crate) enum ThinkingProgress {
    /// まだどちらの情報源も観測していない。
    #[default]
    Unknown,
    /// `estimated_tokens` の累積をドットで示す。
    Tokens { total: u64 },
    /// 届いた本文（ナレーション）をそのまま表示する。
    Text(NarrationText),
}

impl ThinkingProgress {
    /// 1 つの `thinking_delta` を取り込み、書き出す断片を返す。
    ///
    /// `None` は進捗が無いことを表し、思考行を開かない（ブロック終端の
    /// `estimated_tokens: null` かつ本文なしのデルタ等）。`Some` は行を開いてから断片を
    /// 書く。推定トークンがドット 1 つに満たない場合は空文字の `Some` になり、行だけを開く
    /// （短い思考が完全に消えると、考えていた区間そのものがログから失われるため）。
    pub(crate) fn advance(&mut self, text: &str, estimated_tokens: Option<u64>) -> Option<String> {
        if matches!(self, Self::Unknown) {
            if !text.is_empty() {
                *self = Self::Text(NarrationText::new(NARRATION_MAX_CHARS));
            } else if estimated_tokens.is_some() {
                *self = Self::Tokens { total: 0 };
            } else {
                // 本文も推定トークンも無いデルタでは情報源を確定させない。
                return None;
            }
        }
        match self {
            Self::Unknown => None,
            Self::Tokens { total } => {
                let delta = estimated_tokens?;
                let before = *total / THINKING_TOKENS_PER_DOT;
                *total = total.saturating_add(delta);
                let after = *total / THINKING_TOKENS_PER_DOT;
                let dots = (after - before).min(MAX_DOTS_PER_DELTA);
                Some(".".repeat(usize::try_from(dots).unwrap_or(0)))
            }
            Self::Text(narration) => Some(narration.push(text)).filter(|s| !s.is_empty()),
        }
    }

    /// 割り込みの通知のために思考行が閉じられたことを伝える。
    fn line_closed(&mut self) {
        if let Self::Text(narration) = self {
            narration.line_closed();
        }
    }
}

/// ナレーション本文を、デルタに分かれて届いても 1 行として読める形で書き出す状態。
///
/// - 改行・タブを含む空白の連なりは空白 1 つに畳む。空白は保留しておき、次に表示する
///   文字が来たときだけ書く（本文の先頭と、全件が持つ末尾の `\n\n` を行に残さない）。
/// - ESC などの制御文字は捨てる。ESC を落とせばエスケープシーケンスは成立しないので、
///   シーケンスがデルタをまたいでも状態を持たずに済む。
/// - 上限を超える文字が届いたら `…` を 1 回だけ書き、以後は何も書かない。上限に届いた
///   だけ（末尾の空白しか残っていない）では省略記号を付けない。
/// - 割り込みの通知で行が閉じられた後に続きが届いたら、`↳ ` を付けて続きだと分かるようにする。
#[derive(Debug)]
pub(crate) struct NarrationText {
    max_chars: usize,
    /// 書き出した文字数（畳んだ空白を含む）。上限の判定に使う。
    shown: usize,
    /// 保留している空白があるか。
    pending_space: bool,
    /// いま開いている物理行に本文を書いたか。
    line_has_text: bool,
    /// 本文を書いた行が割り込みで閉じられ、続きを `↳ ` から書き始めるか。
    continued: bool,
    /// 上限を超えて省略記号を書いたか。
    truncated: bool,
}

impl NarrationText {
    pub(crate) fn new(max_chars: usize) -> Self {
        Self {
            max_chars,
            shown: 0,
            pending_space: false,
            line_has_text: false,
            continued: false,
            truncated: false,
        }
    }

    /// 本文の断片を取り込み、書き出す文字列を返す（書くものが無ければ空文字）。
    pub(crate) fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        if self.truncated {
            return out;
        }
        for ch in text.chars() {
            if ch.is_whitespace() {
                if self.shown > 0 {
                    self.pending_space = true;
                }
                continue;
            }
            if ch.is_control() {
                continue;
            }
            let space = self.pending_space && self.line_has_text;
            let needed = 1 + usize::from(space);
            if self.shown.saturating_add(needed) > self.max_chars {
                self.start_continuation(&mut out);
                out.push('\u{2026}');
                self.truncated = true;
                return out;
            }
            self.start_continuation(&mut out);
            if space {
                out.push(' ');
                self.shown += 1;
            }
            self.pending_space = false;
            out.push(ch);
            self.shown += 1;
            self.line_has_text = true;
        }
        out
    }

    /// 本文全体を 1 行に畳んだ文字列を返す（`assistant` イベントから一度に表示する経路用）。
    pub(crate) fn collapse(text: &str, max_chars: usize) -> String {
        Self::new(max_chars).push(text)
    }

    fn start_continuation(&mut self, out: &mut String) {
        if !self.line_has_text && self.continued {
            out.push_str("\u{21b3} ");
            self.continued = false;
        }
    }

    fn line_closed(&mut self) {
        if self.line_has_text {
            self.continued = true;
        }
        self.line_has_text = false;
    }
}

pub(crate) struct ContentBlockState {
    pub(crate) kind: BlockKind,
    pub(crate) tool_name: String,
    pub(crate) tool_input: String,
    pub(crate) text: String,
    /// 思考デルタの進捗。情報源（推定トークン / 本文バイト）はブロック単位で固定する。
    pub(crate) thinking_progress: ThinkingProgress,
    pub(crate) thinking_started: bool,
}

impl ContentBlockState {
    pub(crate) fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            tool_name: String::new(),
            tool_input: String::new(),
            text: String::new(),
            thinking_progress: ThinkingProgress::Unknown,
            thinking_started: false,
        }
    }

    pub(crate) fn from_start_block(block: &serde_json::Value) -> Self {
        let kind = block_kind(block["type"].as_str().unwrap_or(""));
        let mut state = Self::new(kind);

        if matches!(kind, BlockKind::ToolUse | BlockKind::ServerToolUse) {
            state.tool_name = block["name"].as_str().unwrap_or("?").to_string();
            if let Some(input) = block.get("input")
                && !input.is_null()
                && input.as_object().map(|obj| !obj.is_empty()).unwrap_or(true)
                && let Ok(serialized) = serde_json::to_string(input)
            {
                state.tool_input = serialized;
            }
        }

        if kind == BlockKind::Text {
            state.text = block["text"].as_str().unwrap_or("").to_string();
        }

        state
    }

    pub(crate) fn ensure_thinking_started(&mut self, out: &mut impl Write) -> Result<()> {
        if !self.thinking_started {
            write!(out, "\x1b[2m\u{1f4ad} ")?;
            out.flush()?;
            self.thinking_started = true;
        }
        Ok(())
    }
}

pub(crate) fn block_kind(block_type: &str) -> BlockKind {
    match block_type {
        "text" => BlockKind::Text,
        "thinking" => BlockKind::Thinking,
        "tool_use" => BlockKind::ToolUse,
        "server_tool_use" => BlockKind::ServerToolUse,
        _ => BlockKind::Unknown,
    }
}

pub(crate) fn infer_block_kind_from_delta(delta_type: &str) -> BlockKind {
    match delta_type {
        "text_delta" => BlockKind::Text,
        "thinking_delta" | "signature_delta" => BlockKind::Thinking,
        "input_json_delta" => BlockKind::ToolUse,
        _ => BlockKind::Unknown,
    }
}

pub(crate) fn finalize_block(out: &mut impl Write, block: ContentBlockState) -> Result<()> {
    match block.kind {
        BlockKind::Thinking => {
            if block.thinking_started {
                writeln!(out, "\x1b[0m")?;
            }
        }
        BlockKind::ToolUse | BlockKind::ServerToolUse => {
            let tool_name = if block.tool_name.is_empty() {
                "?"
            } else {
                block.tool_name.as_str()
            };
            write_tool_use(out, tool_name, &block.tool_input, "")?;
        }
        BlockKind::Text => {
            // テキストデルタは改行なしで逐次書き出すため、ブロック終了時に
            // 末尾が改行で終わっていなければ改行を補う。次に続くツール使用や
            // 思考ブロックが同じ行に連結されるのを防ぐ。
            if !block.text.is_empty() && !block.text.ends_with('\n') {
                writeln!(out)?;
            }
        }
        BlockKind::Unknown => {}
    }

    Ok(())
}

/// ストリームと `assistant` 補完経路で同じツール行・差分を出す。
pub(crate) fn write_tool_use(
    out: &mut impl Write,
    tool_name: &str,
    input_json: &str,
    owner: &str,
) -> Result<()> {
    let detail = extract_tool_detail(tool_name, input_json);
    if detail.is_empty() {
        writeln!(out, "\x1b[36m\u{1f527} {}{}\x1b[0m", tool_name, owner)?;
    } else {
        writeln!(
            out,
            "\x1b[36m\u{1f527} {}{}\x1b[0m \x1b[2m{}\x1b[0m",
            tool_name, owner, detail
        )?;
    }
    if let Some(diff) = format_tool_diff(tool_name, input_json) {
        write!(out, "{}", diff)?;
    }
    Ok(())
}

/// 未確定のブロックが行を開いたままなら、その行を閉じる。
///
/// 思考ブロックは `\x1b[2m💭 ` を改行なしで書き、以後ドットを追記していく。
/// `--include-partial-messages` では同一メッセージの `assistant` イベントが
/// 思考の途中で届くため、通知行をそのまま書くと思考行の途中に連結され、
/// 通知末尾の `\x1b[0m` が思考ブロックの dim を打ち消してしまう。
/// 通知を出す直前にこれを呼んで、独立した行から書き始められるようにする。
pub(crate) fn break_open_line(
    out: &mut impl Write,
    blocks: &mut HashMap<usize, ContentBlockState>,
) -> Result<()> {
    for block in blocks.values_mut() {
        match block.kind {
            BlockKind::Thinking if block.thinking_started => {
                writeln!(out, "\x1b[0m")?;
                // 次の thinking_delta で 💭 プレフィックスから描き直させる。
                block.thinking_started = false;
                block.thinking_progress.line_closed();
            }
            BlockKind::Text if !block.text.is_empty() && !block.text.ends_with('\n') => {
                writeln!(out)?;
                // finalize_block が改行を二重に足さないよう、書き出した分を記録する。
                block.text.push('\n');
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn finalize_open_blocks(
    out: &mut impl Write,
    blocks: &mut HashMap<usize, ContentBlockState>,
) -> Result<()> {
    let mut indices: Vec<_> = blocks.keys().copied().collect();
    indices.sort_unstable();
    for index in indices {
        if let Some(block) = blocks.remove(&index) {
            finalize_block(out, block)?;
        }
    }
    Ok(())
}
