pub(super) fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn task_log_base(idx: usize, display_name: &str) -> String {
    format!("{idx:04}_{}", sanitize_filename(display_name))
}

pub(super) fn strip_ansi_from_dir(dir: &std::path::Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "log").unwrap_or(false)
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            let cleaned = strip_ansi(&content);
            let _ = std::fs::write(&path, cleaned);
        }
    }
}

/// ログの末尾を上限付きで読み、最後の空でない行を返す（不正な UTF-8 や ANSI だけの行を含むログにも対応する）。
pub(crate) fn last_log_line(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let offset = file.metadata()?.len().saturating_sub(65536);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(65536).read_to_end(&mut bytes)?;
    let text = strip_ansi(&String::from_utf8_lossy(&bytes));
    Ok(text
        .rsplit(['\r', '\n'])
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(500)
        .collect())
}

/// 文字列からエスケープシーケンスだけを取り除く（改行・タブなどの制御文字は残す）。
///
/// シーケンスの解析は、整形器と同じ [`crate::format_stream::util::skip_escape`] に任せる。
/// 以前は独自の解析を持っていたため、文字集合の指定（`ESC (B`）の終端バイトの漏れを
/// 2 か所で別々に直していたうえ、こちらだけ次の取りこぼしが残っていた。
///
/// - 閉じていない OSC（`ESC ] ...` の後に BEL も ST も無い）が、改行を越えてログの残り
///   すべてを消していた（整形器の解析は改行で止める）
/// - CSI の途中に ASCII の終端バイトが来ないと、その間の日本語などの本文まで消していた
/// - DCS / SOS / PM / APC（`ESC P` / `ESC X` / `ESC ^` / `ESC _`）の中身が本文として残っていた
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            crate::format_stream::util::skip_escape(&mut chars);
        } else {
            result.push(c);
        }
    }
    result
}

pub(super) fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        "...".to_string()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_skips_ansi_only_lines_and_handles_binary_bytes() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"old\nactual failure\xff\r\n\x1b[0m\n \n").unwrap();
        assert_eq!(
            last_log_line(file.path()).unwrap(),
            "actual failure\u{fffd}"
        );
        std::fs::write(file.path(), "日本語".repeat(50000)).unwrap();
        assert_eq!(last_log_line(file.path()).unwrap().chars().count(), 500);
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let input = "\x1b[1mBold\x1b[0m normal \x1b[31mred\x1b[0m";
        assert_eq!(strip_ansi(input), "Bold normal red");
    }

    #[test]
    fn strip_ansi_preserves_plain_text() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn strip_ansi_removes_osc_with_bel() {
        let input = "\x1b]2;pane title\x07ok";
        assert_eq!(strip_ansi(input), "ok");
    }

    #[test]
    fn strip_ansi_removes_osc_with_st() {
        let input = "\x1b]2;pane title\x1b\\ok";
        assert_eq!(strip_ansi(input), "ok");
    }

    #[test]
    fn strip_ansi_removes_bracketed_paste() {
        let input = "\x1b[200~pasted text\x1b[201~";
        assert_eq!(strip_ansi(input), "pasted text");
    }

    #[test]
    fn strip_ansi_handles_mixed_sequences() {
        let input = "\x1b]2;title\x07\x1b[1mBold\x1b[0m text\x1b]0;icon\x1b\\end";
        assert_eq!(strip_ansi(input), "Bold textend");
    }

    #[test]
    fn strip_ansi_removes_charset_designation() {
        // \x1b(B (G0 を ASCII に指定) は 3 バイト構成。introducer だけ捨てると
        // 終端バイト "B" が漏れる回帰。両方スキップされることを確認する。
        let input = "before\x1b(Bafter";
        assert_eq!(strip_ansi(input), "beforeafter");
    }

    #[test]
    fn strip_ansi_removes_line_drawing_charset() {
        // \x1b(0 (G0 を DEC 罫線集合に指定) も終端バイト "0" を残さない。
        let input = "\x1b(0lqk\x1b(Bplain";
        assert_eq!(strip_ansi(input), "lqkplain");
    }

    #[test]
    fn strip_ansi_charset_designation_at_end() {
        // 末尾の不完全な charset designation (introducer のみ) でも panic せず消費される。
        let input = "text\x1b(";
        assert_eq!(strip_ansi(input), "text");
    }

    #[test]
    fn strip_ansi_lone_esc_at_end() {
        // 末尾の孤立ESCは安全に消費される
        let input = "text\x1b";
        assert_eq!(strip_ansi(input), "text");
    }

    #[test]
    fn strip_ansi_incomplete_csi_at_end() {
        // 終端バイトなしの不完全なCSIシーケンスは安全に除去される
        let input = "text\x1b[1";
        assert_eq!(strip_ansi(input), "text");
    }

    #[test]
    fn strip_ansi_incomplete_osc_at_end() {
        // BEL/ST終端なしの不完全なOSCシーケンスは安全に除去される
        let input = "text\x1b]2;title";
        assert_eq!(strip_ansi(input), "text");
    }

    #[test]
    fn strip_ansi_keeps_the_lines_after_an_unterminated_osc() {
        // 閉じていない OSC（出力の途中で落ちた場合など）がログの残りを丸ごと消さない。
        let input = "before\x1b]0;title\nnext line\x1b[31m red\x1b[0m\nlast";
        assert_eq!(strip_ansi(input), "before\nnext line red\nlast");
    }

    #[test]
    fn strip_ansi_keeps_text_inside_a_broken_csi() {
        // ASCII の終端バイトが来ない壊れた CSI で、続く日本語の本文まで消さない。
        assert_eq!(strip_ansi("前\x1b[1日本語Z"), "前日本語Z");
    }

    #[test]
    fn strip_ansi_removes_string_sequences_with_their_payload() {
        // DCS / APC の中身は本文ではない（以前は `ESC P` だけ捨てて中身を残していた）。
        let input = "a\x1bPq#0;2;0;0;0\x1b\\b\x1b_payload\x07c";
        assert_eq!(strip_ansi(input), "abc");
    }

    #[test]
    fn strip_ansi_other_escape_skips_one_char() {
        // ESC + 非 `[`/`]`/charset-introducer 文字（例: \x1b= = DECKPAM）は
        // ESC と次の 1 文字のみスキップする。charset designation (\x1b( 等) は
        // strip_ansi_removes_charset_designation で別途カバー。
        let input = "before\x1b=after";
        assert_eq!(strip_ansi(input), "beforeafter");
    }

    #[test]
    fn sanitize_filename_replaces_special_chars() {
        assert_eq!(sanitize_filename("my-project"), "my-project");
        assert_eq!(sanitize_filename("path/to/repo"), "path_to_repo");
        assert_eq!(sanitize_filename("a b@c"), "a_b_c");
    }

    #[test]
    fn task_log_base_is_unique_even_with_same_display_name() {
        assert_ne!(task_log_base(1, "repo"), task_log_base(2, "repo"));
    }

    #[test]
    fn task_log_base_sanitizes_display_name() {
        assert_eq!(task_log_base(3, "path/to/repo"), "0003_path_to_repo");
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("12345", 5), "12345");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        assert_eq!(truncate("abcdefghij", 7), "abcd...");
    }

    #[test]
    fn truncate_multibyte_counts_chars() {
        // 5文字の日本語文字列を3文字に切り詰め
        assert_eq!(truncate("あいうえお", 3), "...");
        assert_eq!(truncate("あいうえお", 5), "あいうえお");
        assert_eq!(truncate("あいうえお", 4), "あ...");
    }

    #[test]
    fn truncate_max_len_3() {
        // max_len=3 の場合は "..." のみ
        assert_eq!(truncate("hello", 3), "...");
    }

    #[test]
    fn strip_ansi_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn sanitize_filename_empty_string() {
        assert_eq!(sanitize_filename(""), "");
    }

    #[test]
    fn sanitize_filename_unicode() {
        // 日本語文字はアルファニューメリックとして扱われる
        let result = sanitize_filename("日本語repo");
        assert!(result.contains("repo"));
    }

    #[test]
    fn truncate_max_len_zero() {
        // max_len=0 の場合
        assert_eq!(truncate("hello", 0), "...");
    }

    #[test]
    fn truncate_max_len_one() {
        assert_eq!(truncate("hello", 1), "...");
    }

    #[test]
    fn truncate_max_len_two() {
        assert_eq!(truncate("hello", 2), "...");
    }

    #[test]
    fn truncate_emoji_string() {
        // 絵文字を含む文字列の切り詰め
        let input = "🔥🚀✨🎉💡";
        assert_eq!(truncate(input, 5), "🔥🚀✨🎉💡");
        assert_eq!(truncate(input, 4), "🔥...");
    }

    #[test]
    fn sanitize_filename_preserves_dots() {
        assert_eq!(sanitize_filename("file.log"), "file.log");
        assert_eq!(sanitize_filename("v1.2.3"), "v1.2.3");
    }

    #[test]
    fn task_log_base_zero_padded() {
        assert_eq!(task_log_base(1, "repo"), "0001_repo");
        assert_eq!(task_log_base(9999, "repo"), "9999_repo");
    }

    #[test]
    fn strip_ansi_from_dir_cleans_log_files() {
        // .log ファイルから ANSI エスケープコードが除去されることを確認
        let tmp = tempfile::TempDir::new().unwrap();
        let log_path = tmp.path().join("test.log");
        let jsonl_path = tmp.path().join("test.jsonl");
        let txt_path = tmp.path().join("test.txt");

        std::fs::write(&log_path, "\x1b[1mBold\x1b[0m text").unwrap();
        std::fs::write(&jsonl_path, "\x1b[31mred\x1b[0m").unwrap();
        std::fs::write(&txt_path, "\x1b[32mgreen\x1b[0m").unwrap();

        strip_ansi_from_dir(tmp.path());

        // .log ファイルのみ ANSI が除去される
        assert_eq!(std::fs::read_to_string(&log_path).unwrap(), "Bold text");
        // .jsonl と .txt は変更されない
        assert_eq!(
            std::fs::read_to_string(&jsonl_path).unwrap(),
            "\x1b[31mred\x1b[0m"
        );
        assert_eq!(
            std::fs::read_to_string(&txt_path).unwrap(),
            "\x1b[32mgreen\x1b[0m"
        );
    }

    #[test]
    fn strip_ansi_from_dir_nonexistent_dir_does_not_panic() {
        // 存在しないディレクトリでもパニックしない
        strip_ansi_from_dir(std::path::Path::new("/nonexistent/dir"));
    }

    #[test]
    fn strip_ansi_256_color_sequence() {
        // 256色シーケンス（マルチパラメータ CSI）が除去される
        let input = "\x1b[38;5;196mred text\x1b[0m";
        assert_eq!(strip_ansi(input), "red text");
    }

    #[test]
    fn strip_ansi_truecolor_sequence() {
        // 24ビットトゥルーカラーシーケンスが除去される
        let input = "\x1b[38;2;255;0;0mred\x1b[48;2;0;0;255mblue bg\x1b[0m";
        assert_eq!(strip_ansi(input), "redblue bg");
    }

    #[test]
    fn strip_ansi_consecutive_sequences() {
        // テキストなしの連続シーケンスが正しく除去される
        let input = "\x1b[1m\x1b[31m\x1b[4mformatted\x1b[0m\x1b[0m\x1b[0m";
        assert_eq!(strip_ansi(input), "formatted");
    }
}
