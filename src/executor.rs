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
    let mut agent = agent.clone();
    ensure_required_flags(&mut agent);
    ExecutionPlan {
        agent,
        tasks: targets,
    }
}

/// Auto-inject required flags for known agents.
/// For `claude`, `--verbose`, `--output-format stream-json`, and `--include-partial-messages`
/// are mandatory for proper log capture and must always be present.
fn ensure_required_flags(agent: &mut Agent) {
    let executable = agent.command.first().map(|s| s.as_str()).unwrap_or("");
    if executable != "claude" {
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

    // Create report directory for this run
    let now = chrono::Local::now();
    let run_dir = report_dir.join(format!(
        "{}_{}",
        now.format("%Y%m%d_%H%M%S"),
        plan.agent.name
    ));
    std::fs::create_dir_all(&run_dir)?;

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

        // Signal handler: set flag only; actual handling is in the error check after each command
        script += concat!(
            "CURRENT_FAILED_MARKER=\"\"\n",
            "CANCELLED=0\n",
            "handle_cancel() {\n",
            "  CANCELLED=1\n",
            "  if [ -n \"$CURRENT_FAILED_MARKER\" ]; then touch \"$CURRENT_FAILED_MARKER\"; fi\n",
            "}\n",
            "trap handle_cancel INT TERM\n",
        );

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
            // Write prompt to a temp file and pass it via command substitution
            let prompt_file = tmp_dir.join(format!("prompt-{}.txt", idx));
            std::fs::write(&prompt_file, &task.prompt)?;
            let cmd_str = build_shell_command(&plan.agent.command, &prompt_file, &task.directory);
            let done_marker =
                shell_escape(&marker_dir.join(format!("done-{}", idx)).to_string_lossy());
            let failed_marker =
                shell_escape(&marker_dir.join(format!("failed-{}", idx)).to_string_lossy());
            let error_prefix = shell_escape(&format!("[{}] ", task.display_name));
            let log_base = task_log_base(idx, &task.display_name);
            let log_file =
                shell_escape(&run_dir.join(format!("{}.log", log_base)).to_string_lossy());

            // Set current task's failed marker for signal handler
            script += &format!("CURRENT_FAILED_MARKER={}\n", failed_marker);

            script += &build_task_header_script(idx, total, &task.display_name);
            let is_claude = plan
                .agent
                .command
                .first()
                .map(|s| s.as_str() == "claude")
                .unwrap_or(false);
            if is_claude {
                // Tee raw JSON to .jsonl, then pipe through format-stream for readable output to .log
                let jsonl_file = shell_escape(
                    &run_dir
                        .join(format!("{}.jsonl", log_base))
                        .to_string_lossy(),
                );
                let fmt_cmd = shell_escape(&exe_path.to_string_lossy());
                script += &format!(
                    "{} 2>&1 | tee {} | {} format-stream 2>&1 | tee {}\n",
                    cmd_str, jsonl_file, fmt_cmd, log_file
                );
            } else {
                // Non-claude agents: pipe directly to log file
                script += &format!("{} 2>&1 | tee {}\n", cmd_str, log_file);
            }
            script += "CMD_EXIT=${PIPESTATUS[0]}\n";
            // Clear signal handler target (exit code captured, normal flow handles it)
            script += "CURRENT_FAILED_MARKER=\"\"\n";
            // Check exit code
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
                    "    touch {failed}; touch {wdone}\n",
                    "    printf '\\033]2;Worker {w} error\\033\\\\'\n",
                    "    echo '━━━ Error - stopped ━━━'\n",
                    "    exec sleep infinity\n",
                    "  fi\n",
                    "else\n",
                    "  {mark}\n",
                    "  touch {done}\n",
                    "fi\n",
                ),
                prefix = error_prefix,
                error = error_file,
                failed = failed_marker,
                wdone = worker_done_marker,
                w = w + 1,
                mark = format!(
                    "{} mark {} {} {}",
                    shell_escape(&exe_path.to_string_lossy()),
                    shell_escape(&plan.agent.name),
                    shell_escape(&task.directory.to_string_lossy()),
                    shell_escape(&state_file.to_string_lossy()),
                ),
                done = done_marker,
            );
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
        &run_dir,
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

    // Enable scrollback and mouse support
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-t", session, "history-limit", "50000"])
        .status();
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-t", session, "mouse", "on"])
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

    // Strip ANSI escape codes from log files
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
            printf "\r\033[2K ❌ Completed with failures: %d succeeded / %d failed\n" "$DONE" "$FAILED"
        else
            printf "\r\033[2K ✅ All %d/%d tasks completed!\n" "$DONE" "$TOTAL"
        fi
        echo ""
        echo " 📁 Logs: $REPORT_DIR"
        echo ""
        echo " Press Ctrl-C to close session."
        exec sleep infinity
    fi

    # If stopped, check if all workers have finished
    if [ $STOPPED -eq 1 ]; then
        WORKERS_DONE=$(find "$MARKER_DIR" -name 'worker-done-*' 2>/dev/null | wc -l | tr -d ' ')
        if [ "$WORKERS_DONE" -ge "$WORKER_COUNT" ]; then
            printf "\r\033[2K ⏹ Stopped: %d/%d processed (%d failed)\n" "$PROCESSED" "$TOTAL" "$FAILED"
            echo ""
            echo " 📁 Logs: $REPORT_DIR"
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
        printf "\r\033[2K ⏱ %dd %02dh %02dm %02ds  [%s] %d/%d (%d%%, fail:%d)" \
            "$D" "$H" "$M" "$S" "$BAR" "$PROCESSED" "$TOTAL" "$PCT" "$FAILED"
    else
        printf "\r\033[2K ⏳ Stopping...  [%s] %d/%d (%d%%, fail:%d)" \
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
        report_dir = report_dir_escaped,
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
    // Pass prompt as argument via command substitution $(cat file)
    // Using stdin pipe doesn't work reliably with claude -p
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
        if path.extension().map(|e| e == "log").unwrap_or(false) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let cleaned = strip_ansi(&content);
                let _ = std::fs::write(&path, cleaned);
            }
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
                    // CSI sequence: \x1b[...final_byte (0x40-0x7E)
                    chars.next();
                    for ch in chars.by_ref() {
                        if is_csi_final(ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC sequence: \x1b]...ST (ST = \x1b\\ or \x07)
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
                    // Other escape sequences (e.g., \x1b(B) — skip next char
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
            std::path::Path::new("/tmp/report dir"),
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
        assert!(agent
            .command
            .contains(&"--include-partial-messages".to_string()));
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
        assert!(agent
            .command
            .contains(&"--output-format=stream-json".to_string()));
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
}
