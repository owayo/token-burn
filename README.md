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
- **Monitor usage panel**: When ai-usage integration is enabled, the tmux monitor pane shows `ai-usage --statusline --logos` (each account's 5h / weekly utilization bars) refreshed every 10 seconds, rendered from a cached `--input` snapshot alongside the per-second progress bar. The refresh returns as soon as `ai-usage` does, so it never freezes the pane and the progress bar keeps its per-second update
- **Multi-account expansion**: Expands a single agent across multiple accounts (e.g. `claude` → `claude-work` / `claude-home`), each launched with its own environment and tracked separately in `state.json`
- **Cross-account continuation**: `dedup_scope` lets one account resume where another stopped instead of re-visiting the same repositories, while still recording which account did the work — opt out per run with `--dedup-scope agent`
- **Credential-safe command display**: Redacts environment assignments and common credential option values as `<redacted>` in dry-run plans and ai-usage startup errors while executing the original values unchanged
- **Smart scheduling**: Automatically selects the agent closest to its reset deadline
- **Deadline-aware stop**: Stops starting new tasks when the reset time arrives and waits for current tasks to finish
- **Parallel execution**: Runs multiple prompts concurrently in tmux split panes with progress monitor
- **Self-closing run**: Workers close their own pane as soon as they run out of tasks, and the monitor tears down the tmux session once everything is processed — no Ctrl-C needed. The final tally and log path are reprinted on the terminal you started from
- **Detach-safe tmux runtime**: Keeps worker scripts and queues when you detach, so background tasks continue safely until the tmux session ends
- **Failure-safe tmux startup**: Removes the partially created session and temporary runtime directory if pane construction fails
- **Unattended Claude execution**: Automatically disallows Claude Code's `AskUserQuestion` tool so token-burn jobs do not block waiting for interactive answers
- **Sub-agent monitoring**: Real-time start, progress, status updates, and completion notifications for Claude Code team/agent tasks; `task_started` prefers the concrete `subagent_type`, failed notifications include their summary, and `task_updated` with `killed` is highlighted as a failure
- **System notification visibility**: Shows Claude Code system notifications such as stop-hook errors, plus hook diagnostics when `hook_progress` / `hook_response` include stderr or output
- **Long-running tool heartbeat**: Displays `tool_progress` elapsed time for long-running tools instead of leaving the monitor apparently idle
- **Refusal fallback visibility**: Displays `model_refusal_fallback` source/destination models and category without exposing the event's content or explanation
- **Richer tool details**: Shows `Read` offset/limit/view range, unparseable tool input length (`unparsed:<n> chars`, for the model's malformed JSON output or a stream truncated by a rate limit/disconnect), `Edit` replace-all state, `Bash` timeout/background/sandbox-disabled state, `BashOutput` target background bash id (`bash:<id>`) with optional filter, `Agent` background state, `Grep`/`Glob` output mode/type, ignore-case, only-matching, multiline, glob, head/context/offset limits, delay/reason for `ScheduleWakeup`, URL/prompt summary for `WebFetch`, query/domain filters for `WebSearch`, query/`max_results` for `ToolSearch`, monitor description/timeout/condition/persistent state for `Monitor`, stopped task ID(s) and reason for `TaskStop`, `TaskList` calls, task ID for `TaskGet`, task ID/block/timeout for `TaskOutput`, `Workflow` launch target (workflow name extracted from the inline script's `meta.name` with script size, or the named workflow / script path), `TaskCreate` subject/description/active form, `TaskUpdate` task ID/status/owner/subject/description, `SendMessage` summaries, `SlashCommand` executed command string, legacy `AskUserQuestion` prompts/options when present, Tavily/Codex MCP model/sandbox/approval details, and library/query details for Context7 MCP tools
- **Sub-agent stop visibility**: `task_notification` events with `status="stopped"` (e.g. forced via `TaskStop`) are now surfaced in the live monitor; missing usage metrics are omitted instead of being shown as zero
- **Tool error summary**: When a `tool_result` is `is_error:true`, the live monitor appends a short, single-line summary (truncated to 120 characters, with single-line or multi-line `<tool_use_error>` wrappers stripped) so the cause of a failed tool call is visible without opening the jsonl
- **Tool result metadata**: Surfaces important top-level `tool_use_result` metadata such as truncated output, applied limits, stale-read hints, `user-modified` markers when Edit/Write detect a concurrent user edit, `stale-recovered` when Edit recovers from stale read state, `memdir-stamped` when Claude Code stamps a memory directory, failure details (`error:` / `message:`), Bash stdout/stderr summaries (`stdout:` / `stderr:`), structured MCP/Codex summaries (`structured:`), successful string or text-block-array MCP result summaries (`result:`), Edit result file paths and structured patch size (`file:<path>`, `patch:<hunks> ... +added/-removed`, `replace_all`), auto-backgrounding, clamped wakeups, persisted output size, return-code interpretation, Agent duration/token/tool counts, sub-agent type (`agent:`), resolved model (`model:`), sub-agent edited line counts (`edits:+added/-removed`), async Agent IDs (`agent-id:`), and Agent IDs resumed by `SendMessage` (`resumed-agent:`), Grep/ToolSearch result counts and mode, WebSearch result counts/search count/duration, WebFetch HTTP status code and response size (`http:200 OK`, `bytes:120.2KB`), Read partial-read line ratios (`lines:<n>/<total>`) and token-cap truncation (`truncated:token-cap`), git commit operations (sha/kind), task counts/task IDs/task types, TaskOutput retrieval status, readable Agent output files, Monitor timeout/persistent state, TaskUpdate status transitions and changed fields beyond status (`updated:<field1>,<field2>`), async Agent launches (`async` when `run_in_background=true`), ScheduleWakeup scheduled time, Skill command names with allowed-tool counts (`allowed-tools:<n>`), and launched workflow names (`workflow:<name>`)
- **Session header**: Prints one line per session from the `init` event — model, Claude Code version, and permission mode (`ℹ Session <model> (v<version>, <permissionMode>)`). None of these appear anywhere else in the stream: `result.modelUsage` only reveals the models that were billed, so the CLI version and whether the run used `bypassPermissions` were otherwise lost
- **Observed background metadata**: Shows a background handoff's wait ceiling as `wait-timeout:<duration>`, its working-directory note as `cwd-hint:<summary>`, and permission-rule non-execution as `not-executed:permission-rule`
- **Observed stream-json edge cases**: Shows assistant-level model fallbacks (`from.model` → `to.model`) and cache-miss diagnostics with affected input-token counts, deduplicating repeated partial messages by message ID; suppresses high-frequency `background_tasks_changed` snapshots already represented by task events; keeps visible system/rate-limit notifications on separate lines when they arrive between text or thinking deltas, without adding line breaks for ignored events; shows optional Agent `model` / `isolation` launch settings, marks `isImage:true` tool results as `image`, and counts every `structuredPatch[].lines` entry beginning with `+` or `-` (including added/removed content that itself begins with `++` / `--`)
- **Logging pipeline safety**: Marks a task failed if `format-stream`, `tee`, or raw jsonl capture fails instead of recording it as completed. A target directory deleted or renamed between scan and execution is reported accurately as `target directory is unavailable` instead of an unrelated logging pipeline failure
- **Per-model usage**: Displays token usage, cost, cache read/creation tokens, web search counts, and the model's context window / max output limits (e.g. `ctx:1M`, `max_out:64K`) per model in the result summary
- **API timing**: Shows API response time, time to first token (`ttft`), time to first stream token (`stream:`, the pure streaming latency excluding queue/retry waits), and time-to-request (`req:<n>ms`) alongside wall-clock duration
- **Fast mode indicator**: Shows fast mode state when active and reports `fast_mode_disabled_reason` when the provider explains why it is unavailable
- **Terminal reason & permission denials**: Surfaces non-`completed` `terminal_reason` and denied tool call count/tool names in the result summary
- **Result metadata**: Displays `usage.service_tier`, `usage.speed`, non-empty inference geo, iteration count, and result origin kind when present
- **Rate limit alerts**: Displays utilization warnings, rejected request notifications, overage status and overage reset time (shown on warning and rejection events too), and the server-side warning threshold that was crossed (e.g. `warning at 90%`) for `allowed_warning` events; auto-stops when the configured threshold is exceeded (if the stop file cannot be created due to ENOSPC/permissions, the failure is surfaced instead of being silently swallowed)
- **Limit-aware result classification**: Treats limit-reached results — clock times including minutes such as `resets 2:30am`, and messages such as `You've hit your session limit` or `You've hit your org's monthly spend limit` — as rate limits rather than retryable provider errors, since retrying cannot clear them
- **Dated reset times**: Reset times that fall on a later day are shown as `MM/DD HH:MM`, since `seven_day` and overage windows can reset up to a month out and a bare clock time reads as "later today"
- **Transient connection errors retried**: Connection-level failures without an HTTP status (e.g. `API Error: Connection closed mid-response`) are classified as retryable rather than permanent, so the worker moves on to the next target instead of stopping (the target is reprocessed on the next run)
- **Subagent failure reasons**: When a subagent fails or is killed, the underlying cause (API error, etc.) is shown alongside the notification
- **API retry visibility**: Shows retry attempts with error details during transient failures
- **Collision-safe logs**: Per-task logs are numbered to avoid overwrite when display names collide
- **Prompt files**: Prompts can be `.md` files or inline strings
- **Resume**: Automatically skips already-processed directories; configurable skip duration
- **Concurrent-safe state**: Parallel workers update `state.json` with atomic rename under a stable sidecar lock file; malformed or unreadable existing state aborts the update without overwriting previously recorded history
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

# List all target directories in processing order (without `--limit`)
token-burn list

# Run only specific repositories
token-burn run ~/GitHub/repo-a ./repo-b

# Run token consumption
token-burn run
```

### Commands

| Command | Description |
|---------|-------------|
| `run` | Execute token consumption (default) |
| `list` | List target directories in processing order (ignores `--limit`, does not execute) |
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
| `--workers <N>` | `-w` | Number of concurrent workers (`N >= 1`, overrides `parallelism`) |
| `--public-only` | | Process only repositories detected as public |
| `--dedup-scope <SCOPE>` | | How widely processed-target history is shared: `global` / `provider` / `agent` (overrides `dedup_scope`) |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

`--dedup-scope` overrides the configured [`dedup_scope`](#sharing-processed-target-history-across-agents) for a single run. Use `--dedup-scope agent` to opt out of sharing and let this account re-visit repositories another account already processed.

`--workers` overrides the configured `parallelism` for a single run. The number of workers that actually start is capped by the number of tasks, and the effective value is shown as `Workers:` in the execution plan (visible with `--dry-run`).

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
| `parallelism` | Number of concurrent tasks (`>= 1`, overridable per run with `--workers`) | `3` |
| `skip_within` | Skip directories processed within this duration | `"7d"`, `"24h"`, `"1d12h"` |
| `cleanup_after` | Auto-delete report directories older than this duration | `"7d"` (default) |
| `report_dir` | Directory to save execution logs (relative paths are resolved against the current working directory) | `~/Documents/token-burn` (default) |
| `limit` | Maximum number of targets to process per run (`>= 1`) | `10` (default) |
| `rate_limit_threshold` | Auto-stop when rate limit utilization exceeds this percentage (`1-100`) | `95` (default) |
| `dedup_scope` | How widely processed-target history is shared (`global` / `provider` / `agent`) | `agent` (default) |

`skip_within` and `cleanup_after` accept duration strings using `d` (days), `h` (hours), `m` (minutes), and `s` (seconds). Invalid or unrepresentable values are rejected when the config file is loaded. If `skip_within` is omitted, directories processed since the previous reset are skipped. A representable duration that still exceeds the date-time range cannot panic: `skip_within` falls back to the previous-reset cutoff with a warning, while cleanup returns an error. Use `--fresh` to ignore saved state entirely.

`rate_limit_threshold` is enforced on two paths. During a task, Claude Code's stream-json `rate_limit_event` is monitored in real time and execution stops once the threshold is exceeded. In addition, when [ai-usage integration](#ai-usage-integration-optional) is enabled, after each task completes the agent's real utilization is re-checked against this threshold using the higher of the matching `(profile, provider)` pair's `weekly` and `five_hour` `used_percent` values; this applies to both `claude` and `codex` agents (the latter previously had no real-time monitoring).

State is stored in `<config-dir>/state.json` (same directory as the active config file). Updates are written to a same-directory temporary file and atomically swapped into place with `rename`, while a stable sidecar lock file such as `.state.json.lock` serializes parallel workers. If the existing file contains malformed JSON, the update fails without replacing it, preserving the original data for recovery instead of silently discarding processed-target history. Within each agent, entries are written most-recently-processed first (ties broken by ascending path), so the newest activity stays at the top of the file. With the default config path, this is `~/.config/token-burn/state.json`.

#### Sharing processed-target history across agents

`state.json` records history under the expanded agent name, so by default a repository processed by one account is still pending for every other account. When you run the same CLI under two accounts, the second run starts over from the same repositories instead of continuing where the first left off. `dedup_scope` controls how widely that history is consulted:

| Value | Which history is consulted when deciding to skip |
|-------|--------------------------------------------------|
| `global` | Every agent, including names that only exist in `state.json` (renamed or removed agents). One account continues where another stopped |
| `provider` | Agents sharing the same `provider` (e.g. `codex` accounts share with each other, but not with `claude`). Agents without a `provider`, and names absent from the config, consult only their own history |
| `agent` | Only the running agent (default; previous behavior) |

Writes are unaffected: completion is always recorded under the agent that actually ran it, so `state.json` keeps the full per-account history and its schema is unchanged. Only the *read* side widens.

`global` and `provider` require `skip_within`. The cutoff used when `skip_within` is omitted is the running agent's own previous reset time, which is agent-specific — applying it to another agent's history would make the skip window depend on which agent you happened to launch. Configs that ask for a shared scope without `skip_within` are rejected at load time.

Pass `--dedup-scope <global|provider|agent>` to override the configured value for a single run — use `--dedup-scope agent` when you deliberately want a second account to re-visit repositories another account already covered. Skips are reported with the scope, the window, and which agents' records caused them:

```
  Skipped: 8 targets (already processed; scope: global, window: 2d)
    by agent: codex=5, codex-alt=2, claude=1
```

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

**Claude auto-injected environment**: `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0` is added to the Claude process environment by default. Without it, `claude -p` waits at most 600s for background tasks (backgrounded subagents / workflows) after the main turn ends, then kills them ("Background tasks still running after 600s; terminating.") and reports success even though the work never finished. `0` waits indefinitely so background agents can complete and re-drive the main loop. Set the variable explicitly in the agent or profile `env` to override (an empty string unsets it, restoring Claude's default ceiling).

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
- The instant from `resets_at` is preserved, then converted to the local fixed offset for status/run display so UTC ai-usage output is shown in the user's local time.
- When resolution fails — the command is missing or fails, no matching `(profile, provider)` is found, the response reports `ok: false`, or the selected window is null — the configured `fallback` applies:
  - `fixed`: fall back to the fixed weekday calculation (the schedule source is shown as `fixed fallback: <reason>`).
  - `skip`: drop the affected agent from the candidate list.
  - `error`: stop with an error.
- `status` and `run` display each agent's schedule **source** (`ai-usage (weekly)`, `fixed`, or `fixed fallback`) so token-burn never falls back silently.
- **Post-task usage gate**: After each task completes, token-burn re-queries `ai-usage --json` and compares the matching `(profile, provider)` pair's `weekly` and `five_hour` `used_percent` against `rate_limit_threshold`. If either window meets or exceeds the threshold, a stop file is created so no further tasks start. This applies to both `claude` and `codex` agents, giving `codex` (which has no in-task `rate_limit_event` stream) a real-utilization stop signal.
- The `ai-usage --json` output is cached with a short TTL (20 seconds) so parallel workers do not each spawn a redundant query. The stop-file creation is idempotent and safe to call concurrently from multiple workers.
- The usage gate is **fail-closed**: if the query fails, or the matching account reports `ok:false` (e.g. ai-usage flags an expired auth), utilization cannot be confirmed, so tasks are stopped to stay on the safe side. When no matching entry is found, or the account is `ok:true` but `used_percent` is missing, execution continues instead, to avoid over-stopping on incomplete data.

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
| `public_first` | Group public repositories ahead of private ones in the processing order. Applied when **any** `[[scan]]` enables it; if every scan sets `false` (or the config has no `[[scan]]`), visibility does not affect the order | `true` |
| `recursive` | Recurse into subdirectories to find nested git repositories | `false` |
| `exclude` | Directory names to skip during scan | `[]` |

When `username` is set, visibility lookup uses the repository name parsed from each repository's `origin` remote URL (case-insensitive), so local directory names can differ from remote repository names.

Owner and repository names are extracted from the last two segments of the remote URL path, so GitLab subgroup URLs such as `git@gitlab.example.com:group/subgroup/repo.git` resolve to `subgroup` as the owner and `repo` as the repository name.

When `username` is not set, repositories are included even if they do not have an `origin` remote. In that case visibility remains `Unknown`.

Symlinks are skipped during directory scanning to prevent infinite recursion from circular links.

Directories that cannot be read — for example a subdirectory without read permission — are skipped with a warning and the scan continues, matching how missing `base_dirs` and symlinks are handled. A single unreadable subdirectory no longer aborts `run` / `list` before any repository is processed.

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

### Processing order

Targets are processed **least-recently-modified first**: the repository whose newest file change is the oldest goes first. `defer` keeps its priority, and visibility groups public repositories ahead of private ones **only when at least one `[[scan]]` sets `public_first = true`**. The reordering happens within those groups, and it is a stable sort, so targets sharing a modification time keep their original order. Repositories whose modification time cannot be determined go last within their group. `token-burn run PATH...` keeps the order given on the command line.

When every `[[scan]]` sets `public_first = false` (or the config has no `[[scan]]` at all), visibility is left out of the sort key entirely, so the order depends only on `defer` and modification time. This matters together with `limit`: while visibility grouping is active, private repositories are never reached as long as at least `limit` public repositories remain queued.

Without this, the processing order was fixed, so every run took the first `limit` targets from the same list head. The already-processed cutoff (`skip_within`, or the previous reset) is an absolute time window, so once a run falls outside it the whole history is invalidated at once and the same head repositories are picked again — while the tail is never reached.

The order is based on the repository's own last file modification time rather than the recorded processing time, so a run that was cut short by a rate limit (and therefore changed nothing) is not treated as progress. The timestamp comes from the newest mtime among files listed by `git ls-files`, which naturally excludes build artifacts and `.gitignore`d paths while still picking up uncommitted edits. `list` and `run` print it next to each target as `(modified: ...)` so the resulting order can be verified at a glance.

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
