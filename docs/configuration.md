# Configuration

Default config location: `~/.config/token-burn/config.toml`

Run `token-burn init` to generate a config template. The [README](../README.md#configuration) shows a minimal config; this page describes every section.

## Settings

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
| `rate_limit_threshold` | Hold back new tasks when the five-hour / seven-day window utilization reaches this percentage (`1-100`). A seven-day window stops the run permanently; a five-hour window only pauses until it resets. The monthly overage window never triggers either. See [Rate limits](rate-limits.md) | `95` (default) |
| `dedup_scope` | How widely processed-target history is shared (`global` / `provider` / `agent`) | `agent` (default) |
| `resume_interrupted` | Save a Claude Code session cut off by a rate limit and continue it on the next run instead of starting over. `false` turns this off: interrupted sessions are neither saved nor resumed. See [Resuming interrupted sessions](usage.md#resuming-interrupted-sessions) | `true` (default) |

`skip_within` and `cleanup_after` accept duration strings using `d` (days), `h` (hours), `m` (minutes), and `s` (seconds). Invalid or unrepresentable values are rejected when the config file is loaded. If `skip_within` is omitted, directories processed since the previous reset are skipped. A representable duration that still exceeds the date-time range cannot panic: `skip_within` falls back to the previous-reset cutoff with a warning, while cleanup returns an error. Use `--fresh` to ignore saved state entirely — both the processed history and saved interrupted sessions.

State is stored in `<config-dir>/state.json` (same directory as the active config file). Updates are written to a same-directory temporary file and atomically swapped into place with `rename`, while a stable sidecar lock file such as `.state.json.lock` serializes parallel workers. If the existing file contains malformed JSON, the update fails without replacing it, preserving the original data for recovery instead of silently discarding processed-target history. Within each agent, entries are written most-recently-processed first (ties broken by ascending path), so the newest activity stays at the top of the file. With the default config path, this is `~/.config/token-burn/state.json`. Sessions interrupted by a rate limit are saved separately in `resume.json` in the same directory; see [Resuming interrupted sessions](usage.md#resuming-interrupted-sessions).

### Sharing processed-target history across agents

`state.json` records history under the expanded agent name, so by default a repository processed by one account is still pending for every other account. When you run the same CLI under two accounts, the second run starts over from the same repositories instead of continuing where the first left off. `dedup_scope` controls how widely that history is consulted:

| Value | Which history is consulted when deciding to skip |
|-------|--------------------------------------------------|
| `global` | Every agent, including names that only exist in `state.json` (renamed or removed agents). One account continues where another stopped |
| `provider` | Agents sharing the same `provider` (e.g. `codex` accounts share with each other, but not with `claude`). Agents without a `provider` (an empty or whitespace-only value counts as unset), and names absent from the config, consult only their own history |
| `agent` | Only the running agent (default; previous behavior) |

Writes are unaffected: completion is always recorded under the agent that actually ran it, so `state.json` keeps the full per-account history and its schema is unchanged. Only the *read* side widens.

`global` and `provider` require `skip_within`. The cutoff used when `skip_within` is omitted is the running agent's own previous reset time, which is agent-specific — applying it to another agent's history would make the skip window depend on which agent you happened to launch. Configs that ask for a shared scope without `skip_within` are rejected at load time.

Pass `--dedup-scope <global|provider|agent>` to override the configured value for a single run — use `--dedup-scope agent` when you deliberately want a second account to re-visit repositories another account already covered. Skips are reported with the scope, the window, and which agents' records caused them:

```text
  Skipped: 8 targets (already processed; scope: global, window: 2d)
    by agent: codex=5, codex-alt=2, claude=1
```

## Agents

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

`name` must not be empty and must be unique after profile expansion — the expanded name doubles as the `state.json` key, the report directory name, and the `--agent` selector, so a duplicate makes the second agent unreachable and silently merges the two agents' processed history. `command` must contain at least one element, and the first element must be a non-empty executable name. `prompt` overrides the global `[prompts].default` for this agent; target-level `prompt` takes highest priority.

`reset_weekday`, `reset_time`, and `timezone` are normally required. They may be omitted only when ai-usage integration is enabled for the agent **and** the effective `fallback` is not `fixed`, since in that case the fixed-schedule calculation is never used. Otherwise they are still required as the fallback schedule. See [ai-usage integration (optional)](#ai-usage-integration-optional).

**Prompt priority**: `[[targets]].prompt` > `[[agents]].prompt` > `[prompts].default`

**Claude auto-injected flags**: When the executable is `claude`, the following flags are enforced: `-p`, `--verbose`, `--output-format stream-json`, `--include-partial-messages`, and `--disallowedTools=AskUserQuestion`. Missing flags are appended automatically, an existing `--output-format` value is normalized to `stream-json` (including `--output-format=...` form), and an existing `--disallowedTools` / `--disallowed-tools` list is normalized and extended with `AskUserQuestion` when needed. The logging flags are required for proper log capture and progress monitoring; `AskUserQuestion` is denied so unattended token-burn jobs cannot stop on an interactive question. You do not need to include them in your config.

**Claude auto-injected environment**: `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0` is added to the Claude process environment by default. Without it, `claude -p` waits at most 600s for background tasks (backgrounded subagents / workflows) after the main turn ends, then kills them ("Background tasks still running after 600s; terminating.") and reports success even though the work never finished. `0` waits indefinitely so background agents can complete and re-drive the main loop. Set the variable explicitly in the agent or profile `env` to override (an empty string unsets it, restoring Claude's default ceiling).

`reset_weekday` accepts: `monday` `tuesday` `wednesday` `thursday` `friday` `saturday` `sunday` (or short forms: `mon` `tue` `wed` `thu` `fri` `sat` `sun`)

## ai-usage integration (optional)

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

### `[ai_usage]` (global)

| Field | Description | Default |
|-------|-------------|---------|
| `enabled` | Enable the integration. When omitted or `false`, only the fixed weekday calculation is used | `false` |
| `command` | Command and arguments used to query usage data (must emit JSON) | `["ai-usage", "--json"]` |
| `window` | Window whose `resets_at` is used to compute the deadline: `weekly`, `five_hour`, or `nearest` | `weekly` |
| `fallback` | Behavior when resolution fails: `fixed`, `skip`, or `error` | `fixed` |
| `state_window` | Window used for the processed-target cutoff: `weekly` or `selected` | `weekly` |

### `[[ai_usage.profiles]]`

| Field | Description |
|-------|-------------|
| `name` | Internal reference name. Used in the expanded agent name `<agent>-<name>` and referenced from `[agents.ai_usage].profiles` |
| `profile` | Value matched against the `profile` field of `ai-usage --json` output (case-sensitive) |
| `env` | Environment variables applied when launching this account. Keys must match `[A-Za-z_][A-Za-z0-9_]*`; values are `~`-expanded. Merged into (and override) the agent's `env` |

### `[agents.ai_usage]` (per agent)

| Field | Description |
|-------|-------------|
| `profiles` | Profile names (from `[[ai_usage.profiles]].name`) this agent uses. Multiple names expand the agent into one instance per account |
| `window` | Optional override of the global `[ai_usage].window` for this agent |
| `fallback` | Optional override of the global `[ai_usage].fallback` for this agent |

### Behavior

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
- **Post-task usage gate**: After each task completes, token-burn re-queries `ai-usage --json` and compares the matching `(profile, provider)` pair's `weekly` and `five_hour` `used_percent` against `rate_limit_threshold`. A window at or over the threshold holds back further tasks — permanently for a weekly window, until the window resets for a short one. This applies to both `claude` and `codex` agents, giving `codex` (which has no in-task `rate_limit_event` stream) a real-utilization stop signal.
- The `ai-usage --json` output is cached with a short TTL (20 seconds) so parallel workers do not each spawn a redundant query. The stop-file creation is idempotent and safe to call concurrently from multiple workers; a pause is updated under an exclusive lock and only ever moves the resume time later.
- The usage gate is **fail-closed**: if the query fails, or the matching account reports `ok:false` (e.g. ai-usage flags an expired auth), utilization cannot be confirmed, so tasks are stopped to stay on the safe side. When no matching entry is found, or the account is `ok:true` but `used_percent` is missing, execution continues instead, to avoid over-stopping on incomplete data.

## Auto-scan (multiple sources)

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

## Prompts

Prompt values ending with `.md` are read as file paths. Relative paths resolve from the config directory.

```toml
[prompts]
default = "prompts/default.md"
# resume = "prompts/resume.md"   # optional: continuation prompt sent when resuming an interrupted session
```

`resume` (optional) is the continuation prompt sent when [resuming an interrupted session](usage.md#resuming-interrupted-sessions). It is resolved the same way as `default`, and a built-in English prompt is used when it is omitted. Only this prompt is sent on resume: the original instructions are already in the session's history.

## Explicit targets (merged with scan results)

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
