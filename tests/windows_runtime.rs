#![cfg(windows)]
//! Exercise the native executable, ConPTY and detached runner with Git for Windows.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn wait(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(45),
            "Windows runner timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn json(path: &Path) -> serde_json::Value {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

struct Run {
    root: tempfile::TempDir,
    config: PathBuf,
}

impl Run {
    fn new(payload: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix("token-burn 日本語 ")
            .tempdir()
            .unwrap();
        let path = root.path();
        fs::create_dir(path.join("first repo")).unwrap();
        let script = path.join("mock agent.sh");
        fs::write(&script, payload).unwrap();
        let bash = std::env::var_os("TOKEN_BURN_BASH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("ProgramFiles").unwrap()).join("Git/bin/bash.exe")
            });
        let q = |path: &Path| serde_json::to_string(&path.to_string_lossy()).unwrap();
        let config = path.join("config.toml");
        fs::write(
            &config,
            format!(
                r#"
[settings]
parallelism = 2
report_dir = {}
[prompts]
default = "Review this repository."
[[agents]]
name = "mock"
command = [{}, {}]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "UTC"
[[targets]]
directory = {}
"#,
                q(&path.join("reports")),
                q(&bash),
                q(&script),
                q(&path.join("first repo"))
            ),
        )
        .unwrap();
        Self { root, config }
    }

    fn start(&self) -> Child {
        let log = fs::File::create(self.root.path().join("client.log")).unwrap();
        Command::new(env!("CARGO_BIN_EXE_token-burn"))
            .args(["--config"])
            .arg(&self.config)
            .arg("run")
            .env("NUMBER_OF_PROCESSORS", "999")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap()
    }

    fn directory(&self) -> PathBuf {
        let path = self.config.with_file_name("active-run.json");
        let mut result = None;
        wait(|| {
            result = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<PathBuf>(&bytes).ok());
            result.is_some()
        });
        result.unwrap().parent().unwrap().to_owned()
    }

    fn control(&self, name: &str, control: serde_json::Value) {
        let dir = self.directory().join("controls");
        let tmp = dir.join(format!("{name}.tmp"));
        fs::write(&tmp, control.to_string()).unwrap();
        fs::rename(tmp, dir.join(format!("{name}.json"))).unwrap();
    }

    fn stop(&self) {
        self.control("stop-1", serde_json::json!("Interrupt"));
        self.control("stop-2", serde_json::json!("Interrupt"));
        wait(|| json(&self.directory().join("snapshot.json"))["finished"] == true);
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let Some(path) = fs::read(self.config.with_file_name("active-run.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PathBuf>(&bytes).ok())
        else {
            return;
        };
        let Some(directory) = path.parent() else {
            return;
        };
        if json(&directory.join("snapshot.json"))["finished"] == true {
            return;
        }
        // Cleanup must also work during unwinding, without hiding the original failure.
        for index in 0..2 {
            let target = directory.join(format!("controls/cleanup-{index}.json"));
            let temporary = target.with_extension("tmp");
            if fs::write(&temporary, b"\"Interrupt\"").is_ok() {
                let _ = fs::rename(temporary, target);
            }
        }
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5)
            && json(&directory.join("snapshot.json"))["finished"] != true
        {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[test]
fn native_workers_run_in_a_pty_with_unicode_and_spaces_in_paths() {
    let run = Run::new(
        "test -t 0 || exit 8\ntest \"$NUMBER_OF_PROCESSORS\" = 999 || exit 9\nprintf 'Windows 日本語\\n'\n",
    );
    let mut client = run.start();
    let mut status = None;
    wait(|| {
        status = client.try_wait().unwrap();
        status.is_some()
    });
    assert!(
        status.unwrap().success(),
        "{}",
        fs::read_to_string(run.root.path().join("client.log")).unwrap()
    );
    let snapshot = json(&run.directory().join("snapshot.json"));
    assert_eq!(snapshot["finished"], true);
    assert!(
        snapshot["summary"][0].as_str().unwrap().contains("All 1/1"),
        "{snapshot}"
    );
    assert_eq!(
        json(&run.root.path().join("state.json"))["mock"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    let reports = || {
        fs::read_dir(run.root.path().join("reports"))
            .unwrap()
            .count()
    };
    let before = reports();
    let mut next = run.start();
    let mut status = None;
    wait(|| {
        status = next.try_wait().unwrap();
        status.is_some()
    });
    assert!(status.unwrap().success());
    assert_eq!(
        reports(),
        before,
        "a processed target must not start a second run"
    );
}

#[test]
fn closing_the_client_keeps_the_runner_alive_and_new_targets_can_be_added() {
    let run = Run::new("stty size > size.txt\necho started\nsleep 3\necho finished\n");
    let mut client = run.start();
    let directory = run.directory();
    wait(|| json(&directory.join("snapshot.json"))["ready"] == true);
    run.control(
        "keep-open",
        serde_json::json!({"AddLease":{"until":chrono::Utc::now().timestamp()+10}}),
    );
    client.kill().unwrap();
    client.wait().unwrap();
    let mut renewed = Instant::now();
    wait(|| {
        if renewed.elapsed() >= Duration::from_secs(1) {
            run.control(
                "keep-open",
                serde_json::json!({"AddLease":{"until":chrono::Utc::now().timestamp()+10}}),
            );
            renewed = Instant::now();
        }
        json(&directory.join("snapshot.json"))["workers"][0]["finished"] == true
    });
    run.control(
        "resize-idle",
        serde_json::json!({"Resize":{"worker":0,"rows":19,"cols":77}}),
    );
    wait(|| {
        json(&directory.join("snapshot.json"))["workers"][0]["lines"]
            .as_array()
            .is_some_and(|lines| lines.len() == 19)
    });
    let extra = run.root.path().join("追加 repo");
    fs::create_dir(&extra).unwrap();
    run.control(
        "append",
        serde_json::json!({"AddTarget":{"id":42, "path":&extra}}),
    );
    wait(|| json(&directory.join("snapshot.json"))["addition"]["id"] == 42);
    assert!(json(&directory.join("snapshot.json"))["addition"]["error"].is_null());
    run.control("release-lease", serde_json::json!({"AddLease":{"until":0}}));
    let output = Command::new(env!("CARGO_BIN_EXE_token-burn"))
        .arg("attach")
        .arg(&directory)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(json(&directory.join("snapshot.json"))["finished"], true);
    assert_eq!(
        fs::read_to_string(extra.join("size.txt"))
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["19", "77"]
    );
    assert_eq!(
        json(&run.root.path().join("state.json"))["mock"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn force_stop_terminates_native_descendants_of_the_bash_worker() {
    let run = Run::new(
        "powershell.exe -NoProfile -Command '$PID | Set-Content -Path native.pid; Start-Sleep -Seconds 120'\n",
    );
    let mut client = run.start();
    let pid_file = run.root.path().join("first repo/native.pid");
    wait(|| pid_file.exists());
    let pid: u32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .trim_start_matches('\u{feff}')
        .parse()
        .unwrap();
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    assert!(!process.is_null());
    run.stop();
    assert_eq!(
        unsafe { WaitForSingleObject(process, 5000) },
        0,
        "native descendant survived forced stop"
    );
    unsafe {
        CloseHandle(process);
    }
    wait(|| client.try_wait().unwrap().is_some());
    assert!(!run.root.path().join("state.json").exists());
}

#[test]
fn input_mode_ctrl_c_reaches_the_agent() {
    let run = Run::new(
        "trap 'echo interrupted > interrupted.txt; exit 130' INT\necho ready > ready.txt\nwhile :; do sleep 0.1; done\n",
    );
    let mut client = run.start();
    let target = run.root.path().join("first repo");
    wait(|| target.join("ready.txt").exists());
    run.control(
        "ctrl-c",
        serde_json::json!({"Input":{"worker":0,"bytes":[3]}}),
    );
    wait(|| target.join("interrupted.txt").exists());
    wait(|| client.try_wait().unwrap().is_some());
    assert_eq!(
        json(&run.directory().join("snapshot.json"))["finished"],
        true
    );
}
