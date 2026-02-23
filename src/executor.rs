use anyhow::{Context, Result};
use colored::Colorize;
use std::time::Duration;

use crate::config::Agent;
use crate::display;
use crate::scanner::{ResolvedTarget, Visibility};

pub struct ExecutionPlan {
    pub agent: Agent,
    pub tasks: Vec<ResolvedTarget>,
}

pub fn build_plan(agent: &Agent, targets: Vec<ResolvedTarget>) -> ExecutionPlan {
    ExecutionPlan {
        agent: agent.clone(),
        tasks: targets,
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
) -> Result<()> {
    // Check tmux is available
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .context("tmux is not installed")?;

    let session = "token-burn";

    // Kill existing session if any
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", session])
        .output();

    let tmp_dir = std::env::temp_dir().join("token-burn");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir)?;

    let total = plan.tasks.len();

    // Distribute tasks round-robin to workers
    let worker_count = parallelism.min(total);
    let mut worker_tasks: Vec<Vec<(usize, &ResolvedTarget)>> = vec![vec![]; worker_count];
    for (i, task) in plan.tasks.iter().enumerate() {
        worker_tasks[i % worker_count].push((i + 1, task));
    }

    // Generate worker scripts (each creates a marker file per completed task)
    let marker_dir = tmp_dir.join("markers");
    std::fs::create_dir_all(&marker_dir)?;

    let exe_path =
        std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("token-burn"));
    let stop_file = tmp_dir.join("stop");

    let mut script_paths = Vec::new();
    for (w, tasks) in worker_tasks.iter().enumerate() {
        let script_path = tmp_dir.join(format!("worker-{}.sh", w));
        let mut script = String::from("#!/bin/bash\n");
        let worker_done_marker = shell_escape(
            &marker_dir
                .join(format!("worker-done-{}", w))
                .to_string_lossy(),
        );
        let stop_file_escaped = shell_escape(&stop_file.to_string_lossy());
        let error_file = shell_escape(&marker_dir.join(format!("error-{}", w)).to_string_lossy());
        for (i, (idx, task)) in tasks.iter().enumerate() {
            let idx = *idx;
            // Check stop signal before starting next task (skip first task)
            if i > 0 {
                script += &format!(
                    "if [ -f {} ]; then printf '\\033]2;Worker {} stopped\\033\\\\'; echo '━━━ Stopped ━━━'; touch {}; exec sleep infinity; fi\n",
                    stop_file_escaped,
                    w + 1,
                    worker_done_marker,
                );
            }
            let cmd_str = build_shell_command(&plan.agent.command, &task.prompt, &task.directory);
            let done_marker =
                shell_escape(&marker_dir.join(format!("done-{}", idx)).to_string_lossy());
            let failed_marker =
                shell_escape(&marker_dir.join(format!("failed-{}", idx)).to_string_lossy());
            let error_prefix = shell_escape(&format!("[{}] ", task.display_name));

            script += &build_task_header_script(idx, total, &task.display_name);
            script += &format!("{}\n", cmd_str);
            // Check exit code - stop worker on failure
            script += &format!(
                concat!(
                    "if [ $? -ne 0 ]; then ",
                    "ERROR_MSG=$(tmux capture-pane -t \"$TMUX_PANE\" -p -J -S -10 | grep -v '^$' | tail -1); ",
                    "printf '%s%s\\n' {prefix} \"$ERROR_MSG\" > {error}; ",
                    "touch {failed}; touch {wdone}; ",
                    "printf '\\033]2;Worker {w} error\\033\\\\'; ",
                    "echo '━━━ Error - stopped ━━━'; ",
                    "exec sleep infinity; ",
                    "fi\n",
                ),
                prefix = error_prefix,
                error = error_file,
                failed = failed_marker,
                wdone = worker_done_marker,
                w = w + 1,
            );
            script += &format!(
                "{} mark {} {} {}\n",
                shell_escape(&exe_path.to_string_lossy()),
                shell_escape(&plan.agent.name),
                shell_escape(&task.directory.to_string_lossy()),
                shell_escape(&state_file.to_string_lossy()),
            );
            script += &format!("touch {}\n", done_marker);
            script += "echo ''\n";
        }
        script += &format!("printf '\\033]2;Worker {} done\\033\\\\'\n", w + 1);
        script += "echo '━━━ All tasks completed ━━━'\n";
        script += &format!("touch {}\n", worker_done_marker);
        script += "exec sleep infinity\n";
        std::fs::write(&script_path, &script)?;
        std::process::Command::new("chmod")
            .args(["+x", &script_path.to_string_lossy()])
            .output()?;
        script_paths.push(script_path);
    }

    // Generate monitor script for left pane
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
    );
    std::fs::write(&monitor_path, &monitor_script)?;
    std::process::Command::new("chmod")
        .args(["+x", &monitor_path.to_string_lossy()])
        .output()?;

    // Create tmux session with monitor (left pane)
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

    // Split right for first worker
    std::process::Command::new("tmux")
        .args([
            "split-window",
            "-h",
            "-t",
            session,
            &script_paths[0].to_string_lossy(),
        ])
        .status()?;

    // Add remaining workers as vertical splits in the right area
    for script in &script_paths[1..] {
        // Target the last pane (right side) for vertical split
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

    // Even out the right-side panes
    let _ = std::process::Command::new("tmux")
        .args(["select-layout", "-t", session, "main-vertical"])
        .status();

    // Set left pane (monitor) width to ~30%
    let _ = std::process::Command::new("tmux")
        .args(["resize-pane", "-t", &format!("{}:.0", session), "-x", "35%"])
        .status();

    // Enable pane border titles
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

    // Focus monitor pane
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

    // Attach (blocks until session ends or is killed)
    std::process::Command::new("tmux")
        .args(["attach-session", "-t", session])
        .status()
        .context("Failed to attach to tmux session")?;

    // Clean up
    let _ = std::fs::remove_dir_all(&tmp_dir);

    println!();
    println!("{}", "tmux session ended.".bold());
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
) -> String {
    let agent_escaped = shell_escape(agent_name);
    let command_escaped = shell_escape(command_str);
    let reset_escaped = shell_escape(reset_info);
    let marker_dir_escaped = shell_escape(&marker_dir.to_string_lossy());
    let session_escaped = shell_escape(session);
    let stop_file_escaped = shell_escape(&stop_file.to_string_lossy());

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
STOPPED=0
DISPLAYED_ERRORS=""

handle_signal() {{
    if [ $STOPPED -eq 0 ]; then
        STOPPED=1
        touch "$STOP_FILE"
        echo ""
        echo " ⏳ Waiting for current tasks to finish..."
        echo "    Press Ctrl-C again to force kill."
    else
        echo ""
        echo " Force killing session..."
        tmux kill-session -t "$SESSION" 2>/dev/null
        exit
    fi
}}
trap handle_signal INT TERM

printf '\033]2;token-burn\033\\'

END=$(($(date +%s) + DEADLINE))

echo "━━━━━━━━━━━━━━━━━━━━━━"
echo " 🔥 token-burn"
echo "━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo " Agent:   $AGENT"
echo " Command: $COMMAND"
echo " Reset:   $RESET"
echo " Tasks:   $TOTAL"
echo ""

while true; do
    NOW=$(date +%s)
    REMAINING=$((END - NOW))
    DONE=$(find "$MARKER_DIR" -name 'done-*' 2>/dev/null | wc -l | tr -d ' ')
    FAILED=$(find "$MARKER_DIR" -name 'failed-*' 2>/dev/null | wc -l | tr -d ' ')
    PROCESSED=$((DONE + FAILED))

    # Deadline check
    if [ $REMAINING -le 0 ] && [ $STOPPED -eq 0 ]; then
        STOPPED=1
        touch "$STOP_FILE"
        echo ""
        echo " ⚠ DEADLINE REACHED"
        echo " ⏳ Waiting for current tasks to finish..."
        echo "    Press Ctrl-C to force kill."
    fi

    # All tasks processed (including failures)
    if [ "$PROCESSED" -ge "$TOTAL" ]; then
        if [ "$FAILED" -gt 0 ]; then
            printf "\r ❌ Completed with failures: %d succeeded / %d failed          \n" "$DONE" "$FAILED"
        else
            printf "\r ✅ All %d/%d tasks completed!          \n" "$DONE" "$TOTAL"
        fi
        echo ""
        echo " Press Ctrl-C to close session."
        exec sleep infinity
    fi

    # If stopped, check if all workers have finished
    if [ $STOPPED -eq 1 ]; then
        WORKERS_DONE=$(find "$MARKER_DIR" -name 'worker-done-*' 2>/dev/null | wc -l | tr -d ' ')
        if [ "$WORKERS_DONE" -ge "$WORKER_COUNT" ]; then
            printf "\r ⏹ Stopped: %d/%d processed (%d failed)          \n" "$PROCESSED" "$TOTAL" "$FAILED"
            echo ""
            echo " Press Ctrl-C to close session."
            exec sleep infinity
        fi
    fi

    # Display new errors
    for f in $(find "$MARKER_DIR" -name 'error-*' 2>/dev/null); do
        EFILE=$(basename "$f")
        case "$DISPLAYED_ERRORS" in
            *"$EFILE"*) ;;
            *)
                echo ""
                echo " ❌ $(cat "$f")"
                DISPLAYED_ERRORS="$DISPLAYED_ERRORS $EFILE"
                ;;
        esac
    done

    # Progress bar
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
        printf "\r ⏱ %dd %02dh %02dm %02ds  [%s] %d/%d (%d%%%%, fail:%d)" \
            "$D" "$H" "$M" "$S" "$BAR" "$PROCESSED" "$TOTAL" "$PCT" "$FAILED"
    else
        printf "\r ⏳ Stopping...  [%s] %d/%d (%d%%%%, fail:%d)" \
            "$BAR" "$PROCESSED" "$TOTAL" "$PCT" "$FAILED"
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

fn build_shell_command(cmd_parts: &[String], prompt: &str, directory: &std::path::Path) -> String {
    let mut parts: Vec<String> = vec![format!("cd {}", shell_escape(&directory.to_string_lossy()))];
    let mut cmd: Vec<String> = cmd_parts.iter().map(|s| shell_escape(s)).collect();
    cmd.push(shell_escape(prompt));
    parts.push(cmd.join(" "));
    parts.join(" && ")
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
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
        assert!(script.contains("printf '\\033]2;%s\\033\\\\' '[1/3] repo'\\''; touch /tmp/pwn #'"));
        assert!(script.contains("echo '━━━ [1/3] repo'\\''; touch /tmp/pwn # ━━━'"));
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
        );

        assert!(script.contains("AGENT='ag\"$(touch /tmp/pwn)\"'"));
        assert!(script.contains("FAILED=$(find \"$MARKER_DIR\" -name 'failed-*'"));
        assert!(script.contains("PROCESSED=$((DONE + FAILED))"));
        assert!(script.contains("Completed with failures"));
    }

    #[test]
    fn shell_escape_escapes_single_quotes() {
        assert_eq!(shell_escape("a'b"), "'a'\\''b'");
    }
}
