//! 表示用の文字列処理（制御文字の除去・1 行化・コマンドの表示形・経過時間・色付け）。
//!
//! どれも副作用の無い純粋関数で、表示にだけ使う。実行するコマンドや生ログには触れない。

use std::time::Duration;

// 制御文字の除去は format-stream と共通（`format_stream::util::sanitize`）。
pub(super) use crate::format_stream::util::sanitize;
use crate::format_stream::util::truncate_str;

/// 制御文字を除いたうえで、改行（とその前後の空白）を空白 1 つに畳んで 1 行にする。
///
/// 行の中の空白はそのまま残す（`printf 'a  b'` のようなコマンドの見た目を変えない）。
pub(super) fn collapse(s: &str) -> String {
    sanitize(s)
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`collapse`] した文字列を `max` 文字（char 単位）までに切り詰める。日本語の途中でも
/// 文字の境界で切る。
pub(super) fn one_line(s: &str, max: usize) -> String {
    truncate_str(&collapse(s), max)
}

/// コマンド出力の末尾から、空でない行を最大 `count` 行、元の順序で返す。
///
/// 進捗表示は CR で同じ行を上書きするので、各行は端末と同じく最後の CR 以降（空なら
/// その手前）を表示内容とみなす。`aggregated_output` は数 MB になり得るため、末尾から
/// 必要な行数だけを見る。
pub(super) fn output_tail_lines(output: &str, count: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if count == 0 {
        return lines;
    }
    for raw in output.rsplit('\n') {
        let Some(line) = raw
            .rsplit('\r')
            .map(collapse)
            .find(|segment| !segment.is_empty())
        else {
            continue;
        };
        lines.push(line);
        if lines.len() == count {
            break;
        }
    }
    lines.reverse();
    lines
}

/// `command_execution` の `command` を表示用の 1 行にする（表示専用。実行内容は変えない）。
///
/// codex はコマンドをログインシェル経由（`/bin/zsh -lc '<script>'`）で起動し、`command` には
/// その argv を shlex 形式で連結した文字列が入る。毎行に `/bin/zsh -lc '...'` が付くと
/// 狭いペインでは本体が切り詰められて読めないため、シェルの包みとクォートを外して
/// スクリプト本体だけを見せる。ヒアドキュメントのような複数行のスクリプトは 1 行に畳む。
/// コマンドが空（欠損）なら `?` を返す（`$ ` の後ろが空のままだと読み違える）。
pub(super) fn display_command(command: &str) -> String {
    let script = strip_shell_wrapper(command).unwrap_or(command);
    let shown = match unquote_word(script) {
        Some(inner) => collapse(&inner),
        None => collapse(script),
    };
    if shown.is_empty() {
        "?".to_string()
    } else {
        shown
    }
}

/// `/bin/zsh -lc <script>` / `bash -c <script>` のようなシェル経由の起動から `<script>` を取り出す。
///
/// シェルはパスの末尾が `sh` / `bash` / `zsh` のもの、フラグは `-c` / `-lc` だけを扱う。
/// それ以外の形は包みとみなさず `None` を返す（呼び出し側は元の文字列をそのまま使う）。
fn strip_shell_wrapper(command: &str) -> Option<&str> {
    let (program, rest) = command.split_once(' ')?;
    let shell = program.rsplit('/').next().unwrap_or(program);
    if !matches!(shell, "sh" | "bash" | "zsh") {
        return None;
    }
    let (flag, script) = rest.trim_start_matches(' ').split_once(' ')?;
    if !matches!(flag, "-c" | "-lc") {
        return None;
    }
    let script = script.trim_matches(' ');
    (!script.is_empty()).then_some(script)
}

/// シェルの 1 語として書かれた文字列からクォートを外す。
///
/// 引用符で囲んだ断片（`'...'` / `"..."`）を隙間なく連結した形だけを扱う。shlex 形式は
/// `'` を含むスクリプトを `'echo '"'"'hi'"'"''` のような断片の連結で表すため、単一の
/// 断片だけでなく連結も外す。二重引用符の中で外すエスケープは POSIX と同じ `\"` `\\`
/// `\$` `` \` `` だけで、それ以外のバックスラッシュは文字として残す。
///
/// 複数の語・引用符の外の文字・閉じていない引用符を含む場合は `None`（そのまま表示する）。
fn unquote_word(word: &str) -> Option<String> {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    ch => out.push(ch),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => match chars.peek() {
                        Some(&next) if matches!(next, '"' | '\\' | '$' | '`') => {
                            out.push(next);
                            chars.next();
                        }
                        _ => out.push('\\'),
                    },
                    ch => out.push(ch),
                }
            },
            _ => return None,
        }
        quoted = true;
    }
    quoted.then_some(out)
}

/// 経過時間を短く表示する（`0.4s` / `12.3s` / `2m 05s` / `1h 02m`）。
///
/// 1 分未満は 0.1 秒単位を切り捨てで出す（四捨五入すると 59.96 秒が `60.0s` になる）。
pub(super) fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}.{}s", elapsed.subsec_millis() / 100)
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, secs % 3600 / 60)
    }
}

// 色付け。format-stream と同じ配色（操作は cyan、完了・状態は dim、集計・警告は yellow、
// 失敗は red）に揃え、claude と codex のペインを同じ感覚で読めるようにする。
// 行末で必ずリセットし、色が次の行へ漏れないようにする。

pub(super) fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}

pub(super) fn cyan(s: &str) -> String {
    format!("\x1b[36m{s}\x1b[0m")
}

pub(super) fn yellow(s: &str) -> String {
    format!("\x1b[33m{s}\x1b[0m")
}

pub(super) fn red(s: &str) -> String {
    format!("\x1b[31m{s}\x1b[0m")
}
