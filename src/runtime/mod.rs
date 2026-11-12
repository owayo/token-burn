//! Detached execution and the native split-screen client share atomic snapshots.
mod pty;
mod runner;
#[cfg(test)]
mod tests;
mod ui;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub use runner::run;

#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub monitor: crate::monitor::Manifest,
    pub workers: Vec<PathBuf>,
    pub state_file: PathBuf,
    pub started_at: chrono::DateTime<chrono::FixedOffset>,
    pub targets: Vec<PathBuf>,
    pub save_interrupted: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub pid: u32,
    pub ready: bool,
    pub finished: bool,
    pub failure: Option<String>,
    pub summary: Vec<String>,
    pub workers: Vec<Panel>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Panel {
    #[serde(default)]
    pub target: Option<PathBuf>,
    pub lines: Vec<String>,
    pub cursor: Option<(u16, u16)>,
    pub finished: bool,
    pub scrollback: usize,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Control {
    Interrupt,
    Resize { worker: usize, rows: u16, cols: u16 },
    Input { worker: usize, bytes: Vec<u8> },
    Scroll { worker: usize, delta: isize },
    MonitorSize { rows: u16, cols: u16 },
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("Missing snapshot directory")?)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.persist(path)
        .with_context(|| format!("Cannot write {}", path.display()))?;
    Ok(())
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?,
    )
    .with_context(|| format!("Invalid session data: {}", path.display()))
}

pub fn send(directory: &Path, control: &Control) -> Result<()> {
    if let Control::Input { worker, bytes } = control
        && bytes.len() > 4096
    {
        for chunk in bytes.chunks(4096) {
            send(
                directory,
                &Control::Input {
                    worker: *worker,
                    bytes: chunk.to_vec(),
                },
            )?;
        }
        return Ok(());
    }
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "{:010}-{sequence:020}-{:020}.json",
        std::process::id(),
        chrono::Utc::now().timestamp_micros()
    );
    atomic_json(&directory.join("controls").join(name), control)
}

pub fn launch(path: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let directory = path.parent().context("Missing session directory")?;
    fs::create_dir_all(directory.join("controls"))?;
    let log = fs::File::create(directory.join("runner.log"))?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("runner")
        .arg("--session")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; the child has not created any threads.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("Cannot start the background runner")?;
    let startup = (|| -> Result<()> {
        let started = std::time::Instant::now();
        loop {
            if let Ok(snapshot) = read_json::<Snapshot>(&directory.join("snapshot.json")) {
                anyhow::ensure!(
                    snapshot.failure.is_none(),
                    "Runner startup failed: {}",
                    snapshot.failure.unwrap_or_default()
                );
                if snapshot.ready {
                    break;
                }
            }
            anyhow::ensure!(
                child.try_wait()?.is_none(),
                "Runner exited during startup; see {}",
                directory.join("runner.log").display()
            );
            anyhow::ensure!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "Runner did not become ready; see {}",
                directory.join("runner.log").display()
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        Ok(())
    })();
    if startup.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    startup
}

pub fn attach(path: &Path) -> Result<bool> {
    ui::attach(
        if path.is_dir() {
            path.join("session.json")
        } else {
            path.to_path_buf()
        }
        .as_path(),
    )
}

pub fn print_summary(snapshot: &Snapshot, session: &Session) {
    println!();
    for line in &snapshot.summary {
        println!("{line}");
    }
    if let Some(error) = &snapshot.failure {
        eprintln!("Runner error: {error}");
    }
    println!("Logs: {}", session.monitor.report_dir.display());
}

pub fn ansi_lines(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
    use ratatui::style::{Color, Modifier};
    lines
        .iter()
        .map(|line| {
            let mut output = String::new();
            for span in &line.spans {
                output.push_str("\x1b[0m");
                for (color, background) in [(span.style.fg, false), (span.style.bg, true)] {
                    let code = if background { 48 } else { 38 };
                    if let Some(color) = color {
                        match color {
                            Color::Indexed(index) => {
                                output.push_str(&format!("\x1b[{code};5;{index}m"))
                            }
                            Color::Rgb(r, g, b) => {
                                output.push_str(&format!("\x1b[{code};2;{r};{g};{b}m"))
                            }
                            _ => {}
                        }
                    }
                }
                for (modifier, code) in [
                    (Modifier::BOLD, 1),
                    (Modifier::DIM, 2),
                    (Modifier::ITALIC, 3),
                    (Modifier::UNDERLINED, 4),
                    (Modifier::REVERSED, 7),
                ] {
                    if span.style.add_modifier.contains(modifier) {
                        output.push_str(&format!("\x1b[{code}m"));
                    }
                }
                output.push_str(&span.content);
            }
            output.push_str("\x1b[0m");
            output
        })
        .collect()
}
