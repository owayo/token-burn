# CLI reference

Every command and option of `token-burn`. The [README](../README.md#usage) shows the everyday commands.

## Commands

| Command | Description |
|---------|-------------|
| `run` | Execute token consumption (default) |
| `list` | List target directories in processing order (ignores `--limit`, does not execute) |
| `status` | Show agent reset status |
| `init` | Initialize config file and prompt templates |
| `clean` | Clean up old report directories |

`init` also accepts `--force` (`-f`) to overwrite existing files without confirmation.

`clean` accepts `--older-than` to override the configured `cleanup_after` duration (e.g., `--older-than 3d`).

## Options

| Option | Short | Description |
|--------|-------|-------------|
| `--config <PATH>` | `-c` | Config file path (default: `~/.config/token-burn/config.toml`) |
| `--agent <NAME>` | | Force specific agent |
| `--dry-run` | `-n` | Preview without executing |
| `--fresh` | | Ignore saved state — both the processed history and saved interrupted sessions — and process all targets from scratch |
| `--no-resume` | | Start every target in a new session, ignoring saved interrupted sessions (processed targets are still skipped) |
| `--limit <N>` | `-l` | Maximum number of targets to process (`N >= 1`) |
| `--no-limit` | | Process all targets without limit |
| `--workers <N>` | `-w` | Number of concurrent workers (`N >= 1`, overrides `parallelism`) |
| `--interactive` | `-i` | Pick the targets and their execution order in a TUI before running (`run` only, requires a TTY) |
| `--public-only` | | Process only repositories detected as public |
| `--dedup-scope <SCOPE>` | | How widely processed-target history is shared: `global` / `provider` / `agent` (overrides `dedup_scope`) |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

`--no-resume` ignores [saved interrupted sessions](usage.md#resuming-interrupted-sessions) for a single run while processed targets are still skipped. A target with a saved session starts a new session instead, and the saved session is discarded right before that new session starts — from then on its context is older than the repository. New interruptions during the run are still saved. `--fresh` goes further and ignores both the processed history and the saved sessions.

`--dedup-scope` overrides the configured [`dedup_scope`](configuration.md#sharing-processed-target-history-across-agents) for a single run. Use `--dedup-scope agent` to opt out of sharing and let this account re-visit repositories another account already processed.

`--workers` overrides the configured `parallelism` for a single run. The number of workers that actually start is capped by the number of tasks, and the effective value is shown as `Workers:` in the execution plan (visible with `--dry-run`).

`--interactive` opens a picker before the run. Every candidate is listed — not only the first `limit` — with the first `limit` rows pre-selected, so pressing Enter runs exactly what a non-interactive run would. Keys: `↑↓` / `j` `k` to move, `Space` to toggle, `J` / `K` (or `Shift+↑↓`) to move a row and change the order, `a` / `n` to select all or none, `g` / `G` for top and bottom, `Enter` to run, `q` / `Esc` to cancel. The number shown on each selected row is the order workers will process it in, and rows that will resume an interrupted session are marked with `↻`. It needs a real terminal, so it errors out when stdin or stdout is redirected; combine it with `--dry-run` to review the plan without executing.

## Target paths

When you pass one or more `PATH` arguments to `run`, scan discovery and state-based skipping are bypassed for those directories. Equivalent paths such as `repo` and `./repo` are normalized and deduplicated, so the same directory is never executed twice in a single run. Saved interrupted sessions still apply, so after a rate limit, re-running the same `token-burn run PATH...` continues the interrupted session.
