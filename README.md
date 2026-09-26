<p align="center">
  <img src="docs/images/app.png" width="128" alt="token-burn">
</p>

<h1 align="center">token-burn</h1>

<p align="center">
  CLI tool that spends leftover Claude Code and Codex CLI tokens before the weekly reset by running your prompts across your repositories in parallel
</p>

<!-- standard:badges:start -->
<h3 align="center">Supported Platforms</h3>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
</p>

<p align="center">
  <a href="https://github.com/owayo/token-burn/actions/workflows/ci.yml"><img src="https://github.com/owayo/token-burn/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/token-burn/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/token-burn" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/token-burn" alt="License"></a>
</p>

<p align="center">
  <a href="README.md">English</a> |
  <a href="README.ja.md">日本語</a>
</p>
<!-- standard:badges:end -->

---

Claude Code / Codex CLI tokens reset weekly with no rollover. Inspired by the Japanese *mottainai* (もったいない) spirit — the belief that waste is something to be avoided — **token-burn** puts those remaining tokens to work. It runs your prompts across repositories in parallel before the reset deadline — code reviews, bug hunts, refactoring, test improvements, or anything else you define. When the reset time arrives, token-burn stops starting new tasks and waits for the tasks already running to finish.

<p align="center"><img src="docs/images/screenshot.png" width="800" alt="token-burn running"></p>

## Features

- **Deadline-aware scheduling**: Picks the agent whose reset deadline is closest, stops starting new tasks when the reset time arrives, and waits for the running tasks to finish
- **Parallel runs in tmux**: Runs several prompts at once in split panes with a progress monitor; workers close their own panes when the queue is empty, and detaching leaves the run going
- **Repository discovery**: Scans directories for git repositories from several `[[scan]]` sources, keeps those whose remote owner matches your username, and processes each directory once
- **Processing order**: Puts public repositories ahead of private ones, orders each group least-recently-modified first, and skips targets processed recently (`skip_within`)
- **Interactive target picker**: `-i` / `--interactive` opens a TUI where you choose which repositories to process and in what order
- **Claude Code, Codex CLI and custom agents**: Adds the flags an unattended run needs, such as stream-json output and a disallowed `AskUserQuestion` for Claude Code and `approval_policy=never` for Codex CLI
- **Real reset times from ai-usage**: Optionally reads each account's reset time and utilization from `ai-usage --json`, falling back to the fixed weekly schedule
- **Multiple accounts**: Expands one agent across accounts (`claude-work` / `claude-home`), each with its own environment and history, and `dedup_scope` lets one account continue where another stopped
- **Rate-limit aware**: Stops for good when the weekly window reaches `rate_limit_threshold`, but pauses until the reset when the five-hour window does; workers recheck usage after every extended pause before taking a task, and the monthly overage window never stops a run
- **Resume interrupted sessions**: Continues a Claude Code session cut off by a rate limit with `claude --resume` on the next run instead of starting over
- **Readable live monitor**: Renders Claude Code's stream-json as readable lines — tool calls with their key arguments, subagent activity, hook feedback, context compaction, thinking and token usage, cost per model, and why a session failed
- **Safe state and logs**: Updates `state.json` atomically under a lock, numbers per-task logs so they never overwrite each other, and marks a task failed when its log pipeline breaks
- **Dry run**: `-n` / `--dry-run` previews the plan, showing environment assignments and credential options in the commands as `<redacted>`

## Requirements

- **tmux**: Required for split-pane execution
- **Claude Code** and/or **Codex CLI**: At least one agent must be installed
- **gh CLI**: Required for repository visibility detection
- **ai-usage** (optional): Needed only for the [ai-usage integration](docs/configuration.md#ai-usage-integration-optional)

## Installation

<!-- standard:install:start -->
### Homebrew (macOS)

```bash
brew install owayo/token-burn/token-burn
```

### Cargo

Requires Rust 1.98 or later.

```bash
cargo install --git https://github.com/owayo/token-burn --locked
```

### From GitHub Releases

Download the archive for your platform from [Releases](https://github.com/owayo/token-burn/releases/latest), extract it, and put `token-burn` on your `PATH`. Each release also includes `SHA256SUMS` for checking the downloads.

| Platform | Archive |
|---|---|
| macOS (Intel) | `token-burn-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `token-burn-aarch64-apple-darwin.tar.gz` |

On macOS, if you downloaded the archive with a browser, remove the quarantine attribute before running it: `xattr -d com.apple.quarantine token-burn`.

### From Source

Requires [mise](https://mise.jdx.dev/) (the Rust toolchain is pinned in `mise.toml`).

```bash
git clone https://github.com/owayo/token-burn.git
cd token-burn
make install
```

`make install` installs to `/usr/local/bin`. Set `INSTALL_PATH` to change it (for example `make install INSTALL_PATH="$HOME/.local/bin"`).
<!-- standard:install:end -->

## Quickstart

Create the config file and the default prompts (`~/.config/token-burn/config.toml` and `prompts/` next to it):

```bash
token-burn init
```

Edit `config.toml`: set `base_dirs` and `username` in `[[scan]]`, and the `command` and reset schedule of each agent in `[[agents]]`. Then check the reset times and preview the plan before the first run:

```bash
token-burn status
token-burn run -n
```

## Usage

```bash
# Run token consumption (up to `limit` targets)
token-burn run

# Choose the targets and their order in a TUI first
token-burn run -i

# Run only specific repositories (scan and skip rules are bypassed)
token-burn run ~/GitHub/repo-a ./repo-b

# List every target in processing order without running anything
token-burn list

# Remove report directories older than three days
token-burn clean --older-than 3d
```

When the reset time arrives, token-burn stops starting new tasks and waits for the running ones to finish:

<p align="center"><img src="docs/images/deadline.png" width="800" alt="Deadline reached — waiting for tasks to finish"></p>

More detail:

- [CLI reference](docs/cli-reference.md): every command and option
- [Usage details](docs/usage.md): processing order, resuming interrupted sessions, the tmux runtime, logs and state
- [Rate limits](docs/rate-limits.md): when a run stops and when it only pauses
- [Live monitor](docs/monitor.md): what the worker and monitor panes show

## Configuration

Default config location: `~/.config/token-burn/config.toml` (pass `-c` / `--config` to use another file). `state.json` (processed targets) and `resume.json` (interrupted sessions) are kept in the same directory.

A minimal config with one agent and one scan source:

```toml
[settings]
parallelism = 3
skip_within = "1d"
limit = 10

[prompts]
default = "prompts/default.md"

[[agents]]
name = "claude"
command = ["claude", "--dangerously-skip-permissions", "--model", "opus"]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "Asia/Tokyo"

[[scan]]
base_dirs = ["~/GitHub"]
username = "yourname"
```

Prompt values ending with `.md` are read as files relative to the config directory. **Prompt priority**: `[[targets]].prompt` > `[[agents]].prompt` > `[prompts].default`

Every section — `[settings]`, `[[agents]]`, `[ai_usage]` (real reset times and multiple accounts), `[[scan]]`, `[prompts]`, and `[[targets]]` — is described in [docs/configuration.md](docs/configuration.md).

## Development

<!-- standard:dev:start -->
Requires [mise](https://mise.jdx.dev/). Tool versions are pinned in `mise.toml`.

```bash
make setup   # Install the toolchain (mise) and dependencies
make ci      # Run the same checks as CI (no changes)
```

| Command | Description |
|---|---|
| `make setup` | Install the toolchain (mise) and dependencies |
| `make build` | Build a debug binary |
| `make release` | Build a release binary |
| `make run` | Run the debug binary (arguments via ARGS="...") |
| `make test` | Run the tests |
| `make lint` | Run clippy with warnings as errors |
| `make fmt` | Format the code (rewrites files) |
| `make fmt-check` | Check the formatting (no changes) |
| `make check` | Run fmt-check and lint (no changes) |
| `make ci` | Run the same checks as CI (no changes) |
| `make install` | Install the release binary to INSTALL_PATH (default /usr/local/bin) |
| `make uninstall` | Remove the binary from INSTALL_PATH |
| `make clean` | Remove build artifacts |

Run `make` to list every target. Releases are published from GitHub Actions (**Actions → Release → Run workflow**).
<!-- standard:dev:end -->

## License

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
