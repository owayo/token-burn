use std::path::Path;

use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::runtime::Control;

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct Editor {
    text: String,
    cursor: usize,
    pub pending: Option<u64>,
    pub error: String,
}

pub(super) enum Outcome {
    Cancel,
    Send(Control),
    Continue,
}

impl Editor {
    pub fn event(&mut self, event: Event, base: &Path) -> Outcome {
        if self.pending.is_some() {
            return match event {
                Event::Key(key)
                    if key.kind != KeyEventKind::Release
                        && key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    Outcome::Send(Control::Interrupt)
                }
                _ => Outcome::Continue,
            };
        }
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Esc => return Outcome::Cancel,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Outcome::Cancel;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.text.clear();
                    self.cursor = 0;
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !ch.is_control() =>
                {
                    self.insert(&ch.to_string())
                }
                KeyCode::Left => {
                    self.cursor = self.text[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(at, _)| at)
                }
                KeyCode::Right => {
                    self.cursor += self.text[self.cursor..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf8)
                }
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = self.text.len(),
                KeyCode::Backspace if self.cursor > 0 => {
                    let previous = self.text[..self.cursor]
                        .char_indices()
                        .next_back()
                        .unwrap()
                        .0;
                    self.text.replace_range(previous..self.cursor, "");
                    self.cursor = previous;
                }
                KeyCode::Delete if self.cursor < self.text.len() => {
                    self.text.remove(self.cursor);
                }
                KeyCode::Enter => {
                    if self.text.trim().is_empty() {
                        self.error = "Enter a directory path".into();
                    } else {
                        let expanded = shellexpand::tilde(&self.text);
                        let path = base.join(expanded.as_ref());
                        let id = chrono::Utc::now().timestamp_micros().unsigned_abs();
                        self.pending = Some(id);
                        self.error.clear();
                        return Outcome::Send(Control::AddTarget { id, path });
                    }
                }
                _ => {}
            },
            Event::Paste(text) if text.chars().any(char::is_control) => {
                self.error = "Paste one directory path without line breaks".into()
            }
            Event::Paste(text) => self.insert(&text),
            _ => {}
        }
        Outcome::Continue
    }

    fn insert(&mut self, text: &str) {
        if self.text.len() + text.len() > 8192 {
            self.error = "Directory path is too long".into();
            return;
        }
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.error.clear();
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let width = area.width.min(80);
        let height = area.height.min(7);
        let area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::bordered().title("Add directory to this run");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let column = self.text[..self.cursor].width();
        let scroll = column.saturating_sub(usize::from(inner.width.saturating_sub(1)));
        frame.render_widget(
            Paragraph::new(self.text.as_str()).scroll((0, scroll as u16)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let hint = if self.pending.is_some() {
            "Adding..."
        } else {
            "Enter: add  Esc: cancel  Ctrl-U: clear"
        };
        if inner.height > 1 {
            frame.render_widget(
                Paragraph::new(hint),
                Rect::new(inner.x, inner.y + 1, inner.width, 1),
            );
        }
        if inner.height > 2 {
            frame.render_widget(
                Paragraph::new(crate::format_stream::util::truncate_inline(
                    &self.error,
                    300,
                )),
                Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 2),
            );
        }
        frame.set_cursor_position((inner.x + (column - scroll) as u16, inner.y));
    }
}
