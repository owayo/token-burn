use super::Panel;
use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    num::NonZeroU16,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub(super) struct Worker {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    input: std::sync::mpsc::SyncSender<Vec<u8>>,
    terminal: Arc<Mutex<vt100::Parser<TerminalReplies>>>,
    eof: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    pub finished: bool,
}

fn dimension(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value.max(1)).unwrap()
}

impl Worker {
    pub fn spawn(script: &Path) -> Result<Self> {
        let pair = portable_pty::native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new("/bin/bash");
        command.arg(script);
        command.env("TERM", "xterm-256color");
        command.env_remove("NO_COLOR");
        command.env_remove("TMUX");
        command.env_remove("TMUX_PANE");
        command.env_remove("COLUMNS");
        command.env_remove("LINES");
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (input, pending) = std::sync::mpsc::sync_channel::<Vec<u8>>(16);
        std::thread::Builder::new()
            .name("worker-input".into())
            .spawn(move || {
                for bytes in pending {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })?;
        let terminal = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            dimension(24),
            dimension(100),
            5000,
            TerminalReplies(input.clone()),
        )));
        let eof = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(true));
        let (parser, closed, changed) = (terminal.clone(), eof.clone(), dirty.clone());
        std::thread::Builder::new()
            .name("worker-output".into())
            .spawn(move || {
                let mut buffer = [0; 16384];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let mut parser = parser.lock().unwrap();
                            let (rows, cols) = parser.screen().size();
                            // Keep draining the PTY even if malformed output trips the parser.
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                parser.process(&buffer[..count]);
                            }))
                            .is_err()
                            {
                                let replies = parser.callbacks().0.clone();
                                *parser = vt100::Parser::new_with_callbacks(
                                    rows,
                                    cols,
                                    5000,
                                    TerminalReplies(replies),
                                );
                            }
                            changed.store(true, Ordering::Release);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                closed.store(true, Ordering::Release);
            })?;
        Ok(Self {
            master: pair.master,
            child,
            input,
            terminal,
            eof,
            dirty,
            finished: false,
        })
    }

    pub fn poll(&mut self) -> Result<Option<bool>> {
        if self.finished {
            return Ok(None);
        }
        if let Some(status) = self.child.try_wait()? {
            self.finished = true;
            self.dirty.store(true, Ordering::Release);
            return Ok(Some(status.success()));
        }
        Ok(None)
    }

    pub fn drained(&self) -> bool {
        self.eof.load(Ordering::Acquire)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = (rows.clamp(1, 300), cols.clamp(1, 500));
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.terminal
            .lock()
            .unwrap()
            .set_size(dimension(rows), dimension(cols));
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<bool> {
        match self.input.try_send(bytes.to_vec()) {
            Ok(()) => Ok(true),
            Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(false),
            Err(error) => Err(error).context("Worker input is closed"),
        }
    }

    pub fn scroll(&self, delta: isize) {
        let mut parser = self.terminal.lock().unwrap();
        let position = if delta == isize::MIN {
            0
        } else {
            parser.screen().scrollback().saturating_add_signed(delta)
        };
        parser.screen_mut().set_scrollback(position);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn snapshot(&self) -> Panel {
        let parser = self.terminal.lock().unwrap();
        let screen = parser.screen();
        Panel {
            target: None,
            lines: terminal_lines(screen),
            cursor: (!screen.hide_cursor() && screen.scrollback() == 0)
                .then(|| screen.cursor_position()),
            finished: self.finished,
            scrollback: screen.scrollback(),
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
        }
    }

    pub fn changed(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    pub fn kill(&mut self) {
        if !self.finished {
            // Every worker is a separate controlling-terminal session. Kill its process group,
            // including the active pipeline, before collecting the shell.
            if let Some(pid) = self
                .child
                .process_id()
                .and_then(|pid| i32::try_from(pid).ok())
                && pid > 1
                && pid != unsafe { libc::getpgrp() }
            {
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
            }
            let _ = self.child.kill();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                match self.child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
            self.finished = true;
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill();
    }
}

fn terminal_lines(screen: &vt100::Screen) -> Vec<String> {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
    };
    fn color(value: vt100::Color) -> Color {
        match value {
            vt100::Color::Default => Color::Reset,
            vt100::Color::Idx(index) => Color::Indexed(index),
            vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
    let (rows, cols) = screen.size();
    let mut lines = Vec::new();
    for row in 0..rows.get() {
        let mut spans = Vec::new();
        let mut text = String::new();
        let mut previous = Style::default();
        for col in 0..cols.get() {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut style = Style::default()
                .fg(color(cell.fgcolor()))
                .bg(color(cell.bgcolor()));
            for (active, modifier) in [
                (cell.bold(), Modifier::BOLD),
                (cell.dim(), Modifier::DIM),
                (cell.italic(), Modifier::ITALIC),
                (cell.underline(), Modifier::UNDERLINED),
                (cell.inverse(), Modifier::REVERSED),
            ] {
                if active {
                    style = style.add_modifier(modifier);
                }
            }
            if style != previous && !text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut text), previous));
            }
            previous = style;
            text.push_str(if cell.has_contents() {
                cell.contents()
            } else {
                " "
            });
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, previous));
        }
        lines.push(Line::from(spans));
    }
    super::ansi_lines(&lines)
}

struct TerminalReplies(std::sync::mpsc::SyncSender<Vec<u8>>);
impl vt100::Callbacks for TerminalReplies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        code: char,
    ) {
        let argument = params
            .first()
            .and_then(|values| values.first())
            .copied()
            .unwrap_or(0);
        let reply = match (first, code, argument) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                Some(format!("\x1b[{};{}R", row + 1, col + 1))
            }
            (None, 'n', 5) => Some("\x1b[0n".into()),
            (None, 'c', 0) => Some("\x1b[?1;2c".into()),
            (Some(b'>'), 'c', 0) => Some("\x1b[>0;1;0c".into()),
            _ => None,
        };
        if let Some(reply) = reply {
            let _ = self.0.try_send(reply.into_bytes());
        }
    }
}
