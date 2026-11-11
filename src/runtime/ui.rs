use super::{Control, Session, Snapshot, read_json, send};
use anyhow::{Context, Result};
use fs2::FileExt;
use ratatui::crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Paragraph},
};
use std::{fs::OpenOptions, io::IsTerminal, path::Path, time::Duration};

#[derive(Default)]
struct ViewState {
    focus: usize,
    zoom: bool,
    input: bool,
    notice: String,
}

pub(super) fn attach(path: &Path) -> Result<bool> {
    let session: Session = read_json(path)?;
    let directory = path.parent().context("Missing session directory")?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        loop {
            let snapshot: Snapshot = read_json(&directory.join("snapshot.json"))?;
            if snapshot.finished {
                super::print_summary(&snapshot, &session);
                return Ok(true);
            }
            check_alive(directory)?;
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("ui.lock"))?;
    lock.try_lock_exclusive()
        .context("This session already has an attached screen")?;
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            ratatui::restore();
            return Err(error.into());
        }
    };
    let result = (|| -> Result<bool> {
        execute!(std::io::stdout(), EnableBracketedPaste)?;
        let mut view = ViewState::default();
        let mut old_sizes = Vec::new();
        loop {
            let snapshot: Snapshot = read_json(&directory.join("snapshot.json"))?;
            let monitor: Vec<String> =
                read_json(&directory.join("monitor-view.json")).unwrap_or_default();
            let layout = areas(terminal.size()?.into(), snapshot.workers.len(), &view);
            if layout != old_sizes && !snapshot.finished {
                for (index, rect) in &layout {
                    let control = if *index == 0 {
                        Control::MonitorSize {
                            rows: rect.height.saturating_sub(2).max(1),
                            cols: rect.width.saturating_sub(2).max(1),
                        }
                    } else {
                        Control::Resize {
                            worker: index - 1,
                            rows: rect.height.saturating_sub(2).max(1),
                            cols: rect.width.saturating_sub(2).max(1),
                        }
                    };
                    send(directory, &control)?;
                }
                old_sizes = layout;
            }
            terminal.draw(|frame| draw(frame, &snapshot, &monitor, &view))?;
            if snapshot.finished {
                return Ok(true);
            }
            check_alive(directory)?;
            if event::poll(Duration::from_millis(100))? {
                let action = match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if view.input {
                            if key.code == KeyCode::Char('g')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                view.input = false;
                                None
                            } else {
                                encode_key(key, snapshot.workers[view.focus - 1].application_cursor)
                                    .map(|bytes| Control::Input {
                                        worker: view.focus - 1,
                                        bytes,
                                    })
                            }
                        } else if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            Some(Control::Interrupt)
                        } else {
                            match key.code {
                                KeyCode::Char('d' | 'q') => return Ok(false),
                                KeyCode::Tab | KeyCode::Right => {
                                    view.focus = (view.focus + 1) % (snapshot.workers.len() + 1);
                                    None
                                }
                                KeyCode::BackTab | KeyCode::Left => {
                                    view.focus =
                                        view.focus.checked_sub(1).unwrap_or(snapshot.workers.len());
                                    None
                                }
                                KeyCode::Char('z') => {
                                    view.zoom = !view.zoom;
                                    None
                                }
                                KeyCode::Char('i') if view.focus > 0 => {
                                    view.input = true;
                                    None
                                }
                                KeyCode::PageUp | KeyCode::Char('k') if view.focus > 0 => {
                                    Some(Control::Scroll {
                                        worker: view.focus - 1,
                                        delta: 10,
                                    })
                                }
                                KeyCode::PageDown | KeyCode::Char('j') if view.focus > 0 => {
                                    Some(Control::Scroll {
                                        worker: view.focus - 1,
                                        delta: -10,
                                    })
                                }
                                KeyCode::End if view.focus > 0 => Some(Control::Scroll {
                                    worker: view.focus - 1,
                                    delta: isize::MIN,
                                }),
                                _ => None,
                            }
                        }
                    }
                    Event::Paste(text) if view.input => Some(Control::Input {
                        worker: view.focus - 1,
                        bytes: if snapshot.workers[view.focus - 1].bracketed_paste {
                            format!("\x1b[200~{text}\x1b[201~").into_bytes()
                        } else {
                            text.into_bytes()
                        },
                    }),
                    _ => None,
                };
                if let Some(action) = action
                    && let Err(error) = send(directory, &action)
                {
                    view.notice = format!("{error:#}");
                }
            }
        }
    })();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    let completed = result?;
    if completed {
        super::print_summary(&read_json(&directory.join("snapshot.json"))?, &session);
    } else {
        println!(
            "Execution continues in the background.\nReattach: token-burn attach '{}'",
            directory.display()
        );
    }
    Ok(completed)
}

fn check_alive(directory: &Path) -> Result<()> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.join("runner.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => {
            // 直前に読んだ画面が実行中でも、ロック解放前に完了が公開されている場合がある。
            let snapshot: Snapshot = read_json(&directory.join("snapshot.json"))?;
            anyhow::ensure!(
                snapshot.finished,
                "The runner is no longer running. Logs: {}",
                directory.display()
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn areas(area: Rect, count: usize, view: &ViewState) -> Vec<(usize, Rect)> {
    let [body, _] = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(area);
    if view.zoom || body.width < 75 || body.height < 10 {
        return vec![(view.focus.min(count), body)];
    }
    let [monitor, workers] =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Min(0)]).areas(body);
    let visible = count.min(usize::from(workers.height / 5).max(1));
    if visible == 0 {
        return vec![(0, body)];
    }
    let start = view.focus.saturating_sub(1) / visible * visible;
    let start = start.min(count.saturating_sub(visible));
    let mut output = vec![(0, monitor)];
    let rows = Layout::vertical(vec![Constraint::Fill(1); visible]).split(workers);
    output.extend(
        rows.iter()
            .enumerate()
            .map(|(index, area)| (start + index + 1, *area)),
    );
    output
}

fn draw(frame: &mut Frame, snapshot: &Snapshot, monitor: &[String], view: &ViewState) {
    let area = frame.area();
    for (index, rect) in areas(area, snapshot.workers.len(), view) {
        let title = if index == 0 {
            "token-burn · Monitor".into()
        } else {
            let worker = &snapshot.workers[index - 1];
            format!(
                "Worker {index}{}{}",
                if worker.finished { " · done" } else { "" },
                if worker.scrollback > 0 {
                    " · scroll"
                } else {
                    ""
                }
            )
        };
        let block = Block::bordered()
            .title(title)
            .border_style(Style::default().fg(if index == view.focus {
                Color::Cyan
            } else {
                Color::DarkGray
            }));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let source = if index == 0 {
            monitor
        } else {
            &snapshot.workers[index - 1].lines
        };
        let lines = source
            .iter()
            .flat_map(|line| crate::monitor::ansi::parse_lines(line.as_bytes()))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), inner);
        if view.input
            && index == view.focus
            && let Some((row, col)) = snapshot.workers[index - 1].cursor
            && row < inner.height
            && col < inner.width
        {
            frame.set_cursor_position((inner.x + col, inner.y + row));
        }
    }
    let footer = Rect::new(
        area.x,
        area.bottom().saturating_sub(2),
        area.width,
        2.min(area.height),
    );
    let hint = if view.input {
        "INPUT → worker | Ctrl-G: return to navigation"
    } else {
        "Tab/←→: focus  z: zoom  PgUp/PgDn: scroll  End: live  i: input\nd/q: detach  Ctrl-C: stop after running tasks / again: force stop"
    };
    frame.render_widget(
        Paragraph::new(if view.notice.is_empty() {
            hint
        } else {
            &view.notice
        }),
        footer,
    );
}

fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    if application_cursor {
        match key.code {
            KeyCode::Up => return Some(b"\x1bOA".to_vec()),
            KeyCode::Down => return Some(b"\x1bOB".to_vec()),
            KeyCode::Right => return Some(b"\x1bOC".to_vec()),
            KeyCode::Left => return Some(b"\x1bOD".to_vec()),
            _ => {}
        }
    }
    if let KeyCode::Char(ch) = key.code {
        let mut bytes = if key.modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii() {
            vec![(ch as u8) & 0x1f]
        } else {
            ch.to_string().into_bytes()
        };
        if key.modifiers.contains(KeyModifiers::ALT) {
            bytes.insert(0, 0x1b);
        }
        return Some(bytes);
    }
    Some(match key.code {
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => vec![127],
        KeyCode::Esc => vec![27],
        KeyCode::Tab => vec![9],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn narrow_and_crowded_screens_keep_the_focused_worker_visible() {
        for (width, height) in [(20, 5), (80, 24), (180, 50)] {
            let view = ViewState {
                focus: 17,
                ..Default::default()
            };
            let panes = areas(Rect::new(0, 0, width, height), 20, &view);
            assert!(panes.iter().any(|(index, _)| *index == 17));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let snapshot = Snapshot {
                workers: (0..20).map(|_| super::super::Panel::default()).collect(),
                ..Default::default()
            };
            terminal
                .draw(|frame| draw(frame, &snapshot, &[], &view))
                .unwrap();
        }
    }
    #[test]
    fn input_keys_respect_application_cursor_mode() {
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true).unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            )
            .unwrap(),
            [3]
        );
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('語'), KeyModifiers::NONE),
                false
            )
            .unwrap(),
            "語".as_bytes()
        );
    }
    #[test]
    fn liveness_check_reloads_completion_after_the_runner_unlocks() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("runner.lock"))
            .unwrap();
        super::super::atomic_json(&directory.join("snapshot.json"), &Snapshot::default()).unwrap();
        assert!(check_alive(directory).is_err());
        lock.lock_exclusive().unwrap();
        assert!(check_alive(directory).is_ok());
        drop(lock);
        super::super::atomic_json(
            &directory.join("snapshot.json"),
            &Snapshot {
                finished: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(check_alive(directory).is_ok());
    }
}
