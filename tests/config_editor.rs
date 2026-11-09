use std::process::Command;

#[test]
fn config_requires_a_terminal_before_reading_or_creating_files() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("not-created/config.toml");
    for args in [vec!["config", "--config"], vec!["--config"]] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_token-burn"));
        command.args(&args).arg(&path);
        if args.len() == 1 {
            command.arg("config");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("config requires a terminal"));
        assert!(!path.parent().unwrap().exists());
    }
}
