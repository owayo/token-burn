#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn write_executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// 実際の CLI で起動準備を通し、tmux と AI コマンドは実行せず生成物と引数を回収する。
fn prepare_execution() -> (tempfile::TempDir, i64) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let bin = root.join("bin");
    let capture = root.join("capture");
    for dir in [&bin, &capture, &root.join("repo-1"), &root.join("repo-2")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    write_executable(
        &bin.join("tmux"),
        r#"#!/bin/bash
printf '%s\n' "$*" >> "$TB_CALLS"
case "$1" in
  -V) echo 'tmux 3.6' ;;
  new-session)
    cp "$TMPDIR/token-burn/monitor.sh" "$TB_CAPTURE/monitor.sh"
    cp "$TMPDIR/token-burn/worker-0.sh" "$TB_CAPTURE/worker.sh"
    ;;
  has-session) exit 1 ;;
esac
exit 0
"#,
    );
    let reset = chrono::Utc::now() + chrono::Duration::hours(1);
    let usage = serde_json::json!({"accounts": [{
        "profile": "Work", "provider": "codex", "ok": true,
        "weekly": {"resets_at": reset.to_rfc3339(), "used_percent": 10.0}
    }]});
    std::fs::write(root.join("usage.json"), usage.to_string()).unwrap();
    write_executable(
        &bin.join("ai-usage"),
        "#!/bin/bash\ncat \"$TB_USAGE\"\nsleep 1.1\n",
    );
    let quote = |p: &Path| serde_json::to_string(&p.to_string_lossy()).unwrap();
    let config = format!(
        r#"
[settings]
parallelism = 2
report_dir = {reports}
[prompts]
default = "Review the project."
[ai_usage]
enabled = true
command = [{usage}]
fallback = "error"
[[ai_usage.profiles]]
name = "work"
profile = "Work"
[[agents]]
name = "test"
provider = "codex"
command = ["env", "-u", "UNUSED", "API_TOKEN=secret-env", "codex", "exec", "--api-key", "secret-option", "-c", "model=test-model"]
[agents.ai_usage]
profiles = ["work"]
[[targets]]
directory = {target}
"#,
        reports = quote(&root.join("reports")),
        usage = quote(&bin.join("ai-usage")),
        target = quote(&root.join("repo-1"))
    );
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, config).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_token-burn"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--workers",
            "2",
            "run",
        ])
        .args([root.join("repo-1"), root.join("repo-2")])
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("TMPDIR", root)
        .env("TB_CALLS", capture.join("tmux.log"))
        .env("TB_CAPTURE", &capture)
        .env("TB_USAGE", root.join("usage.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (tmp, reset.timestamp())
}

#[test]
fn startup_delay_does_not_extend_reset_deadline() {
    let (tmp, reset) = prepare_execution();
    let monitor = std::fs::read_to_string(tmp.path().join("capture/monitor.sh")).unwrap();
    let worker = std::fs::read_to_string(tmp.path().join("capture/worker.sh")).unwrap();
    assert!(
        monitor.lines().any(|line| line == format!("END={reset}")),
        "モニターは元のリセット時刻で止める"
    );
    assert!(
        worker.contains(&format!("--deadline-epoch {reset}")),
        "起動準備にかかった時間を期限へ加えてはいけない"
    );
}

#[test]
fn monitor_redacts_authentication_but_keeps_model_settings() {
    let (tmp, _) = prepare_execution();
    let monitor = std::fs::read_to_string(tmp.path().join("capture/monitor.sh")).unwrap();
    assert!(!monitor.contains("secret-env"));
    assert!(!monitor.contains("secret-option"));
    assert!(monitor.contains("<redacted>"));
    assert!(monitor.contains("model=test-model"));
}

#[test]
fn all_tmux_targets_match_only_the_owned_session() {
    let (tmp, _) = prepare_execution();
    let calls = std::fs::read_to_string(tmp.path().join("capture/tmux.log")).unwrap();
    let mut targets = 0;
    for call in calls.lines() {
        let args: Vec<_> = call.split_whitespace().collect();
        for pair in args.windows(2) {
            if pair[0] == "-t" {
                targets += 1;
                assert!(
                    pair[1] == "=token-burn" || pair[1].starts_with("=token-burn:"),
                    "曖昧な対象: {call}"
                );
            }
        }
    }
    assert!(targets >= 10, "接続・分割・設定・終了判定を検査する");
}

#[test]
fn tmux_window_targets_work_without_touching_similar_session() {
    if !Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|out| out.status.success())
    {
        return;
    }
    let (tmp, _) = prepare_execution();
    struct Server(std::path::PathBuf);
    impl Server {
        fn run(&self, args: &[&str]) -> std::process::Output {
            Command::new("tmux")
                .arg("-S")
                .arg(&self.0)
                .args(["-f", "/dev/null"])
                .args(args)
                .output()
                .unwrap()
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.run(&["kill-server"]);
        }
    }
    let server = Server(tmp.path().join("tmux.sock"));
    for name in ["token-burn-notes", "token-burn"] {
        let out = server.run(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "120",
            "-y",
            "40",
            "sleep 30",
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let calls = std::fs::read_to_string(tmp.path().join("capture/tmux.log")).unwrap();
    for call in calls.lines() {
        let mut args: Vec<_> = call.split_whitespace().collect();
        if !matches!(
            args[0],
            "split-window" | "select-layout" | "resize-pane" | "set-option" | "select-pane"
        ) {
            continue;
        }
        if args[0] == "split-window" {
            // 本物の AI コマンドやモニターを起動せず、生成された対象指定だけを検査する。
            *args.last_mut().unwrap() = "sleep 30";
            args.insert(1, "-d");
        } else if args[0] == "set-option" && call.contains("pane-border-format") {
            // ログでは引用符を保存していないため、書式文字列を引数 1 個へ戻す。
            args.truncate(3);
            args.push("pane-border-format");
            args.push(" #{pane_title} ");
        }
        let out = server.run(&args);
        assert!(
            out.status.success(),
            "{call}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let notes = server.run(&["list-panes", "-t", "=token-burn-notes:"]);
    assert_eq!(String::from_utf8_lossy(&notes.stdout).lines().count(), 1);
    let owned = server.run(&["list-panes", "-t", "=token-burn:"]);
    assert_eq!(String::from_utf8_lossy(&owned.stdout).lines().count(), 3);
    assert!(
        server
            .run(&["kill-session", "-t", "=token-burn"])
            .status
            .success()
    );
    // 目的のセッションが終了した後でも前方一致で別セッションへ分割してはいけない。
    assert!(
        !server
            .run(&["split-window", "-d", "-t", "=token-burn:", "sleep 30"])
            .status
            .success()
    );
    let notes = server.run(&["list-panes", "-t", "=token-burn-notes:"]);
    assert_eq!(String::from_utf8_lossy(&notes.stdout).lines().count(), 1);
}
