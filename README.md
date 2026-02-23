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

Claude Code / Codex CLI tokens reset weekly with no rollover. Inspired by the Japanese *mottainai* (もったいない) spirit — the belief that waste is something to be avoided — **token-burn** puts those remaining tokens to work. It runs your prompts across repositories in parallel before the reset deadline — code reviews, bug hunts, refactoring, test improvements, or anything else you define. When the reset time arrives, all running processes are automatically terminated.

## Features

- **Auto-discovery**: Scans directories for git repos, filters by username in remote URL
- **Multiple scan sources**: Define separate scan configs for GitHub, GitLab, etc.
- **Visibility-aware**: Prioritizes public repositories over private ones
- **Multi-agent**: Supports Claude Code, Codex CLI, and custom agents
- **Smart scheduling**: Automatically selects the agent closest to its reset deadline
- **Deadline enforcement**: Kills all child processes when the reset time arrives
- **Parallel execution**: Runs multiple prompts concurrently in tmux split panes with progress monitor
- **Prompt files**: Prompts can be `.md` files or inline strings
- **Resume**: Automatically skips already-processed directories; configurable skip duration
- **Dry run**: Preview execution plan without running commands

## Requirements

- **OS**: macOS
- **tmux**: Required for split-pane execution
- **Rust**: 1.70+ (for building from source)
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

# Run token consumption
token-burn run
```

### Commands

| Command | Description |
|---------|-------------|
| `run` | Execute token consumption (default) |
| `status` | Show agent reset status |
| `init` | Initialize config file and prompt templates |

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--config <PATH>` | `-c` | Config file path (default: `~/.config/token-burn/config.toml`) |
| `--agent <NAME>` | | Force specific agent |
| `--dry-run` | `-n` | Preview without executing |
| `--fresh` | | Ignore saved state and process all targets |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

`init` also accepts `--force` (`-f`) to overwrite existing files without confirmation.

## Configuration

Default config location: `~/.config/token-burn/config.toml`

Run `token-burn init` to generate a config template, or see [config.toml.example](config.toml.example).

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

`skip_within` accepts duration strings: `d` (days), `h` (hours), `m` (minutes), `s` (seconds). If omitted, directories processed since the previous reset are skipped. Use `--fresh` to ignore saved state entirely.

State is stored in `~/.config/token-burn/state.json`.

### Agents

```toml
[[agents]]
name = "claude"
command = ["claude", "-p", "--dangerously-skip-permissions", "--model", "opus"]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "Asia/Tokyo"
```

| Field | Description | Example |
|-------|-------------|---------|
| `name` | Agent identifier | `"claude"` |
| `command` | Command and arguments | `["claude", "-p"]` |
| `reset_weekday` | Reset day of week | `"monday"` |
| `reset_time` | Reset time (HH:MM) | `"09:00"` |
| `timezone` | IANA timezone | `"Asia/Tokyo"` |

`reset_weekday` accepts: `monday` `tuesday` `wednesday` `thursday` `friday` `saturday` `sunday` (or short forms: `mon` `tue` `wed` `thu` `fri` `sat` `sun`)

### Auto-scan (multiple sources)

```toml
[[scan]]
base_dirs = ["~/GitHub"]
username = "owayo"
public_first = true
exclude = ["archived-project"]

[[scan]]
base_dirs = ["~/git"]
username = "owayo"
public_first = false
```

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
