//! `token-burn run --interactive` の対象選択 TUI。
//!
//! 実行するリポジトリと**実行順**をユーザーが確定するための画面。ワーカーは
//! `pending-0001..N` を番号順に claim するため、ここで確定した並びがそのまま
//! 処理順になる。
//!
//! 状態遷移（カーソル移動・選択・並べ替え）は [`SelectorState`] に閉じ込め、端末の
//! 初期化・復元と描画だけを [`select_targets`] が持つ。キー処理を描画から切り離して
//! いるので、端末を用意せずにユニットテストできる。

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::scanner::{ResolvedTarget, Visibility};

/// TUI の 1 行。ターゲットと選択状態を持つ。
#[derive(Debug, Clone)]
pub struct SelectorItem {
    pub target: ResolvedTarget,
    pub selected: bool,
    /// 追跡ファイルの最終更新時刻（取得できたものだけ）。
    pub last_modified: Option<DateTime<Utc>>,
}

/// 画面ヘッダーに出す実行コンテキスト。
pub struct RunContext {
    pub agent_name: String,
    /// リセットまでの残り時間（`display::format_duration` の整形済み文字列）。
    pub reset_in: String,
    /// スケジュールの導出元（`ScheduleSource::label`）。
    pub schedule_source: String,
    pub workers: usize,
}

/// キー入力を処理した結果。
#[derive(Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// 画面を続ける。
    Continue,
    /// 決定。選択済みターゲットをこの並びで実行する。
    Confirm,
    /// キャンセル。何も実行しない。
    Cancel,
}

/// TUI 全体の結果。
#[derive(Debug)]
pub enum Outcome {
    /// 実行対象（TUI 上の並び順）。
    Confirmed(Vec<ResolvedTarget>),
    Cancelled,
}

/// 選択と並べ替えの状態。描画・端末制御を含まない。
pub struct SelectorState {
    items: Vec<SelectorItem>,
    cursor: usize,
    /// フッターに出す一時メッセージ（選択 0 件で決定しようとした等）。
    notice: Option<String>,
}

impl SelectorState {
    /// 既存のソート順（変更が古い順 / defer は後ろ）のまま並べ、先頭 `initial_selected`
    /// 件を選択済みにする。非対話実行で `limit` 件が処理されるのと同じ初期状態にして、
    /// そのまま Enter を押せば従来と同じ結果になるようにしている。
    pub fn new(
        targets: Vec<ResolvedTarget>,
        modified: &HashMap<PathBuf, DateTime<Utc>>,
        initial_selected: usize,
    ) -> Self {
        let items = targets
            .into_iter()
            .enumerate()
            .map(|(i, target)| SelectorItem {
                last_modified: modified.get(&target.directory).copied(),
                selected: i < initial_selected,
                target,
            })
            .collect();
        Self {
            items,
            cursor: 0,
            notice: None,
        }
    }

    pub fn items(&self) -> &[SelectorItem] {
        &self.items
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selected_count(&self) -> usize {
        self.items.iter().filter(|i| i.selected).count()
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 選択済みターゲットを画面の並び順で取り出す。
    pub fn into_selected_targets(self) -> Vec<ResolvedTarget> {
        self.items
            .into_iter()
            .filter(|i| i.selected)
            .map(|i| i.target)
            .collect()
    }

    /// キー入力を状態へ反映する。
    pub fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        // キーを離したイベント（Windows / kitty プロトコルで届く）は 1 打鍵を
        // 2 回処理してしまうため無視する。
        if key.kind == KeyEventKind::Release {
            return KeyOutcome::Continue;
        }
        self.notice = None;
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Char('c' | 'C') if ctrl => KeyOutcome::Cancel,
            KeyCode::Esc | KeyCode::Char('q') => KeyOutcome::Cancel,
            KeyCode::Enter => self.confirm(),
            KeyCode::Char(' ') => {
                self.toggle();
                KeyOutcome::Continue
            }
            // 並べ替えはカーソル移動より先に判定する（Shift+↑↓ を素の ↑↓ に落とさない）。
            KeyCode::Up if shift => self.reorder(-1),
            KeyCode::Down if shift => self.reorder(1),
            // 端末によって Shift+K/J は大文字ではなく、小文字 + SHIFT として届く。
            // 通常の k/j より先に拾わないと、行ではなくカーソルだけが動いてしまう。
            KeyCode::Char('k') if shift => self.reorder(-1),
            KeyCode::Char('j') if shift => self.reorder(1),
            KeyCode::Char('K') => self.reorder(-1),
            KeyCode::Char('J') => self.reorder(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Home | KeyCode::Char('g') => self.jump(0),
            KeyCode::End | KeyCode::Char('G') => self.jump(self.items.len().saturating_sub(1)),
            KeyCode::Char('a') => self.set_all(true),
            KeyCode::Char('n') => self.set_all(false),
            _ => KeyOutcome::Continue,
        }
    }

    /// 選択が空のまま実行させない。全解除の誤操作をそのまま「対象なし」で
    /// 走らせると、TUI を出した意味が無いまま何も起きずに終わる。
    fn confirm(&mut self) -> KeyOutcome {
        if self.selected_count() == 0 {
            self.notice = Some("Select at least one target (Space to toggle)".to_string());
            return KeyOutcome::Continue;
        }
        KeyOutcome::Confirm
    }

    fn toggle(&mut self) {
        if let Some(item) = self.items.get_mut(self.cursor) {
            item.selected = !item.selected;
        }
    }

    fn move_cursor(&mut self, delta: isize) -> KeyOutcome {
        if self.items.is_empty() {
            return KeyOutcome::Continue;
        }
        let last = self.items.len() - 1;
        self.cursor = match delta {
            d if d < 0 => self.cursor.saturating_sub(d.unsigned_abs()),
            d => (self.cursor + d as usize).min(last),
        };
        KeyOutcome::Continue
    }

    fn jump(&mut self, index: usize) -> KeyOutcome {
        if !self.items.is_empty() {
            self.cursor = index.min(self.items.len() - 1);
        }
        KeyOutcome::Continue
    }

    /// カーソル行を隣と入れ替え、カーソルも一緒に動かす（掴んだまま運ぶ操作）。
    fn reorder(&mut self, delta: isize) -> KeyOutcome {
        if self.items.is_empty() {
            return KeyOutcome::Continue;
        }
        let target = match delta {
            d if d < 0 => match self.cursor.checked_sub(d.unsigned_abs()) {
                Some(index) => index,
                None => return KeyOutcome::Continue,
            },
            d => {
                let index = self.cursor + d as usize;
                if index >= self.items.len() {
                    return KeyOutcome::Continue;
                }
                index
            }
        };
        self.items.swap(self.cursor, target);
        self.cursor = target;
        KeyOutcome::Continue
    }

    fn set_all(&mut self, selected: bool) -> KeyOutcome {
        for item in &mut self.items {
            item.selected = selected;
        }
        KeyOutcome::Continue
    }
}

/// 対象選択 TUI を起動し、確定した実行対象を返す。
///
/// 端末を持たない実行（パイプ・CI）では画面を出せないため、黙って非対話に落とさず
/// エラーにする。`--interactive` を明示したのに選択画面が出ないまま実行が始まると、
/// 意図と違う対象へトークンを使ってしまう。
pub fn select_targets(
    targets: Vec<ResolvedTarget>,
    modified: &HashMap<PathBuf, DateTime<Utc>>,
    initial_selected: usize,
    ctx: &RunContext,
) -> Result<Outcome> {
    anyhow::ensure!(
        !targets.is_empty(),
        "No targets to select (interactive mode needs at least one candidate)"
    );
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "--interactive requires a terminal; stdin and stdout must both be a TTY"
    );

    let mut state = SelectorState::new(targets, modified, initial_selected);
    // ratatui::init() は raw mode / alternate screen へ入り、panic hook も差し替えて
    // パニック時に端末を戻す。復元漏れは直後に起動する tmux の表示を壊すため、
    // ループのエラーは持ち帰って restore() の後に伝搬する。
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut state, ctx);
    ratatui::restore();

    match result? {
        KeyOutcome::Confirm => Ok(Outcome::Confirmed(state.into_selected_targets())),
        _ => Ok(Outcome::Cancelled),
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    state: &mut SelectorState,
    ctx: &RunContext,
) -> Result<KeyOutcome> {
    let mut list_state = ListState::default();
    loop {
        list_state.select(Some(state.cursor()));
        terminal
            .draw(|frame| draw(frame, state, ctx, &mut list_state))
            .context("failed to draw the selection screen")?;
        // Resize / Mouse などは描画し直すだけでよいので、キー以外は読み捨てる。
        if let Event::Key(key) = event::read().context("failed to read a terminal event")? {
            match state.on_key(key) {
                KeyOutcome::Continue => {}
                outcome => return Ok(outcome),
            }
        }
    }
}

fn draw(frame: &mut Frame, state: &SelectorState, ctx: &RunContext, list_state: &mut ListState) {
    let [header_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    frame.render_widget(header(state, ctx), header_area);

    // パスは残り幅に合わせて縮める。固定列（マーク・実行順・可視性・名前・更新日時）と
    // 枠・選択記号の分を引いた残りを割り当て、狭い端末でも列がずれないようにする。
    let path_width = (list_area.width as usize)
        .saturating_sub(FIXED_COLUMNS_WIDTH)
        .max(MIN_PATH_WIDTH);
    let rows: Vec<ListItem> = order_numbers(state.items())
        .into_iter()
        .zip(state.items())
        .map(|(order, item)| ListItem::new(row_line(item, order, path_width)))
        .collect();
    let list = List::new(rows)
        .block(Block::bordered().title(" Targets (execution order) "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, list_area, list_state);

    frame.render_widget(footer(state), footer_area);
}

fn header(state: &SelectorState, ctx: &RunContext) -> Paragraph<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                " 🔥 token-burn ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("— pick targets and their order", dim),
        ]),
        Line::from(vec![
            Span::styled(" Agent:    ", dim),
            Span::styled(ctx.agent_name.clone(), Style::default().fg(Color::Cyan)),
            Span::styled(format!("  (reset in {}, ", ctx.reset_in), dim),
            Span::styled(format!("{})", ctx.schedule_source), dim),
        ]),
        Line::from(vec![
            Span::styled(" Selected: ", dim),
            Span::styled(
                format!("{}", state.selected_count()),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" / {} candidates", state.items().len()), dim),
            Span::styled(format!("   Workers: {}", ctx.workers), dim),
        ]),
    ];
    Paragraph::new(lines)
}

fn footer(state: &SelectorState) -> Paragraph<'static> {
    let text = match state.notice() {
        Some(notice) => Line::from(Span::styled(
            format!(" {notice}"),
            Style::default().fg(Color::Yellow),
        )),
        None => Line::from(Span::styled(
            " [Enter] Run   [Space] Toggle   [J/K] Reorder   [j/k ↑↓] Move   [a] All   [n] None   [g/G] Top/Bottom   [Esc] Cancel",
            Style::default().add_modifier(Modifier::DIM),
        )),
    };
    Paragraph::new(text).block(Block::bordered())
}

/// 選択済みの行にだけ 1 から実行順を振る。未選択行は `None`。
///
/// 実行順が画面に出ていないと「並べ替えたつもりの順番でワーカーが処理する」ことを
/// 確認できない。未選択行を飛ばして採番するので、表示された番号がそのまま
/// `pending-<n>` の順序になる。
fn order_numbers(items: &[SelectorItem]) -> Vec<Option<usize>> {
    let mut next = 0;
    items
        .iter()
        .map(|item| {
            if !item.selected {
                return None;
            }
            next += 1;
            Some(next)
        })
        .collect()
}

/// 表示名の列幅。これを超える名前は末尾を `…` にする。
const NAME_WIDTH: usize = 28;
/// 更新日時の列幅（`2026-08-09 16:49` と `(mtime unknown)` の両方が収まる）。
const MODIFIED_WIDTH: usize = 16;
/// パス列に使える幅を出すために差し引く固定分。1 桁でも足りないと行が右端で切れ、
/// パス末尾のリポジトリ名が読めなくなるため、内訳を明示して数える。
const FIXED_COLUMNS_WIDTH: usize = 4        // "[x] "
    + 5                                     // "  1. "
    + 10                                    // "[UNKNOWN] "
    + NAME_WIDTH
    + 2                                     // 名前と更新日時の区切り
    + MODIFIED_WIDTH
    + 2                                     // 更新日時とパスの区切り
    + 2                                     // リストの枠（左右）
    + 2; // 選択記号 "▶ "
/// 端末が狭くてもパス列に最低限これだけは残す。
const MIN_PATH_WIDTH: usize = 16;

fn row_line(item: &SelectorItem, order: Option<usize>, path_width: usize) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mark = if item.selected { "[x]" } else { "[ ]" };
    let mark_style = if item.selected {
        Style::default().fg(Color::Green)
    } else {
        dim
    };
    let order_text = match order {
        Some(n) => format!("{n:>3}. "),
        None => "     ".to_string(),
    };
    let visibility = format!("{:<10}", format!("[{}]", item.target.visibility));
    let visibility_style = match item.target.visibility {
        Visibility::Public => Style::default().fg(Color::Green),
        Visibility::Private => Style::default().fg(Color::Yellow),
        Visibility::Unknown => dim,
    };
    let name = pad_end(
        &truncate_chars(&item.target.display_name, NAME_WIDTH),
        NAME_WIDTH,
    );
    let modified = match item.last_modified {
        Some(ts) => ts
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => "(mtime unknown)".to_string(),
    };
    let modified = pad_end(&modified, MODIFIED_WIDTH);

    Line::from(vec![
        Span::styled(format!("{mark} "), mark_style),
        Span::styled(order_text, dim),
        Span::styled(visibility, visibility_style),
        Span::raw(name),
        Span::styled(format!("  {modified}"), dim),
        Span::styled(
            format!("  {}", display_path(&item.target.directory, path_width)),
            dim,
        ),
    ])
}

/// 文字数で切り詰める（超過分は末尾を `…` に置き換える）。
///
/// バイト単位で切ると日本語を含むリポジトリ名で文字境界を割ってパニックするため、
/// 常に char 単位で数える。
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let kept: String = text.chars().take(max - 1).collect();
    format!("{kept}…")
}

/// 文字数で右側を空白埋めする（`format!("{:<width$}")` は char 数ではなくバイト幅で
/// 数えるため、マルチバイト名で列がずれる）。
fn pad_end(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        return text.to_string();
    }
    format!("{text}{}", " ".repeat(width - count))
}

/// 表示用にパスを縮める。ホーム配下は `~` に畳み、それでも長い場合は**先頭**を `…` で
/// 落とす。末尾にリポジトリ名が来るため、切るなら前を捨てる方が識別しやすい。
fn display_path(path: &Path, max: usize) -> String {
    let raw = match dirs::home_dir() {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    };
    let count = raw.chars().count();
    if count <= max || max == 0 {
        return raw;
    }
    let tail: String = raw.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str) -> ResolvedTarget {
        ResolvedTarget {
            directory: PathBuf::from(format!("/repos/{name}")),
            display_name: name.to_string(),
            prompt: "prompt".to_string(),
            visibility: Visibility::Unknown,
            defer: false,
        }
    }

    fn state_with(count: usize, initial_selected: usize) -> SelectorState {
        let targets = (0..count).map(|i| target(&format!("r{i}"))).collect();
        SelectorState::new(targets, &HashMap::new(), initial_selected)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn names(state: &SelectorState) -> Vec<String> {
        state
            .items()
            .iter()
            .map(|i| i.target.display_name.clone())
            .collect()
    }

    fn selected_names(state: SelectorState) -> Vec<String> {
        state
            .into_selected_targets()
            .into_iter()
            .map(|t| t.display_name)
            .collect()
    }

    /// 初期状態は既存のソート順のまま、先頭 limit 件だけが選択済み。
    /// そのまま Enter を押せば非対話実行と同じ対象になる。
    #[test]
    fn new_preselects_the_first_limit_items() {
        let state = state_with(5, 2);

        assert_eq!(state.selected_count(), 2);
        assert_eq!(state.cursor(), 0);
        assert_eq!(selected_names(state), vec!["r0", "r1"]);
    }

    /// limit がターゲット数を超える（--no-limit で usize::MAX になる）場合も全選択で収まる。
    #[test]
    fn new_handles_initial_selection_larger_than_items() {
        let state = state_with(3, usize::MAX);

        assert_eq!(state.selected_count(), 3);
    }

    #[test]
    fn cursor_moves_and_clamps_at_both_ends() {
        let mut state = state_with(3, 0);

        assert_eq!(state.on_key(press(KeyCode::Up)), KeyOutcome::Continue);
        assert_eq!(state.cursor(), 0, "先頭より上には行かない");

        state.on_key(press(KeyCode::Char('j')));
        state.on_key(press(KeyCode::Down));
        assert_eq!(state.cursor(), 2);

        state.on_key(press(KeyCode::Down));
        assert_eq!(state.cursor(), 2, "末尾より下には行かない");

        state.on_key(press(KeyCode::Char('g')));
        assert_eq!(state.cursor(), 0);
        state.on_key(press(KeyCode::Char('G')));
        assert_eq!(state.cursor(), 2);
    }

    #[test]
    fn space_toggles_only_the_cursor_row() {
        let mut state = state_with(3, 0);

        state.on_key(press(KeyCode::Char(' ')));
        assert_eq!(state.selected_count(), 1);
        state.on_key(press(KeyCode::Char(' ')));
        assert_eq!(state.selected_count(), 0, "同じ行で二度押すと解除される");

        state.on_key(press(KeyCode::Down));
        state.on_key(press(KeyCode::Char(' ')));
        assert_eq!(selected_names(state), vec!["r1"]);
    }

    /// 並べ替えはカーソルごと運ぶ。押し続けたときに掴んだ行を追い続けられる。
    #[test]
    fn reorder_carries_the_cursor_with_the_row() {
        let mut state = state_with(3, 3);
        state.on_key(press(KeyCode::Down)); // r1 へ

        state.on_key(press(KeyCode::Char('K')));
        assert_eq!(names(&state), vec!["r1", "r0", "r2"]);
        assert_eq!(state.cursor(), 0, "動かした行にカーソルが付いてくる");

        state.on_key(press(KeyCode::Char('J')));
        state.on_key(press(KeyCode::Char('J')));
        assert_eq!(names(&state), vec!["r0", "r2", "r1"]);
        assert_eq!(state.cursor(), 2);
    }

    /// Shift+↑↓ は素の ↑↓ に落とさず並べ替えとして扱う。
    #[test]
    fn shift_arrows_reorder_instead_of_moving_the_cursor() {
        let mut state = state_with(3, 3);
        state.on_key(press(KeyCode::Down));

        state.on_key(press_with(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(names(&state), vec!["r1", "r0", "r2"]);

        state.on_key(press_with(KeyCode::Down, KeyModifiers::SHIFT));
        assert_eq!(names(&state), vec!["r0", "r1", "r2"]);
    }

    /// 端末によって Shift+K/J は `Char('K'/'J')` ではなく
    /// `Char('k'/'j') + SHIFT` として報告される。この場合もカーソル移動ではなく
    /// 行の並べ替えとして扱う。
    #[test]
    fn shifted_lowercase_letter_keys_reorder_rows() {
        let mut state = state_with(3, 3);
        state.on_key(press(KeyCode::Char('G'))); // r2 へ

        state.on_key(press_with(KeyCode::Char('k'), KeyModifiers::SHIFT));
        state.on_key(press_with(KeyCode::Char('k'), KeyModifiers::SHIFT));
        assert_eq!(names(&state), vec!["r2", "r0", "r1"]);
        assert_eq!(state.cursor(), 0, "動かした r2 にカーソルが付いてくる");

        state.on_key(press_with(KeyCode::Char('j'), KeyModifiers::SHIFT));
        assert_eq!(names(&state), vec!["r0", "r2", "r1"]);
        assert_eq!(state.cursor(), 1);
    }

    #[test]
    fn reorder_at_the_edges_keeps_the_order() {
        let mut state = state_with(2, 2);

        state.on_key(press(KeyCode::Char('K')));
        assert_eq!(names(&state), vec!["r0", "r1"], "先頭は上へ動かせない");
        assert_eq!(state.cursor(), 0);

        state.on_key(press(KeyCode::Char('G')));
        state.on_key(press(KeyCode::Char('J')));
        assert_eq!(names(&state), vec!["r0", "r1"], "末尾は下へ動かせない");
        assert_eq!(state.cursor(), 1);
    }

    #[test]
    fn select_all_and_clear_all() {
        let mut state = state_with(4, 1);

        state.on_key(press(KeyCode::Char('a')));
        assert_eq!(state.selected_count(), 4);

        state.on_key(press(KeyCode::Char('n')));
        assert_eq!(state.selected_count(), 0);
    }

    #[test]
    fn enter_confirms_in_screen_order() {
        let mut state = state_with(3, 3);
        // r2 を先頭へ運ぶ
        state.on_key(press(KeyCode::Char('G')));
        state.on_key(press(KeyCode::Char('K')));
        state.on_key(press(KeyCode::Char('K')));
        assert_eq!(names(&state), vec!["r2", "r0", "r1"]);

        // 並べ替え後の 2 行目にいる r0 を対象から外す
        state.on_key(press(KeyCode::Char('g')));
        state.on_key(press(KeyCode::Char('j')));
        state.on_key(press(KeyCode::Char(' ')));

        assert_eq!(state.on_key(press(KeyCode::Enter)), KeyOutcome::Confirm);
        assert_eq!(selected_names(state), vec!["r2", "r1"]);
    }

    /// 選択 0 件の Enter は実行させず、理由をフッターに出す。
    #[test]
    fn enter_with_no_selection_shows_a_notice_instead_of_running() {
        let mut state = state_with(3, 0);

        assert_eq!(state.on_key(press(KeyCode::Enter)), KeyOutcome::Continue);
        assert!(
            state.notice().is_some_and(|n| n.contains("at least one")),
            "選択が空である理由を表示するべき: {:?}",
            state.notice()
        );

        // 1 件選べば通知が消えて決定できる。
        state.on_key(press(KeyCode::Char(' ')));
        assert!(state.notice().is_none());
        assert_eq!(state.on_key(press(KeyCode::Enter)), KeyOutcome::Confirm);
    }

    #[test]
    fn q_esc_and_ctrl_c_cancel() {
        for key in [
            press(KeyCode::Char('q')),
            press(KeyCode::Esc),
            press_with(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut state = state_with(2, 2);
            assert_eq!(
                state.on_key(key),
                KeyOutcome::Cancel,
                "{key:?} でキャンセル"
            );
        }
    }

    /// キーを離したイベントを処理すると 1 打鍵が 2 回効いてしまう。
    #[test]
    fn key_release_events_are_ignored() {
        let mut state = state_with(3, 0);
        let mut release = press(KeyCode::Char(' '));
        release.kind = KeyEventKind::Release;

        assert_eq!(state.on_key(release), KeyOutcome::Continue);
        assert_eq!(state.selected_count(), 0);
    }

    /// 実行順は選択済み行だけに 1 から振る（未選択行は飛ばす）。
    #[test]
    fn order_numbers_count_only_selected_rows() {
        let mut state = state_with(4, 0);
        state.on_key(press(KeyCode::Char(' '))); // r0 を選択
        state.on_key(press(KeyCode::Down));
        state.on_key(press(KeyCode::Down));
        state.on_key(press(KeyCode::Char(' '))); // r2 を選択

        assert_eq!(
            order_numbers(state.items()),
            vec![Some(1), None, Some(2), None]
        );
    }

    /// 未知のキーは状態を変えない（誤爆でキャンセル・決定しない）。
    #[test]
    fn unknown_keys_do_nothing() {
        let mut state = state_with(3, 1);

        assert_eq!(
            state.on_key(press(KeyCode::Char('z'))),
            KeyOutcome::Continue
        );
        assert_eq!(state.on_key(press(KeyCode::Tab)), KeyOutcome::Continue);
        assert_eq!(state.cursor(), 0);
        assert_eq!(state.selected_count(), 1);
    }

    /// 空リストでも操作でパニックしない（呼び出し側が弾くが、状態側も落ちない）。
    #[test]
    fn empty_list_survives_every_key() {
        let mut state = SelectorState::new(Vec::new(), &HashMap::new(), 5);

        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Char('J'),
            KeyCode::Char('K'),
            KeyCode::Char('a'),
            KeyCode::Char('G'),
        ] {
            assert_eq!(state.on_key(press(code)), KeyOutcome::Continue);
        }
        assert_eq!(state.selected_count(), 0);
        assert_eq!(state.on_key(press(KeyCode::Enter)), KeyOutcome::Continue);
    }

    /// 行の表示には選択マーク・実行順・可視性・名前・パスが載る。
    #[test]
    fn row_line_shows_order_and_metadata() {
        let mut item = SelectorItem {
            target: target("repo-a"),
            selected: true,
            last_modified: None,
        };
        let text: String = row_line(&item, Some(3), 40)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("[x]"), "選択マーク: {text}");
        assert!(text.contains("3."), "実行順: {text}");
        assert!(text.contains("[UNKNOWN]"), "可視性: {text}");
        assert!(text.contains("repo-a"), "表示名: {text}");
        assert!(text.contains("/repos/repo-a"), "パス: {text}");

        item.selected = false;
        let text: String = row_line(&item, None, 40)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("[ ]"), "未選択マーク: {text}");
        assert!(!text.contains("3."), "未選択行に実行順は出さない: {text}");
    }

    /// 列は char 単位で数える。バイト単位で切ると日本語を含む名前で文字境界を割って
    /// パニックし、パディングもずれる。
    #[test]
    fn truncate_chars_cuts_on_character_boundaries() {
        assert_eq!(truncate_chars("abcdef", 10), "abcdef");
        assert_eq!(
            truncate_chars("abcdef", 6),
            "abcdef",
            "ちょうどなら切らない"
        );
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("日本語のリポジトリ", 5), "日本語の…");
        assert_eq!(truncate_chars("abc", 0), "");
    }

    #[test]
    fn pad_end_counts_characters_not_bytes() {
        assert_eq!(pad_end("ab", 4), "ab  ");
        assert_eq!(pad_end("abcd", 4), "abcd");
        assert_eq!(pad_end("abcde", 4), "abcde", "幅を超える場合は切らない");
        assert_eq!(pad_end("日本", 4), "日本  ");
    }

    /// パスは末尾（リポジトリ名側）を残して先頭を落とす。前を残すと、どのリポジトリか
    /// 分からない共通プレフィックスだけが並ぶ。
    #[test]
    fn display_path_keeps_the_tail_when_truncating() {
        let long = Path::new("/very/long/prefix/that/does/not/fit/repo-name");

        let shown = display_path(long, 20);

        assert!(shown.starts_with('…'), "先頭を落とすべき: {shown}");
        assert!(shown.ends_with("repo-name"), "末尾は残すべき: {shown}");
        assert_eq!(shown.chars().count(), 20, "指定幅に収めるべき: {shown}");
    }

    #[test]
    fn display_path_folds_the_home_directory() {
        let Some(home) = dirs::home_dir() else {
            return;
        };

        let shown = display_path(&home.join("GitHub/token-burn"), 60);

        assert_eq!(shown, "~/GitHub/token-burn");
    }

    #[test]
    fn display_path_keeps_short_paths_as_is() {
        assert_eq!(display_path(Path::new("/tmp/repo"), 40), "/tmp/repo");
    }

    /// TTY が無い環境では非対話に落とさずエラーにする（テスト実行時は TTY 無し）。
    #[test]
    fn select_targets_requires_a_tty() {
        let ctx = RunContext {
            agent_name: "claude".to_string(),
            reset_in: "1d".to_string(),
            schedule_source: "fixed".to_string(),
            workers: 2,
        };

        let error = select_targets(vec![target("r0")], &HashMap::new(), 1, &ctx)
            .expect_err("TTY が無ければエラーになるべき");

        assert!(
            error.to_string().contains("requires a terminal"),
            "端末が必要だと伝えるべき: {error}"
        );
    }

    /// 候補が空のまま TUI を出さない（表示するものが無く、Enter も押せない）。
    #[test]
    fn select_targets_rejects_an_empty_candidate_list() {
        let ctx = RunContext {
            agent_name: "claude".to_string(),
            reset_in: "1d".to_string(),
            schedule_source: "fixed".to_string(),
            workers: 2,
        };

        let error = select_targets(Vec::new(), &HashMap::new(), 1, &ctx)
            .expect_err("候補が空ならエラーになるべき");

        assert!(
            error.to_string().contains("No targets"),
            "候補が無いと伝えるべき: {error}"
        );
    }
}
