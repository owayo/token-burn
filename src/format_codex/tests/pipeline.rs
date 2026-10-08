//! 入力の読み切り・生ログ・表示の失敗と終了コードのテスト。
//!
//! どの失敗でも入力は EOF まで読み切る（途中で抜けるとパイプが閉じ、上流の codex が
//! SIGPIPE で落ちる）ことを、生ログに全バイトが残ることで確かめる。

use super::super::{CHANNEL_CAPACITY, Outcome, process, process_inline, pump_lines};
use super::*;
use std::io::{self, BufReader, Cursor, Read, Write};

/// 常に失敗する書き込み先（読み手が先に終わったパイプの代わり）。
struct BrokenWriter;

impl Write for BrokenWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "reader went away",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "reader went away",
        ))
    }
}

/// `limit` バイトまでは受け付け、それを超える書き込みは失敗する（ディスクフルの代わり）。
struct FullDisk {
    limit: usize,
    written: Vec<u8>,
}

impl Write for FullDisk {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written.len() + buf.len() > self.limit {
            return Err(io::Error::other("no space left on device"));
        }
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 中身を返し切った後は読み取りエラーを返し続ける入力。
struct FailingReader {
    data: Cursor<Vec<u8>>,
}

impl Read for FailingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.data.read(buf)? {
            0 => Err(io::Error::other("input/output error")),
            n => Ok(n),
        }
    }
}

/// 最初の読み取りだけ `WouldBlock` を返す入力（非ブロッキングにされたパイプの代わり）。
struct WouldBlockOnce {
    data: Cursor<Vec<u8>>,
    blocked: bool,
}

impl Read for WouldBlockOnce {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.blocked {
            self.blocked = true;
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.data.read(buf)
    }
}

/// 生ログの検証用の入力。不正な UTF-8・CRLF・空行・改行で終わらない最終行を含む。
fn tricky_input() -> Vec<u8> {
    let mut input = Vec::new();
    input.extend_from_slice(br#"{"type":"thread.started","thread_id":"t-1"}"#);
    input.extend_from_slice(b"\r\n");
    input.extend_from_slice(b"stderr: bad \xff\xfe bytes \xe3\x81\n");
    input.extend_from_slice(b"\n");
    input.extend_from_slice(
        br#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":1}}"#,
    );
    input.extend_from_slice(b"\nlast line without a newline");
    input
}

/// チャネルの容量を大きく超える件数のイベント。
fn many_events() -> Vec<u8> {
    let mut input = Vec::new();
    for index in 0..CHANNEL_CAPACITY * 3 {
        input.extend_from_slice(
            format!(
                r#"{{"type":"item.completed","item":{{"id":"m{index}","type":"agent_message","text":"message {index}"}}}}"#
            )
            .as_bytes(),
        );
        input.push(b'\n');
    }
    input
}

#[test]
fn pump_writes_raw_bytes_exactly_and_delivers_every_line() {
    let input = tricky_input();
    let mut raw = Vec::new();
    let mut delivered = Vec::new();
    let outcome = pump_lines(Cursor::new(input.clone()), Some(&mut raw), |line| {
        delivered.push(line)
    });

    assert_eq!(outcome, Outcome::default());
    assert_eq!(raw, input, "raw output must keep the bytes as received");
    assert_eq!(delivered.len(), 5);
    assert_eq!(delivered.concat(), input);
    assert_eq!(delivered[1], b"stderr: bad \xff\xfe bytes \xe3\x81\n");
    assert_eq!(delivered[4], b"last line without a newline");
}

#[test]
fn process_writes_the_raw_file_byte_for_byte() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("codex.jsonl");
    let input = tricky_input();
    let mut display = Vec::new();
    let mut formatter = new_formatter(Instant::now());

    let outcome = process(
        Cursor::new(input.clone()),
        Some(std::fs::File::create(&path).expect("create raw file")),
        &mut display,
        &mut formatter,
    );

    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(std::fs::read(&path).expect("read raw file"), input);
    let shown = strip_sgr(&String::from_utf8(display).expect("display is UTF-8"));
    assert_eq!(
        shown,
        "  \u{2139} Codex session t-1\n\
         stderr: bad \u{fffd}\u{fffd} bytes \u{fffd}\n\
         \u{1f4ca} in:5 out:1 [12:34:56]\n\
         last line without a newline\n"
    );
}

#[test]
fn process_without_raw_output_succeeds() {
    let mut display = Vec::new();
    let mut formatter = new_formatter(Instant::now());
    let outcome = process(
        Cursor::new(b"hello\n".to_vec()),
        None::<Vec<u8>>,
        &mut display,
        &mut formatter,
    );
    assert_eq!(outcome, Outcome::default());
    assert_eq!(display, b"hello\n");
}

#[test]
fn display_failure_keeps_reading_to_eof_and_exits_with_1() {
    let input = many_events();
    let mut raw = Vec::new();
    let mut formatter = new_formatter(Instant::now());

    let outcome = process(
        Cursor::new(input.clone()),
        Some(&mut raw),
        &mut BrokenWriter,
        &mut formatter,
    );

    // 表示先が壊れても、チャネルを満杯にしたまま止まらず EOF まで読み、生ログは全部残る。
    assert_eq!(raw, input);
    assert!(outcome.display_failed);
    assert!(!outcome.raw_failed && !outcome.read_failed);
    assert_eq!(outcome.exit_code(), 1);
}

#[test]
fn raw_failure_keeps_the_display_going_and_exits_with_1() {
    let input = many_events();
    let mut raw = FullDisk {
        limit: 300,
        written: Vec::new(),
    };
    let mut display = Vec::new();
    let mut formatter = new_formatter(Instant::now());

    let outcome = process(
        Cursor::new(input),
        Some(&mut raw),
        &mut display,
        &mut formatter,
    );

    assert!(outcome.raw_failed);
    assert!(!outcome.display_failed && !outcome.read_failed);
    assert_eq!(outcome.exit_code(), 1);
    // 書けた分（失敗した行の手前まで）は残り、失敗した後は書こうとしない。
    assert!(!raw.written.is_empty() && raw.written.len() <= 300);
    assert!(raw.written.ends_with(b"\n"));
    // 表示は最後のイベントまで続く。
    let shown = strip_sgr(&String::from_utf8(display).expect("display is UTF-8"));
    let last = CHANNEL_CAPACITY * 3 - 1;
    assert_eq!(shown.lines().count(), CHANNEL_CAPACITY * 3);
    assert!(shown.ends_with(&format!("\u{1f4ac} message {last}\n")));
}

#[test]
fn read_error_delivers_the_partial_line_and_exits_with_1() {
    let reader = FailingReader {
        data: Cursor::new(b"first\npartial".to_vec()),
    };
    let mut raw = Vec::new();
    let mut delivered = Vec::new();
    let outcome = pump_lines(BufReader::new(reader), Some(&mut raw), |line| {
        delivered.push(line)
    });

    assert!(outcome.read_failed);
    assert_eq!(outcome.exit_code(), 1);
    assert_eq!(raw, b"first\npartial");
    assert_eq!(delivered, [b"first\n".to_vec(), b"partial".to_vec()]);
}

#[test]
fn would_block_is_retried_instead_of_stopping() {
    let reader = WouldBlockOnce {
        data: Cursor::new(b"one\ntwo\n".to_vec()),
        blocked: false,
    };
    let mut raw = Vec::new();
    let mut display = Vec::new();
    let mut formatter = new_formatter(Instant::now());
    let outcome = process(reader, Some(&mut raw), &mut display, &mut formatter);

    assert_eq!(outcome, Outcome::default());
    assert_eq!(raw, b"one\ntwo\n");
    assert_eq!(display, b"one\ntwo\n");
}

#[test]
fn inline_fallback_formats_and_reads_everything() {
    let input = tricky_input();
    let mut raw = Vec::new();
    let mut display = Vec::new();
    let mut formatter = new_formatter(Instant::now());
    let outcome = process_inline(
        Cursor::new(input.clone()),
        Some(&mut raw),
        &mut display,
        &mut formatter,
    );
    assert_eq!(outcome, Outcome::default());
    assert_eq!(raw, input);
    assert!(
        strip_sgr(&String::from_utf8(display).expect("display is UTF-8"))
            .starts_with("  \u{2139} Codex session t-1\n")
    );
}

#[test]
fn inline_fallback_keeps_reading_after_a_display_failure() {
    let input = many_events();
    let mut raw = Vec::new();
    let mut formatter = new_formatter(Instant::now());
    let outcome = process_inline(
        Cursor::new(input.clone()),
        Some(&mut raw),
        &mut BrokenWriter,
        &mut formatter,
    );
    assert_eq!(raw, input);
    assert!(outcome.display_failed);
    assert_eq!(outcome.exit_code(), 1);
}

#[test]
fn exit_code_is_1_for_any_failure() {
    assert_eq!(Outcome::default().exit_code(), 0);
    for outcome in [
        Outcome {
            raw_failed: true,
            ..Outcome::default()
        },
        Outcome {
            display_failed: true,
            ..Outcome::default()
        },
        Outcome {
            read_failed: true,
            ..Outcome::default()
        },
    ] {
        assert_eq!(outcome.exit_code(), 1, "{outcome:?}");
    }
}
