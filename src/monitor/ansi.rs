//! ANSI の色指定（SGR）付きの文字列を ratatui の行へ変換する。
//!
//! ai-usage の statusline は色付きの文字列を返す。描画を ratatui が持つようになったので、
//! そのまま端末へ流すとカーソル位置と画面の差分管理が食い違う。SGR だけを解釈して
//! スタイルに変え、それ以外のエスケープシーケンスと制御文字は捨てる（ペインのタイトルを
//! 書き換える OSC などを素通しさせない）。

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// バイト列を行ごとに変換する。末尾の改行による空行は含めない。
pub fn parse_lines(bytes: &[u8]) -> Vec<Line<'static>> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<Line<'static>> = text
        .split('\n')
        .map(|line| parse_line(line.strip_suffix('\r').unwrap_or(line)))
        .collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// エスケープシーケンスと制御文字を取り除いた文字列。
pub fn strip(text: &str) -> String {
    parse_line(text)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn parse_line(line: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    // CSI: パラメータ (0x30–0x3F)、中間 (0x20–0x2F)、終端 (0x40–0x7E)。
                    let mut params = String::new();
                    let mut terminator = None;
                    for next in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&next) {
                            terminator = Some(next);
                            break;
                        }
                        params.push(next);
                    }
                    if terminator == Some('m') {
                        let next_style = apply_sgr(style, &params);
                        if next_style != style && !text.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut text), style));
                        }
                        style = next_style;
                    }
                }
                // OSC と、DCS・SOS・PM・APC の文字列: BEL か ST (ESC \) まで捨てる。
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // 中間バイト（0x20–0x2F）付きのエスケープ（文字集合の指定 ESC ( B など）は、
                // 終端バイトまで読み捨てる。1 文字ぶんだけ捨てると `B` が本文に残る。
                Some(c) if ('\x20'..='\x2f').contains(&c) => {
                    for next in chars.by_ref() {
                        if !('\x20'..='\x2f').contains(&next) {
                            break;
                        }
                    }
                }
                // そのほかの 2 文字のエスケープ（ESC = など）は 1 文字ぶん読み捨てる。
                _ => {}
            },
            '\t' => text.push(' '),
            c if c.is_control() => {}
            c => text.push(c),
        }
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    // 空のパラメータは 0（リセット）。`38;5;N` / `38;2;R;G;B` は続く値をまとめて読む。
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split([';', ':'])
            .map(|code| code.parse().unwrap_or(0))
            .collect()
    };
    let mut iter = codes.into_iter();
    while let Some(code) = iter.next() {
        style = match code {
            0 => Style::default(),
            1 => style.add_modifier(Modifier::BOLD),
            2 => style.add_modifier(Modifier::DIM),
            3 => style.add_modifier(Modifier::ITALIC),
            4 => style.add_modifier(Modifier::UNDERLINED),
            7 => style.add_modifier(Modifier::REVERSED),
            22 => style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style.remove_modifier(Modifier::ITALIC),
            24 => style.remove_modifier(Modifier::UNDERLINED),
            27 => style.remove_modifier(Modifier::REVERSED),
            30..=37 => style.fg(Color::Indexed((code - 30) as u8)),
            39 => style.fg(Color::Reset),
            40..=47 => style.bg(Color::Indexed((code - 40) as u8)),
            49 => style.bg(Color::Reset),
            90..=97 => style.fg(Color::Indexed((code - 90 + 8) as u8)),
            100..=107 => style.bg(Color::Indexed((code - 100 + 8) as u8)),
            38 | 48 => {
                let color = match iter.next() {
                    Some(5) => iter
                        .next()
                        .map(|index| Color::Indexed(index.min(255) as u8)),
                    Some(2) => {
                        let mut channel = || iter.next().map(|value| value.min(255) as u8);
                        match (channel(), channel(), channel()) {
                            (Some(r), Some(g), Some(b)) => Some(Color::Rgb(r, g, b)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match (code, color) {
                    (38, Some(color)) => style.fg(color),
                    (48, Some(color)) => style.bg(color),
                    _ => style,
                }
            }
            _ => style,
        };
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn parses_the_colors_ai_usage_emits() {
        // ai-usage の statusline が実際に出す形（truecolor のロゴ、256 色、太字、リセット）。
        let input = "  \x1b[38;2;217;119;87m\u{100002}  \x1b[0m\x1b[1;38;5;196mowa  \x1b[0m\x1b[38;5;245m5h \x1b[0m\n";
        let lines = parse_lines(input.as_bytes());
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        assert_eq!(text(&lines[0]), "  \u{100002}  owa  5h ");
        assert_eq!(spans[0].style, Style::default());
        assert_eq!(spans[1].style.fg, Some(Color::Rgb(217, 119, 87)));
        assert_eq!(spans[2].style.fg, Some(Color::Indexed(196)));
        assert!(spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[3].style.fg, Some(Color::Indexed(245)));
        assert!(!spans[3].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn drops_other_escape_sequences_and_control_characters() {
        // ペインのタイトル変更（OSC）やカーソル移動を描画へ持ち込まない。
        let input = "a\x1b]2;title\x07b\x1b[2Kc\x1b]0;x\x1b\\d\x07\x08e\tf";
        assert_eq!(strip(input), "abcde f");
    }

    #[test]
    fn drops_character_set_designations_and_string_sequences() {
        // ESC ( B（G0 を ASCII に）の `B` や、DCS の中身を本文に残さない。
        assert_eq!(strip("\x1b(Bhello"), "hello");
        assert_eq!(strip("\x1b)0x\x1b(By"), "xy");
        assert_eq!(strip("\x1b#8z"), "z");
        assert_eq!(strip("a\x1bPq#0;2;0;0;0\x1b\\b"), "ab");
        assert_eq!(strip("c\x1b_payload\x07d"), "cd");
    }

    #[test]
    fn keeps_every_line_and_drops_only_the_trailing_newline() {
        let lines = parse_lines(b"one\r\ntwo\n\nthree\n");
        let texts: Vec<String> = lines.iter().map(text).collect();
        assert_eq!(texts, ["one", "two", "", "three"]);
    }

    #[test]
    fn tolerates_broken_sequences() {
        // 途中で切れた SGR や範囲外の値で panic しない。
        assert_eq!(strip("x\x1b[38;5"), "x");
        let lines = parse_lines("\x1b[38;5;999mz\x1b[38;2;1mq".as_bytes());
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(255)));
        assert_eq!(text(&lines[0]), "zq");
        let lossy = parse_lines(b"\xffok");
        assert_eq!(text(&lossy[0]), "\u{fffd}ok");
    }
}
