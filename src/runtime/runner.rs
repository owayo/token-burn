use super::{Control, Session, Snapshot, atomic_json, pty::Worker, read_json};
use anyhow::{Context, Result};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    path::Path,
    time::{Duration, Instant},
};

pub async fn run(path: &Path) -> Result<()> {
    let directory = path.parent().context("Missing session directory")?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("runner.lock"))?;
    lock.try_lock_exclusive()
        .context("This session already has a runner")?;
    let result = supervise(path).await;
    crate::monitor::process::terminate_all();
    if let Err(error) = &result {
        let mut state = read_json::<Snapshot>(&directory.join("snapshot.json")).unwrap_or_default();
        state.finished = true;
        state.failure = Some(format!("{error:#}"));
        let _ = atomic_json(&directory.join("snapshot.json"), &state);
    }
    result
}

async fn supervise(path: &Path) -> Result<()> {
    let directory = path.parent().unwrap();
    let session: Session = read_json(path)?;
    let active = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(session.state_file.with_file_name("active-run.lock"))?;
    active
        .try_lock_exclusive()
        .context("Another run is already active for this configuration; use token-burn attach")?;
    let mut workers = Vec::new();
    for script in &session.workers {
        workers.push(Worker::spawn(script)?);
    }
    let mut state = Snapshot {
        pid: std::process::id(),
        ready: true,
        workers: workers.iter().map(Worker::snapshot).collect(),
        ..Default::default()
    };
    // Workers can claim tasks only after every PTY has been created. The runner owns this
    // handshake so losing the launching terminal cannot strand an otherwise ready run.
    fs::write(directory.join("start"), b"")?;
    atomic_json(&directory.join("snapshot.json"), &state)?;
    let (interrupts, rx) = tokio::sync::mpsc::unbounded_channel();
    let monitor_path = directory.join("monitor.json");
    session.monitor.write(&monitor_path)?;
    let monitor = crate::monitor::run(&monitor_path, rx);
    tokio::pin!(monitor);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut monitor_finished = false;
    let mut finished_at = None;
    loop {
        tokio::select! {
            result = &mut monitor, if !monitor_finished => {
                let force = result?;
                monitor_finished = true;
                if force { for worker in &mut workers { worker.kill(); } }
            }
            _ = tick.tick() => {}
        }
        let mut changed = false;
        let mut controls =
            fs::read_dir(directory.join("controls"))?.collect::<std::io::Result<Vec<_>>>()?;
        controls.sort_by_key(|entry| entry.file_name());
        for entry in controls
            .into_iter()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .take(128)
        {
            let action = (|| -> Result<Control> {
                anyhow::ensure!(
                    entry.metadata()?.len() <= 65536,
                    "Control request is too large"
                );
                read_json(&entry.path())
            })();
            let action = match action {
                Ok(action) => action,
                Err(error) => {
                    let _ = fs::remove_file(entry.path());
                    use std::io::Write;
                    let _ = writeln!(std::io::stderr(), "Ignoring invalid control: {error:#}");
                    continue;
                }
            };
            let result = match action {
                Control::Interrupt => {
                    let _ = interrupts.send(());
                    Ok(())
                }
                Control::MonitorSize { rows, cols } => {
                    atomic_json(&directory.join("monitor-size.json"), &(rows, cols))
                }
                Control::Resize { worker, rows, cols } => workers
                    .get_mut(worker)
                    .map_or(Ok(()), |worker| worker.resize(rows, cols)),
                Control::Input { worker, bytes } => {
                    if let Some(worker) = workers.get_mut(worker).filter(|worker| !worker.finished)
                    {
                        match worker.write(&bytes) {
                            Ok(false) => break, // Keep this request and preserve input ordering.
                            Ok(true) => Ok(()),
                            Err(error) => Err(error),
                        }
                    } else {
                        Ok(())
                    }
                }
                Control::Scroll { worker, delta } => {
                    if let Some(worker) = workers.get(worker) {
                        worker.scroll(delta);
                    }
                    Ok(())
                }
            };
            fs::remove_file(entry.path())?;
            if let Err(error) = result {
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "Control failed: {error:#}");
            }
        }
        for (index, worker) in workers.iter_mut().enumerate() {
            if let Some(success) = worker.poll()? {
                let marker = session
                    .monitor
                    .marker_dir
                    .join(format!("worker-done-{index}"));
                if !marker.try_exists()? {
                    fs::write(&marker, b"")?;
                    if !success {
                        state.failure = Some(format!(
                            "Worker {} exited unexpectedly; see its log pane",
                            index + 1
                        ));
                    }
                }
                changed = true;
            }
            changed |= worker.changed();
        }
        let all_finished = workers.iter().all(|worker| worker.finished);
        if all_finished {
            let since = *finished_at.get_or_insert_with(Instant::now);
            if monitor_finished
                && (workers.iter().all(Worker::drained) || since.elapsed() > Duration::from_secs(2))
            {
                break;
            }
        }
        if changed {
            state.workers = workers.iter().map(Worker::snapshot).collect();
            atomic_json(&directory.join("snapshot.json"), &state)?;
        }
    }
    let summary = crate::executor::collect_run_summary(&session.monitor.marker_dir);
    crate::executor::write_errors_file(&session.monitor.report_dir, &summary.errors)?;
    state.summary.push(summary.describe(session.monitor.total));
    state
        .summary
        .extend(summary.error_lines(!summary.errors.is_empty()));
    if session.save_interrupted {
        let store = crate::resume::ResumeStore::load_or_warn(&crate::resume::resume_path(
            &session.state_file,
        ));
        let count = session
            .targets
            .iter()
            .filter_map(|path| store.get(&session.monitor.agent, path))
            .filter(|entry| entry.interrupted_at >= session.started_at)
            .count();
        if count > 0 {
            state.summary.push(format!(
                "Saved {count} interrupted sessions — run the same command again to continue"
            ));
        }
    }
    state.workers = workers.iter().map(Worker::snapshot).collect();
    state.finished = true;
    crate::executor::finish_logs(&session.monitor.report_dir);
    atomic_json(&directory.join("snapshot.json"), &state)?;
    // Secret prompts and executable scripts are only needed while the run is active.
    for name in ["tasks", "queue"] {
        let _ = fs::remove_dir_all(directory.join(name));
    }
    for entry in fs::read_dir(directory)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("prompt-") || name == "resume-prompt.txt" || name.starts_with("worker-")
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}
