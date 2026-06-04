//! `format_stream` の単体テスト。機能別にサブモジュールへ分割している。
//! 共有ヘルパー（ANSI 除去・`process` 実行ラッパー）と、各サブモジュールが
//! `use super::*` で参照する import プレリュードをここに集約する。

use super::diff::{format_diff_lines, format_tool_diff};
use super::rate_limit::format_resets_at;
use super::state::UsageSummary;
use super::tools::detail::extract_tool_detail;
use super::util::{format_number, format_token_size, truncate_str};
use super::*;
use std::io::Cursor;

mod blocks;
mod detail;
mod diff;
mod metadata;
mod process;
mod rate_limit;
mod result;
mod util;

fn strip_ansi(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn run_process(input: &str) -> String {
    run_process_with_opts(input, None, None, 95)
}

fn run_process_with_raw_log(input: &str, raw_output: Option<&std::path::Path>) -> String {
    run_process_with_opts(input, raw_output, None, 95)
}

fn run_process_with_opts(
    input: &str,
    raw_output: Option<&std::path::Path>,
    stop_file: Option<&std::path::Path>,
    threshold: u8,
) -> String {
    let reader = Cursor::new(input.as_bytes().to_vec());
    let mut output = Vec::new();
    process(reader, &mut output, raw_output, stop_file, threshold).unwrap();
    String::from_utf8(output).unwrap()
}
