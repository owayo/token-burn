//! Host shell, paths and termination notifications shared by the runner and monitor.
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
        // All generated paths use C:/ or UNC syntax. Do not rewrite prompts or CLI arguments.
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
                // A detached runner continues when a terminal sends SIGHUP.
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
