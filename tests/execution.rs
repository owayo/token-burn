#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[test]
fn interactive_opens_the_picker_when_every_project_is_processed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path().join("processed-project");
    std::fs::create_dir(&repo).unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[settings]
parallelism = 1
[prompts]
default = "Review the project."
[[agents]]
name = "test"
command = ["echo"]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "UTC"
[[targets]]
directory = {}
"#,
            serde_json::to_string(&repo.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    let history = serde_json::json!({
        "test": {repo.to_string_lossy().as_ref(): chrono::Utc::now().to_rfc3339()}
    });
    std::fs::write(tmp.path().join("state.json"), history.to_string()).unwrap();

    let run = |interactive: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_token-burn"));
        cmd.arg("--config").arg(&config).args(["run", "--dry-run"]);
        if interactive {
            cmd.arg("-i");
        }
        cmd.output().unwrap()
    };

    let plain = run(false);
    assert!(
        plain.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&plain.stdout),
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(String::from_utf8_lossy(&plain.stdout).contains("All targets already processed"));

    // リダイレクト下では TUI の TTY 検査で止まるはず。履歴で候補が消えると、
    // この検査に達せず「全件処理済み」として成功終了してしまう。
    let interactive = run(true);
    assert!(!interactive.status.success());
    assert!(
        String::from_utf8_lossy(&interactive.stderr).contains("requires a terminal"),
        "{}",
        String::from_utf8_lossy(&interactive.stderr)
    );
}

fn write_executable(path: &Path, script: &str) {
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// 実際の CLI で起動準備を通し、tmux と AI コマンドは実行せず生成物と引数を回収する。
fn prepare_execution() -> (tempfile::TempDir, i64) {
    let reset = chrono::Utc::now() + chrono::Duration::hours(1);
    let usage = serde_json::json!({"accounts": [{
        "profile": "Work", "provider": "codex", "ok": true,
        "weekly": {"resets_at": reset.to_rfc3339(), "used_percent": 10.0}
    }]});
    let (tmp, output) = run_with_usage(&usage);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (tmp, reset.timestamp())
}

/// 偽の ai-usage が `usage` を返す環境で `token-burn run` を通す。偽の ai-usage は受け取った
/// 引数を `capture/ai-usage.log` へ 1 回 1 行で記録し、偽の tmux は new-session の時点の
/// 起動情報・ワーカースクリプト・pause file を `capture/` へ写す。
fn run_with_usage(usage: &serde_json::Value) -> (tempfile::TempDir, std::process::Output) {
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
    cp "$TMPDIR/token-burn/monitor.json" "$TB_CAPTURE/monitor.json"
    cp "$TMPDIR/token-burn/worker-0.sh" "$TB_CAPTURE/worker.sh"
    if [ -f "$TMPDIR/token-burn/stop-pause.json" ]; then
      cp "$TMPDIR/token-burn/stop-pause.json" "$TB_CAPTURE/stop-pause.json"
    fi
    if [ -f "$TMPDIR/token-burn/ai-usage-cache.json" ]; then
      cp "$TMPDIR/token-burn/ai-usage-cache.json" "$TB_CAPTURE/ai-usage-cache.json"
    fi
    ;;
  has-session) exit 1 ;;
esac
exit 0
"#,
    );
    std::fs::write(root.join("usage.json"), usage.to_string()).unwrap();
    write_executable(
        &bin.join("ai-usage"),
        "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$TB_USAGE_ARGS\"\ncat \"$TB_USAGE\"\nsleep 1.1\n",
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
        .env("TB_USAGE_ARGS", capture.join("ai-usage.log"))
        .output()
        .unwrap();
    (tmp, output)
}

#[test]
fn usage_checks_refresh_and_the_startup_fetch_is_reused_for_the_cache() {
    let (tmp, _) = prepare_execution();
    let capture = tmp.path().join("capture");
    // スケジュールの解決と実行開始時の判定は、ai-usage の共有キャッシュを通さずに取り直す。
    // statusline 用キャッシュの初期化は実行開始時の判定の出力を使い、3 回目は取らない。
    let calls = std::fs::read_to_string(capture.join("ai-usage.log")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["--refresh", "--refresh"]
    );
    let cache = std::fs::read_to_string(capture.join("ai-usage-cache.json")).unwrap();
    assert!(cache.contains("\"Work\""), "{cache}");
    // usage-gate は呼ぶたびに取り直すので、キャッシュのパスを渡さない。
    let worker = std::fs::read_to_string(capture.join("worker.sh")).unwrap();
    assert!(worker.contains(" usage-gate "), "{worker}");
    assert!(!worker.contains("--cache-file"), "{worker}");
}

#[test]
fn a_weekly_window_over_the_threshold_starts_nothing() {
    // 週次枠はデッドラインと同じ周期なので、待ってもこの実行の残り時間では回復しない。
    // tmux もファイルも作らずに戻る。
    let reset = chrono::Utc::now() + chrono::Duration::hours(1);
    let usage = serde_json::json!({"accounts": [{
        "profile": "Work", "provider": "codex", "ok": true,
        "weekly": {"kind": "weekly", "resets_at": reset.to_rfc3339(), "used_percent": 96.0}
    }]});
    let (tmp, output) = run_with_usage(&usage);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Stop: weekly 96% >= threshold 95% (ai-usage)"),
        "{stdout}"
    );
    assert!(stdout.contains("Not started"), "{stdout}");
    let calls = std::fs::read_to_string(tmp.path().join("capture/tmux.log")).unwrap_or_default();
    assert!(calls.is_empty(), "tmux を呼んではいけない: {calls}");
    assert!(
        !tmp.path().join("token-burn").exists(),
        "実行用の一時ディレクトリを作ってはいけない"
    );
    assert!(
        !tmp.path().join("reports").exists(),
        "レポートディレクトリを作ってはいけない"
    );
}

#[test]
fn a_five_hour_window_over_the_threshold_starts_paused() {
    // 5 時間枠が閾値に触れていれば、その枠のリセットまで一時停止した状態で始める。
    // 根拠を pause file に残し、モニターが手動リセット等での回復を確かめられるようにする。
    let now = chrono::Utc::now();
    let weekly_reset = now + chrono::Duration::hours(30);
    let five_hour_reset = now + chrono::Duration::minutes(30);
    let usage = serde_json::json!({"accounts": [{
        "profile": "Work", "provider": "codex", "ok": true,
        "weekly": {"kind": "weekly", "resets_at": weekly_reset.to_rfc3339(), "used_percent": 20.0},
        "five_hour": {"kind": "five_hour", "resets_at": five_hour_reset.to_rfc3339(), "used_percent": 97.0}
    }]});
    let (tmp, output) = run_with_usage(&usage);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Hold: five_hour 97% >= threshold 95% (ai-usage)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("the monitor resumes them earlier"),
        "{stdout}"
    );
    let pause = std::fs::read_to_string(tmp.path().join("capture/stop-pause.json")).unwrap();
    assert!(
        pause.contains(&format!("resume_at={}\n", five_hour_reset.timestamp() + 30)),
        "{pause}"
    );
    assert!(pause.contains("source=ai-usage\n"), "{pause}");
    assert!(pause.contains("cause=threshold\n"), "{pause}");
    assert!(pause.contains("used_percent=97\n"), "{pause}");
}

#[test]
fn startup_delay_does_not_extend_reset_deadline() {
    let (tmp, reset) = prepare_execution();
    let monitor: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("capture/monitor.json")).unwrap(),
    )
    .unwrap();
    let worker = std::fs::read_to_string(tmp.path().join("capture/worker.sh")).unwrap();
    assert_eq!(
        monitor["deadline_epoch"],
        serde_json::json!(reset),
        "モニターは元のリセット時刻で止める"
    );
    // モニターペインは `token-burn monitor` を起動し、起動情報はファイルで渡す。
    let calls = std::fs::read_to_string(tmp.path().join("capture/tmux.log")).unwrap();
    let new_session = calls
        .lines()
        .find(|line| line.starts_with("new-session"))
        .unwrap();
    assert!(
        new_session.contains(" monitor --manifest "),
        "{new_session}"
    );
    // ディスク容量は使用量のキャッシュとは別に `--disk --json` で取る。
    let disk = monitor["ai_usage"]["disk"].as_str().unwrap();
    assert!(disk.ends_with("'--disk' '--json'"), "{disk}");
    assert!(
        worker.contains(&format!("--deadline-epoch {reset}")),
        "起動準備にかかった時間を期限へ加えてはいけない"
    );
}

#[test]
fn monitor_redacts_authentication_but_keeps_model_settings() {
    let (tmp, _) = prepare_execution();
    let monitor = std::fs::read_to_string(tmp.path().join("capture/monitor.json")).unwrap();
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
