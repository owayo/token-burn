use super::*;
use ratatui::crossterm::event::KeyEvent;

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn edits_unicode_paths_and_submits_them_relative_to_the_attached_clients_directory() {
    let mut editor = Editor::default();
    let base = Path::new("/work");
    editor.event(Event::Paste("日本語 project".into()), base);
    editor.event(key(KeyCode::Home), base);
    editor.event(key(KeyCode::Right), base);
    editor.event(key(KeyCode::Delete), base);
    editor.event(key(KeyCode::Backspace), base);
    assert_eq!(editor.text, "語 project");
    assert_eq!(editor.cursor, 0);
    editor.event(key(KeyCode::Char('別')), base);
    let Outcome::Send(Control::AddTarget { path, id }) = editor.event(key(KeyCode::Enter), base)
    else {
        panic!("expected a directory request")
    };
    assert_eq!(path, Path::new("/work/別語 project"));
    assert_eq!(editor.pending, Some(id));
    editor.event(key(KeyCode::Char('x')), base);
    assert_eq!(editor.text, "別語 project");
}

#[test]
fn cancelling_or_pasting_multiple_lines_does_not_submit_a_request() {
    let mut editor = Editor::default();
    let base = Path::new("/work");
    assert!(matches!(
        editor.event(key(KeyCode::Esc), base),
        Outcome::Cancel
    ));
    editor.event(key(KeyCode::Enter), base);
    assert!(editor.error.contains("Enter a directory"));
    editor.event(Event::Paste("first\nsecond".into()), base);
    assert!(editor.text.is_empty());
    assert!(editor.error.contains("one directory"));
    editor.event(Event::Paste("/absolute project".into()), base);
    let Outcome::Send(Control::AddTarget { path, .. }) = editor.event(key(KeyCode::Enter), base)
    else {
        panic!("expected a request")
    };
    assert_eq!(path, Path::new("/absolute project"));
}

#[test]
fn renders_long_paths_and_errors_in_small_terminals_without_panicking() {
    let mut editor = Editor::default();
    editor.insert(&"/日本語 project".repeat(80));
    editor.error = "This directory is already in the run".into();
    for (width, height) in [(1, 1), (10, 3), (80, 24)] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| editor.draw(frame)).unwrap();
    }
}
