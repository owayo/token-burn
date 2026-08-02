use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;
use std::time::Duration;

use crate::config::RuntimeAgent;
use crate::display;
use crate::scanner::{ResolvedTarget, Visibility};

mod flags;
mod scripts;
mod util;

use flags::{agent_is_claude, ensure_required_flags};
use scripts::{
    TaskCtx, WorkerCtx, build_refresh_cmd, build_statusline_cmd, build_task_script,
    build_worker_script, env_prefix_parts, generate_monitor_script, shell_escape,
};
use util::{sanitize_filename, strip_ansi_from_dir, truncate};

/// 起動時 ai-usage キャッシュ初期化のハング検知タイムアウト。
/// これを超えたら子プロセスを SIGKILL してキャッシュ初期化をスキップし、
/// monitor 起動を進める。tmux 起動前に token-burn 全体が固まるのを防ぐ。
const AI_USAGE_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// monitor ペイン内で `eval $AI_USAGE_CMD` / `eval $AI_USAGE_REFRESH_CMD` を包む
/// タイムアウト（秒）。10 秒ループ内で 1 回でもハングすると進捗バーの `\r` 更新も
/// 止まって「表示が止まった」ように見えるため、必ずキルする。
const AI_USAGE_MONITOR_TIMEOUT_SECS: u64 = 30;

pub struct ExecutionPlan {
    pub agent: RuntimeAgent,
    pub tasks: Vec<ResolvedTarget>,
    pub usage_gate: Option<UsageGateConfig>,
}

/// ai-usage 使用率ゲートの設定（各タスク完了後にワーカーが実行）。
pub struct UsageGateConfig {
    pub profile: String,
    pub provider: String,
    /// ai-usage を起動するコマンド（例: ["ai-usage", "--json"]）。
    pub command: Vec<String>,
    /// 選択 agent の env（CLAUDE_CONFIG_DIR 等）。usage-gate / monitor statusline /
    /// 起動時キャッシュ初期化を、その agent の実行文脈で動かすために使う。
    /// これにより起動シェルの環境継承で別アカウントの使用率を見るズレを防ぐ。
    pub env: std::collections::BTreeMap<String, String>,
}

pub fn build_plan(
    agent: &RuntimeAgent,
    targets: Vec<ResolvedTarget>,
    ai_usage_command: Option<Vec<String>>,
) -> ExecutionPlan {
    let mut agent = agent.clone();
    // usage-gate / monitor statusline に渡す env は、ユーザーが設定した実行文脈
    // （CLAUDE_CONFIG_DIR 等）のスナップショット。ensure_required_flags が注入する
    // claude 専用のデフォルト env（CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS）を
    // ai-usage 起動にまで持ち込まないよう、注入前に取得する。
    let gate_env = agent.env.clone();
    ensure_required_flags(&mut agent);
    // ai-usage 連携 agent かつグローバルで ai-usage が有効なときだけゲートを設定する。
    let usage_gate = match (agent.ai_usage.as_ref(), ai_usage_command) {
        (Some(rt), Some(command)) => Some(UsageGateConfig {
            profile: rt.profile.clone(),
            provider: rt.provider.clone(),
            command,
            env: gate_env,
        }),
        _ => None,
    };
    ExecutionPlan {
        agent,
        tasks: targets,
        usage_gate,
    }
}

/// 同期パスで ai-usage を起動し、`timeout` を超えたら SIGKILL してキルする。
/// tokio ランタイムに依存せず std::process::Command と try_wait ポーリングで実現する。
///
/// 成功時は raw stdout を返す。stdout/stderr は別スレッドで並行に drain するため、
/// 出力サイズがパイプバッファを超えてもデッドロックしない。非ゼロ終了・spawn 失敗・
/// タイムアウトはエラー。呼び出し側で fail-soft（キャッシュ初期化スキップ）にする。
fn spawn_ai_usage_sync_with_timeout(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    anyhow::ensure!(!command.is_empty(), "ai_usage.command is empty");
    let mut child = std::process::Command::new(&command[0])
        .args(&command[1..])
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn ai-usage command: {}",
                crate::display::format_command(command)
            )
        })?;

    // stdout/stderr は必ず別スレッドで並行に drain する。子の終了を待ってから読むと、
    // 出力がパイプバッファ（macOS では 16KB 程度）を超えたとき子の write(2) がブロックして
    // 終了できず、try_wait が永遠に None を返してタイムアウトまでハングするデッドロックに
    // 陥る（大きな stdout や stderr へのログ出力で発生）。読み取りを終了監視から分離する。
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                // 子は終了済みでパイプは EOF に達するため、join はブロックせず全出力を回収できる。
                let stdout_buf = stdout_handle.join().unwrap_or_default();
                let stderr_buf = stderr_handle.join().unwrap_or_default();
                if !status.success() {
                    let stderr = String::from_utf8_lossy(&stderr_buf);
                    anyhow::bail!("ai-usage exited with {}: {}", status, stderr.trim());
                }
                return Ok(stdout_buf);
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    // kill でパイプが閉じ drain スレッドは EOF で終了する。join してリークを防ぐ。
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    anyhow::bail!(
                        "ai-usage timed out after {}s: {}",
                        timeout.as_secs(),
                        crate::display::format_command(command)
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// 指定したパスのファイルに実行ビットを付与する。
/// `chmod` の非ゼロ終了は無視されないことを保証し、tmux ワーカー起動時の
/// 「permission denied」を未然に検知できるようにする。
fn ensure_executable(path: &Path) -> Result<()> {
    let output = std::process::Command::new("chmod")
        .args(["+x", &path.to_string_lossy()])
        .output()
        .with_context(|| format!("Failed to run chmod on {}", path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("chmod +x {} failed: {}", path.display(), stderr.trim());
    }
    Ok(())
}

pub fn print_plan(plan: &ExecutionPlan) {
    println!("{}", "=== Execution Plan ===".bold());
    println!("Agent: {}", plan.agent.name.cyan());
    println!(
        "Command: {}",
        crate::display::format_command(&plan.agent.command).dimmed()
    );
    println!("Tasks: {}", plan.tasks.len());
    println!();
    for (i, task) in plan.tasks.iter().enumerate() {
        let vis = format!("[{}]", task.visibility);
        let vis_colored = match task.visibility {
            Visibility::Public => vis.green(),
            Visibility::Private => vis.yellow(),
            Visibility::Unknown => vis.dimmed(),
        };
        println!(
            "  {} {} {}",
            format!("[{}]", i + 1).yellow(),
            vis_colored,
            task.display_name
        );
        println!(
            "      Path:   {}",
            task.directory.display().to_string().dimmed()
        );
        println!("      Prompt: {}", truncate(&task.prompt, 60).dimmed());
    }
    println!();
}

/// 実行用の一時ディレクトリを、必ず自分が作った空のディレクトリとして用意する。
///
/// 旧実装は `let _ = remove_dir_all(...)` で削除失敗を握り潰していたため、
/// 消せなかったディレクトリをそのまま再利用していた。`temp_dir()` が共有の
/// `/tmp` になる環境（Linux。macOS は `TMPDIR` がユーザーごと）では、
/// 他ユーザーが先に `/tmp/token-burn` を作っておくと sticky bit により削除が
/// 失敗する一方 `create_dir_all` は成功するため、他人の所有ディレクトリへ
/// ワーカースクリプトやプロンプトを書き込んでしまう。削除失敗は
/// （存在しない場合を除き）エラーとして扱い、作成後は所有者のみアクセス可にする。
fn prepare_run_tmp_dir(tmp_dir: &std::path::Path) -> Result<()> {
    match std::fs::remove_dir_all(tmp_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "failed to clear the temporary run directory {}",
                tmp_dir.display()
            )));
        }
    }
    std::fs::create_dir_all(tmp_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn execute_plan_tmux(
    plan: ExecutionPlan,
    parallelism: usize,
    deadline: Duration,
    state_file: &std::path::Path,
    reset_info: &str,
    report_dir: &std::path::Path,
    rate_limit_threshold: u8,
) -> Result<()> {
    anyhow::ensure!(!plan.tasks.is_empty(), "No tasks to execute");

    // tmux の存在確認
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .context("tmux is not installed")?;

    let session = "token-burn";

    // 既存セッションがあれば終了
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", session])
        .output();

    let tmp_dir = std::env::temp_dir().join("token-burn");
    prepare_run_tmp_dir(&tmp_dir)?;

    // 今回の実行用レポートディレクトリを作成
    let now = chrono::Local::now();
    let safe_agent_name = sanitize_filename(&plan.agent.name);
    let run_dir = report_dir.join(format!(
        "{}_{}",
        now.format("%Y%m%d_%H%M%S"),
        safe_agent_name
    ));
    std::fs::create_dir_all(&run_dir)?;

    let total = plan.tasks.len();
    let worker_count = parallelism.min(total);

    // ワーカー間で共有するタスクキュー
    let marker_dir = tmp_dir.join("markers");
    std::fs::create_dir_all(&marker_dir)?;
    let queue_dir = tmp_dir.join("queue");
    std::fs::create_dir_all(&queue_dir)?;
    let task_dir = tmp_dir.join("tasks");
    std::fs::create_dir_all(&task_dir)?;

    let exe_path =
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("token-burn"));
    let stop_file = tmp_dir.join("stop");
    let is_claude = agent_is_claude(&plan.agent);

    // 各タスクの実行スクリプトと pending マーカーを書き出す
    for (idx_zero, task) in plan.tasks.iter().enumerate() {
        let idx = idx_zero + 1;
        let prompt_file = tmp_dir.join(format!("prompt-{}.txt", idx));
        std::fs::write(&prompt_file, &task.prompt)?;

        let task_script = build_task_script(&TaskCtx {
            idx,
            total,
            task,
            agent: &plan.agent,
            prompt_file: &prompt_file,
            run_dir: &run_dir,
            marker_dir: &marker_dir,
            exe_path: &exe_path,
            state_file,
            stop_file: &stop_file,
            rate_limit_threshold,
            is_claude,
        });
        let task_path = task_dir.join(format!("task-{:04}.sh", idx));
        std::fs::write(&task_path, &task_script)?;

        std::fs::write(queue_dir.join(format!("pending-{:04}", idx)), "")?;
    }

    // ai-usage 連携時、各タスク完了後に使用率をチェックする usage-gate コマンドを組み立てる。
    let cache_file = tmp_dir.join("ai-usage-cache.json");
    let usage_gate_cmd = plan.usage_gate.as_ref().map(|g| {
        // usage-gate プロセス自体に選択 agent の env を前置きする。`--` 以降に KEY=val を
        // 入れると hidden CLI が実行ファイルと誤認するため、サブコマンドより前に置く。
        let mut parts = env_prefix_parts(&g.env);
        parts.extend([
            shell_escape(&exe_path.to_string_lossy()),
            "usage-gate".to_string(),
            "--profile".to_string(),
            shell_escape(&g.profile),
            "--provider".to_string(),
            shell_escape(&g.provider),
            "--threshold".to_string(),
            rate_limit_threshold.to_string(),
            "--stop-file".to_string(),
            shell_escape(&stop_file.to_string_lossy()),
            "--cache-file".to_string(),
            shell_escape(&cache_file.to_string_lossy()),
            "--".to_string(),
        ]);
        parts.extend(g.command.iter().map(|s| shell_escape(s)));
        parts.join(" ")
    });

    // monitor ペインに表示する ai-usage statusline コマンド（--input でキャッシュから高速描画）。
    let usage_statusline_cmd = plan.usage_gate.as_ref().and_then(|g| {
        build_statusline_cmd(&g.command, &cache_file, &g.env, &g.profile, &g.provider)
    });
    // monitor の 10 秒ループで使う ai-usage --json コマンド（env prefix 付き）。
    // 取得結果を `--input` で読む statusline と同じキャッシュファイルへ atomic に
    // 書き戻すため、usage-gate も含めて表示・判定が同じ値で同期する。
    let usage_refresh_cmd = plan
        .usage_gate
        .as_ref()
        .and_then(|g| build_refresh_cmd(&g.command, &g.env));
    // 起動時に ai-usage キャッシュを初期化し、monitor 起動直後から statusline を表示できるようにする
    // （以後は usage-gate が各タスク完了時に 20 秒 TTL で更新する）。
    // ai-usage コマンド自体がハングすると tmux 起動前に token-burn 全体が固まるため、
    // AI_USAGE_STARTUP_TIMEOUT でキルする。失敗時はキャッシュ更新をスキップし、
    // monitor 側の初回 fetch_usage に委ねる。
    if let Some(g) = plan.usage_gate.as_ref() {
        match spawn_ai_usage_sync_with_timeout(&g.command, &g.env, AI_USAGE_STARTUP_TIMEOUT) {
            Ok(bytes) => {
                let _ = std::fs::write(&cache_file, &bytes);
            }
            Err(e) => {
                eprintln!(
                    "{}: failed to initialize ai-usage cache: {} (continuing without startup cache)",
                    "warning".yellow(),
                    e
                );
            }
        }
    }

    let mut script_paths = Vec::new();
    for w in 0..worker_count {
        let script_path = tmp_dir.join(format!("worker-{}.sh", w));
        let worker_script = build_worker_script(&WorkerCtx {
            worker_id: w,
            queue_dir: &queue_dir,
            task_dir: &task_dir,
            marker_dir: &marker_dir,
            stop_file: &stop_file,
            usage_gate_cmd: usage_gate_cmd.as_deref(),
        });
        std::fs::write(&script_path, &worker_script)?;
        ensure_executable(&script_path)?;
        script_paths.push(script_path);
    }

    // 左ペイン用モニタースクリプトを生成
    let monitor_path = tmp_dir.join("monitor.sh");
    let command_str = plan.agent.command.join(" ");
    let monitor_script = generate_monitor_script(
        &plan.agent.name,
        &command_str,
        reset_info,
        total,
        deadline.as_secs(),
        &marker_dir,
        session,
        worker_count,
        &stop_file,
        &run_dir,
        usage_statusline_cmd.as_deref(),
        usage_refresh_cmd.as_deref(),
        plan.usage_gate.as_ref().map(|_| cache_file.as_path()),
    );
    std::fs::write(&monitor_path, &monitor_script)?;
    ensure_executable(&monitor_path)?;

    // モニター（左ペイン）付き tmux セッションを作成。
    // `.status()?` は tmux の起動失敗しか捕まえない（tmux が non-zero 終了しても Ok）。
    // ここで ExitStatus を検証しないと、セッション/ペイン生成に失敗してもワーカーが
    // 起動しないままモニターだけが走り、進捗が進まずデッドラインまでハングする。
    let mut session_created = false;
    let setup_result = (|| -> Result<()> {
        let new_session_status = std::process::Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                session,
                &monitor_path.to_string_lossy(),
            ])
            .status()
            .context("Failed to create tmux session")?;
        anyhow::ensure!(
            new_session_status.success(),
            "tmux new-session failed (exit {:?})",
            new_session_status.code()
        );
        session_created = true;

        // 最初のワーカー用に右ペインを分割
        let split_status = std::process::Command::new("tmux")
            .args([
                "split-window",
                "-h",
                "-t",
                session,
                &script_paths[0].to_string_lossy(),
            ])
            .status()
            .context("Failed to split tmux window for worker")?;
        anyhow::ensure!(
            split_status.success(),
            "tmux split-window failed (exit {:?})",
            split_status.code()
        );

        // 残りのワーカーを右エリアに垂直分割で追加
        for script in &script_paths[1..] {
            // 右側の最後のペインに垂直分割で追加
            let split_status = std::process::Command::new("tmux")
                .args([
                    "split-window",
                    "-v",
                    "-t",
                    &format!("{}:.right", session),
                    &script.to_string_lossy(),
                ])
                .status()
                .context("Failed to split tmux window for worker")?;
            anyhow::ensure!(
                split_status.success(),
                "tmux split-window failed (exit {:?})",
                split_status.code()
            );
        }
        Ok(())
    })();

    if let Err(error) = setup_result {
        if session_created {
            // セッション作成後にペイン初期化が失敗した場合、モニターだけが残って
            // デッドラインまで動き続けないよう、自分で起動したセッションを回収する。
            let _ = std::process::Command::new("tmux")
                .args(["kill-session", "-t", session])
                .status();
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(error);
    }

    // 右側ペインのサイズを均等化
    let _ = std::process::Command::new("tmux")
        .args(["select-layout", "-t", session, "main-vertical"])
        .status();

    // 左ペイン（モニター）の幅を約30%に設定
    let _ = std::process::Command::new("tmux")
        .args(["resize-pane", "-t", &format!("{}:.0", session), "-x", "35%"])
        .status();

    // スクロールバックとマウスサポートを有効化
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-t", session, "history-limit", "50000"])
        .status();
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-t", session, "mouse", "on"])
        .status();

    // ペインボーダータイトルを有効化
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-t", session, "pane-border-status", "top"])
        .status();
    let _ = std::process::Command::new("tmux")
        .args([
            "set-option",
            "-t",
            session,
            "pane-border-format",
            " #{pane_title} ",
        ])
        .status();

    // モニターペインにフォーカス
    let _ = std::process::Command::new("tmux")
        .args(["select-pane", "-t", &format!("{}:.0", session)])
        .status();

    println!(
        "{} {} (deadline: {})",
        "Attached to tmux session:".bold(),
        session.cyan(),
        display::format_duration(deadline).red()
    );
    println!(
        "  {}",
        "Detach: Ctrl-b d | Ctrl-C in monitor pane to abort".dimmed()
    );

    // セッションに接続（終了またはkillされるまでブロック）
    std::process::Command::new("tmux")
        .args(["attach-session", "-t", session])
        .status()
        .context("Failed to attach to tmux session")?;

    let session_alive = std::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if session_alive {
        println!();
        println!(
            "{} {}",
            "Detached from tmux session:".bold(),
            session.cyan()
        );
        println!("  {} tmux attach -t {}", "Reattach:".dimmed(), session);
        println!(
            "  {} {}",
            "Runtime files kept:".dimmed(),
            tmp_dir.display().to_string().cyan()
        );
        return Ok(());
    }

    // クリーンアップ
    let _ = std::fs::remove_dir_all(&tmp_dir);

    // ログファイルから ANSI エスケープコードを除去
    strip_ansi_from_dir(&run_dir);

    println!();
    println!("{}", "tmux session ended.".bold());
    println!(
        "  {} {}",
        "Logs:".dimmed(),
        run_dir.display().to_string().cyan()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::flags::CLAUDE_PRINT_BG_WAIT_ENV;
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ensure_executable_adds_execute_permission() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        let path = tmp.path().join("script.sh");
        std::fs::write(&path, "#!/bin/sh\n").expect("テスト用スクリプトを書き込めるべき");
        let mut permissions = std::fs::metadata(&path)
            .expect("テスト用スクリプトのメタデータを取得できるべき")
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&path, permissions).expect("実行前の権限を設定できるべき");

        ensure_executable(&path).expect("実行権限を付与できるべき");

        let mode = std::fs::metadata(&path)
            .expect("実行後のメタデータを取得できるべき")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_executable_reports_missing_path() {
        let tmp = tempfile::TempDir::new().expect("一時ディレクトリを作成できるべき");
        let missing = tmp.path().join("missing-script.sh");

        let error = ensure_executable(&missing).expect_err("存在しないパスは失敗するべき");

        assert!(error.to_string().contains("chmod +x"));
    }

    #[test]
    fn spawn_ai_usage_sync_redacts_secret_on_spawn_failure() {
        let secret = "actual-secret-value";
        let command = vec![
            "token-burn-command-that-does-not-exist".to_string(),
            "--api-key".to_string(),
            secret.to_string(),
        ];

        let error = spawn_ai_usage_sync_with_timeout(
            &command,
            &std::collections::BTreeMap::new(),
            Duration::from_secs(1),
        )
        .expect_err("存在しないコマンドの起動は失敗するべき");
        let message = error.to_string();

        assert!(message.contains("--api-key <redacted>"));
        assert!(!message.contains(secret));
    }

    #[test]
    fn spawn_ai_usage_sync_kills_hanging_child_within_timeout() {
        // ハングする子プロセス（sleep 60）を 1 秒でキルできることを確認する。
        // タイムアウトなしだと token-burn 起動が固まる回帰を検知するためのテスト。
        let start = std::time::Instant::now();
        let result = spawn_ai_usage_sync_with_timeout(
            &["sleep".to_string(), "60".to_string()],
            &Default::default(),
            Duration::from_secs(1),
        );
        let elapsed = start.elapsed();
        assert!(result.is_err(), "hanging child should return Err");
        let msg = format!("{}", result.err().unwrap());
        assert!(
            msg.contains("timed out"),
            "error should mention timeout, got: {msg}"
        );
        // 1 秒タイムアウト + 50ms ポーリング粒度で最大 ~1.5s まで許容。
        assert!(
            elapsed < Duration::from_secs(3),
            "timeout should fire promptly (took {elapsed:?})"
        );
    }

    #[test]
    fn spawn_ai_usage_sync_returns_stdout_for_fast_command() {
        // 早く終わるコマンドはタイムアウト前に stdout を返す。
        let result = spawn_ai_usage_sync_with_timeout(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf 'hello world'".to_string(),
            ],
            &Default::default(),
            Duration::from_secs(5),
        );
        let bytes = result.expect("should succeed");
        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn spawn_ai_usage_sync_handles_output_larger_than_pipe_buffer() {
        // stdout がパイプバッファ（macOS で ~16KB）を大きく超えても、別スレッドで並行に
        // drain するためデッドロックせず全出力を返す。旧実装（子の終了後にまとめて読む）では
        // 子が write(2) でブロックして終了できず、タイムアウトまでハングする回帰を検知する。
        let size = 100_000;
        let result = spawn_ai_usage_sync_with_timeout(
            &[
                "sh".to_string(),
                "-c".to_string(),
                format!("head -c {size} /dev/zero"),
            ],
            &Default::default(),
            Duration::from_secs(5),
        );
        let bytes = result.expect("large stdout should not deadlock");
        assert_eq!(bytes.len(), size, "全出力が回収されるべき");
    }

    fn make_agent(command: Vec<&str>) -> RuntimeAgent {
        RuntimeAgent {
            name: "claude".to_string(),
            command: command.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn build_plan_clones_agent_and_targets() {
        let agent = make_agent(vec!["claude", "-p"]);
        let targets = vec![ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        }];
        let plan = build_plan(&agent, targets, None);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].display_name, "repo");
        // claude エージェントにはフラグが自動付与される
        assert!(plan.agent.command.contains(&"--verbose".to_string()));
        assert!(
            plan.agent
                .command
                .contains(&"--disallowedTools=AskUserQuestion".to_string())
        );
    }

    #[test]
    fn build_plan_usage_gate_env_excludes_injected_bg_wait() {
        let mut agent = make_agent(vec!["claude", "-p"]);
        agent.ai_usage = Some(crate::config::RuntimeAiUsage {
            profile: "Work".to_string(),
            provider: "claude".to_string(),
        });
        agent.env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/home/u/.claude".to_string(),
        );
        let plan = build_plan(
            &agent,
            vec![],
            Some(vec!["ai-usage".to_string(), "--json".to_string()]),
        );
        let gate = plan.usage_gate.expect("usage_gate should be set");
        // ユーザー設定の env は usage-gate に引き継ぐが、claude 専用の注入 env は含めない
        assert_eq!(
            gate.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/home/u/.claude")
        );
        assert!(!gate.env.contains_key(CLAUDE_PRINT_BG_WAIT_ENV));
        // タスク実行側の env には注入済み
        assert_eq!(
            plan.agent
                .env
                .get(CLAUDE_PRINT_BG_WAIT_ENV)
                .map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn prepare_run_tmp_dir_creates_missing_directory() {
        // 存在しない場合の削除失敗（NotFound）は正常系として無視し、作成まで到達すること。
        let parent = tempfile::TempDir::new().expect("temp dir should be created");
        let target = parent.path().join("run");
        assert!(!target.exists());

        prepare_run_tmp_dir(&target).expect("missing directory should be created");

        assert!(target.is_dir(), "実行用一時ディレクトリが作られるべき");
    }

    #[test]
    fn prepare_run_tmp_dir_clears_existing_contents() {
        // 旧実装は `let _ = remove_dir_all(...)` で削除失敗を握り潰し、消せなかった
        // ディレクトリをそのまま再利用していた。前回実行の残骸（キュー・タスク
        // スクリプト・マーカー）が残ると、完了済みタスクの誤検出につながる。
        let parent = tempfile::TempDir::new().expect("temp dir should be created");
        let target = parent.path().join("run");
        std::fs::create_dir_all(target.join("markers")).expect("nested dir should be created");
        std::fs::write(target.join("stale.sh"), b"old").expect("stale file should be written");
        std::fs::write(target.join("markers/done-1"), b"").expect("stale marker should be written");

        prepare_run_tmp_dir(&target).expect("existing directory should be recreated");

        assert!(target.is_dir());
        assert_eq!(
            std::fs::read_dir(&target)
                .expect("read_dir should succeed")
                .count(),
            0,
            "前回実行の残骸が残ってはいけない"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prepare_run_tmp_dir_restricts_permissions_to_owner_only() {
        // 共有 /tmp を temp_dir に持つ環境で他ユーザーにワーカースクリプトや
        // プロンプトを読み書きされないよう、作成後は必ず 0o700 にする。
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::TempDir::new().expect("temp dir should be created");
        let target = parent.path().join("run");
        // 事前に緩い権限のディレクトリがあっても、作り直して 0o700 になること
        std::fs::create_dir_all(&target).expect("dir should be created");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777))
            .expect("chmod should succeed");

        prepare_run_tmp_dir(&target).expect("directory should be prepared");

        let mode = std::fs::metadata(&target)
            .expect("metadata should be readable")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "所有者のみアクセス可にすべき: {mode:o}"
        );
    }

    #[test]
    fn prepare_run_tmp_dir_errors_when_path_is_not_a_directory() {
        // 削除できないパスをそのまま再利用しないこと（旧実装は握り潰していた）。
        // 通常ファイルを渡すと remove_dir_all が ENOTDIR で失敗するため、root 権限も
        // 特殊なファイルシステムも要らずに「削除失敗」を再現できる。
        let parent = tempfile::TempDir::new().expect("temp dir should be created");
        let target = parent.path().join("run");
        std::fs::write(&target, b"not a directory").expect("file should be written");

        let err = prepare_run_tmp_dir(&target).expect_err("削除失敗はエラーにすべき");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to clear the temporary run directory"),
            "エラーは一時ディレクトリの掃除失敗だと分かる文言にすべき: {msg}"
        );
        // 既存パスを黙って上書き・再利用しない
        assert!(target.is_file(), "既存ファイルは触らないべき");
    }
}
