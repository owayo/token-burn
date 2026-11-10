#![cfg(unix)]

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
