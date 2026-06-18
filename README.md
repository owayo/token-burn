<p align="center">
  <img src="docs/images/app.png" width="128" alt="token-burn">
</p>

<h1 align="center">token-burn</h1>

<p align="center">
  <strong>CLI tool to consume AI coding assistant tokens before weekly reset</strong>
</p>

<p align="center">
  <a href="https://github.com/owayo/token-burn/actions/workflows/ci.yml">
    <img alt="CI" src="https://github.com/owayo/token-burn/actions/workflows/ci.yml/badge.svg?branch=main">
  </a>
  <a href="https://github.com/owayo/token-burn/releases/latest">
    <img alt="Version" src="https://img.shields.io/github/v/release/owayo/token-burn">
  </a>
  <a href="LICENSE">
    <img alt="License" src="https://img.shields.io/github/license/owayo/token-burn">
  </a>
</p>

<p align="center">
  English | <a href="README.ja.md">日本語</a>
</p>

---

## Overview

Claude Code / Codex CLI tokens reset weekly with no rollover. Inspired by the Japanese *mottainai* (もったいない) spirit — the belief that waste is something to be avoided — **token-burn** puts those remaining tokens to work. It runs your prompts across repositories in parallel before the reset deadline — code reviews, bug hunts, refactoring, test improvements, or anything else you define. When the reset time arrives, token-burn stops starting new tasks and waits for the tasks already running to finish.

<p align="center">
  <img src="docs/images/screenshot.png" width="800" alt="token-burn running">
</p>

<p align="center">
  <img src="docs/images/deadline.png" width="800" alt="Deadline reached — waiting for tasks to finish">
</p>

## Features

- **Auto-discovery**: Scans directories for git repos, filters by username in remote URL
- **Multiple scan sources**: Define separate scan configs for GitHub, GitLab, etc.
- **Duplicate-safe scan merge**: If multiple scan sources find the same directory, it is processed only once
- **Visibility-aware**: Prioritizes public repositories over private ones (matched by remote repository name)
- **Multi-agent**: Supports Claude Code, Codex CLI, and custom agents
- **ai-usage integration**: Derives reset times from real usage data via `ai-usage --json` (with the configured fixed-schedule calculation kept as a fallback)
- **Usage-rate gate**: When ai-usage integration is enabled, re-checks each agent's real utilization (`weekly` / `five_hour`) after every task and stops starting new tasks once `rate_limit_threshold` is reached — extending threshold-based auto-stop to `codex`, not just Claude Code's in-task `rate_limit_event`
- **Monitor usage panel**: When ai-usage integration is enabled, the tmux monitor pane shows `ai-usage --statusline --logos` (each account's 5h / weekly utilization bars) refreshed every 10 seconds, rendered from a cached `--input` snapshot alongside the per-second progress bar
- **Multi-account expansion**: Expands a single agent across multiple accounts (e.g. `claude` → `claude-work` / `claude-home`), each launched with its own environment and tracked separately in `state.json`
- **Smart scheduling**: Automatically selects the agent closest to its reset deadline
- **Deadline-aware stop**: Stops starting new tasks when the reset time arrives and waits for current tasks to finish
- **Parallel execution**: Runs multiple prompts concurrently in tmux split panes with progress monitor
- **Detach-safe tmux runtime**: Keeps worker scripts and queues when you detach, so background tasks continue safely until the tmux session ends
- **Unattended Claude execution**: Automatically disallows Claude Code's `AskUserQuestion` tool so token-burn jobs do not block waiting for interactive answers
- **Sub-agent monitoring**: Real-time start, progress, status updates, and completion notifications for Claude Code team/agent tasks
- **System notification visibility**: Shows Claude Code system notifications such as stop-hook errors, plus hook diagnostics when `hook_progress` / `hook_response` include stderr or output
- **Richer tool details**: Shows `Read` offset/limit, `Edit` replace-all state, `Bash` timeout/background/sandbox-disabled state, `Agent` background state, `Grep`/`Glob` output mode/type, ignore-case, only-matching, multiline, glob, head/context/offset limits, delay/reason for `ScheduleWakeup`, URL/prompt summary for `WebFetch`, query/domain filters for `WebSearch`, query/`max_results` for `ToolSearch`, monitor description/timeout/condition/persistent state for `Monitor`, stopped task ID(s) and reason for `TaskStop`, `TaskList` calls, task ID for `TaskGet`, task ID/block/timeout for `TaskOutput`, `TaskCreate` subject/description/active form, `TaskUpdate` task ID/status/owner/subject/description, `SendMessage` summaries, legacy `AskUserQuestion` prompts/options when present, Tavily/Codex MCP model/sandbox/approval details, and library/query details for Context7 MCP tools
- **Sub-agent stop visibility**: `task_notification` events with `status="stopped"` (e.g. forced via `TaskStop`) are now surfaced in the live monitor; missing usage metrics are omitted instead of being shown as zero
- **Tool error summary**: When a `tool_result` is `is_error:true`, the live monitor appends a short, single-line summary (truncated to 120 characters, with single-line or multi-line `<tool_use_error>` wrappers stripped) so the cause of a failed tool call is visible without opening the jsonl
- **Tool result metadata**: Surfaces important top-level `tool_use_result` metadata such as truncated output, applied limits, stale-read hints, auto-backgrounding, clamped wakeups, persisted output size, return-code interpretation, Agent duration/token/tool counts, sub-agent type (`agent:`), resolved model (`model:`), and sub-agent edited line counts (`edits:+added/-removed`), Grep/ToolSearch result counts and mode, WebSearch result counts/search count/duration, WebFetch HTTP status code and response size (`http:200 OK`, `bytes:120.2KB`), Read partial-read line ratios (`lines:<n>/<total>`) and token-cap truncation (`truncated:token-cap`), git commit operations (sha/kind), task counts/task IDs, TaskOutput retrieval status, readable Agent output files, Monitor timeout/persistent state, TaskUpdate status transitions and changed fields beyond status (`updated:<field1>,<field2>`), async Agent launches (`async` when `run_in_background=true`), ScheduleWakeup scheduled time, and Skill command names with allowed-tool counts (`allowed-tools:<n>`)
- **Logging pipeline safety**: Marks a task failed if `format-stream`, `tee`, or raw jsonl capture fails instead of recording it as completed
- **Per-model usage**: Displays token usage, cost, cache read/creation tokens, web search counts, and the model's context window / max output limits (e.g. `ctx:1M`, `max_out:64K`) per model in the result summary
- **API timing**: Shows API response time, time to first token (`ttft`), time to first stream token (`stream:`, the pure streaming latency excluding queue/retry waits), and time-to-request (`req:<n>ms`) alongside wall-clock duration
- **Fast mode indicator**: Shows fast mode state when active
- **Terminal reason & permission denials**: Surfaces non-`completed` `terminal_reason` and denied tool call count/tool names in the result summary
- **Result metadata**: Displays `usage.service_tier`, `usage.speed`, non-empty inference geo, iteration count, and result origin kind when present
- **Rate limit alerts**: Displays utilization warnings, rejected request notifications, allowed-event reset/overage reset details when present, and the server-side warning threshold that was crossed (e.g. `warning at 90%`) for `allowed_warning` events; auto-stops when the configured threshold is exceeded
- **API retry visibility**: Shows retry attempts with error details during transient failures
- **Collision-safe logs**: Per-task logs are numbered to avoid overwrite when display names collide
- **Prompt files**: Prompts can be `.md` files or inline strings
- **Resume**: Automatically skips already-processed directories; configurable skip duration
- **Concurrent-safe state**: Parallel workers update `state.json` atomically with file locking
- **Dry run**: Preview execution plan without running commands

## Requirements

- **OS**: macOS
- **tmux**: Required for split-pane execution
- **Rust**: 1.85+ (for building from source)
- **gh CLI**: Required for repository visibility detection
- **Claude Code** and/or **Codex CLI**: At least one agent must be installed

## Installation

### Homebrew (macOS/Linux)

```bash
brew install owayo/token-burn/token-burn
```

### From Source

```bash
git clone https://github.com/owayo/token-burn.git
cd token-burn
make install
```

### From GitHub Releases

Download the latest binary from [Releases](https://github.com/owayo/token-burn/releases).

#### macOS (Apple Silicon)

```bash
curl -L https://github.com/owayo/token-burn/releases/latest/download/token-burn-aarch64-apple-darwin.tar.gz | tar xz
sudo mv token-burn /usr/local/bin/
```

#### macOS (Intel)

```bash
curl -L https://github.com/owayo/token-burn/releases/latest/download/token-burn-x86_64-apple-darwin.tar.gz | tar xz
sudo mv token-burn /usr/local/bin/
```

## Usage

### Quick Start

```bash
# Initialize config file and default prompt
token-burn init

# Check agent reset status
token-burn status

# Preview execution plan
token-burn run -n

# Run only specific repositories
token-burn run ~/GitHub/repo-a ./repo-b

# Run token consumption
token-burn run
```

### Commands

| Command | Description |
|---------|-------------|
| `run` | Execute token consumption (default) |
| `status` | Show agent reset status |
| `init` | Initialize config file and prompt templates |
| `clean` | Clean up old report directories |

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--config <PATH>` | `-c` | Config file path (default: `~/.config/token-burn/config.toml`) |
| `--agent <NAME>` | | Force specific agent |
| `--dry-run` | `-n` | Preview without executing |
| `--fresh` | | Ignore saved state and process all targets |
| `--limit <N>` | `-l` | Maximum number of targets to process (`N >= 1`) |
| `--no-limit` | | Process all targets without limit |
| `--public-only` | | Process only repositories detected as public |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

`init` also accepts `--force` (`-f`) to overwrite existing files without confirmation.

`clean` accepts `--older-than` to override the configured `cleanup_after` duration (e.g., `--older-than 3d`).

When you pass one or more `PATH` arguments to `run`, scan discovery and state-based skipping are bypassed for those directories. Equivalent paths such as `repo` and `./repo` are normalized and deduplicated, so the same directory is never executed twice in a single run.

## Configuration

Default config location: `~/.config/token-burn/config.toml`

Run `token-burn init` to generate a config template.

### Settings

```toml
[settings]
parallelism = 3
skip_within = "7d"    # optional
```

| Field | Description | Example |
|-------|-------------|---------|
| `parallelism` | Number of concurrent tasks | `3` |
| `skip_within` | Skip directories processed within this duration | `"7d"`, `"24h"`, `"1d12h"` |
| `cleanup_after` | Auto-delete report directories older than this duration | `"7d"` (default) |
| `report_dir` | Directory to save execution logs | `~/Documents/token-burn` (default) |
| `limit` | Maximum number of targets to process per run (`>= 1`) | `10` (default) |
| `rate_limit_threshold` | Auto-stop when rate limit utilization exceeds this percentage (`1-100`) | `95` (default) |

`skip_within` and `cleanup_after` accept duration strings using `d` (days), `h` (hours), `m` (minutes), and `s` (seconds). Invalid values are rejected when the config file is loaded. If `skip_within` is omitted, directories processed since the previous reset are skipped. Excessively large values are also rejected. Use `--fresh` to ignore saved state entirely.

`rate_limit_threshold` is enforced on two paths. During a task, Claude Code's stream-json `rate_limit_event` is monitored in real time and execution stops once the threshold is exceeded. In addition, when [ai-usage integration](#ai-usage-integration-optional) is enabled, after each task completes the agent's real utilization is re-checked against this threshold using the higher of the matching `(profile, provider)` pair's `weekly` and `five_hour` `used_percent` values; this applies to both `claude` and `codex` agents (the latter previously had no real-time monitoring).

State is stored in `<config-dir>/state.json` (same directory as the active config file) and updated atomically to avoid lost updates during parallel runs. With the default config path, this is `~/.config/token-burn/state.json`.

### Agents

```toml
[[agents]]
name = "claude"
command = ["claude", "--dangerously-skip-permissions", "--model", "opus"]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "Asia/Tokyo"
prompt = "prompts/test-coverage.md"  # optional

[[agents]]
name = "codex"
command = ["codex", "exec", "--full-auto", "-c", "model='gpt-5.3-codex'", "-c", "model_reasoning_effort='xhigh'"]
reset_weekday = "thursday"
reset_time = "09:00"
timezone = "Asia/Tokyo"
# prompt = "prompts/codex.md"
```

| Field | Description | Example |
|-------|-------------|---------|
| `name` | Agent identifier | `"claude"` |
| `command` | Command and arguments | `["claude"]` |
| `provider` | Provider name used to match `(profile, provider)` against `ai-usage --json` output. Required when ai-usage integration is enabled for the agent | `"claude"` |
| `env` | Environment variables applied when launching the agent (optional). Keys must match `[A-Za-z_][A-Za-z0-9_]*`; values are `~`-expanded. Merged with (and overridden by) a profile's `env` | `{ CLAUDE_CONFIG_DIR = "~/.config/claude-home" }` |
| `reset_weekday` | Reset day of week | `"monday"` |
| `reset_time` | Reset time (HH:MM) | `"09:00"` |
| `timezone` | IANA timezone | `"Asia/Tokyo"` |
| `prompt` | Agent-specific prompt (optional) | `"prompts/test-coverage.md"` |

`name` must not be empty. `command` must contain at least one element, and the first element must be a non-empty executable name. `prompt` overrides the global `[prompts].default` for this agent; target-level `prompt` takes highest priority.

`reset_weekday`, `reset_time`, and `timezone` are normally required. They may be omitted only when ai-usage integration is enabled for the agent **and** the effective `fallback` is not `fixed`, since in that case the fixed-schedule calculation is never used. Otherwise they are still required as the fallback schedule. See [ai-usage integration (optional)](#ai-usage-integration-optional).

**Prompt priority**: `[[targets]].prompt` > `[[agents]].prompt` > `[prompts].default`

**Claude auto-injected flags**: When the executable is `claude`, the following flags are enforced: `-p`, `--verbose`, `--output-format stream-json`, `--include-partial-messages`, and `--disallowedTools=AskUserQuestion`. Missing flags are appended automatically, an existing `--output-format` value is normalized to `stream-json` (including `--output-format=...` form), and an existing `--disallowedTools` / `--disallowed-tools` list is normalized and extended with `AskUserQuestion` when needed. The logging flags are required for proper log capture and progress monitoring; `AskUserQuestion` is denied so unattended token-burn jobs cannot stop on an interactive question. You do not need to include them in your config.

`reset_weekday` accepts: `monday` `tuesday` `wednesday` `thursday` `friday` `saturday` `sunday` (or short forms: `mon` `tue` `wed` `thu` `fri` `sat` `sun`)

### ai-usage integration (optional)

By default, each agent's reset deadline is computed from its fixed `reset_weekday` / `reset_time` / `timezone`. The optional `[ai_usage]` integration instead derives reset times from real usage data reported by an external `ai-usage --json` tool (from the selected window's `resets_at`). The fixed-schedule calculation is kept as a fallback, so token-burn never silently loses a deadline when live data is unavailable.

The integration also lets you expand a single agent across multiple accounts (profiles). For example, a `claude` agent referencing `["work", "home"]` expands into two agents, `claude-work` and `claude-home`, each launched with its own environment and tracked under its own key in `state.json`. A profile referenced alone keeps the agent's own name (e.g. a `codex` agent referencing only `["home"]` stays `codex`); the `<agent>-<profile>` suffix is added only when two or more profiles are referenced. This lets you define each account as a separate agent — handy when accounts launch via different wrapper commands — without redundant names, and keeps `state.json` keys stable.

```toml
[ai_usage]                # optional. If omitted or enabled = false, only the fixed weekday calculation is used
enabled = true
command = ["ai-usage", "--json"]   # default
window = "weekly"         # weekly | five_hour | nearest — window used to compute the deadline (default: weekly)
fallback = "fixed"        # fixed | skip | error — what to do when resolution fails (default: fixed)
state_window = "weekly"   # weekly | selected — window used for the processed-target cutoff (default: weekly)

[[ai_usage.profiles]]
name = "work"             # internal reference name (used in the expanded name <agent>-<name>)
profile = "Work"          # matched against the "profile" field of ai-usage --json output (case-sensitive)
env = { CLAUDE_CONFIG_DIR = "~/.config/claude-work" }  # env applied when launching this account (~-expanded)

[[ai_usage.profiles]]
name = "home"
profile = "Home"
env = { CLAUDE_CONFIG_DIR = "~/.config/claude-home" }

[[agents]]
name = "claude"
provider = "claude"       # used to match (profile, provider) against ai-usage output. Required when ai-usage is enabled
command = ["claude"]
# env = { ... }           # optional base env; overridden by a profile's env on key collisions
reset_weekday = "monday"  # optional when ai-usage is enabled and fallback != fixed; required otherwise (used as fallback)
reset_time = "09:00"
timezone = "Asia/Tokyo"
[agents.ai_usage]
profiles = ["work", "home"]   # profile names to reference; multiple names expand into per-account agents
# window = "weekly"           # optional: override the global [ai_usage].window for this agent
# fallback = "fixed"          # optional: override the global [ai_usage].fallback for this agent
```

#### `[ai_usage]` (global)

| Field | Description | Default |
|-------|-------------|---------|
| `enabled` | Enable the integration. When omitted or `false`, only the fixed weekday calculation is used | `false` |
| `command` | Command and arguments used to query usage data (must emit JSON) | `["ai-usage", "--json"]` |
| `window` | Window whose `resets_at` is used to compute the deadline: `weekly`, `five_hour`, or `nearest` | `weekly` |
| `fallback` | Behavior when resolution fails: `fixed`, `skip`, or `error` | `fixed` |
| `state_window` | Window used for the processed-target cutoff: `weekly` or `selected` | `weekly` |

#### `[[ai_usage.profiles]]`

| Field | Description |
|-------|-------------|
| `name` | Internal reference name. Used in the expanded agent name `<agent>-<name>` and referenced from `[agents.ai_usage].profiles` |
| `profile` | Value matched against the `profile` field of `ai-usage --json` output (case-sensitive) |
| `env` | Environment variables applied when launching this account. Keys must match `[A-Za-z_][A-Za-z0-9_]*`; values are `~`-expanded. Merged into (and override) the agent's `env` |

#### `[agents.ai_usage]` (per agent)

| Field | Description |
|-------|-------------|
| `profiles` | Profile names (from `[[ai_usage.profiles]].name`) this agent uses. Multiple names expand the agent into one instance per account |
| `window` | Optional override of the global `[ai_usage].window` for this agent |
| `fallback` | Optional override of the global `[ai_usage].fallback` for this agent |

#### Behavior

- At run time, each agent is expanded across its referenced profiles. For example, `claude` with `["work", "home"]` becomes two agents, `claude-work` and `claude-home`, each launched with its profile's `env`.
- Expanded names are also used as `state.json` keys, so processed-target state is tracked separately per account.
- `ai-usage --json` is invoked only once per process.
- The reset time is taken from the `resets_at` value of the selected window (e.g. `weekly`) for the matching `(profile, provider)` pair.
- When resolution fails — the command is missing or fails, no matching `(profile, provider)` is found, the response reports `ok: false`, or the selected window is null — the configured `fallback` applies:
  - `fixed`: fall back to the fixed weekday calculation (the schedule source is shown as `fixed fallback: <reason>`).
  - `skip`: drop the affected agent from the candidate list.
  - `error`: stop with an error.
- `status` and `run` display each agent's schedule **source** (`ai-usage (weekly)`, `fixed`, or `fixed fallback`) so token-burn never falls back silently.
- **Post-task usage gate**: After each task completes, token-burn re-queries `ai-usage --json` and compares the matching `(profile, provider)` pair's `weekly` and `five_hour` `used_percent` against `rate_limit_threshold`. If either window meets or exceeds the threshold, a stop file is created so no further tasks start. This applies to both `claude` and `codex` agents, giving `codex` (which has no in-task `rate_limit_event` stream) a real-utilization stop signal.
- The `ai-usage --json` output is cached with a short TTL (20 seconds) so parallel workers do not each spawn a redundant query. The stop-file creation is idempotent and safe to call concurrently from multiple workers.
- The usage gate is **fail-closed**: if the query fails (utilization cannot be confirmed), tasks are stopped to stay on the safe side. When no matching entry is found or `used_percent` is missing, execution continues instead, to avoid over-stopping on incomplete data.

### Auto-scan (multiple sources)

```toml
[[scan]]
base_dirs = ["~/GitHub"]
username = "yourname"
public_first = true
exclude = ["archived-project"]

[[scan]]
base_dirs = ["~/git"]
username = "yourname"
recursive = true
public_first = false
```

| Field | Description | Default |
|-------|-------------|---------|
| `base_dirs` | Directories to scan for git repositories | (required) |
| `username` | Filter repos whose remote URL owner matches this username | (none — all repos included) |
| `public_first` | Sort public repositories before private ones so they are processed first | `true` |
| `recursive` | Recurse into subdirectories to find nested git repositories | `false` |
| `exclude` | Directory names to skip during scan | `[]` |

When `username` is set, visibility lookup uses the repository name parsed from each repository's `origin` remote URL (case-insensitive), so local directory names can differ from remote repository names.

When `username` is not set, repositories are included even if they do not have an `origin` remote. In that case visibility remains `Unknown`.

Symlinks are skipped during directory scanning to prevent infinite recursion from circular links.

If multiple `[[scan]]` entries discover the same repository directory, scan results are deduplicated by directory path so the same repository is not executed twice in a single run.

Directory paths are normalized to absolute paths before deduplication and state tracking, so equivalent relative paths such as `repo` and `./repo` are treated as the same target.

The same normalization and deduplication rule also applies when `token-burn run PATH...` is used to force specific directories.

### Prompts

Prompt values ending with `.md` are read as file paths. Relative paths resolve from the config directory.

```toml
[prompts]
default = "prompts/default.md"
```

### Explicit targets (merged with scan results)

```toml
[[targets]]
directory = "~/GitHub/important-project"
prompt = "prompts/test-coverage.md"
```

| Field | Description |
|-------|-------------|
| `directory` | Path to the target directory (required). Must be an existing directory |
| `prompt` | Prompt override for this target. If omitted, `[prompts].default` is used |

If a target's `directory` matches a scan result, the explicit target takes precedence.

## Development

```bash
# Build
make build

# Run tests
make test

# Run clippy and format check
make check

# Build release
make release
```

## License

[MIT](LICENSE)
