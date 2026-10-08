//! `format_codex` の単体テスト。機能別にサブモジュールへ分割している。
//! 共有ヘルパー（色の除去・時刻を指定して行を流すラッパー）をここに集約する。

use super::formatter::CodexFormatter;
use std::path::PathBuf;
use std::time::{Duration, Instant};

mod events;
mod heartbeat;
mod pipeline;
mod text;

/// 整形器のカレントディレクトリ（実データのサンプルのリポジトリと同じ）。
const CWD: &str = "/abs/repo";

/// 表示行に添える壁時計の固定値。
const CLOCK: &str = "12:34:56";

fn fixed_clock() -> String {
    CLOCK.to_string()
}

fn new_formatter(start: Instant) -> CodexFormatter {
    CodexFormatter::with_clock(start, Some(PathBuf::from(CWD)), fixed_clock)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// SGR（色）などの CSI を取り除き、表示内容だけを比べられるようにする。
fn strip_sgr(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// 生バイト列の 1 行を時刻を指定して流し、その行で書かれた内容をそのまま返す（色付き）。
fn feed_bytes(formatter: &mut CodexFormatter, line: &[u8], at: Instant) -> String {
    let mut out = Vec::new();
    formatter
        .on_line(line, at, &mut out)
        .expect("Vec への書き込みは失敗しない");
    String::from_utf8(out).expect("表示は UTF-8")
}

/// 1 行（改行は付け足す）を時刻を指定して流し、その行で書かれた内容（色を除く）を返す。
fn feed(formatter: &mut CodexFormatter, line: &str, at: Instant) -> String {
    strip_sgr(&feed_bytes(formatter, format!("{line}\n").as_bytes(), at))
}

/// 行が届かないまま待ちが明けたことを通知し、書かれた内容（色を除く）を返す。
fn idle(formatter: &mut CodexFormatter, at: Instant) -> String {
    let mut out = Vec::new();
    formatter
        .on_idle(at, &mut out)
        .expect("Vec への書き込みは失敗しない");
    strip_sgr(&String::from_utf8(out).expect("表示は UTF-8"))
}

/// 行を 1 秒おきに受け取ったものとして順に流し、全体の表示（色を除く）を返す。
fn render(lines: &[&str]) -> String {
    let start = Instant::now();
    let mut formatter = new_formatter(start);
    lines
        .iter()
        .zip(0..)
        .map(|(line, index)| feed(&mut formatter, line, start + secs(index)))
        .collect()
}
