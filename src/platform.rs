//! runner とモニターが共有する、ホストのシェル・パス・終了通知の扱い。
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;

pub fn path_key(path: &std::path::Path) -> String {
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => match prefix.kind() {
                    Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                        normalized.push(format!("{}:", char::from(drive).to_ascii_uppercase()))
                    }
                    Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => normalized
                        .push(format!(
                            r"\\{}\{}",
                            server.to_string_lossy(),
                            share.to_string_lossy()
                        )),
                    _ => normalized.push(prefix.as_os_str()),
                },
                Component::RootDir => normalized.push(r"\"),
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                Component::Normal(part) => normalized.push(part),
            }
        }
        windows_path(&normalized.to_string_lossy())
    }
}

pub fn lock_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || cfg!(windows) && error.raw_os_error() == Some(33) // ERROR_LOCK_VIOLATION
}

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub fn shell_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    windows_path(&path)
}

/// Windows で AltGr を使って打った文字か。
///
/// Windows のコンソールは AltGr を右 Alt と左 Ctrl の同時押しとして報告し、crossterm はそれを
/// `CONTROL | ALT` に変換する（AltGr と区別しない）。ドイツ語やフランス語の配列では
/// `@ \ ~ { } [ ]` などをこの組み合わせで打つので、Ctrl・Alt 付きの文字を捨てる入力欄では
/// それらを入力できず、ワーカーへの入力では Ctrl の制御文字（`@` なら NUL）に変わっていた。
/// AltGr で英字（ASCII）を打つ配列は無いので、英字は本物の Ctrl+Alt の組み合わせとして残す。
pub fn is_alt_gr_char(key: &ratatui::crossterm::event::KeyEvent) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    cfg!(windows)
        && key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(ch) if !ch.is_ascii_alphabetic() && !ch.is_control())
}

/// Git Bash（msys2）へコマンドラインで渡す引数を、msys2-runtime の解析規則で引用する。
///
/// Rust の `Command::arg` は Windows では MSVC（`CommandLineToArgvW`）の規則で引用し、`"` の
/// 直前にない `\` はそのまま書く。ところが msys2-runtime は、親がネイティブのプロセスのときは
/// 独自の規則でコマンドラインを読み、`"..."` の中の `\\` を `\` に、`\"` を `"` に戻す
/// （winsup/cygwin/dcrt0.cc の `quoted` / `globify`）。そのため `bash -c` に渡したスクリプトの
/// 中の連続した `\`（UNC パスの `\\server\share` など）が半分になり、statusline のキャッシュの
/// パスや env の値が別のパスとして届いていた。全体を `"` で囲み、`\` と `"` の前に `\` を置いた
/// うえで、`raw_arg` でそのまま渡す（引用符の中なので glob の展開もされない）。
#[cfg(any(windows, test))]
pub fn msys2_arg(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for ch in arg.chars() {
        if matches!(ch, '\\' | '"') {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

#[cfg(any(windows, test))]
fn windows_path(path: &str) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let path = path
        .strip_prefix(r"UNC\")
        .map_or_else(|| path.to_owned(), |rest| format!(r"\\{rest}"));
    path.replace('\\', "/")
}

#[cfg(not(windows))]
pub fn bash() -> std::io::Result<PathBuf> {
    Ok(PathBuf::from("/bin/bash"))
}

#[cfg(windows)]
pub fn bash() -> std::io::Result<PathBuf> {
    windows::bash()
}

#[cfg(windows)]
pub fn shell_environment() -> std::io::Result<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    let bash = bash()?;
    let root = bash
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| std::io::Error::other("Invalid Git Bash installation"))?;
    let mut paths = vec![root.join("mingw64/bin"), root.join("usr/bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).map_err(std::io::Error::other)?;
    let mut environment = vec![
        ("PATH".into(), path),
        ("MSYSTEM".into(), "MINGW64".into()),
        // 生成するパスはすべて C:/ か UNC の形にしてある。プロンプトや CLI の引数を書き換えさせない。
        ("MSYS2_ARG_CONV_EXCL".into(), "*".into()),
    ];
    if std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH").is_none_or(|value| value.is_empty()) {
        environment.push((
            "CLAUDE_CODE_GIT_BASH_PATH".into(),
            root.join("bin/bash.exe").into_os_string(),
        ));
    }
    Ok(environment)
}

pub struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
}

impl Signals {
    pub fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
                hangup: signal(SignalKind::hangup())?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    pub async fn recv(&mut self) {
        #[cfg(unix)]
        loop {
            tokio::select! {
                _ = self.interrupt.recv() => return,
                _ = self.terminate.recv() => return,
                // 端末から SIGHUP が届いても、切り離された runner は動き続ける。
                _ = self.hangup.recv() => {},
            }
        }
        #[cfg(not(unix))]
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_gr_characters_count_as_text_only_on_windows() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let alt_gr =
            |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(is_alt_gr_char(&alt_gr('@')), cfg!(windows));
        assert_eq!(is_alt_gr_char(&alt_gr('ą')), cfg!(windows));
        // 英字は本物の Ctrl+Alt の組み合わせとして残す。Ctrl だけ・Alt だけも文字ではない。
        assert!(!is_alt_gr_char(&alt_gr('x')));
        assert!(!is_alt_gr_char(&KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_alt_gr_char(&KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::ALT
        )));
    }

    #[test]
    fn msys2_arguments_escape_backslashes_and_quotes_inside_quotes() {
        // msys2 は引用符の中の `\\` と `\"` を戻す。ほかの文字はそのまま残す。
        assert_eq!(
            msys2_arg(r"printf '%s' '\\server\share'"),
            r#""printf '%s' '\\\\server\\share'""#
        );
        assert_eq!(msys2_arg(r#"a "b" \"#), r#""a \"b\" \\""#);
        assert_eq!(msys2_arg("日本語 *{a,b}~\n"), "\"日本語 *{a,b}~\n\"");
    }

    #[test]
    fn bash_paths_preserve_spaces_unicode_and_unc_roots() {
        assert_eq!(
            windows_path(r"\\?\C:\Work tree\日本語"),
            "C:/Work tree/日本語"
        );
        assert_eq!(
            windows_path(r"\\?\UNC\server\share\repo"),
            "//server/share/repo"
        );
        assert_eq!(windows_path(r"C:\Work\repo"), "C:/Work/repo");
    }

    #[cfg(windows)]
    #[test]
    fn history_keys_match_across_shell_native_and_extended_windows_paths() {
        let forms = [
            r"C:\Work\repo",
            "C:/Work/repo",
            r"\\?\C:\Work\repo",
            r"c:\Work\repo",
        ];
        let mut state = crate::state::State::default();
        state.mark_completed("work", std::path::Path::new(forms[0]));
        for form in forms {
            assert_eq!(path_key(std::path::Path::new(form)), "C:/Work/repo");
            assert!(
                state
                    .last_processed_in_scope(std::path::Path::new(form), |_| true)
                    .is_some()
            );
        }
        assert_eq!(
            path_key(std::path::Path::new(r"\\?\UNC\server\share\repo")),
            "//server/share/repo"
        );
    }
}
