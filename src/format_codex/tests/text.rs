//! 表示用の文字列処理（制御文字の除去・1 行化・コマンドの表示形・経過時間・出力末尾）のテスト。

use super::super::text::{
    collapse, display_command, format_elapsed, one_line, output_tail_lines, sanitize,
};
use super::*;

// --- sanitize: エスケープシーケンスと制御文字 ---

#[test]
fn sanitize_removes_csi_sequences() {
    assert_eq!(
        sanitize("\x1b[1;31mError\x1b[0m: \x1b[2Kdone\x1b[?25l"),
        "Error: done"
    );
}

#[test]
fn sanitize_removes_osc_terminated_by_bel_or_st() {
    assert_eq!(sanitize("\x1b]0;pane title\x07ok"), "ok");
    assert_eq!(sanitize("\x1b]2;title\x1b\\ok"), "ok");
    assert_eq!(
        sanitize("\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\"),
        "link"
    );
}

#[test]
fn sanitize_removes_other_string_sequences() {
    // DCS / APC も ST まで読み飛ばす。
    assert_eq!(
        sanitize("a\x1bPq#0;2;0;0;0\x1b\\b\x1b_payload\x1b\\c"),
        "abc"
    );
}

#[test]
fn sanitize_removes_two_and_three_byte_escapes() {
    // `ESC (B` は終端の `B` まで消さないと文字として漏れる。
    assert_eq!(sanitize("before\x1b(Bafter"), "beforeafter");
    assert_eq!(sanitize("\x1b(0lqk\x1b(Bplain"), "lqkplain");
    assert_eq!(sanitize("\x1b=keypad\x1b>\x1bMup\x1b7\x1b8"), "keypadup");
}

#[test]
fn sanitize_drops_c0_c1_and_del_but_keeps_tab_and_newline() {
    assert_eq!(
        sanitize("a\x00b\x01c\x07d\x08e\x7ff\u{9b}g\u{85}h\ti\nj"),
        "abcdefgh\ti\nj"
    );
}

#[test]
fn sanitize_normalizes_carriage_returns_to_newlines() {
    assert_eq!(sanitize("one\r\ntwo\rthree\n"), "one\ntwo\nthree\n");
}

#[test]
fn sanitize_keeps_text_after_a_malformed_escape() {
    // CSI の途中に終端バイト以外が来たら、そこから先は通常の文字として残す。
    assert_eq!(sanitize("\x1b[12日本語"), "日本語");
    // 閉じていない OSC は行末で止め、次の行を食べない。
    assert_eq!(sanitize("\x1b]0;unterminated\nnext line"), "\nnext line");
    // 末尾の ESC だけ、ESC が 2 つ続く場合も panic しない。
    assert_eq!(sanitize("tail\x1b"), "tail");
    assert_eq!(sanitize("\x1b\x1b[31mred"), "red");
}

#[test]
fn sanitize_keeps_multibyte_text() {
    assert_eq!(sanitize("日本語のテキスト 🎉"), "日本語のテキスト 🎉");
}

// --- collapse / one_line ---

#[test]
fn collapse_joins_lines_and_keeps_inner_spacing() {
    assert_eq!(
        collapse("cat <<'EOF'\n  line 1\n\n  line  2\nEOF\n"),
        "cat <<'EOF' line 1 line  2 EOF"
    );
    assert_eq!(collapse(" \n\r\n "), "");
}

#[test]
fn one_line_truncates_by_chars_not_bytes() {
    let shown = one_line(&"あ".repeat(10), 5);
    assert_eq!(shown, "ああ...");
    assert_eq!(shown.chars().count(), 5);
    assert_eq!(one_line("short", 10), "short");
}

// --- display_command: シェルの包みとクォートを外す ---

#[test]
fn display_command_unwraps_single_quoted_scripts() {
    assert_eq!(
        display_command("/bin/zsh -lc 'cat hello.txt'"),
        "cat hello.txt"
    );
    assert_eq!(
        display_command("/bin/bash -lc 'cargo test --all'"),
        "cargo test --all"
    );
    assert_eq!(display_command("/bin/sh -c 'ls -la'"), "ls -la");
    assert_eq!(display_command("zsh -lc 'pwd'"), "pwd");
    assert_eq!(display_command("bash -c 'echo hi'"), "echo hi");
    assert_eq!(display_command("sh -c 'true'"), "true");
    assert_eq!(display_command("/bin/zsh -c 'whoami'"), "whoami");
    assert_eq!(display_command("/bin/zsh  -lc  'spaced'  "), "spaced");
    assert_eq!(
        display_command("/usr/local/bin/bash -lc 'make ci'"),
        "make ci"
    );
}

#[test]
fn display_command_unescapes_double_quoted_scripts() {
    assert_eq!(
        display_command(r#"/bin/zsh -lc "echo \"it's\" \$HOME \`date\` back\\slash""#),
        r#"echo "it's" $HOME `date` back\slash"#
    );
    // 二重引用符の中で意味を持たないバックスラッシュはそのまま残す。
    assert_eq!(
        display_command(r#"/bin/zsh -lc "printf 'a\nb' && grep -E '\d+' f""#),
        r#"printf 'a\nb' && grep -E '\d+' f"#
    );
}

#[test]
fn display_command_joins_shlex_quoted_fragments() {
    // shlex は `'` を含む文字列を断片の連結で表す。
    assert_eq!(
        display_command(r#"/bin/zsh -lc 'git commit -m '"'"'fix: typo'"'"''"#),
        "git commit -m 'fix: typo'"
    );
}

#[test]
fn display_command_collapses_heredocs() {
    assert_eq!(
        display_command("/bin/zsh -lc \"cat > notes.md <<'EOF'\n# Title\n\nbody\nEOF\""),
        "cat > notes.md <<'EOF' # Title body EOF"
    );
    assert_eq!(
        display_command("/bin/bash -lc 'python3 - <<\"PY\"\nprint(1)\nPY'"),
        "python3 - <<\"PY\" print(1) PY"
    );
}

#[test]
fn display_command_leaves_unknown_forms_unchanged() {
    // 包みでないもの
    assert_eq!(display_command("git status --short"), "git status --short");
    assert_eq!(
        display_command("/usr/bin/env bash -lc 'x'"),
        "/usr/bin/env bash -lc 'x'"
    );
    assert_eq!(display_command("fish -c 'ls'"), "fish -c 'ls'");
    assert_eq!(display_command("/bin/zsh -x 'ls'"), "/bin/zsh -x 'ls'");
    // 包みは外すが、残りが 1 語のクォートではないもの
    assert_eq!(display_command("/bin/zsh -lc ls"), "ls");
    assert_eq!(display_command("/bin/zsh -lc 'a' 'b'"), "'a' 'b'");
    assert_eq!(display_command(r#"/bin/zsh -lc "a"b""#), r#""a"b""#);
    assert_eq!(
        display_command("/bin/zsh -lc 'unterminated"),
        "'unterminated"
    );
    assert_eq!(
        display_command(r#"/bin/zsh -lc "unterminated"#),
        r#""unterminated"#
    );
    // 包みだけで中身が無いもの
    assert_eq!(display_command("/bin/zsh -lc "), "/bin/zsh -lc");
    assert_eq!(display_command(""), "?");
}

#[test]
fn display_command_strips_control_characters() {
    assert_eq!(
        display_command("/bin/zsh -lc 'echo \u{1b}[31mred\u{1b}[0m\u{7}'"),
        "echo red"
    );
}

// --- format_elapsed ---

#[test]
fn format_elapsed_uses_tenths_below_a_minute() {
    assert_eq!(format_elapsed(millis(0)), "0.0s");
    assert_eq!(format_elapsed(millis(400)), "0.4s");
    assert_eq!(format_elapsed(millis(12_345)), "12.3s");
    // 切り捨てなので 59.99 秒が `60.0s` にならない。
    assert_eq!(format_elapsed(millis(59_999)), "59.9s");
}

#[test]
fn format_elapsed_uses_minutes_and_hours() {
    assert_eq!(format_elapsed(secs(60)), "1m 00s");
    assert_eq!(format_elapsed(secs(125)), "2m 05s");
    assert_eq!(format_elapsed(secs(3_599)), "59m 59s");
    assert_eq!(format_elapsed(secs(3_600)), "1h 00m");
    assert_eq!(format_elapsed(secs(3_720)), "1h 02m");
    assert_eq!(format_elapsed(secs(90_061)), "25h 01m");
}

// --- output_tail_lines ---

#[test]
fn output_tail_lines_returns_the_last_non_empty_lines_in_order() {
    let output = "one\ntwo\n\nthree\n  \nfour\n\n";
    assert_eq!(output_tail_lines(output, 3), ["two", "three", "four"]);
    assert_eq!(output_tail_lines(output, 1), ["four"]);
    assert_eq!(
        output_tail_lines(output, 10),
        ["one", "two", "three", "four"]
    );
    assert!(output_tail_lines(output, 0).is_empty());
    assert!(output_tail_lines("", 3).is_empty());
}

#[test]
fn output_tail_lines_follows_carriage_return_overwrites() {
    assert_eq!(
        output_tail_lines("a 10%\ra 50%\ra 100%\r\nnext\r\n", 2),
        ["a 100%", "next"]
    );
    // CR の後ろが空（`\r` で終わる）なら手前の内容が見えている。
    assert_eq!(output_tail_lines("progress 99%\r", 1), ["progress 99%"]);
}

#[test]
fn output_tail_lines_skips_lines_that_are_only_escape_sequences() {
    assert_eq!(
        output_tail_lines("\x1b[32mok\x1b[0m\n\x1b[0m\n\x1b[2K\n", 2),
        ["ok"]
    );
}
