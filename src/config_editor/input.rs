use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// UTF-8 byte cursor; all movement and deletion stays on character boundaries.
pub(super) struct TextInput {
    pub text: String,
    pub cursor: usize,
}

impl TextInput {
    pub fn new(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    pub fn insert(&mut self, text: &str) {
        let clean: String = text
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
            .collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    pub fn position(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        (
            before.bytes().filter(|byte| *byte == b'\n').count(),
            unicode_width::UnicodeWidthStr::width(before.rsplit('\n').next().unwrap_or("")),
        )
    }

    pub fn key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if control => {
                let start = self.text[..self.cursor]
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char(ch) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(&ch.to_string())
            }
            KeyCode::Enter => self.insert("\n"),
            KeyCode::Left => self.cursor = self.previous(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Home => {
                self.cursor = self.text[..self.cursor]
                    .rfind('\n')
                    .map_or(0, |index| index + 1)
            }
            KeyCode::End => {
                self.cursor += self.text[self.cursor..]
                    .find('\n')
                    .unwrap_or(self.text.len() - self.cursor)
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let previous = self.previous();
                self.text.drain(previous..self.cursor);
                self.cursor = previous;
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                self.text.drain(self.cursor..self.next());
            }
            KeyCode::Up => self.vertical(-1),
            KeyCode::Down => self.vertical(1),
            _ => {}
        }
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }
    fn next(&self) -> usize {
        self.cursor
            + self.text[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8)
    }

    fn vertical(&mut self, delta: isize) {
        let before = &self.text[..self.cursor];
        let row = before.bytes().filter(|byte| *byte == b'\n').count();
        let col = before.rsplit('\n').next().unwrap_or("").chars().count();
        let target = row.saturating_add_signed(delta);
        if target == row {
            return;
        }
        let mut start = 0;
        for (index, line) in self.text.split('\n').enumerate() {
            if index == target {
                self.cursor = start
                    + line
                        .char_indices()
                        .nth(col)
                        .map_or(line.len(), |(byte, _)| byte);
                return;
            }
            start += line.len() + 1;
        }
    }
}
