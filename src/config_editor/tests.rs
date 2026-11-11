use super::*;

fn field(section: Section, key: &str) -> Field {
    schema::fields(section)
        .into_iter()
        .find(|field| field.key == key)
        .unwrap()
}

fn page(section: Section, index: Option<usize>) -> Page {
    Page { section, index }
}

#[test]
fn new_config_is_only_created_on_save_and_loads_normally() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nested/config.toml");
    let mut draft = Draft::open(&path).unwrap();
    draft.validate().unwrap();
    assert!(!path.exists());
    assert!(draft.save().unwrap().is_none());
    let config = crate::config::Config::load(&path).unwrap();
    assert_eq!(config.settings.parallelism, 3);
    assert!(!draft.dirty());
}

#[test]
fn save_keeps_comments_unknown_keys_tildes_and_a_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let mut draft = Draft::open(&path).unwrap();
    draft.save().unwrap();
    let original = std::fs::read_to_string(&path)
        .unwrap()
        .replace("parallelism = 3", "parallelism = 3 # keep this")
        .replace("[settings]", "[settings]\nfuture_option = 'keep-me'");
    std::fs::write(&path, &original).unwrap();
    let mut draft = Draft::open(&path).unwrap();
    draft
        .set(
            page(Section::Settings, None),
            field(Section::Settings, "parallelism"),
            Some("5"),
        )
        .unwrap();
    let backup = draft.save().unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(backup).unwrap(), original);
    let content = std::fs::read_to_string(path).unwrap();
    assert!(content.contains("parallelism = 5 # keep this"));
    assert!(content.contains("future_option = 'keep-me'"));
    assert!(content.contains("~/GitHub"));
}

#[test]
fn invalid_draft_and_external_updates_leave_original_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let mut draft = Draft::open(&path).unwrap();
    draft.save().unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    draft
        .set(
            page(Section::Settings, None),
            field(Section::Settings, "dedup_scope"),
            Some("global"),
        )
        .unwrap();
    draft
        .set(
            page(Section::Settings, None),
            field(Section::Settings, "skip_within"),
            None,
        )
        .unwrap();
    assert!(draft.save().is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    let mut draft = Draft::open(&path).unwrap();
    std::fs::write(&path, format!("{original}\n# external edit\n")).unwrap();
    assert!(draft.save().unwrap_err().to_string().contains("外部で変更"));
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("# external edit")
    );
}

#[test]
fn profile_rename_updates_references_and_referenced_profiles_cannot_be_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let mut draft = Draft::open(&tmp.path().join("config.toml")).unwrap();
    let index = draft.add(Section::Profiles, "").unwrap();
    let agent = page(Section::Agents, Some(0));
    let profile = page(Section::Profiles, Some(index));
    draft
        .set(
            agent,
            field(Section::Agents, "ai_usage.profiles"),
            Some("work"),
        )
        .unwrap();
    assert!(draft.remove(profile).is_err());
    draft
        .set(profile, field(Section::Profiles, "name"), Some("home"))
        .unwrap();
    assert_eq!(
        draft
            .input(agent, field(Section::Agents, "ai_usage.profiles"))
            .unwrap(),
        "home"
    );
    draft
        .set(agent, field(Section::Agents, "ai_usage.profiles"), None)
        .unwrap();
    draft.remove(profile).unwrap();
}

#[test]
fn edit_cancel_quit_and_save_are_separate_actions() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let mut draft = Draft::open(&path).unwrap();
    draft.save().unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    let mut editor = Editor::new(Draft::open(&path).unwrap());
    editor
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    if let Some(Dialog::Edit { input, .. }) = &mut editor.dialog {
        input.text = "9".into();
        input.cursor = 1;
    } else {
        panic!("input must open");
    }
    editor
        .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    assert!(!editor.draft.dirty());
    editor
        .draft
        .set(
            page(Section::Settings, None),
            field(Section::Settings, "parallelism"),
            Some("6"),
        )
        .unwrap();
    assert!(
        !editor
            .key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap()
    );
    assert!(matches!(editor.dialog, Some(Dialog::Quit)));
    editor
        .key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .unwrap();
    editor
        .key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(editor.dialog, Some(Dialog::Save { .. })));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert!(
        editor
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap()
    );
    assert_eq!(
        crate::config::Config::load(&path)
            .unwrap()
            .settings
            .parallelism,
        6
    );
}

#[test]
fn unicode_input_and_line_arguments_round_trip() {
    let mut input = TextInput::new("a日本語".into());
    input.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    input.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    input.insert("本");
    assert_eq!(input.text, "a日本語");
    let tmp = tempfile::tempdir().unwrap();
    let mut draft = Draft::open(&tmp.path().join("config.toml")).unwrap();
    let agent = page(Section::Agents, Some(0));
    let command = field(Section::Agents, "command");
    let args = "claude\n--model\nmodel with spaces\n\"\"\n\"line1\\nline2\"";
    draft.set(agent, command, Some(args)).unwrap();
    assert_eq!(draft.input(agent, command).unwrap(), args);
}

#[test]
fn rendering_supports_normal_small_and_japanese_terminals() {
    use ratatui::{Terminal, backend::TestBackend};
    let tmp = tempfile::tempdir().unwrap();
    let mut editor = Editor::new(Draft::open(&tmp.path().join("config.toml")).unwrap());
    for (width, height) in [(100, 30), (80, 24), (60, 18), (20, 5)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut nav = ratatui::widgets::ListState::default();
        let mut fields = ratatui::widgets::ListState::default();
        terminal
            .draw(|frame| ui::draw(frame, &editor, &mut nav, &mut fields))
            .unwrap();
        editor.prepare_save();
        terminal
            .draw(|frame| ui::draw(frame, &editor, &mut nav, &mut fields))
            .unwrap();
        editor.dialog = None;
    }
}

#[test]
fn a_small_settings_screen_shows_how_to_discard_unsaved_changes() {
    use ratatui::{Terminal, backend::TestBackend};
    let tmp = tempfile::tempdir().unwrap();
    let mut editor = Editor::new(Draft::open(&tmp.path().join("config.toml")).unwrap());
    editor
        .key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();
    terminal
        .draw(|frame| {
            ui::draw(
                frame,
                &editor,
                &mut Default::default(),
                &mut Default::default(),
            )
        })
        .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    let compact_text: String = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    assert!(compact_text.contains("破棄して終了"), "{text}");
    editor.compact = true;
    assert!(
        editor
            .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap()
    );
    assert!(!editor.saved);
}

#[test]
fn a_small_settings_screen_blocks_saving_until_the_preview_is_visible() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let mut editor = Editor::new(Draft::open(&path).unwrap());
    editor.prepare_save();
    assert!(matches!(editor.dialog, Some(Dialog::Save { .. })));
    editor.compact = true;
    assert!(
        !editor
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap()
    );
    assert!(!editor.saved);
    assert!(!path.exists());
    assert!(matches!(editor.dialog, Some(Dialog::Save { .. })));
    editor.compact = false;
    assert!(
        editor
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap()
    );
    assert!(path.exists());
}

#[test]
fn nested_tables_stay_with_their_agents_after_edits_and_deletions() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let mut draft = Draft::open(&path).unwrap();
    draft.add(Section::Agents, "codex").unwrap();
    draft.add(Section::Profiles, "").unwrap();
    let first = page(Section::Agents, Some(0));
    let second = page(Section::Agents, Some(1));
    draft
        .set(
            first,
            field(Section::Agents, "env"),
            Some("ACCOUNT = \"first\""),
        )
        .unwrap();
    draft
        .set(
            second,
            field(Section::Agents, "env"),
            Some("ACCOUNT = \"second\""),
        )
        .unwrap();
    draft.save().unwrap();
    let mut draft = Draft::open(&path).unwrap();
    draft
        .set(
            first,
            field(Section::Agents, "ai_usage.profiles"),
            Some("work"),
        )
        .unwrap();
    draft.save().unwrap();
    let config = crate::config::Config::load(&path).unwrap();
    assert_eq!(
        config.agents[0].ai_usage.as_ref().unwrap().profiles,
        ["work"]
    );
    assert!(config.agents[1].ai_usage.is_none());
    assert_eq!(config.agents[0].env["ACCOUNT"], "first");
    assert_eq!(config.agents[1].env["ACCOUNT"], "second");
    let mut draft = Draft::open(&path).unwrap();
    draft.remove(first).unwrap();
    draft.save().unwrap();
    let config = crate::config::Config::load(&path).unwrap();
    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.agents[0].env["ACCOUNT"], "second");
    assert!(config.agents[0].ai_usage.is_none());
}

#[test]
fn env_changes_are_reviewable_without_leaking_values_or_command_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let mut draft = Draft::open(&path).unwrap();
    let agent = page(Section::Agents, Some(0));
    draft
        .set(
            agent,
            field(Section::Agents, "env"),
            Some("API_TOKEN = \"old-secret\""),
        )
        .unwrap();
    draft.save().unwrap();
    draft
        .set(
            agent,
            field(Section::Agents, "env"),
            Some("API_TOKEN = \"new-secret\""),
        )
        .unwrap();
    draft
        .set(
            agent,
            field(Section::Agents, "command"),
            Some("claude\n--api-key\ncommand-secret"),
        )
        .unwrap();
    let changes = draft.changes().join("\n");
    assert!(changes.contains("API_TOKEN"));
    assert!(!changes.contains("old-secret"));
    assert!(!changes.contains("new-secret"));
    assert!(!changes.contains("command-secret"));
}

#[test]
fn undo_restores_deletion_and_optional_value_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut editor = Editor::new(Draft::open(&tmp.path().join("config.toml")).unwrap());
    editor.field = 3;
    editor
        .key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))
        .unwrap();
    assert_eq!(
        editor
            .draft
            .input(
                page(Section::Settings, None),
                field(Section::Settings, "skip_within")
            )
            .unwrap(),
        ""
    );
    editor
        .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
        .unwrap();
    assert_eq!(
        editor
            .draft
            .input(
                page(Section::Settings, None),
                field(Section::Settings, "skip_within")
            )
            .unwrap(),
        "1d"
    );
}

#[test]
fn no_final_newline_and_default_value_confirmation_do_not_dirty_existing_config() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let mut draft = Draft::open(&path).unwrap();
    draft.save().unwrap();
    let original = std::fs::read_to_string(&path)
        .unwrap()
        .trim_end()
        .to_string();
    std::fs::write(&path, &original).unwrap();
    let mut draft = Draft::open(&path).unwrap();
    assert!(!draft.dirty());
    draft
        .set(
            page(Section::Settings, None),
            field(Section::Settings, "parallelism"),
            Some("3"),
        )
        .unwrap();
    assert!(!draft.dirty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn saving_through_a_symlink_keeps_link_permissions_and_private_backup() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("actual.toml");
    let mut draft = Draft::open(&path).unwrap();
    draft.save().unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let link = tmp.path().join("config.toml");
    symlink(&path, &link).unwrap();
    let mut draft = Draft::open(&link).unwrap();
    draft
        .set(
            page(Section::Settings, None),
            field(Section::Settings, "parallelism"),
            Some("4"),
        )
        .unwrap();
    let backup = draft.save().unwrap().unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        std::fs::metadata(backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        crate::config::Config::load(&link)
            .unwrap()
            .settings
            .parallelism,
        4
    );
}

#[test]
fn crlf_line_endings_are_preserved_on_save() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    let mut draft = Draft::open(&path).unwrap();
    draft.save().unwrap();
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace('\n', "\r\n");
    std::fs::write(&path, text).unwrap();
    let mut draft = Draft::open(&path).unwrap();
    draft
        .set(
            page(Section::Settings, None),
            field(Section::Settings, "parallelism"),
            Some("4"),
        )
        .unwrap();
    draft.save().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\r\n"));
    assert!(!text.replace("\r\n", "").contains('\n'));
    crate::config::Config::load(&path).unwrap();
}

#[test]
fn profiles_with_quotes_or_newlines_survive_the_picker() {
    let tmp = tempfile::tempdir().unwrap();
    let mut draft = Draft::open(&tmp.path().join("config.toml")).unwrap();
    draft.add(Section::Profiles, "").unwrap();
    let profile = page(Section::Profiles, Some(0));
    let name = "\"home\naccount";
    draft
        .set(profile, field(Section::Profiles, "name"), Some(name))
        .unwrap();
    draft
        .set(
            page(Section::Agents, Some(0)),
            field(Section::Agents, "ai_usage.profiles"),
            Some(&document::lines_text(&[name.into()])),
        )
        .unwrap();
    let mut editor = Editor::new(draft);
    editor.select_page(page(Section::Agents, Some(0)));
    editor.field = editor
        .fields()
        .iter()
        .position(|field| field.key == "ai_usage.profiles")
        .unwrap();
    editor
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    if let Some(Dialog::Choose { checked, .. }) = &editor.dialog {
        assert_eq!(checked.as_ref().unwrap(), &[name]);
    } else {
        panic!("profile picker must open");
    }
    editor
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    editor.draft.validate().unwrap();
}

#[test]
fn inline_agents_accept_nested_profile_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let mut draft = Draft::open(&tmp.path().join("config.toml")).unwrap();
    let agents = draft
        .doc
        .as_table_mut()
        .remove("agents")
        .unwrap()
        .into_value()
        .unwrap();
    draft.doc["agents"] = toml_edit::Item::Value(agents);
    draft.add(Section::Profiles, "").unwrap();
    draft
        .set(
            page(Section::Agents, Some(0)),
            field(Section::Agents, "ai_usage.profiles"),
            Some("work"),
        )
        .unwrap();
    draft.add(Section::Agents, "codex").unwrap();
    draft.save().unwrap();
    let config = crate::config::Config::load(&draft.path).unwrap();
    assert_eq!(
        config.agents[0].ai_usage.as_ref().unwrap().profiles,
        ["work"]
    );
    assert_eq!(config.agents[1].provider.as_deref(), Some("codex"));
}

#[test]
fn invalid_numeric_edit_shows_its_error_in_the_dialog() {
    use ratatui::{Terminal, backend::TestBackend};
    let tmp = tempfile::tempdir().unwrap();
    let mut editor = Editor::new(Draft::open(&tmp.path().join("config.toml")).unwrap());
    editor
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    if let Some(Dialog::Edit { input, .. }) = &mut editor.dialog {
        input.text = "0".into();
        input.cursor = 1;
    }
    editor
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    assert!(matches!(editor.dialog, Some(Dialog::Edit { .. })));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            ui::draw(
                frame,
                &editor,
                &mut Default::default(),
                &mut Default::default(),
            )
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        text.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
            .contains("入力を確認してください")
    );
}
