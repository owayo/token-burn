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
    let session: Session = read_json(path)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("runner.lock"))?;
    lock.try_lock_exclusive()
        .context("This session already has a runner")?;
    let result = supervise(directory, &session).await;
    crate::monitor::process::terminate_all();
    if let Err(error) = &result {
        let mut state = read_json::<Snapshot>(&directory.join("snapshot.json")).unwrap_or_default();
        state.finished = true;
        state.failure = Some(format!("{error:#}"));
        let _ = atomic_json(&directory.join("snapshot.json"), &state);
    }
    cleanup_run_files(directory);
    result
}

async fn supervise(directory: &Path, session: &Session) -> Result<()> {
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
        workers: worker_snapshots(&workers, session),
        ..Default::default()
    };
    // 全 PTY の準備が済んでから claim を許可する。起動元の端末が切断されても
    // 実行を始められるよう、この合図は runner が発行する。
    let monitor_path = directory.join("monitor.json");
    session.monitor.write(&monitor_path)?;
    atomic_json(&directory.join("snapshot.json"), &state)?;
    fs::write(directory.join("start"), b"")?;
    let (interrupts, rx) = tokio::sync::mpsc::unbounded_channel();
    let monitor = crate::monitor::run(&monitor_path, rx);
    tokio::pin!(monitor);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut monitor_finished = false;
    let mut finished_at = None;
    let mut snapshot_pending = false;
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
        process_controls(directory, |action| {
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
                        return worker.write(&bytes);
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
            result.map(|()| true)
        })?;
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
            changed |=
                state.workers[index].target != current_target(session, index, worker.finished);
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
        if changed || snapshot_pending {
            state.workers = worker_snapshots(&workers, session);
            snapshot_pending = match atomic_json(&directory.join("snapshot.json"), &state) {
                Ok(()) => false,
                Err(error) => {
                    // 表示先の障害で PTY を破棄せず、次の周回で最新の画面を公開し直す。
                    if !snapshot_pending {
                        use std::io::Write;
                        let _ = writeln!(std::io::stderr(), "Cannot publish snapshot: {error:#}");
                    }
                    true
                }
            };
        }
    }
    let summary = crate::executor::collect_run_summary(&session.monitor.marker_dir);
    state.summary.push(summary.describe(session.monitor.total));
    let errors_written =
        match crate::executor::write_errors_file(&session.monitor.report_dir, &summary.errors) {
            Ok(written) => written,
            Err(error) => {
                // 補足ファイルの保存失敗で集計や実行資産の片付けを中断しない。
                use std::io::Write;
                let warning = format!("Warning: cannot save error list: {error:#}");
                let _ = writeln!(std::io::stderr(), "{warning}");
                state.summary.push(warning);
                false
            }
        };
    state.summary.extend(summary.error_lines(errors_written));
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
    state.workers = worker_snapshots(&workers, session);
    state.finished = true;
    crate::executor::finish_logs(&session.monitor.report_dir);
    atomic_json(&directory.join("snapshot.json"), &state)?;
    Ok(())
}

fn current_target(session: &Session, worker: usize, finished: bool) -> Option<std::path::PathBuf> {
    if finished {
        return None;
    }
    let task = fs::read_to_string(
        session
            .monitor
            .marker_dir
            .join(format!("worker-task-{worker}")),
    )
    .ok()?;
    let index = task.parse::<usize>().ok()?.checked_sub(1)?;
    session.targets.get(index).cloned()
}

fn worker_snapshots(workers: &[Worker], session: &Session) -> Vec<super::Panel> {
    workers
        .iter()
        .enumerate()
        .map(|(index, worker)| {
            let mut panel = worker.snapshot();
            panel.target = current_target(session, index, worker.finished);
            panel
        })
        .collect()
}

fn cleanup_run_files(directory: &Path) {
    // プロンプトと実行スクリプトは動作中だけ必要なので、完了後に削除する。
    for name in ["tasks", "queue"] {
        let _ = fs::remove_dir_all(directory.join(name));
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("prompt-") || name == "resume-prompt.txt" || name.starts_with("worker-")
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// 保留した入力を残し、適用済みまたは不正な要求だけをキューから取り除く。
fn process_controls(
    directory: &Path,
    mut apply: impl FnMut(Control) -> Result<bool>,
) -> Result<()> {
    let mut controls =
        fs::read_dir(directory.join("controls"))?.collect::<std::io::Result<Vec<_>>>()?;
    controls.sort_by_key(|entry| entry.file_name());
    let mut blocked_inputs = std::collections::HashSet::new();
    let mut attempted = 0;
    for entry in controls
        .into_iter()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
    {
        if attempted >= 128 {
            break;
        }
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
        let input_worker = match &action {
            Control::Input { worker, .. } => Some(*worker),
            _ => None,
        };
        // 同じワーカーの入力順は保ち、詰まった入力の後ろにある停止・リサイズや
        // 他ワーカーの入力は処理する。保留した要求は 128 件の上限に数えない。
        if input_worker.is_some_and(|worker| blocked_inputs.contains(&worker)) {
            continue;
        }
        attempted += 1;
        let result = apply(action);
        if matches!(result, Ok(false)) {
            if let Some(worker) = input_worker {
                blocked_inputs.insert(worker);
            }
            continue;
        }
        fs::remove_file(entry.path())?;
        if let Err(error) = result {
            use std::io::Write;
            let _ = writeln!(std::io::stderr(), "Control failed: {error:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session(directory: &Path) -> Session {
        Session {
            monitor: crate::monitor::Manifest {
                agent: "test".into(),
                command: "test-agent".into(),
                reset: String::new(),
                total: 1,
                worker_count: 1,
                deadline_epoch: chrono::Utc::now().timestamp() + 60,
                marker_dir: directory.join("markers"),
                session: "test".into(),
                stop_file: directory.join("stop"),
                report_dir: directory.to_path_buf(),
                threshold: 95,
                ai_usage: None,
            },
            workers: vec![directory.join("worker-0.sh")],
            state_file: directory.join("state.json"),
            started_at: chrono::Local::now().fixed_offset(),
            targets: Vec::new(),
            save_interrupted: false,
        }
    }

    #[tokio::test]
    async fn startup_preparation_failure_does_not_release_workers() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path();
        let session = test_session(directory);
        fs::write(
            &session.workers[0],
            "while [ ! -f \"$(dirname \"$0\")/start\" ]; do sleep 0.01; done\n",
        )
        .unwrap();
        fs::create_dir(directory.join("monitor.json")).unwrap();
        assert!(supervise(directory, &session).await.is_err());
        assert!(!directory.join("start").exists());
    }

    #[test]
    fn worker_targets_follow_task_numbers_and_clear_on_completion() {
        let temporary = tempfile::tempdir().unwrap();
        let mut session = test_session(temporary.path());
        session.targets = vec!["/work/first".into(), "/work/日本語 project".into()];
        fs::create_dir(&session.monitor.marker_dir).unwrap();
        let marker = session.monitor.marker_dir.join("worker-task-0");
        assert_eq!(current_target(&session, 0, false), None);
        for (task, expected) in [
            ("0001", Some(session.targets[0].clone())),
            ("0002", Some(session.targets[1].clone())),
            ("0", None),
            ("0003", None),
            ("invalid", None),
        ] {
            fs::write(&marker, task).unwrap();
            assert_eq!(current_target(&session, 0, false), expected);
        }
        fs::write(&marker, "0001").unwrap();
        assert_eq!(current_target(&session, 0, true), None);
        assert_eq!(current_target(&session, 1, false), None);
        fs::remove_file(marker).unwrap();
        assert_eq!(current_target(&session, 0, false), None);
    }

    #[test]
    fn full_input_queue_keeps_order_without_blocking_other_controls() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("controls")).unwrap();
        for sequence in 0..256u16 {
            atomic_json(
                &temporary
                    .path()
                    .join(format!("controls/{sequence:04}.json")),
                &Control::Input {
                    worker: 0,
                    bytes: sequence.to_be_bytes().to_vec(),
                },
            )
            .unwrap();
        }
        atomic_json(
            &temporary.path().join("controls/other.json"),
            &Control::Input {
                worker: 1,
                bytes: b"other".to_vec(),
            },
        )
        .unwrap();
        atomic_json(
            &temporary.path().join("controls/resize.json"),
            &Control::Resize {
                worker: 1,
                rows: 17,
                cols: 53,
            },
        )
        .unwrap();
        for index in 0..2 {
            atomic_json(
                &temporary.path().join(format!("controls/stop-{index}.json")),
                &Control::Interrupt,
            )
            .unwrap();
        }
        let (mut attempts, mut other_inputs, mut resizes, mut interrupts) = (0, 0, 0, 0);
        process_controls(temporary.path(), |action| match action {
            Control::Input { worker: 0, .. } => {
                attempts += 1;
                Ok(false)
            }
            Control::Input { worker: 1, bytes } => {
                assert_eq!(bytes, b"other");
                other_inputs += 1;
                Ok(true)
            }
            Control::Resize { .. } => {
                resizes += 1;
                Ok(true)
            }
            Control::Interrupt => {
                interrupts += 1;
                Ok(true)
            }
            _ => panic!("予期しない操作"),
        })
        .unwrap();
        assert_eq!((attempts, other_inputs, resizes, interrupts), (1, 1, 1, 2));
        let mut received = Vec::new();
        for _ in 0..2 {
            process_controls(temporary.path(), |action| {
                let Control::Input { worker: 0, bytes } = action else {
                    panic!("入力だけが残るはず")
                };
                received.push(u16::from_be_bytes(bytes.try_into().unwrap()));
                Ok(true)
            })
            .unwrap();
        }
        assert_eq!(received, (0..256).collect::<Vec<_>>());
        assert_eq!(
            fs::read_dir(temporary.path().join("controls"))
                .unwrap()
                .count(),
            0
        );
    }
}
