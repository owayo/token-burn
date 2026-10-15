//! ログ保存先や表示先の障害で、上流プロセスへの入力パイプを閉じないことを検証する。

use super::*;
use crate::format_stream::output::DrainWriter;
use std::io::{self, Write};

#[derive(Default)]
struct BrokenOutput {
    writes: usize,
}

impl Write for BrokenOutput {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn raw_creation_failure_still_drains_input_and_displays_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let input = "first line\nlast line\n";
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    assert!(process(&mut reader, &mut output, Some(dir.path()), None, 95).is_err());
    assert_eq!(reader.position(), input.len() as u64);
    assert!(String::from_utf8(output).unwrap().contains("last line"));
}

#[test]
fn display_failure_still_saves_the_tail_and_checks_rate_limits() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("raw.jsonl");
    let stop = dir.path().join("stop");
    let input = concat!(
        "first line\n",
        "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"rateLimitType\":\"seven_day\"}}\n",
        "last line\n",
    );
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = BrokenOutput::default();

    assert!(process(&mut reader, &mut output, Some(&raw), Some(&stop), 95).is_err());
    assert_eq!(reader.position(), input.len() as u64);
    assert_eq!(std::fs::read_to_string(raw).unwrap(), input);
    assert!(stop.is_file(), "表示できなくても停止判定を続ける");
    assert_eq!(output.writes, 1, "壊れた表示先には書き直さない");
}

#[test]
fn stop_signal_failure_takes_priority_over_display_failure() {
    let dir = tempfile::tempdir().unwrap();
    let stop = dir.path().join("missing").join("stop");
    let input = concat!(
        "first line\n",
        "{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"rateLimitType\":\"seven_day\"}}\n",
        "last line\n",
    );
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = BrokenOutput::default();

    assert!(process(&mut reader, &mut output, None, Some(&stop), 95).unwrap());
    assert_eq!(reader.position(), input.len() as u64);
}

#[test]
fn display_flush_failure_still_drains_input_and_saves_the_tail() {
    struct FlushFailure;
    impl Write for FlushFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("raw.jsonl");
    let input = "first line\nlast line\n";
    let mut reader = Cursor::new(input.as_bytes());

    assert!(process(&mut reader, FlushFailure, Some(&raw), None, 95).is_err());
    assert_eq!(reader.position(), input.len() as u64);
    assert_eq!(std::fs::read_to_string(raw).unwrap(), input);
}

#[test]
fn interrupted_writes_and_flushes_are_retried() {
    #[derive(Default)]
    struct InterruptedOnce {
        writes: usize,
        flushes: usize,
        bytes: Vec<u8>,
    }
    impl Write for InterruptedOnce {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes == 1 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            // 短い書き込みが続いても、全バイトが保存されることを確認する。
            self.bytes.push(bytes[0]);
            Ok(1)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 1 {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Ok(())
            }
        }
    }

    let mut sink = InterruptedOnce::default();
    let mut writer = DrainWriter::new(&mut sink, "test output write");
    writer.write_all(b"abc").unwrap();
    writer.flush().unwrap();
    assert!(writer.into_error().is_none());
    assert_eq!(sink.bytes, b"abc");
    assert_eq!(sink.flushes, 2);
}

#[test]
fn zero_length_write_is_reported_after_draining() {
    struct WriteZero;
    impl Write for WriteZero {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Ok(0)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let input = "first line\nlast line\n";
    let mut reader = Cursor::new(input.as_bytes());
    let error = process(&mut reader, WriteZero, None, None, 95).unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::WriteZero
    );
    assert_eq!(reader.position(), input.len() as u64);
}

#[test]
fn stop_signal_failure_takes_priority_over_a_later_read_failure() {
    struct FailingReader(Cursor<Vec<u8>>);
    impl io::Read for FailingReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.0.position() == self.0.get_ref().len() as u64 {
                Err(io::ErrorKind::Other.into())
            } else {
                io::Read::read(&mut self.0, bytes)
            }
        }
    }
    impl io::BufRead for FailingReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.0.position() == self.0.get_ref().len() as u64 {
                Err(io::ErrorKind::Other.into())
            } else {
                io::BufRead::fill_buf(&mut self.0)
            }
        }

        fn consume(&mut self, bytes: usize) {
            io::BufRead::consume(&mut self.0, bytes);
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let stop = dir.path().join("missing").join("stop");
    let input = b"{\"type\":\"rate_limit_event\",\"rate_limit_info\":{\"status\":\"rejected\",\"rateLimitType\":\"seven_day\"}}\n";
    let reader = FailingReader(Cursor::new(input.to_vec()));
    assert!(process(reader, Vec::new(), None, Some(&stop), 95).unwrap());
}

#[cfg(target_os = "linux")]
#[test]
fn raw_write_failure_still_displays_the_tail_and_checks_rate_limits() {
    let dir = tempfile::tempdir().unwrap();
    let stop = dir.path().join("stop");
    // バッファを超える入力で書き込み中の ENOSPC を発生させる。
    let input = format!(
        "{}\n{{\"type\":\"rate_limit_event\",\"rate_limit_info\":{{\"status\":\"rejected\",\"rateLimitType\":\"seven_day\"}}}}\nlast line\n",
        "x".repeat(16_384)
    );
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    assert!(
        process(
            &mut reader,
            &mut output,
            Some(std::path::Path::new("/dev/full")),
            Some(&stop),
            95,
        )
        .is_err()
    );
    assert_eq!(reader.position(), input.len() as u64);
    assert!(String::from_utf8(output).unwrap().contains("last line"));
    assert!(stop.is_file());
}
