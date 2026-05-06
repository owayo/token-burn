use anyhow::{Context, Result};
use colored::Colorize;
use std::path::Path;
use std::time::Duration;

use crate::config::Agent;
use crate::display;
use crate::scanner::{ResolvedTarget, Visibility};

pub struct ExecutionPlan {
    pub agent: Agent,
    pub tasks: Vec<ResolvedTarget>,
}

pub fn build_plan(agent: &Agent, targets: Vec<ResolvedTarget>) -> ExecutionPlan {
    let mut agent = agent.clone();
    ensure_required_flags(&mut agent);
    ExecutionPlan {
        agent,
        tasks: targets,
    }
}

/// command の先頭要素が claude 実行ファイル（ラッパースクリプト含む）かを判定する。
/// ファイル名（basename）が "claude" そのもの、または "claude-" / "claude_" で始まる場合に true。
fn is_claude_command(command: &[String]) -> bool {
    let Some(first) = command.first() else {
        return false;
    };
    let basename = std::path::Path::new(first.as_str())
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    basename == "claude" || basename.starts_with("claude-") || basename.starts_with("claude_")
}

/// 既知エージェントに必要なフラグを自動付与する。
/// `claude` の場合、`--verbose`、`--output-format stream-json`、`--include-partial-messages`
/// はログ取得に必須であり、常に存在しなければならない。
fn ensure_required_flags(agent: &mut Agent) {
    if !is_claude_command(&agent.command) {
        return;
    }

    let needs_verbose = !agent.command.iter().any(|s| s == "--verbose");
    let needs_partial = !agent
        .command
        .iter()
        .any(|s| s == "--include-partial-messages");

    let mut has_output_format = false;
    let mut idx = 0usize;
    while idx < agent.command.len() {
        let arg = &agent.command[idx];
        if arg == "--output-format" {
            has_output_format = true;
            let next_is_value = agent
                .command
                .get(idx + 1)
                .map(|s| !s.starts_with('-'))
                .unwrap_or(false);
            if next_is_value {
                if agent.command[idx + 1] != "stream-json" {
                    agent.command[idx + 1] = "stream-json".to_string();
                }
            } else {
                agent.command.insert(idx + 1, "stream-json".to_string());
            }
            break;
        }
        if arg.starts_with("--output-format=") {
            has_output_format = true;
            if arg != "--output-format=stream-json" {
                agent.command[idx] = "--output-format=stream-json".to_string();
            }
            break;
        }
        idx += 1;
    }

    if needs_verbose {
        agent.command.push("--verbose".to_string());
    }
    if !has_output_format {
        agent.command.push("--output-format".to_string());
        agent.command.push("stream-json".to_string());
    }
    if needs_partial {
        agent.command.push("--include-partial-messages".to_string());
    }
}

pub fn print_plan(plan: &ExecutionPlan) {
    println!("{}", "=== Execution Plan ===".bold());
    println!("Agent: {}", plan.agent.name.cyan());
    println!("Command: {}", plan.agent.command.join(" ").dimmed());
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
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir)?;

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
    let is_claude = is_claude_command(&plan.agent.command);

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

    let mut script_paths = Vec::new();
    for w in 0..worker_count {
        let script_path = tmp_dir.join(format!("worker-{}.sh", w));
        let worker_script = build_worker_script(&WorkerCtx {
            worker_id: w,
            queue_dir: &queue_dir,
            task_dir: &task_dir,
            marker_dir: &marker_dir,
            stop_file: &stop_file,
        });
        std::fs::write(&script_path, &worker_script)?;
        std::process::Command::new("chmod")
            .args(["+x", &script_path.to_string_lossy()])
            .output()?;
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
    );
    std::fs::write(&monitor_path, &monitor_script)?;
    std::process::Command::new("chmod")
        .args(["+x", &monitor_path.to_string_lossy()])
        .output()?;

    // モニター（左ペイン）付き tmux セッションを作成
    std::process::Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            session,
            &monitor_path.to_string_lossy(),
        ])
        .status()
        .context("Failed to create tmux session")?;

    // 最初のワーカー用に右ペインを分割
    std::process::Command::new("tmux")
        .args([
            "split-window",
            "-h",
            "-t",
            session,
            &script_paths[0].to_string_lossy(),
        ])
        .status()?;

    // 残りのワーカーを右エリアに垂直分割で追加
    for script in &script_paths[1..] {
        // 右側の最後のペインに垂直分割で追加
        std::process::Command::new("tmux")
            .args([
                "split-window",
                "-v",
                "-t",
                &format!("{}:.right", session),
                &script.to_string_lossy(),
            ])
            .status()?;
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

#[allow(clippy::too_many_arguments)]
fn generate_monitor_script(
    agent_name: &str,
    command_str: &str,
    reset_info: &str,
    total: usize,
    deadline_secs: u64,
    marker_dir: &std::path::Path,
    session: &str,
    worker_count: usize,
    stop_file: &std::path::Path,
    report_dir: &std::path::Path,
) -> String {
    let agent_escaped = shell_escape(agent_name);
    let command_escaped = shell_escape(command_str);
    let reset_escaped = shell_escape(reset_info);
    let marker_dir_escaped = shell_escape(&marker_dir.to_string_lossy());
    let session_escaped = shell_escape(session);
    let stop_file_escaped = shell_escape(&stop_file.to_string_lossy());
    let report_dir_escaped = shell_escape(&report_dir.to_string_lossy());

    format!(
        r#"#!/bin/bash
AGENT={agent}
COMMAND={command}
RESET={reset}
TOTAL={total}
DEADLINE={deadline}
MARKER_DIR={marker_dir}
SESSION={session}
WORKER_COUNT={worker_count}
STOP_FILE={stop_file}
REPORT_DIR={report_dir}
STOPPED=0
DISPLAYED_ERRORS=":"

handle_signal() {{
    if [ $STOPPED -eq 0 ]; then
        STOPPED=1
        touch "$STOP_FILE"
        echo ""
        echo " ⏳ Waiting for current tasks to finish..."
        echo "    Press Ctrl-C again to force kill."
    else
        echo ""
        echo " 📁 Logs: $REPORT_DIR"
        echo ""
        echo " Force killing session..."
        tmux kill-session -t "$SESSION" 2>/dev/null
        exit
    fi
}}
trap handle_signal INT TERM

printf '\033]2;token-burn\033\\'
printf '\033[?7l'

END=$(($(date +%s) + DEADLINE))

echo "━━━━━━━━━━━━━━━━━━━━━━━━"
echo " 🔥 token-burn 🔥"
echo "━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo " Agent:   $AGENT"
echo " Command: $COMMAND"
echo " Reset:   $RESET"
echo " Tasks:   $TOTAL"
echo " Workers: $WORKER_COUNT"
echo " Logs:    $REPORT_DIR"
echo ""

while true; do
    NOW=$(date +%s)
    REMAINING=$((END - NOW))
    DONE=$(find "$MARKER_DIR" -name 'done-*' 2>/dev/null | wc -l | tr -d ' ')
    FAILED=$(find "$MARKER_DIR" -name 'failed-*' 2>/dev/null | wc -l | tr -d ' ')
    RETRY=$(find "$MARKER_DIR" -name 'retry-*' 2>/dev/null | wc -l | tr -d ' ')
    PROCESSED=$((DONE + FAILED + RETRY))

    # デッドライン到達確認
    if [ $REMAINING -le 0 ] && [ $STOPPED -eq 0 ]; then
        STOPPED=1
        touch "$STOP_FILE"
        echo ""
        echo " ⚠ DEADLINE REACHED"
        echo " ⏳ Waiting for current tasks to finish..."
        echo "    Press Ctrl-C to force kill."
    fi

    # 失敗・リトライを含め、全タスクが処理済みか確認
    if [ "$PROCESSED" -ge "$TOTAL" ]; then
        if [ "$FAILED" -gt 0 ] || [ "$RETRY" -gt 0 ]; then
            printf "\r\033[2K ⚠  Completed: %d succeeded / %d failed / %d retry\n" "$DONE" "$FAILED" "$RETRY"
        else
            printf "\r\033[2K ✅ All %d/%d tasks completed!\n" "$DONE" "$TOTAL"
        fi
        echo ""
        echo " 📁 Logs: $REPORT_DIR"
        echo ""
        echo " Press Ctrl-C to close session."
        exec sleep infinity
    fi

    # 停止中の場合は全ワーカーの終了を確認
    if [ $STOPPED -eq 1 ]; then
        WORKERS_DONE=$(find "$MARKER_DIR" -name 'worker-done-*' 2>/dev/null | wc -l | tr -d ' ')
        if [ "$WORKERS_DONE" -ge "$WORKER_COUNT" ]; then
            printf "\r\033[2K ⏹ Stopped: %d/%d processed (fail:%d retry:%d)\n" "$PROCESSED" "$TOTAL" "$FAILED" "$RETRY"
            echo ""
            echo " 📁 Logs: $REPORT_DIR"
            echo ""
            echo " Press Ctrl-C to close session."
            exec sleep infinity
        fi
    fi

    # 新規エラーを表示
    for f in $(find "$MARKER_DIR" -name 'error-*' 2>/dev/null); do
        EFILE=$(basename "$f")
        case "$DISPLAYED_ERRORS" in
            *":$EFILE:"*) ;;
            *)
                echo ""
                echo " ❌ $(cat "$f")"
                DISPLAYED_ERRORS="$DISPLAYED_ERRORS$EFILE:"
                ;;
        esac
    done

    # 進捗バー
    if [ $TOTAL -gt 0 ]; then
        PCT=$((PROCESSED * 100 / TOTAL))
        BAR_W=20
        FILLED=$((PCT * BAR_W / 100))
        EMPTY=$((BAR_W - FILLED))
        BAR=""
        for i in $(seq 1 $FILLED 2>/dev/null); do BAR="${{BAR}}█"; done
        for i in $(seq 1 $EMPTY 2>/dev/null); do BAR="${{BAR}}░"; done
    else
        BAR="░░░░░░░░░░░░░░░░░░░░"
        PCT=0
    fi

    if [ $STOPPED -eq 0 ]; then
        D=$((REMAINING / 86400))
        H=$(((REMAINING % 86400) / 3600))
        M=$(((REMAINING % 3600) / 60))
        S=$((REMAINING % 60))
        printf "\r\033[2K ⏱ %dd %02dh %02dm %02ds  [%s] %d/%d (%d%%, fail:%d retry:%d)" \
            "$D" "$H" "$M" "$S" "$BAR" "$PROCESSED" "$TOTAL" "$PCT" "$FAILED" "$RETRY"
    else
        printf "\r\033[2K ⏳ Stopping...  [%s] %d/%d (%d%%, fail:%d retry:%d)" \
            "$BAR" "$PROCESSED" "$TOTAL" "$PCT" "$FAILED" "$RETRY"
    fi

    sleep 1
done
"#,
        session = session_escaped,
        agent = agent_escaped,
        command = command_escaped,
        reset = reset_escaped,
        total = total,
        deadline = deadline_secs,
        marker_dir = marker_dir_escaped,
        worker_count = worker_count,
        stop_file = stop_file_escaped,
        report_dir = report_dir_escaped,
    )
}

struct TaskCtx<'a> {
    idx: usize,
    total: usize,
    task: &'a ResolvedTarget,
    agent: &'a Agent,
    prompt_file: &'a Path,
    run_dir: &'a Path,
    marker_dir: &'a Path,
    exe_path: &'a Path,
    state_file: &'a Path,
    stop_file: &'a Path,
    rate_limit_threshold: u8,
    is_claude: bool,
}

struct WorkerCtx<'a> {
    worker_id: usize,
    queue_dir: &'a Path,
    task_dir: &'a Path,
    marker_dir: &'a Path,
    stop_file: &'a Path,
}

/// キューから claim したワーカーが source して実行する、タスク単位のシェルスクリプトを生成する。
fn build_task_script(ctx: &TaskCtx<'_>) -> String {
    let log_base = task_log_base(ctx.idx, &ctx.task.display_name);
    let log_file = shell_escape(
        &ctx.run_dir
            .join(format!("{log_base}.log"))
            .to_string_lossy(),
    );
    let jsonl_file = shell_escape(
        &ctx.run_dir
            .join(format!("{log_base}.jsonl"))
            .to_string_lossy(),
    );
    let done_marker = shell_escape(
        &ctx.marker_dir
            .join(format!("done-{}", ctx.idx))
            .to_string_lossy(),
    );
    let failed_marker = shell_escape(
        &ctx.marker_dir
            .join(format!("failed-{}", ctx.idx))
            .to_string_lossy(),
    );
    let retry_marker = shell_escape(
        &ctx.marker_dir
            .join(format!("retry-{}", ctx.idx))
            .to_string_lossy(),
    );
    let error_file = shell_escape(
        &ctx.marker_dir
            .join(format!("error-{}", ctx.idx))
            .to_string_lossy(),
    );
    let error_prefix = shell_escape(&format!("[{}] ", ctx.task.display_name));
    let stop_file_escaped = shell_escape(&ctx.stop_file.to_string_lossy());
    let cmd_str = build_shell_command(&ctx.agent.command, ctx.prompt_file, &ctx.task.directory);
    let mark_cmd = format!(
        "{} mark {} {} {}",
        shell_escape(&ctx.exe_path.to_string_lossy()),
        shell_escape(&ctx.agent.name),
        shell_escape(&ctx.task.directory.to_string_lossy()),
        shell_escape(&ctx.state_file.to_string_lossy()),
    );

    let mut script = String::new();
    // 現在処理中のタスクをシグナルハンドラから参照できるようにする
    script += &format!("CURRENT_FAILED_MARKER={failed_marker}\n");
    script += &build_task_header_script(ctx.idx, ctx.total, &ctx.task.display_name);

    if ctx.is_claude {
        let tb_cmd = shell_escape(&ctx.exe_path.to_string_lossy());
        script += &format!(
            "{cmd_str} 2>&1 | {tb_cmd} format-stream --raw-output {jsonl_file} --stop-file {stop_file_escaped} --threshold {rate_limit_threshold} 2>&1 | tee {log_file}\n",
            rate_limit_threshold = ctx.rate_limit_threshold,
        );
        script += "PIPE_STATUS=(\"${PIPESTATUS[@]}\")\n";
        script += "CMD_EXIT=${PIPE_STATUS[0]}\n";
        script += "FORMAT_EXIT=${PIPE_STATUS[1]}\n";
        script += "TEE_EXIT=${PIPE_STATUS[2]}\n";
        script += "CURRENT_FAILED_MARKER=\"\"\n";
        script += &format!(
            concat!(
                "if [ \"$FORMAT_EXIT\" -ne 0 ] || [ \"$TEE_EXIT\" -ne 0 ] || [ ! -s {jsonl} ]; then\n",
                "  printf '%slogging/classification pipeline failed (format=%s tee=%s)\\n' {prefix} \"$FORMAT_EXIT\" \"$TEE_EXIT\" > {error}\n",
                "  touch {failed}\n",
                "  echo '━━━ Error - logging pipeline failed ━━━'\n",
                "  echo ''\n",
                "  return 0\n",
                "fi\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
            jsonl = jsonl_file,
        );
    } else {
        script += &format!("{cmd_str} 2>&1 | tee {log_file}\n");
        script += "PIPE_STATUS=(\"${PIPESTATUS[@]}\")\n";
        script += "CMD_EXIT=${PIPE_STATUS[0]}\n";
        script += "TEE_EXIT=${PIPE_STATUS[1]}\n";
        script += "CURRENT_FAILED_MARKER=\"\"\n";
        script += &format!(
            concat!(
                "if [ \"$TEE_EXIT\" -ne 0 ]; then\n",
                "  printf '%slogging pipeline failed (tee=%s)\\n' {prefix} \"$TEE_EXIT\" > {error}\n",
                "  touch {failed}\n",
                "  echo '━━━ Error - logging pipeline failed ━━━'\n",
                "  echo ''\n",
                "  return 0\n",
                "fi\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
        );
    }

    if ctx.is_claude {
        let tb_cmd = shell_escape(&ctx.exe_path.to_string_lossy());
        script += &format!(
            concat!(
                "CLASSIFIED=$({tb} classify-result {jsonl} 2>/dev/null)\n",
                "CLASS_CODE=$?\n",
                "case $CLASS_CODE in\n",
                "  2)\n",
                // 後続タスクが誤って「Cancelled」と判定されないよう、ここでフラグを必ずリセットする
                "    CANCELLED=0\n",
                "    touch {failed}\n",
                "    echo '━━━ Rate limited - not marking as completed ━━━'\n",
                "    ;;\n",
                "  3)\n",
                // 後続タスクが誤って「Cancelled」と判定されないよう、ここでフラグを必ずリセットする
                "    CANCELLED=0\n",
                "    if [ -n \"$CLASSIFIED\" ]; then\n",
                "      printf '%s%s\\n' {prefix} \"$CLASSIFIED\" > {error}\n",
                "    fi\n",
                "    touch {retry}\n",
                "    echo \"━━━ Retryable error (will retry next run): $CLASSIFIED ━━━\"\n",
                "    ;;\n",
                "  1)\n",
                "    if [ $CANCELLED -eq 1 ]; then\n",
                "      CANCELLED=0\n",
                "      touch {failed}\n",
                "      echo '━━━ Cancelled ━━━'\n",
                "    else\n",
                "      printf '%s%s\\n' {prefix} \"$CLASSIFIED\" > {error}\n",
                "      touch {failed}\n",
                "      echo '━━━ Error - continuing ━━━'\n",
                "    fi\n",
                "    ;;\n",
                "  *)\n",
                "    if [ \"$CMD_EXIT\" -ne 0 ]; then\n",
                "      if [ $CANCELLED -eq 1 ]; then\n",
                "        CANCELLED=0\n",
                "        touch {failed}\n",
                "        echo '━━━ Cancelled ━━━'\n",
                "      else\n",
                "        ERROR_MSG=$(tmux capture-pane -t \"$TMUX_PANE\" -p -J -S -10 | grep -v '^$' | tail -1)\n",
                "        printf '%s%s\\n' {prefix} \"$ERROR_MSG\" > {error}\n",
                "        touch {failed}\n",
                "        echo '━━━ Error - continuing ━━━'\n",
                "      fi\n",
                "    else\n",
                "      {mark}\n",
                "      touch {done}\n",
                "    fi\n",
                "    ;;\n",
                "esac\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
            retry = retry_marker,
            done = done_marker,
            jsonl = jsonl_file,
            tb = tb_cmd,
            mark = mark_cmd,
        );
    } else {
        script += &format!(
            concat!(
                "if [ \"$CMD_EXIT\" -ne 0 ]; then\n",
                "  if [ $CANCELLED -eq 1 ]; then\n",
                "    CANCELLED=0\n",
                "    touch {failed}\n",
                "    echo '━━━ Cancelled ━━━'\n",
                "  else\n",
                "    ERROR_MSG=$(tmux capture-pane -t \"$TMUX_PANE\" -p -J -S -10 | grep -v '^$' | tail -1)\n",
                "    printf '%s%s\\n' {prefix} \"$ERROR_MSG\" > {error}\n",
                "    touch {failed}\n",
                "    echo '━━━ Error - continuing ━━━'\n",
                "  fi\n",
                "else\n",
                "  {mark}\n",
                "  touch {done}\n",
                "fi\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
            done = done_marker,
            mark = mark_cmd,
        );
    }
    script += "echo ''\n";
    script
}

/// 共通ワーカースクリプト: queue_dir/pending-* をアトミックに claim しつつタスクを逐次実行する。
fn build_worker_script(ctx: &WorkerCtx<'_>) -> String {
    let w = ctx.worker_id + 1;
    let queue_dir = shell_escape(&ctx.queue_dir.to_string_lossy());
    let task_dir = shell_escape(&ctx.task_dir.to_string_lossy());
    let stop_file = shell_escape(&ctx.stop_file.to_string_lossy());
    let worker_done = shell_escape(
        &ctx.marker_dir
            .join(format!("worker-done-{}", ctx.worker_id))
            .to_string_lossy(),
    );

    format!(
        concat!(
            "#!/bin/bash\n",
            "CURRENT_FAILED_MARKER=\"\"\n",
            "CANCELLED=0\n",
            "handle_cancel() {{\n",
            "  CANCELLED=1\n",
            "  if [ -n \"$CURRENT_FAILED_MARKER\" ]; then touch \"$CURRENT_FAILED_MARKER\"; fi\n",
            "}}\n",
            "trap handle_cancel INT TERM\n",
            "\n",
            "QUEUE_DIR={queue_dir}\n",
            "TASK_DIR={task_dir}\n",
            "\n",
            "while true; do\n",
            "  if [ -f {stop_file} ]; then\n",
            "    printf '\\033]2;Worker {w} stopped\\033\\\\'\n",
            "    echo '━━━ Stopped ━━━'\n",
            "    break\n",
            "  fi\n",
            "  CLAIMED=\"\"\n",
            "  for pending in \"$QUEUE_DIR\"/pending-*; do\n",
            "    [ -e \"$pending\" ] || continue\n",
            "    base=$(basename \"$pending\")\n",
            "    idx=${{base#pending-}}\n",
            "    if mv \"$pending\" \"$QUEUE_DIR/claimed-$idx\" 2>/dev/null; then\n",
            "      CLAIMED=\"$idx\"\n",
            "      break\n",
            "    fi\n",
            "  done\n",
            "  if [ -z \"$CLAIMED\" ]; then\n",
            "    break\n",
            "  fi\n",
            "  TASK_SCRIPT=\"$TASK_DIR/task-$CLAIMED.sh\"\n",
            "  if [ ! -f \"$TASK_SCRIPT\" ]; then\n",
            "    echo \"━━━ Missing task script: $TASK_SCRIPT ━━━\"\n",
            "    continue\n",
            "  fi\n",
            "  # shellcheck disable=SC1090\n",
            "  source \"$TASK_SCRIPT\"\n",
            "done\n",
            "\n",
            "printf '\\033]2;Worker {w} done\\033\\\\'\n",
            "echo '━━━ All tasks completed ━━━'\n",
            "touch {worker_done}\n",
            "exec sleep infinity\n",
        ),
        queue_dir = queue_dir,
        task_dir = task_dir,
        stop_file = stop_file,
        w = w,
        worker_done = worker_done,
    )
}

fn build_task_header_script(idx: usize, total: usize, display_name: &str) -> String {
    let pane_title = format!("[{}/{}] {}", idx, total, display_name);
    let section_title = format!("━━━ [{}/{}] {} ━━━", idx, total, display_name);
    format!(
        "printf '\\033]2;%s\\033\\\\' {}\necho {}\n",
        shell_escape(&pane_title),
        shell_escape(&section_title),
    )
}

fn build_shell_command(
    cmd_parts: &[String],
    prompt_file: &std::path::Path,
    directory: &std::path::Path,
) -> String {
    let mut parts: Vec<String> = vec![format!("cd {}", shell_escape(&directory.to_string_lossy()))];
    let cmd: Vec<String> = cmd_parts.iter().map(|s| shell_escape(s)).collect();
    // プロンプトをコマンド置換 $(cat file) で引数として渡す
    // stdin パイプは claude -p で確実に動作しないため
    parts.push(format!(
        "{} \"$(cat {})\"",
        cmd.join(" "),
        shell_escape(&prompt_file.to_string_lossy())
    ));
    parts.join(" && ")
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn task_log_base(idx: usize, display_name: &str) -> String {
    format!("{idx:04}_{}", sanitize_filename(display_name))
}

fn strip_ansi_from_dir(dir: &std::path::Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "log").unwrap_or(false)
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            let cleaned = strip_ansi(&content);
            let _ = std::fs::write(&path, cleaned);
        }
    }
}

fn strip_ansi(s: &str) -> String {
    fn is_csi_final(c: char) -> bool {
        ('\u{40}'..='\u{7e}').contains(&c)
    }

    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    // CSIシーケンス: \x1b[...終端バイト (0x40-0x7E)
                    chars.next();
                    for ch in chars.by_ref() {
                        if is_csi_final(ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSCシーケンス: \x1b]...ST (ST = \x1b\\ または \x07)
                    chars.next();
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    // その他のエスケープシーケンス (例: \x1b(B) — 次の文字をスキップ
                    chars.next();
                }
                None => break,
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        "...".to_string()
    } else {
        let truncated: String = s.chars().take(max_len - 3).collect();
        format!("{}...", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_task_header_script_escapes_display_name() {
        let script = build_task_header_script(1, 3, "repo'; touch /tmp/pwn #");
        assert!(
            script.contains("printf '\\033]2;%s\\033\\\\' '[1/3] repo'\\''; touch /tmp/pwn #'")
        );
        assert!(script.contains("echo '━━━ [1/3] repo'\\''; touch /tmp/pwn # ━━━'"));
    }

    fn task_ctx_for_test<'a>(
        idx: usize,
        agent: &'a Agent,
        task: &'a ResolvedTarget,
        tmp: &'a std::path::Path,
        is_claude: bool,
    ) -> TaskCtx<'a> {
        TaskCtx {
            idx,
            total: 3,
            task,
            agent,
            prompt_file: std::path::Path::new("/tmp/prompt.txt"),
            run_dir: tmp,
            marker_dir: tmp,
            exe_path: std::path::Path::new("/usr/local/bin/token-burn"),
            state_file: std::path::Path::new("/tmp/state.json"),
            stop_file: std::path::Path::new("/tmp/stop"),
            rate_limit_threshold: 95,
            is_claude,
        }
    }

    #[test]
    fn build_task_script_for_claude_uses_classify_result_and_no_sleep_infinity() {
        let agent = Agent {
            name: "claude".to_string(),
            command: vec!["claude".to_string(), "-p".to_string()],
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        };
        let task = ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        };
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(7, &agent, &task, &tmp, true);
        let script = build_task_script(&ctx);

        // キュー方式ではエラー時にワーカーを止めない
        assert!(
            !script.contains("exec sleep infinity"),
            "タスクスクリプトは sleep infinity せず次タスクに進むべき: {script}"
        );
        assert!(!script.contains("touch {wdone}"));
        assert!(!script.contains("touch ") || !script.contains("worker-done"));
        // jsonl 分類呼び出し
        assert!(script.contains("classify-result"));
        assert!(script.contains("Error - continuing"));
        assert!(script.contains("PIPE_STATUS=(\"${PIPESTATUS[@]}\")"));
        assert!(script.contains("FORMAT_EXIT=${PIPE_STATUS[1]}"));
        assert!(script.contains("TEE_EXIT=${PIPE_STATUS[2]}"));
        assert!(script.contains("logging/classification pipeline failed"));
        assert!(script.contains("[ ! -s '/tmp/0007_repo.jsonl' ]"));
        // error マーカーは task idx 単位
        assert!(script.contains("/error-7"));
        assert!(script.contains("/failed-7"));
        assert!(script.contains("/retry-7"));
        assert!(script.contains("/done-7"));
    }

    #[test]
    fn build_task_script_resets_cancelled_in_rate_limited_and_retry_branches() {
        // SIGINT で CANCELLED=1 になった後、レート制限 (CLASS_CODE=2) や
        // リトライ可能エラー (CLASS_CODE=3) で終了したタスクは CANCELLED を
        // リセットしないと、後続タスクで誤って Cancelled と判定されてしまう。
        let agent = Agent {
            name: "claude".to_string(),
            command: vec!["claude".to_string(), "-p".to_string()],
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        };
        let task = ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        };
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(1, &agent, &task, &tmp, true);
        let script = build_task_script(&ctx);

        // CLASS_CODE=2 (レート制限) ブランチで CANCELLED をリセットすること
        let rate_limited_idx = script
            .find("Rate limited - not marking as completed")
            .expect("rate limited branch missing");
        let preceding = &script[..rate_limited_idx];
        let last_case2 = preceding.rfind("  2)\n").expect("case 2 branch missing");
        assert!(
            script[last_case2..rate_limited_idx].contains("CANCELLED=0"),
            "CLASS_CODE=2 branch must reset CANCELLED:\n{}",
            &script[last_case2..rate_limited_idx]
        );

        // CLASS_CODE=3 (リトライ可能) ブランチで CANCELLED をリセットすること
        let retry_idx = script
            .find("Retryable error (will retry next run)")
            .expect("retry branch missing");
        let preceding = &script[..retry_idx];
        let last_case3 = preceding.rfind("  3)\n").expect("case 3 branch missing");
        assert!(
            script[last_case3..retry_idx].contains("CANCELLED=0"),
            "CLASS_CODE=3 branch must reset CANCELLED:\n{}",
            &script[last_case3..retry_idx]
        );
    }

    #[test]
    fn build_task_script_for_non_claude_skips_classify() {
        let agent = Agent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        };
        let task = ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        };
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(2, &agent, &task, &tmp, false);
        let script = build_task_script(&ctx);

        assert!(!script.contains("classify-result"));
        assert!(!script.contains("exec sleep infinity"));
        assert!(script.contains("Error - continuing"));
        assert!(script.contains("TEE_EXIT=${PIPE_STATUS[1]}"));
        assert!(script.contains("logging pipeline failed"));
    }

    #[test]
    fn build_worker_script_consumes_queue_atomically() {
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            stop_file: &tmp.join("stop"),
        });

        assert!(script.contains("#!/bin/bash"));
        // mv でアトミック claim
        assert!(script.contains("mv \"$pending\" \"$QUEUE_DIR/claimed-$idx\""));
        // source で個別タスクを取り込む
        assert!(script.contains("source \"$TASK_SCRIPT\""));
        // ワーカー完了マーカー
        assert!(script.contains("worker-done-0"));
        // 停止シグナル対応
        assert!(script.contains("trap handle_cancel INT TERM"));
    }

    #[test]
    fn build_worker_script_escapes_paths_with_spaces() {
        let script = build_worker_script(&WorkerCtx {
            worker_id: 1,
            queue_dir: std::path::Path::new("/tmp/my queue"),
            task_dir: std::path::Path::new("/tmp/my tasks"),
            marker_dir: std::path::Path::new("/tmp/my markers"),
            stop_file: std::path::Path::new("/tmp/my stop"),
        });
        assert!(script.contains("QUEUE_DIR='/tmp/my queue'"));
        assert!(script.contains("TASK_DIR='/tmp/my tasks'"));
        assert!(script.contains("'/tmp/my stop'"));
        assert!(script.contains("'/tmp/my markers/worker-done-1'"));
    }

    #[test]
    fn generate_monitor_script_handles_failed_markers_and_escapes_values() {
        let script = generate_monitor_script(
            "ag\"$(touch /tmp/pwn)\"",
            "claude -p",
            "2026/02/24 09:00",
            2,
            60,
            std::path::Path::new("/tmp/marker dir"),
            "token-burn",
            1,
            std::path::Path::new("/tmp/stop file"),
            std::path::Path::new("/tmp/report dir"),
        );

        assert!(script.contains("AGENT='ag\"$(touch /tmp/pwn)\"'"));
        assert!(script.contains("DISPLAYED_ERRORS=\":\""));
        assert!(script.contains("*\":$EFILE:\"*"));
        assert!(script.contains("FAILED=$(find \"$MARKER_DIR\" -name 'failed-*'"));
        assert!(script.contains("RETRY=$(find \"$MARKER_DIR\" -name 'retry-*'"));
        assert!(script.contains("PROCESSED=$((DONE + FAILED + RETRY))"));
        assert!(script.contains("Completed: %d succeeded / %d failed / %d retry"));
        assert!(script.contains("fail:%d retry:%d"));
    }

    #[test]
    fn shell_escape_escapes_single_quotes() {
        assert_eq!(shell_escape("a'b"), "'a'\\''b'");
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let input = "\x1b[1mBold\x1b[0m normal \x1b[31mred\x1b[0m";
        assert_eq!(strip_ansi(input), "Bold normal red");
    }

    #[test]
    fn strip_ansi_preserves_plain_text() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn strip_ansi_removes_osc_with_bel() {
        let input = "\x1b]2;pane title\x07ok";
        assert_eq!(strip_ansi(input), "ok");
    }

    #[test]
    fn strip_ansi_removes_osc_with_st() {
        let input = "\x1b]2;pane title\x1b\\ok";
        assert_eq!(strip_ansi(input), "ok");
    }

    #[test]
    fn strip_ansi_removes_bracketed_paste() {
        let input = "\x1b[200~pasted text\x1b[201~";
        assert_eq!(strip_ansi(input), "pasted text");
    }

    #[test]
    fn strip_ansi_handles_mixed_sequences() {
        let input = "\x1b]2;title\x07\x1b[1mBold\x1b[0m text\x1b]0;icon\x1b\\end";
        assert_eq!(strip_ansi(input), "Bold textend");
    }

    #[test]
    fn strip_ansi_lone_esc_at_end() {
        // 末尾の孤立ESCは安全に消費される
        let input = "text\x1b";
        assert_eq!(strip_ansi(input), "text");
    }

    #[test]
    fn strip_ansi_incomplete_csi_at_end() {
        // 終端バイトなしの不完全なCSIシーケンスは安全に除去される
        let input = "text\x1b[1";
        assert_eq!(strip_ansi(input), "text");
    }

    #[test]
    fn strip_ansi_incomplete_osc_at_end() {
        // BEL/ST終端なしの不完全なOSCシーケンスは安全に除去される
        let input = "text\x1b]2;title";
        assert_eq!(strip_ansi(input), "text");
    }

    #[test]
    fn strip_ansi_other_escape_skips_one_char() {
        // ESC + 非 `[`/`]` 文字 → ESCと次の1文字のみスキップ
        let input = "before\x1b(after";
        assert_eq!(strip_ansi(input), "beforeafter");
    }

    #[test]
    fn sanitize_filename_replaces_special_chars() {
        assert_eq!(sanitize_filename("my-project"), "my-project");
        assert_eq!(sanitize_filename("path/to/repo"), "path_to_repo");
        assert_eq!(sanitize_filename("a b@c"), "a_b_c");
    }

    #[test]
    fn task_log_base_is_unique_even_with_same_display_name() {
        assert_ne!(task_log_base(1, "repo"), task_log_base(2, "repo"));
    }

    #[test]
    fn task_log_base_sanitizes_display_name() {
        assert_eq!(task_log_base(3, "path/to/repo"), "0003_path_to_repo");
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("12345", 5), "12345");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        assert_eq!(truncate("abcdefghij", 7), "abcd...");
    }

    #[test]
    fn truncate_multibyte_counts_chars() {
        // 5文字の日本語文字列を3文字に切り詰め
        assert_eq!(truncate("あいうえお", 3), "...");
        assert_eq!(truncate("あいうえお", 5), "あいうえお");
        assert_eq!(truncate("あいうえお", 4), "あ...");
    }

    #[test]
    fn build_shell_command_escapes_paths() {
        let cmd = vec!["claude".to_string(), "-p".to_string()];
        let prompt = std::path::Path::new("/tmp/prompt.txt");
        let dir = std::path::Path::new("/home/user/my project");
        let result = build_shell_command(&cmd, prompt, dir);
        assert!(result.contains("cd '/home/user/my project'"));
        assert!(result.contains("'claude' '-p'"));
        assert!(result.contains("$(cat '/tmp/prompt.txt')"));
    }

    fn make_agent(command: Vec<&str>) -> Agent {
        Agent {
            name: "claude".to_string(),
            command: command.into_iter().map(String::from).collect(),
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        }
    }

    #[test]
    fn ensure_required_flags_adds_missing_claude_flags() {
        let mut agent = make_agent(vec!["claude", "-p"]);
        ensure_required_flags(&mut agent);
        assert!(agent.command.contains(&"--verbose".to_string()));
        assert!(agent.command.contains(&"--output-format".to_string()));
        assert!(agent.command.contains(&"stream-json".to_string()));
        assert!(
            agent
                .command
                .contains(&"--include-partial-messages".to_string())
        );
    }

    #[test]
    fn ensure_required_flags_skips_existing_flags() {
        let mut agent = make_agent(vec![
            "claude",
            "-p",
            "--verbose",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
        ]);
        let original_len = agent.command.len();
        ensure_required_flags(&mut agent);
        assert_eq!(agent.command.len(), original_len);
    }

    #[test]
    fn ensure_required_flags_rewrites_non_stream_json_output_format() {
        let mut agent = make_agent(vec!["claude", "-p", "--output-format", "text"]);
        ensure_required_flags(&mut agent);

        let idx = agent
            .command
            .iter()
            .position(|s| s == "--output-format")
            .expect("output-format flag should exist");
        assert_eq!(agent.command.get(idx + 1), Some(&"stream-json".to_string()));
    }

    #[test]
    fn ensure_required_flags_supports_equals_style_output_format() {
        let mut agent = make_agent(vec!["claude", "-p", "--output-format=stream-json"]);
        let original_len = agent.command.len();
        ensure_required_flags(&mut agent);
        assert_eq!(agent.command.len(), original_len + 2);
        assert!(
            agent
                .command
                .contains(&"--output-format=stream-json".to_string())
        );
        assert!(!agent.command.iter().any(|s| s == "--output-format"));
    }

    #[test]
    fn ensure_required_flags_adds_missing_output_format_value() {
        let mut agent = make_agent(vec!["claude", "-p", "--output-format"]);
        ensure_required_flags(&mut agent);
        let idx = agent
            .command
            .iter()
            .position(|s| s == "--output-format")
            .expect("output-format flag should exist");
        assert_eq!(agent.command.get(idx + 1), Some(&"stream-json".to_string()));
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
        let plan = build_plan(&agent, targets);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].display_name, "repo");
        // claude エージェントにはフラグが自動付与される
        assert!(plan.agent.command.contains(&"--verbose".to_string()));
    }

    #[test]
    fn truncate_max_len_3() {
        // max_len=3 の場合は "..." のみ
        assert_eq!(truncate("hello", 3), "...");
    }

    #[test]
    fn strip_ansi_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn sanitize_filename_empty_string() {
        assert_eq!(sanitize_filename(""), "");
    }

    #[test]
    fn sanitize_filename_unicode() {
        // 日本語文字はアルファニューメリックとして扱われる
        let result = sanitize_filename("日本語repo");
        assert!(result.contains("repo"));
    }

    #[test]
    fn truncate_max_len_zero() {
        // max_len=0 の場合
        assert_eq!(truncate("hello", 0), "...");
    }

    #[test]
    fn truncate_max_len_one() {
        assert_eq!(truncate("hello", 1), "...");
    }

    #[test]
    fn truncate_max_len_two() {
        assert_eq!(truncate("hello", 2), "...");
    }

    #[test]
    fn truncate_emoji_string() {
        // 絵文字を含む文字列の切り詰め
        let input = "🔥🚀✨🎉💡";
        assert_eq!(truncate(input, 5), "🔥🚀✨🎉💡");
        assert_eq!(truncate(input, 4), "🔥...");
    }

    #[test]
    fn ensure_required_flags_empty_command_returns_early() {
        // command が空の場合（executable が空文字列にならない）
        let mut agent = Agent {
            name: "test".to_string(),
            command: vec![],
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        };
        let original_len = agent.command.len();
        ensure_required_flags(&mut agent);
        // 空のcommandは "claude" ではないので何も変更されない
        assert_eq!(agent.command.len(), original_len);
    }

    #[test]
    fn sanitize_filename_preserves_dots() {
        assert_eq!(sanitize_filename("file.log"), "file.log");
        assert_eq!(sanitize_filename("v1.2.3"), "v1.2.3");
    }

    #[test]
    fn task_log_base_zero_padded() {
        assert_eq!(task_log_base(1, "repo"), "0001_repo");
        assert_eq!(task_log_base(9999, "repo"), "9999_repo");
    }

    #[test]
    fn ensure_required_flags_ignores_non_claude_agent() {
        let mut agent = Agent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            reset_weekday: "thursday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        };
        let original_len = agent.command.len();
        ensure_required_flags(&mut agent);
        assert_eq!(agent.command.len(), original_len);
    }

    #[test]
    fn strip_ansi_from_dir_cleans_log_files() {
        // .log ファイルから ANSI エスケープコードが除去されることを確認
        let tmp = tempfile::TempDir::new().unwrap();
        let log_path = tmp.path().join("test.log");
        let jsonl_path = tmp.path().join("test.jsonl");
        let txt_path = tmp.path().join("test.txt");

        std::fs::write(&log_path, "\x1b[1mBold\x1b[0m text").unwrap();
        std::fs::write(&jsonl_path, "\x1b[31mred\x1b[0m").unwrap();
        std::fs::write(&txt_path, "\x1b[32mgreen\x1b[0m").unwrap();

        strip_ansi_from_dir(tmp.path());

        // .log ファイルのみ ANSI が除去される
        assert_eq!(std::fs::read_to_string(&log_path).unwrap(), "Bold text");
        // .jsonl と .txt は変更されない
        assert_eq!(
            std::fs::read_to_string(&jsonl_path).unwrap(),
            "\x1b[31mred\x1b[0m"
        );
        assert_eq!(
            std::fs::read_to_string(&txt_path).unwrap(),
            "\x1b[32mgreen\x1b[0m"
        );
    }

    #[test]
    fn strip_ansi_from_dir_nonexistent_dir_does_not_panic() {
        // 存在しないディレクトリでもパニックしない
        strip_ansi_from_dir(std::path::Path::new("/nonexistent/dir"));
    }

    #[test]
    fn strip_ansi_256_color_sequence() {
        // 256色シーケンス（マルチパラメータ CSI）が除去される
        let input = "\x1b[38;5;196mred text\x1b[0m";
        assert_eq!(strip_ansi(input), "red text");
    }

    #[test]
    fn strip_ansi_truecolor_sequence() {
        // 24ビットトゥルーカラーシーケンスが除去される
        let input = "\x1b[38;2;255;0;0mred\x1b[48;2;0;0;255mblue bg\x1b[0m";
        assert_eq!(strip_ansi(input), "redblue bg");
    }

    #[test]
    fn strip_ansi_consecutive_sequences() {
        // テキストなしの連続シーケンスが正しく除去される
        let input = "\x1b[1m\x1b[31m\x1b[4mformatted\x1b[0m\x1b[0m\x1b[0m";
        assert_eq!(strip_ansi(input), "formatted");
    }

    #[test]
    fn is_claude_command_detects_bare_claude() {
        assert!(is_claude_command(&["claude".to_string()]));
        assert!(is_claude_command(&["claude".to_string(), "-p".to_string()]));
    }

    #[test]
    fn is_claude_command_detects_wrapper_script() {
        assert!(is_claude_command(&[
            "/Users/owa/shell/claude-wrapper.sh".to_string()
        ]));
        assert!(is_claude_command(&[
            "./claude-wrapper.sh".to_string(),
            "-p".to_string(),
        ]));
        assert!(is_claude_command(&["claude-code.sh".to_string()]));
        assert!(is_claude_command(&["claude_custom".to_string()]));
    }

    #[test]
    fn is_claude_command_rejects_non_claude() {
        assert!(!is_claude_command(&["codex".to_string()]));
        assert!(!is_claude_command(&["my-claude-fork".to_string()]));
        assert!(!is_claude_command(&[]));
    }

    #[test]
    fn ensure_required_flags_works_with_wrapper() {
        let mut agent = Agent {
            name: "claude".to_string(),
            command: vec![
                "/Users/owa/shell/claude-wrapper.sh".to_string(),
                "-p".to_string(),
            ],
            reset_weekday: "friday".to_string(),
            reset_time: "13:00".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            prompt: None,
        };
        ensure_required_flags(&mut agent);
        assert!(agent.command.contains(&"--verbose".to_string()));
        assert!(agent.command.contains(&"--output-format".to_string()));
        assert!(agent.command.contains(&"stream-json".to_string()));
        assert!(
            agent
                .command
                .contains(&"--include-partial-messages".to_string())
        );
    }

    #[test]
    fn ensure_required_flags_rewrites_equals_style_non_stream_json() {
        // --output-format=text のequals形式は --output-format=stream-json に書き換えられる
        let mut agent = make_agent(vec!["claude", "-p", "--output-format=text"]);
        ensure_required_flags(&mut agent);
        assert!(
            agent
                .command
                .contains(&"--output-format=stream-json".to_string()),
            "equals形式の値が stream-json に書き換えられるべき: {:?}",
            agent.command
        );
        assert!(
            !agent.command.contains(&"--output-format=text".to_string()),
            "元の値が残るべきでない: {:?}",
            agent.command
        );
    }

    #[test]
    fn build_shell_command_includes_cd_and_prompt() {
        let cmd = build_shell_command(
            &["claude".to_string(), "-p".to_string()],
            std::path::Path::new("/tmp/prompt.txt"),
            std::path::Path::new("/home/user/repo"),
        );
        assert!(cmd.contains("cd '/home/user/repo'"));
        assert!(cmd.contains("$(cat '/tmp/prompt.txt')"));
        assert!(cmd.contains("'claude' '-p'"));
    }
}
