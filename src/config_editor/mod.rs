//! Persistent configuration editor. Draft changes never start agents or scan repositories.
mod document;
mod input;
mod schema;
mod ui;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use ratatui::crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use std::io::IsTerminal;
use std::path::Path;

use document::Draft;
use input::TextInput;
use schema::{Field, Kind, Section};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Page {
    section: Section,
    index: Option<usize>,
}

enum Dialog {
    Edit {
        field: Field,
        input: TextInput,
    },
    Choose {
        field: Field,
        options: Vec<(String, String)>,
        selected: usize,
        checked: Option<Vec<String>>,
    },
    AddAgent(usize),
    Delete,
    Save {
        lines: Vec<String>,
        offset: u16,
    },
    Quit,
}

struct Editor {
    draft: Draft,
    page: usize,
    field: usize,
    sidebar: bool,
    dialog: Option<Dialog>,
    notice: String,
    saved: bool,
    advanced: bool,
    undo: Vec<toml_edit::DocumentMut>,
}

impl Editor {
    fn new(draft: Draft) -> Self {
        let notice = if draft.is_new() {
            "新しい設定です。仮のリセット日時（月曜 09:00 UTC）と探索先を変更して保存してください。"
        } else {
            "項目を選ぶと説明が表示されます。変更は保存するまで反映されません。"
        }
        .into();
        Self {
            draft,
            page: 0,
            field: 0,
            sidebar: false,
            dialog: None,
            notice,
            saved: false,
            advanced: false,
            undo: Vec::new(),
        }
    }

    fn pages(&self) -> Vec<Page> {
        let mut pages = Vec::new();
        for section in Section::ALL {
            if section.repeated() {
                pages.extend((0..self.draft.count(section)).map(|index| Page {
                    section,
                    index: Some(index),
                }));
            }
            pages.push(Page {
                section,
                index: None,
            });
        }
        pages
    }

    fn current(&self) -> Page {
        self.pages()[self.page]
    }

    fn fields(&self) -> Vec<Field> {
        let page = self.current();
        if page.section.repeated() && page.index.is_none() {
            Vec::new()
        } else {
            schema::fields(page.section)
                .into_iter()
                .filter(|field| self.advanced || !field.label.contains("（詳細）"))
                .collect()
        }
    }

    fn key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != KeyEventKind::Release
            && self.dialog.is_none()
            && key.code == KeyCode::Char('u')
            && key.modifiers.is_empty()
        {
            if let Some(doc) = self.undo.pop() {
                self.draft.doc = doc;
                self.page = self.page.min(self.pages().len() - 1);
                self.field = 0;
                self.notice = "直前の変更を取り消しました。".into();
            } else {
                self.notice = "取り消せる変更はありません。".into();
            }
            return Ok(false);
        }
        let before = self.draft.doc.clone();
        let result = self.apply_key(key);
        if before.to_string() != self.draft.doc.to_string() && !self.saved {
            if self.undo.len() == 64 {
                self.undo.remove(0);
            }
            self.undo.push(before);
        }
        result
    }

    fn apply_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            self.dialog = None;
            return Ok(self.quit());
        }
        if let Some(dialog) = self.dialog.take() {
            return self.dialog_key(dialog, key);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(self.quit()),
            KeyCode::Esc => self.sidebar = true,
            KeyCode::Char('s') | KeyCode::F(2) => self.prepare_save(),
            KeyCode::Char('x') => {
                self.advanced = !self.advanced;
                self.field = 0;
            }
            KeyCode::Tab | KeyCode::BackTab => self.sidebar = !self.sidebar,
            KeyCode::Left | KeyCode::Char('h') => self.sidebar = true,
            KeyCode::Right | KeyCode::Char('l') => self.sidebar = false,
            KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
            KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
            KeyCode::Char('a') => self.add(),
            KeyCode::Char('d') if self.current().index.is_some() => {
                self.dialog = Some(Dialog::Delete)
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.current().section.repeated() && self.current().index.is_none() {
                    self.add();
                } else if self.sidebar {
                    self.sidebar = false;
                } else {
                    self.edit()?;
                }
            }
            KeyCode::Backspace | KeyCode::Delete if !self.sidebar => {
                if let Some(field) = self.fields().get(self.field).copied() {
                    if field.optional {
                        self.draft.set(self.current(), field, None)?;
                        self.notice =
                            "任意値を解除しました。保存前なら u で直前の変更を取り消せます。"
                                .into();
                    } else {
                        self.notice = "必須項目は削除できません。Enter で編集してください。".into();
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn navigate(&mut self, delta: isize) {
        if self.sidebar {
            self.page = self
                .page
                .saturating_add_signed(delta)
                .min(self.pages().len() - 1);
            self.field = 0;
        } else {
            self.field = self
                .field
                .saturating_add_signed(delta)
                .min(self.fields().len().saturating_sub(1));
        }
    }

    fn quit(&mut self) -> bool {
        if !self.draft.dirty() {
            true
        } else {
            self.dialog = Some(Dialog::Quit);
            false
        }
    }

    fn prepare_save(&mut self) {
        match self.draft.validate() {
            Ok(()) if self.draft.dirty() => {
                self.notice.clear();
                self.dialog = Some(Dialog::Save {
                    lines: self.draft.changes(),
                    offset: 0,
                });
            }
            Ok(()) => self.notice = "変更はありません。".into(),
            Err(error) => self.notice = format!("保存できません: {error:#}"),
        }
    }

    fn add(&mut self) {
        if self.current().section == Section::Agents {
            self.dialog = Some(Dialog::AddAgent(0));
        } else if self.current().section.repeated() {
            let section = self.current().section;
            match self.draft.add(section, "") {
                Ok(index) => self.select_page(Page {
                    section,
                    index: Some(index),
                }),
                Err(error) => self.notice = error.to_string(),
            }
        } else {
            self.notice =
                "エージェント・探索先・個別対象・プロファイルのページで追加できます。".into();
        }
    }

    fn select_page(&mut self, page: Page) {
        self.page = self.pages().iter().position(|p| *p == page).unwrap_or(0);
        self.field = 0;
        self.sidebar = false;
    }

    fn edit(&mut self) -> Result<()> {
        let Some(field) = self.fields().get(self.field).copied() else {
            return Ok(());
        };
        let current = self.draft.input(self.current(), field)?;
        self.notice.clear();
        match field.kind {
            Kind::Bool(_) => self.draft.set(
                self.current(),
                field,
                Some(if current == "true" { "false" } else { "true" }),
            )?,
            Kind::Choice(options, _) => {
                let mut options: Vec<_> = options
                    .iter()
                    .map(|(value, label)| (value.to_string(), label.to_string()))
                    .collect();
                if field.optional {
                    options.insert(0, (String::new(), "未設定（指定を解除）".into()));
                }
                if !current.is_empty() && !options.iter().any(|(value, _)| *value == current) {
                    options.insert(0, (current.clone(), format!("現在の値: {current}")));
                }
                let selected = options
                    .iter()
                    .position(|(value, _)| *value == current)
                    .unwrap_or(0);
                self.dialog = Some(Dialog::Choose {
                    field,
                    options,
                    selected,
                    checked: None,
                });
            }
            Kind::Profiles => {
                let mut options = self.draft.profile_options();
                let checked = document::parse_lines(&current)?;
                for value in &checked {
                    if !options.iter().any(|(name, _)| name == value) {
                        options.push((
                            value.clone(),
                            format!("{value}（未定義。選択を解除してください）"),
                        ));
                    }
                }
                if options.is_empty() {
                    self.notice =
                        "先に「プロファイルを追加」でアカウントを登録してください。".into();
                } else {
                    self.dialog = Some(Dialog::Choose {
                        field,
                        options,
                        selected: 0,
                        checked: Some(checked),
                    });
                }
            }
            _ => {
                self.dialog = Some(Dialog::Edit {
                    field,
                    input: TextInput::new(current),
                })
            }
        }
        Ok(())
    }

    fn dialog_key(&mut self, mut dialog: Dialog, key: KeyEvent) -> Result<bool> {
        if key.code == KeyCode::Esc {
            return Ok(false);
        }
        let confirm = key.code == KeyCode::Enter;
        match &mut dialog {
            Dialog::Edit { field, input } => {
                if (confirm && !field.multiline())
                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('s'))
                {
                    match self.draft.set(self.current(), *field, Some(&input.text)) {
                        Ok(()) => {
                            self.notice = "変更を保持しました。s で確認・保存できます。".into();
                            return Ok(false);
                        }
                        Err(error) => self.notice = format!("入力を確認してください: {error:#}"),
                    }
                } else {
                    input.key(key);
                }
            }
            Dialog::Choose {
                field,
                options,
                selected,
                checked,
            } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(options.len() - 1)
                }
                KeyCode::Char(' ') if checked.is_some() => {
                    let values = checked.as_mut().unwrap();
                    let value = &options[*selected].0;
                    if values.contains(value) {
                        values.retain(|v| v != value);
                    } else {
                        values.push(value.clone());
                    }
                }
                KeyCode::Enter => {
                    let value = checked
                        .as_ref()
                        .map(|values| document::lines_text(values))
                        .unwrap_or_else(|| options[*selected].0.clone());
                    self.draft.set(self.current(), *field, Some(&value))?;
                    return Ok(false);
                }
                _ => {}
            },
            Dialog::AddAgent(selected) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(2),
                KeyCode::Enter => {
                    let index = self
                        .draft
                        .add(Section::Agents, ["claude", "codex", "custom"][*selected])?;
                    self.select_page(Page {
                        section: Section::Agents,
                        index: Some(index),
                    });
                    return Ok(false);
                }
                _ => {}
            },
            Dialog::Delete if key.code == KeyCode::Char('y') => {
                let page = self.current();
                match self.draft.remove(page) {
                    Ok(()) => self.select_page(Page {
                        section: page.section,
                        index: None,
                    }),
                    Err(error) => self.notice = error.to_string(),
                }
                return Ok(false);
            }
            Dialog::Save { offset, .. } => {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => *offset = offset.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => *offset = offset.saturating_add(1),
                    KeyCode::Enter => match self.draft.save() {
                        Ok(backup) => {
                            self.notice = backup.map(|path| format!("保存しました。バックアップ: {}", path.display())).unwrap_or_else(|| "設定を作成しました。token-burn run -n で実行計画を確認できます。".into());
                            self.saved = true;
                            return Ok(true);
                        }
                        Err(error) => {
                            self.notice = format!("保存できません: {error:#}");
                            return Ok(false);
                        }
                    },
                    _ => {}
                }
            }
            Dialog::Quit if key.code == KeyCode::Char('d') => return Ok(true),
            Dialog::Quit if key.code == KeyCode::Char('s') => {
                self.prepare_save();
                return Ok(false);
            }
            _ => {}
        }
        self.dialog = Some(dialog);
        Ok(false)
    }
}

pub fn run(path: &Path) -> Result<()> {
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "config requires a terminal; stdin and stdout must both be a TTY"
    );
    let mut editor = Editor::new(Draft::open(path)?);
    let mut terminal = ratatui::try_init().context("Failed to initialize the settings terminal")?;
    let result = (|| -> Result<()> {
        execute!(std::io::stdout(), EnableBracketedPaste)?;
        let mut nav_state = ratatui::widgets::ListState::default();
        let mut field_state = ratatui::widgets::ListState::default();
        loop {
            terminal.draw(|frame| ui::draw(frame, &editor, &mut nav_state, &mut field_state))?;
            match event::read()? {
                Event::Key(key) => match editor.key(key) {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => editor.notice = format!("操作できませんでした: {error:#}"),
                },
                Event::Paste(text) => {
                    if let Some(Dialog::Edit { field, input }) = &mut editor.dialog {
                        input.insert(&if field.multiline() {
                            text.replace("\r\n", "\n").replace('\r', "\n")
                        } else {
                            text.replace(['\r', '\n'], " ")
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result?;
    if editor.saved {
        println!("{}", editor.notice);
    } else {
        println!("設定の変更を保存せずに終了しました。");
    }
    Ok(())
}
