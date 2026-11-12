#![cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    temporary: tempfile::TempDir,
    config: PathBuf,
}
impl Fixture {
    fn new(script: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        for name in ["first", "second"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        let agent = root.join("test-agent");
        fs::write(&agent, format!("#!/bin/bash\n{script}\n")).unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
        let q = |path: PathBuf| serde_json::to_string(&path.to_string_lossy()).unwrap();
        let config = root.join("config.toml");
        fs::write(
            &config,
            format!(
                r#"
[settings]
parallelism = 2
report_dir = {}
[prompts]
default = "test prompt"
[[agents]]
name = "test"
command = [{}]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "UTC"
[[targets]]
directory = {}
[[targets]]
directory = {}
"#,
                q(root.join("reports")),
                q(agent),
                q(root.join("first")),
                q(root.join("second"))
            ),
        )
        .unwrap();
        Self { temporary, config }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_token-burn"));
        command
            .arg("--config")
            .arg(&self.config)
            .arg("run")
            .env("NO_COLOR", "1");
        command
    }
    fn start(&self) -> Child {
        let log = fs::File::create(self.temporary.path().join("client.log")).unwrap();
        self.command()
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap()
    }
    fn directory(&self) -> PathBuf {
        let path = self.config.with_file_name("active-run.json");
        wait_until(|| path.exists());
        let session: PathBuf = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        session.parent().unwrap().to_path_buf()
    }
    fn state(&self) -> serde_json::Value {
        json(&self.directory().join("snapshot.json"))
    }
    fn stop(&self) {
        let directory = self.directory();
        for index in 0..2 {
            fs::write(
                directory
                    .join("controls")
                    .join(format!("stop-{index}.json")),
                "\"Interrupt\"",
            )
            .unwrap();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let path = self.config.with_file_name("active-run.json");
        if let Ok(data) = fs::read(path)
            && let Ok(path) = serde_json::from_slice::<PathBuf>(&data)
        {
            let directory = path.parent().unwrap();
            if json(&directory.join("snapshot.json"))["finished"] != true {
                for index in 0..2 {
                    let _ = fs::write(
                        directory
                            .join("controls")
                            .join(format!("stop-{index}.json")),
                        "\"Interrupt\"",
                    );
                }
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(4) {
                    if json(&directory.join("snapshot.json"))["finished"] == true {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}
fn json(path: &Path) -> serde_json::Value {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "native execution timed out"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn wait_child(child: &mut Child) -> std::process::ExitStatus {
    let mut status = None;
    wait_until(|| {
        status = child.try_wait().unwrap();
        status.is_some()
    });
    status.unwrap()
}

#[test]
fn native_workers_have_ttys_run_in_parallel_and_record_success_without_tmux() {
    let fixture = Fixture::new(
        "test -t 0 || exit 8\ntest -z \"${NO_COLOR+x}\" || exit 9\nprintf '\\033[32mnative 日本語\\033[0m\\n'",
    );
    let mut client = fixture.start();
    assert!(wait_child(&mut client).success());
    let snapshot = fixture.state();
    assert_eq!(snapshot["finished"], true);
    assert_eq!(snapshot["workers"].as_array().unwrap().len(), 2);
    assert!(snapshot["summary"][0].as_str().unwrap().contains("All 2/2"));
    let state = json(&fixture.temporary.path().join("state.json"));
    assert_eq!(state["test"].as_object().unwrap().len(), 2);
    assert!(
        snapshot["workers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|worker| worker["lines"].to_string().contains("native 日本語"))
    );
}

#[test]
fn monitor_shows_the_sessions_actual_model_and_effort() {
    let fixture = Fixture::new(
        r#"
root=$(dirname "$0")
case "$(basename "$PWD")" in
  first) session=018cc251-f400-7000-8000-000000000001 ;;
  *) session=018cc251-f400-7000-8000-000000000002 ;;
esac
mkdir -p "$CODEX_HOME/sessions/2024/01/01"
printf '%s\n' '{"type":"turn_context","payload":{"model":"actual-session-model","effort":"xhigh"}}' > "$CODEX_HOME/sessions/2024/01/01/rollout-example-$session.jsonl"
printf '{"type":"thread.started","thread_id":"%s"}\n' "$session"
while [ ! -e "$root/release" ]; do sleep 0.05; done
"#,
    );
    let root = fixture.temporary.path();
    let settings = fs::read_to_string(&fixture.config).unwrap().replace(
        "name = \"test\"",
        &format!(
            "name = \"test\"\nprovider = \"codex\"\ncodex_output = \"json\"\nenv = {{ CODEX_HOME = {} }}",
            serde_json::to_string(&root.join("config-home").to_string_lossy()).unwrap()
        ),
    );
    fs::write(&fixture.config, settings).unwrap();
    let mut client = fixture.start();
    let directory = fixture.directory();
    wait_until(|| {
        let view = json(&directory.join("monitor-view.json")).to_string();
        view.contains("Model:")
            && view.contains("actual-session-model")
            && view.contains("Effort:")
            && view.contains("xhigh")
    });
    fs::write(root.join("release"), "").unwrap();
    assert!(wait_child(&mut client).success());
}

#[test]
fn failing_tasks_keep_their_actual_error_without_terminal_capture_commands() {
    let fixture = Fixture::new("echo actual-agent-failure >&2\nexit 7");
    let mut client = fixture.start();
    assert!(wait_child(&mut client).success());
    let session = json(&fixture.directory().join("session.json"));
    let report = PathBuf::from(session["monitor"]["report_dir"].as_str().unwrap());
    let errors = fs::read_to_string(report.join("errors.txt")).unwrap();
    assert!(errors.contains("actual-agent-failure"), "{errors}");
    assert!(!fixture.temporary.path().join("state.json").exists());
}

#[test]
fn two_interrupts_force_stop_the_runner_and_its_workers() {
    let fixture = Fixture::new("echo $$ >> \"$0.pids\"\ntrap '' HUP\necho running\nsleep 60");
    let mut client = fixture.start();
    let directory = fixture.directory();
    wait_until(|| {
        fs::read_dir(directory.join("queue")).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("claimed-"))
        })
    });
    let pid_file = fixture.temporary.path().join("test-agent.pids");
    wait_until(|| fs::read_to_string(&pid_file).is_ok_and(|text| text.lines().count() == 2));
    let pids: Vec<i32> = fs::read_to_string(pid_file)
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect();
    fixture.stop();
    assert!(wait_child(&mut client).success());
    assert_eq!(fixture.state()["finished"], true);
    assert!(!fixture.temporary.path().join("state.json").exists());
    wait_until(|| pids.iter().all(|pid| unsafe { libc::kill(*pid, 0) } != 0));
}

#[test]
fn active_run_rejects_duplicate_launch_and_ignores_malformed_controls() {
    let fixture = Fixture::new("echo running\nsleep 60");
    let mut first = fixture.start();
    let directory = fixture.directory();
    fs::write(directory.join("controls/invalid.json"), b"not-json").unwrap();
    wait_until(|| !directory.join("controls/invalid.json").exists());
    assert_eq!(fixture.state()["finished"], false);
    let second = fixture.command().output().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("Another run is already active"));
    fixture.stop();
    assert!(wait_child(&mut first).success());
}

#[test]
fn large_paste_keeps_interrupts_and_resize_working() {
    let fixture = Fixture::new("stty -echo -icanon\necho running\nsleep 60");
    let mut client = fixture.start();
    let directory = fixture.directory();
    for index in 0..64 {
        fs::write(
            directory
                .join("controls")
                .join(format!("input-{index:04}.json")),
            serde_json::to_vec(&serde_json::json!({
                "Input": {"worker": 0, "bytes": vec![b'x'; 4096]}
            }))
            .unwrap(),
        )
        .unwrap();
    }
    fs::write(
        directory.join("controls/resize.json"),
        r#"{"Resize":{"worker":1,"rows":17,"cols":53}}"#,
    )
    .unwrap();
    fixture.stop();
    let start = Instant::now();
    let mut status = None;
    while start.elapsed() < Duration::from_secs(20) {
        status = client.try_wait().unwrap();
        if status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let stopped_by_controls = status.is_some();
    if !stopped_by_controls {
        // 回帰時も runner のシグナル経路で片付け、ワーカーを残さない。
        let pid = json(&directory.join("snapshot.json"))["pid"]
            .as_u64()
            .unwrap() as i32;
        for _ in 0..2 {
            unsafe { libc::kill(pid, libc::SIGINT) };
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    assert!(wait_child(&mut client).success());
    assert!(stopped_by_controls, "入力待ちで停止要求が遮断された");
    let snapshot = fixture.state();
    assert_eq!(snapshot["finished"], true);
    assert_eq!(
        snapshot["workers"][1]["lines"].as_array().unwrap().len(),
        17
    );
}

#[test]
fn errors_file_failure_does_not_discard_the_run_summary() {
    let fixture = Fixture::new("sleep 1\necho task-failure\nexit 1");
    let mut client = fixture.start();
    let directory = fixture.directory();
    fs::create_dir(directory.parent().unwrap().join("errors.txt")).unwrap();
    assert!(wait_child(&mut client).success());
    let snapshot = fixture.state();
    assert_eq!(snapshot["finished"], true);
    assert!(snapshot["failure"].is_null(), "{snapshot}");
    let summary = snapshot["summary"].as_array().unwrap();
    assert!(
        summary
            .iter()
            .any(|line| line.as_str().unwrap().contains("2 failed")),
        "{snapshot}"
    );
    assert!(
        summary
            .iter()
            .any(|line| line.as_str().unwrap().contains("task-failure")),
        "{snapshot}"
    );
    assert!(
        !summary
            .iter()
            .any(|line| line.as_str().unwrap().contains("see errors.txt"))
    );
    let log = fs::read_to_string(directory.join("runner.log")).unwrap();
    assert!(log.contains("errors.txt"), "{log}");
    wait_until(|| !directory.join("tasks").exists() && !directory.join("queue").exists());
}

#[test]
fn default_attach_uses_the_same_directory_for_a_symlinked_config() {
    let mut fixture = Fixture::new("sleep 1\necho completed");
    let linked = fixture.temporary.path().join("linked");
    fs::create_dir(&linked).unwrap();
    std::os::unix::fs::symlink(&fixture.config, linked.join("config.toml")).unwrap();
    fixture.config = linked.join("config.toml");
    let mut client = fixture.start();
    fixture.directory();
    let output = Command::new(env!("CARGO_BIN_EXE_token-burn"))
        .arg("--config")
        .arg(&fixture.config)
        .arg("attach")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("All 2/2 tasks completed"));
    assert!(wait_child(&mut client).success());
}

#[test]
fn snapshot_write_failure_keeps_workers_running_and_retries_publication() {
    let fixture = Fixture::new("echo began\nsleep 3\necho completed");
    let mut client = fixture.start();
    let directory = fixture.directory();
    let snapshot = directory.join("snapshot.json");
    let backup = directory.join("snapshot-backup.json");
    wait_until(|| json(&snapshot)["ready"] == true);
    fs::copy(&snapshot, &backup).unwrap();
    fs::remove_file(&snapshot).unwrap();
    fs::create_dir(&snapshot).unwrap();
    fs::write(
        directory.join("controls/resize.json"),
        r#"{"Resize":{"worker":0,"rows":25,"cols":80}}"#,
    )
    .unwrap();
    wait_until(|| {
        fs::read_to_string(directory.join("runner.log"))
            .is_ok_and(|text| text.contains("snapshot.json"))
    });
    let log = fs::read_to_string(directory.join("runner.log")).unwrap();
    fs::remove_dir(&snapshot).unwrap();
    fs::rename(&backup, &snapshot).unwrap();
    assert!(log.contains("Cannot publish snapshot"), "{log}");
    wait_until(|| json(&snapshot)["finished"] == true);
    let state = json(&snapshot);
    assert!(state["failure"].is_null(), "{state}");
    assert!(
        state["summary"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap().contains("All 2/2 tasks completed")),
        "{state}"
    );
    assert!(wait_child(&mut client).success());
}

#[test]
fn detached_tui_can_reattach_and_wait_for_the_same_run() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let fixture = Fixture::new("echo detached-worker\nsleep 3");
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 28,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_token-burn"));
    command.args(["--config", fixture.config.to_str().unwrap(), "run"]);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let detected = seen.clone();
    let reader_thread = std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        let mut screen = vt100::Parser::new(
            std::num::NonZeroU16::new(28).unwrap(),
            std::num::NonZeroU16::new(100).unwrap(),
            0,
        );
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            screen.process(&buffer[..count]);
            if screen.screen().contents().contains("Ctrl-C: stop") {
                detected.store(true, std::sync::atomic::Ordering::Release);
            }
        }
    });
    let directory = fixture.directory();
    wait_until(|| seen.load(std::sync::atomic::Ordering::Acquire));
    use std::io::Write;
    pair.master.take_writer().unwrap().write_all(b"d").unwrap();
    wait_until(|| child.try_wait().unwrap().is_some());
    assert_ne!(fixture.state()["finished"], true);
    let mut attach = Command::new(env!("CARGO_BIN_EXE_token-burn"))
        .arg("attach")
        .arg(&directory)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_child(&mut attach).success());
    assert_eq!(fixture.state()["finished"], true);
    drop(pair.master);
    reader_thread.join().unwrap();
}

#[test]
fn startup_usage_deadline_cache_redaction_and_pause_are_preserved() {
    let fixture = Fixture::new("echo must-not-start");
    let root = fixture.temporary.path();
    let reset = chrono::Utc::now() + chrono::Duration::hours(2);
    let usage = serde_json::json!({"accounts":[{"profile":"Work","provider":"codex","ok":true,"weekly":{"kind":"weekly","resets_at":reset.to_rfc3339(),"used_percent":20},"five_hour":{"kind":"five_hour","resets_at":(chrono::Utc::now()+chrono::Duration::minutes(10)).to_rfc3339(),"used_percent":97}}]});
    fs::write(root.join("usage.json"), usage.to_string()).unwrap();
    let script = root.join("usage");
    fs::write(
        &script,
        format!(
            "#!/bin/bash\nprintf '%s\\n' \"$*\" >> '{}'\ncat '{}'\n",
            root.join("usage-calls").display(),
            root.join("usage.json").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let original = fs::read_to_string(&fixture.config).unwrap();
    let original = original.replace("name = \"test\"", "name = \"test\"\nprovider = \"codex\"");
    fs::write(&fixture.config,format!("{original}\n[ai_usage]\nenabled=true\ncommand=[{}]\nfallback=\"error\"\n[[ai_usage.profiles]]\nname=\"work\"\nprofile=\"Work\"\n[agents.ai_usage]\nprofiles=[\"work\"]\n",serde_json::to_string(&script.to_string_lossy()).unwrap())).unwrap();
    let mut client = fixture.start();
    let directory = fixture.directory();
    let session = json(&directory.join("session.json"));
    assert_eq!(session["monitor"]["deadline_epoch"], reset.timestamp());
    assert!(directory.join("ai-usage-cache.json").exists());
    let pause = fs::read_to_string(directory.join("stop-pause.json")).unwrap();
    assert!(pause.contains("used_percent=97\n"));
    assert!(pause.contains("source=ai-usage\n"));
    assert!(!fixture.temporary.path().join("state.json").exists());
    fixture.stop();
    assert!(wait_child(&mut client).success());
}
