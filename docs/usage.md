# Usage

How a run orders and resumes its targets, how it runs in tmux, and what it leaves behind. The [README](../README.md#usage) shows the everyday commands, and [cli-reference.md](cli-reference.md) lists every command and option.

## Processing order

Targets are processed **least-recently-modified first**: the repository whose newest file change is the oldest goes first. `defer` keeps its priority, and visibility groups public repositories ahead of private ones **only when at least one `[[scan]]` sets `public_first = true`**. The reordering happens within those groups, and it is a stable sort, so targets sharing a modification time keep their original order. Repositories whose modification time cannot be determined go last within their group. Targets that will [resume an interrupted session](#resuming-interrupted-sessions) are placed ahead of the modification-time order within their group. `token-burn run PATH...` keeps the order given on the command line.

When every `[[scan]]` sets `public_first = false` (or the config has no `[[scan]]` at all), visibility is left out of the sort key entirely, so the order depends only on `defer` and modification time. This matters together with `limit`: while visibility grouping is active, private repositories are never reached as long as at least `limit` public repositories remain queued.

Without this, the processing order was fixed, so every run took the first `limit` targets from the same list head. The already-processed cutoff (`skip_within`, or the previous reset) is an absolute time window, so once a run falls outside it the whole history is invalidated at once and the same head repositories are picked again — while the tail is never reached.

The order is based on the repository's own last file modification time rather than the recorded processing time, so a run that was cut short by a rate limit (and therefore changed nothing) is not treated as progress. The timestamp comes from the newest mtime among files listed by `git ls-files`, which naturally excludes build artifacts and `.gitignore`d paths while still picking up uncommitted edits. `list` and `run` print it next to each target as `(modified: ...)` so the resulting order can be verified at a glance.

## Resuming interrupted sessions

When a Claude Code task ends because the account hit a rate limit (for example `You've hit your session limit · resets 2:30pm (Asia/Tokyo)`), token-burn saves that session's ID. The next `token-burn run` that picks the same repository with the same agent continues the interrupted session with `claude --resume <session_id>` and a continuation prompt instead of starting the work over:

1. `token-burn run` — a task is cut off by the rate limit. It is still reported as failed, its session ID is saved, and the end-of-run summary says so (`↻ Saved 1 interrupted session — run the same command again to continue it`, with the reset time when known).
2. Run the same command again: `token-burn run`, or `token-burn run ~/GitHub/my-repo` for a single repository. If the interruption recorded when the five-hour window resets and that time has not come yet, the whole run starts paused until then — the same pause a run uses when the five-hour window fills up, since that window belongs to the account and would reject the other tasks too. It stops instead if waiting would pass the deadline, and the execution plan shows the hold.
3. The target is listed with a `↻ resume` line (short session ID and when it was rate limited), moved to the front of its group, and continues where the interrupted session stopped.

A saved session is resumed only when all of the following hold. Otherwise the target starts a new session, `list` / `run` show why the saved session was not used, and the unused saved session is discarded right before the new session starts. Keeping it would let a later run go back to that older session if the new one ended without being saved (for example on a retryable error).

- Resuming is enabled: neither `--no-resume` nor `--fresh` is given, and `resume_interrupted` is not `false`.
- The agent's `command` does not pass its own session flags (`--resume` / `-r`, `--continue` / `-c`, `--session-id`, `--fork-session`, `--no-session-persistence`, `--from-pr`). If it does, an added `--resume` would conflict with them, so interrupted sessions of that agent are neither saved nor resumed (a session saved earlier is listed with the reason it is not used).
- The target's prompt is unchanged since the interruption. Continuing old instructions after you edited them makes no sense.
- No agent within the current [`dedup_scope`](configuration.md#sharing-processed-target-history-across-agents) has processed the target since the interruption — with `dedup_scope = "global"`, for example, another account may have finished it in the meantime.

There is no fixed expiry. Claude Code deletes old transcripts according to its own retention settings, so whether a session still exists is found out by resuming it:

| Outcome of the resumed task | Saved session |
|-----------------------------|---------------|
| Success | Removed; the target is recorded in `state.json` |
| Rate-limited again | Saved again with the new interruption time and a cleared failure count, so the next run continues the same session |
| The session no longer exists | Removed, and a new session starts right away with the original prompt in the same task. That attempt is logged to `<NNNN>_<name>.fresh.jsonl` / `.fresh.log` next to the original log |
| Any other failure, including a crash that leaves no result | Kept, since failures such as an expired login are not the session's fault. After 3 failed resume attempts it is dropped, and the next run starts fresh |
| Retryable error or cancellation (Ctrl-C) | Kept unchanged |

The built-in continuation prompt (in English) tells Claude that the previous session was cut off by a rate limit and this one continues it; to check the repository first (`git status`, the current branch, `git worktree list`, uncommitted changes); that subagents and background tasks that were running have stopped, so their results must be verified rather than assumed; not to blindly repeat operations that already completed, such as commits, pushes, and releases; and then to finish the rest of the original instructions. Replace it with [`[prompts].resume`](configuration.md#prompts).

Only `claude` agents are covered, and only when the task ended on a rate limit. Retryable errors (such as `API Error: Connection closed mid-response`), crashes, cancellations, and logging-pipeline failures still start over on the next run, and Codex sessions are not resumed. Sessions are saved per agent: a transcript lives in that account's `CLAUDE_CONFIG_DIR`, so another account cannot continue it.

Resumable targets are moved to the front of their group. `defer` and `public_first` grouping still come first, but within a group resumable targets go ahead of the least-recently-modified order: an interrupted repository was being modified right up to the interruption, so oldest-first ordering alone would push it to the end, where `limit` could drop it before it is ever resumed. `token-burn run PATH...` keeps the command-line order and still resumes. For a resumed task, the execution plan shows `Resume: <session id>` and the continuation prompt as its prompt, and the `-i` picker marks the row with `↻`.

Saved sessions are stored in `resume.json` next to `state.json` (default `~/.config/token-burn/resume.json`) and updated with the same sidecar lock and atomic rename. They live in a separate file so that `state.json` keeps its format and older token-burn versions can still read it. `--no-resume` ignores saved sessions for one run, and `resume_interrupted = false` turns the feature off entirely.

## Running in tmux

- **Parallel execution**: Runs multiple prompts concurrently in tmux split panes with progress monitor
- **Self-closing run**: Workers close their own pane as soon as they run out of tasks, and the monitor tears down the tmux session once everything is processed — no Ctrl-C needed. The final tally and log path are reprinted on the terminal you started from
- **Detach-safe tmux runtime**: Keeps worker scripts and queues when you detach, so background tasks continue safely until the tmux session ends
- **Failure-safe tmux startup**: Removes the partially created session and temporary runtime directory if pane construction fails

What the monitor pane and the worker panes show is described in [monitor.md](monitor.md).

## Dry run

- **Dry run**: Preview execution plan without running commands
- **Credential-safe command display**: Redacts environment assignments and common credential option values as `<redacted>` in dry-run plans and ai-usage startup errors while executing the original values unchanged. Only `KEY=VALUE` pairs that precede the executable (the `env FOO=1 cmd` prefix) count as environment assignments — subcommand options such as `codex -c model='gpt-5.3-codex'` or the auto-injected `-c approval_policy=never` stay visible, since hiding them would defeat the point of a dry run

## Logs and state

- **Collision-safe logs**: Per-task logs are numbered to avoid overwrite when display names collide
- **Logging pipeline safety**: Marks a task failed if `format-stream`, `tee`, or raw jsonl capture fails instead of recording it as completed. A target directory deleted or renamed between scan and execution is reported accurately as `target directory is unavailable` instead of an unrelated logging pipeline failure
- **Concurrent-safe state**: Parallel workers update `state.json` with atomic rename under a stable sidecar lock file; malformed or unreadable existing state aborts the update without overwriting previously recorded history

Logs are written under `report_dir` (default `~/Documents/token-burn`), and report directories older than `cleanup_after` are deleted automatically; see [Settings](configuration.md#settings).
