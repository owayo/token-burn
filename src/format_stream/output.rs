//! 書き込み先が壊れても入力パイプを閉じず、最後まで停止判定を続けるための出力。

use std::io::{self, Write};

/// 最初の書き込みエラーを保持し、それ以降の書き込みを捨てる。
///
/// エラーは EOF 後に呼び出し元へ返す。途中で伝播すると上流の Claude が SIGPIPE で
/// 終了するうえ、後から届くレート制限イベントも検査できなくなる。
pub(super) struct DrainWriter<W> {
    inner: W,
    description: &'static str,
    error: Option<io::Error>,
}

impl<W> DrainWriter<W> {
    pub(super) fn new(inner: W, description: &'static str) -> Self {
        Self {
            inner,
            description,
            error: None,
        }
    }

    pub(super) fn into_error(self) -> Option<io::Error> {
        self.error
    }

    fn record_error(&mut self, error: io::Error) {
        warn_output_error(self.description, &error);
        self.error = Some(error);
    }
}

/// 診断先も閉じている場合は警告を諦め、入力の処理を続ける。
pub(super) fn warn_output_error(operation: &str, error: &io::Error) {
    let _ = writeln!(
        io::stderr().lock(),
        "format-stream: {operation} failed ({error}); continuing to read input"
    );
}

impl<W: Write> Write for DrainWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.error.is_none() {
            match self.inner.write(bytes) {
                Ok(0) if !bytes.is_empty() => {
                    self.record_error(io::ErrorKind::WriteZero.into());
                }
                Ok(written) => return Ok(written),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
                Err(error) => self.record_error(error),
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        while self.error.is_none() {
            match self.inner.flush() {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => self.record_error(error),
            }
        }
        Ok(())
    }
}
