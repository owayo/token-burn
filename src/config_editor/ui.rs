use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use super::{Dialog, Editor};

pub(super) fn draw(
    frame: &mut Frame,
    editor: &Editor,
    nav_state: &mut ListState,
    field_state: &mut ListState,
) {
    let area = frame.area();
    if area.width < 60 || area.height < 18 {
        frame.render_widget(Paragraph::new("設定画面には 60 列 × 18 行以上が必要です。\n端末を広げてください。q / Esc で終了。"), area);
        return;
    }
    let [header, body, notice, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(2),
    ])
    .areas(area);
    let status = if editor.draft.is_new() {
        "新規作成"
    } else if editor.draft.dirty() {
        "未保存の変更あり"
    } else {
        "保存済み"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("token-burn  設定", Style::default().fg(Color::Cyan).bold()),
                Span::raw(format!("   {status}")),
            ]),
            Line::from(editor.draft.path.display().to_string()),
        ]),
        header,
    );
    let [navigation, form] =
        Layout::horizontal([Constraint::Length(27), Constraint::Min(20)]).areas(body);
    let pages = editor.pages();
    let nav: Vec<_> = pages
        .iter()
        .map(|page| {
            let name = if page.index.is_some() {
                format!("  {}", editor.draft.name(*page))
            } else if page.section.repeated() {
                format!("+ {}を追加", page.section.title())
            } else {
                page.section.title().into()
            };
            ListItem::new(clean(&name))
        })
        .collect();
    nav_state.select(Some(editor.page));
    frame.render_stateful_widget(
        List::new(nav)
            .block(panel("カテゴリ", editor.sidebar))
            .highlight_symbol("› ")
            .highlight_style(highlight(editor.sidebar)),
        navigation,
        nav_state,
    );
    let fields = editor.fields();
    if fields.is_empty() {
        frame.render_widget(Paragraph::new(format!("{}を追加\n\nEnter または a で追加します。\n\n追加後に名前、コマンド、パスなどを編集できます。", editor.current().section.title())).wrap(Wrap { trim: false }).block(panel("追加", !editor.sidebar)), form);
    } else {
        let [list_area, help_area] =
            Layout::vertical([Constraint::Min(4), Constraint::Length(6)]).areas(form);
        let items: Vec<_> = fields
            .iter()
            .map(|field| {
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(field.label, Style::default().bold()),
                        Span::styled(
                            if field.optional { "  任意" } else { "" },
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]),
                    Line::from(Span::styled(
                        format!(
                            "  {}",
                            clean(&editor.draft.display(editor.current(), *field))
                        ),
                        Style::default().fg(Color::Cyan),
                    )),
                ])
            })
            .collect();
        field_state.select(Some(editor.field));
        frame.render_stateful_widget(
            List::new(items)
                .block(panel(editor.current().section.title(), !editor.sidebar))
                .highlight_symbol("› ")
                .highlight_style(highlight(!editor.sidebar)),
            list_area,
            field_state,
        );
        let field = fields[editor.field];
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(field.key, Style::default().fg(Color::Cyan))),
                Line::from(field.help),
            ])
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title("項目の説明")),
            help_area,
        );
    }
    frame.render_widget(
        Paragraph::new(clean(&editor.notice))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Yellow)),
        notice,
    );
    frame.render_widget(Paragraph::new("Tab/←→: 切替  ↑↓/jk: 移動  Enter: 編集  x: 詳細\ns/F2: 保存  a: 追加  d: 削除  Del: 解除  u: 取消  q: 終了"), footer);
    if let Some(dialog) = &editor.dialog {
        draw_dialog(frame, dialog, &editor.notice);
    }
}

fn panel(title: &str, focused: bool) -> Block<'_> {
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
}

fn highlight(focused: bool) -> Style {
    if focused {
        Style::default().bg(Color::Cyan).fg(Color::Black)
    } else {
        Style::default().bg(Color::DarkGray)
    }
}

fn draw_dialog(frame: &mut Frame, dialog: &Dialog, notice: &str) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(110);
    let height = area.height.saturating_sub(2);
    let popup = Rect::new(area.x + (area.width - width) / 2, area.y + 1, width, height);
    frame.render_widget(Clear, popup);
    let title = match dialog {
        Dialog::Edit { field, .. } | Dialog::Choose { field, .. } => field.label,
        Dialog::AddAgent(_) => "エージェントを追加",
        Dialog::Delete => "削除の確認",
        Dialog::Save { .. } => "変更内容を確認して保存",
        Dialog::Quit => "未保存の変更があります",
    };
    let border = panel(title, true);
    let inner = border.inner(popup);
    frame.render_widget(border, popup);
    let has_error = matches!(dialog, Dialog::Edit { .. }) && !notice.is_empty();
    let [body, error_area, instructions] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if has_error { 3 } else { 0 }),
        Constraint::Length(3),
    ])
    .areas(inner);
    if has_error {
        frame.render_widget(
            Paragraph::new(clean(notice))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::Red)),
            error_area,
        );
    }
    let hint = match dialog {
        Dialog::Edit { field, input } => {
            let (row, col) = input.position();
            let scroll_y = row
                .saturating_sub(body.height.saturating_sub(1) as usize)
                .min(u16::MAX as usize) as u16;
            let scroll_x = col
                .saturating_sub(body.width.saturating_sub(1) as usize)
                .min(u16::MAX as usize) as u16;
            frame.render_widget(
                Paragraph::new(clean(&input.text)).scroll((scroll_y, scroll_x)),
                body,
            );
            frame.set_cursor_position((
                body.x
                    + (col.saturating_sub(scroll_x as usize) as u16)
                        .min(body.width.saturating_sub(1)),
                body.y
                    + (row.saturating_sub(scroll_y as usize) as u16)
                        .min(body.height.saturating_sub(1)),
            ));
            field.hint()
        }
        Dialog::Choose {
            options,
            selected,
            checked,
            ..
        } => {
            let items = options.iter().map(|(value, label)| {
                let mark = checked
                    .as_ref()
                    .map(|values| {
                        if values.contains(value) {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    })
                    .unwrap_or("");
                ListItem::new(format!("{mark}{}", clean(label)))
            });
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_style(highlight(true))
                    .highlight_symbol("› "),
                body,
                &mut state,
            );
            if checked.is_some() {
                "Space: 選択・解除  Enter: 適用  Esc: 取消"
            } else {
                "↑↓: 選択  Enter: 適用  Esc: 取消"
            }
        }
        Dialog::AddAgent(selected) => {
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(
                List::new(["Claude Code", "Codex CLI", "カスタムコマンド"])
                    .highlight_style(highlight(true))
                    .highlight_symbol("› "),
                body,
                &mut state,
            );
            "↑↓: 選択  Enter: 追加  Esc: 取消。追加したコマンドと固定スケジュールを確認してください。"
        }
        Dialog::Delete => {
            frame.render_widget(
                Paragraph::new("この項目を削除しますか？\n削除は保存時に反映されます。"),
                body,
            );
            "y: 削除  Esc: 取消"
        }
        Dialog::Save { lines, offset } => {
            let lines = preview_lines(lines, body.width);
            let offset = (*offset as usize)
                .min(lines.len().saturating_sub(1))
                .min(u16::MAX as usize) as u16;
            frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), body);
            "↑↓: スクロール  Enter: 保存して終了  Esc: 編集に戻る。既存ファイルはバックアップします。"
        }
        Dialog::Quit => {
            frame.render_widget(
                Paragraph::new("変更を保存しますか？\nEsc で編集を続けられます。"),
                body,
            );
            "s: 確認・保存  d: 破棄して終了  Esc: 編集に戻る"
        }
    };
    frame.render_widget(
        Paragraph::new(hint)
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: false }),
        instructions,
    );
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .collect()
}

fn preview_lines(lines: &[String], width: u16) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;
    let mut output = Vec::new();
    for line in lines {
        let mut current = String::new();
        let mut cells = 0;
        for ch in clean(line).chars() {
            let size = ch.width().unwrap_or(0);
            if cells + size > width as usize && !current.is_empty() {
                output.push(Line::from(std::mem::take(&mut current)));
                cells = 0;
            }
            current.push(ch);
            cells += size;
        }
        output.push(Line::from(current));
    }
    output
}
