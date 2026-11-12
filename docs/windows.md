# Running on Windows

The Windows release targets x64 MSVC. It uses ConPTY for worker terminals and Git for Windows Bash for task scripts. Windows 10 version 1809 or later, or Windows 11, is required. Installation options are listed in the [README](../README.md#installation).

## Before running

Install Git for Windows and the agent CLI you want to use. Log in to the agent CLI and confirm it works in the target repository. Run token-burn from Windows Terminal or PowerShell. A separate tmux installation is unnecessary.

token-burn finds Git Bash through `TOKEN_BURN_BASH`, `CLAUDE_CODE_GIT_BASH_PATH`, the directories on `PATH`, the usual system installation, and the per-user Git installation, in that order. It uses Git's `usr/bin/bash.exe`; it does not use the WSL `bash.exe` in `System32`.

If Git is installed elsewhere, set `TOKEN_BURN_BASH` to its `bin/bash.exe` or `usr/bin/bash.exe` before starting a run:

```powershell
$env:TOKEN_BURN_BASH = 'D:\Tools\Git\bin\bash.exe'
token-burn status
token-burn run -n
```

The runner inherits this setting, so it remains effective after detaching. Git Bash runs task scripts without loading interactive shell aliases; agent commands must name an executable or a script, rather than a PowerShell function or alias.

Use a native executable or a Bash/npm shim without a `.cmd` or `.bat` extension for agent commands. Windows batch files pass arguments through `cmd.exe`, which can reinterpret prompt characters. Windows also limits the complete process command line to 32,767 UTF-16 units, including the terminator; prompts passed as arguments must fit within that limit.

Workers set `MSYS2_ARG_CONV_EXCL=*` so Git Bash does not rewrite prompt arguments. Agent processes inherit this setting too. When an agent invokes a native Windows executable from Bash, use Windows paths such as `C:/Work/file.py`; MSYS paths such as `/tmp/file.py` will not be converted automatically.

## Paths and configuration

Use forward slashes in TOML paths, or single-quoted TOML strings for backslashes:

```toml
[[scan]]
base_dirs = ['C:\Work\GitHub', 'D:\Projects']
```

Spaces and Unicode are supported in target, report, and executable paths. In PowerShell, quote paths containing spaces. Press `a` in the running TUI to add another directory; relative paths are resolved from the attached client's working directory.

ai-usage integration uses the configured command and account profiles on Windows too. Install a version of ai-usage that runs in your environment; otherwise use the fixed reset schedule described in the [README](../README.md#configuration).

## Detaching and stopping

Press `d` or `q` to detach, then use `token-burn attach` to reconnect. Closing the client terminal leaves the independent runner active. In normal mode, Ctrl-C requests a graceful stop; a second Ctrl-C force-stops the workers and their descendant processes through Windows Job Objects.

The runner tries to leave any parent Job Object when starting. If the host forbids this, it starts within that job and prints a warning. In that case, closing the host's job can stop the run; use a terminal outside that restricted host when the run must survive its closure.

Signing out of Windows ends the user's processes, including the runner. Worker Job Objects also stop any remaining descendants when a worker exits.

The monitor reports native CPU, physical memory, and disk capacity. Unix load averages, compressed memory, and swap counters are unavailable on Windows and appear as unavailable rather than estimated values.

## Building from source

The Cargo installation option in the README requires Rust and the Visual Studio C++ build tools for the MSVC target. The Makefile workflow additionally requires GNU Make and a Bash environment. Windows CI builds the MSVC executable and runs clippy, unit tests, and native runner tests, including Unicode paths, directory addition, reattachment, and forced process-tree termination.
