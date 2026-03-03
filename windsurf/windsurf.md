Cascade

# Cascade Hooks

Execute custom shell commands at key points in Cascade’s workflow for logging, security controls, validation, and enterprise governance with pre and post hooks.

Cascade Hooks enable you to execute custom shell commands at key points during Cascade’s workflow. This powerful extensibility feature allows you to log operations, enforce guardrails, run validation checks, or integrate with external systems.

Hooks are designed for power users and enterprise teams who need fine-grained control over Cascade’s behavior. They require basic shell scripting knowledge.

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#what-you-can-build)

What You Can Build

Hooks unlock a wide range of automation and governance capabilities:

* **Logging & Analytics**: Track every file read, code change, command executed, user prompt, or Cascade response for compliance and usage analysis
* **Security Controls**: Block Cascade from accessing sensitive files, running dangerous commands, or processing policy-violating prompts
* **Quality Assurance**: Run linters, formatters, or tests automatically after code modifications
* **Custom Workflows**: Integrate with issue trackers, notification systems, or deployment pipelines
* **Team Standardization**: Enforce coding standards and best practices across your organization

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#how-hooks-work)

How Hooks Work

Hooks are shell commands that run automatically when specific Cascade actions occur. Each hook:

1. **Receives context** (details about the action being performed) via JSON as standard input
2. **Executes your script** - Python, Bash, Node.js, or any executable
3. **Returns a result** via exit code and output streams

For **pre-hooks** (executed before an action), your script can **block the action** by exiting with exit code `2`. This makes pre-hooks ideal for implementing security policies or validation checks.

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#configuration)

Configuration

Hooks are configured in JSON files that can be placed at three different levels. Cascade loads and merges hooks from all locations, giving teams flexibility in how they distribute and manage hook configurations.

#### [​](https://docs.windsurf.com/windsurf/cascade/hooks#system-level)

System-Level

System-level hooks are ideal for organization-wide policies enforced on shared development machines. For example, you can use them to enforce security policies, compliance requirements, or mandatory code review workflows. Enterprise teams can also configure hooks via the [cloud dashboard](https://docs.windsurf.com/windsurf/cascade/hooks#cloud-dashboard-configuration) without managing local files.

* **macOS**: `/Library/Application Support/Windsurf/hooks.json`
* **Linux/WSL**: `/etc/windsurf/hooks.json`
* **Windows**: `C:\ProgramData\Windsurf\hooks.json`

#### [​](https://docs.windsurf.com/windsurf/cascade/hooks#user-level)

User-Level

User-level hooks are perfect for personal preferences and optional workflows.

* **Windsurf IDE**: `~/.codeium/windsurf/hooks.json`
* **JetBrains Plugin**: `~/.codeium/hooks.json`

#### [​](https://docs.windsurf.com/windsurf/cascade/hooks#workspace-level)

Workspace-Level

Workspace-level hooks allow teams to version control project-specific policies alongside their code. They may include custom validation rules, project-specific integrations, or team-specific workflows.

* **Location**: `.windsurf/hooks.json` in your workspace root

Hooks from all three locations are **merged together**. If the same hook event is configured in multiple locations, all hooks will execute in order: system → user → workspace.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#basic-structure)

Basic Structure

Here is an example of the basic structure of the hooks configuration:

Copy

Ask AI

```
{
  "hooks": {
    "pre_read_code": [
      {
        "command": "python3 /path/to/your/script.py",
        "show_output": true
      }
    ],
    "post_write_code": [
      {
        "command": "python3 /path/to/another/script.py",
        "show_output": true
      }
    ]
  }
}
```

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#configuration-options)

Configuration Options

Each hook accepts the following parameters:

| Parameter | Type | Description |
| - | - | - |
| `command` | string | The shell command to execute. Can be any valid executable with arguments. |
| `show_output` | boolean | Whether to display the hook’s stdout/stderr output on the user-facing Cascade UI. Useful for debugging. |
| `working_directory` | string | Optional. The directory to execute the command from. Defaults to your workspace root. |

**About the `working_directory` parameter:**

* In multi-repo workspaces, the default is the root of the repo currently being worked on
* Relative paths resolve from the default location (workspace or repo root)
* Absolute paths are supported
* Using `~` for home directory expansion is not supported

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#hook-events)

Hook Events

Cascade provides twelve hook events that cover the most critical actions in the agent workflow.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#common-input-structure)

Common Input Structure

All hooks receive a JSON object with the following common fields:

| Field | Type | Description |
| - | - | - |
| `agent_action_name` | string | The hook event name (e.g., “pre\_read\_code”, “post\_write\_code”) |
| `trajectory_id` | string | Unique identifier for the overall Cascade conversation |
| `execution_id` | string | Unique identifier for the single agent turn |
| `timestamp` | string | ISO 8601 timestamp when the hook was triggered |
| `tool_info` | object | Event-specific information (varies by hook type) |

In the following examples, the common fields are omitted for brevity. There are twelve major types of hook events:

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#pre_read_code)

pre\_read\_code

Triggered **before** Cascade reads a code file. This may block the action if the hook exits with code 2. **Use cases**: Restrict file access, log read operations, check permissions **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "pre_read_code",
  "tool_info": {
    "file_path": "/Users/yourname/project/file.py"
  }
}
```

This `file_path` may be a directory path when Cascade reads a directory recursively.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#post_read_code)

post\_read\_code

Triggered **after** Cascade successfully reads a code file. **Use cases**: Log successful reads, track file access patterns **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "post_read_code",
  "tool_info": {
    "file_path": "/Users/yourname/project/file.py"
  }
}
```

This `file_path` may be a directory path when Cascade reads a directory recursively.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#pre_write_code)

pre\_write\_code

Triggered **before** Cascade writes or modifies a code file. This may block the action if the hook exits with code 2. **Use cases**: Prevent modifications to protected files, backup files before changes **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "pre_write_code",
  "tool_info": {
    "file_path": "/Users/yourname/project/file.py",
    "edits": [
      {
        "old_string": "def old_function():\n    pass",
        "new_string": "def new_function():\n    return True"
      }
    ]
  }
}
```

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#post_write_code)

post\_write\_code

Triggered **after** Cascade writes or modifies a code file. **Use cases**: Run linters, formatters, or tests; log code changes **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "post_write_code",
  "tool_info": {
    "file_path": "/Users/yourname/project/file.py",
    "edits": [
      {
        "old_string": "import os",
        "new_string": "import os\nimport sys"
      }
    ]
  }
}
```

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#pre_run_command)

pre\_run\_command

Triggered **before** Cascade executes a terminal command. This may block the action if the hook exits with code 2. **Use cases**: Block dangerous commands, log all command executions, add safety checks **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "pre_run_command",
  "tool_info": {
    "command_line": "npm install package-name",
    "cwd": "/Users/yourname/project"
  }
}
```

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#post_run_command)

post\_run\_command

Triggered **after** Cascade executes a terminal command. **Use cases**: Log command results, trigger follow-up actions **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "post_run_command",
  "tool_info": {
    "command_line": "npm install package-name",
    "cwd": "/Users/yourname/project"
  }
}
```

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#pre_mcp_tool_use)

pre\_mcp\_tool\_use

Triggered **before** Cascade invokes an MCP (Model Context Protocol) tool. This may block the action if the hook exits with code 2. **Use cases**: Log MCP usage, restrict which MCP tools can be used **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "pre_mcp_tool_use",
  "tool_info": {
    "mcp_server_name": "github",
    "mcp_tool_arguments": {
      "owner": "code-owner",
      "repo": "my-cool-repo",
      "title": "Bug report",
      "body": "Description of the bug here"
    },
    "mcp_tool_name": "create_issue"
  }
}
```

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#post_mcp_tool_use)

post\_mcp\_tool\_use

Triggered **after** Cascade successfully invokes an MCP tool. **Use cases**: Log MCP operations, track API usage, see MCP results **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "post_mcp_tool_use",
  "tool_info": {
    "mcp_result": "...",
    "mcp_server_name": "github",
    "mcp_tool_arguments": {
      "owner": "code-owner",
      "perPage": 1,
      "repo": "my-cool-repo",
      "sha": "main"
    },
    "mcp_tool_name": "list_commits"
  }
}
```

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#pre_user_prompt)

pre\_user\_prompt

Triggered **before** Cascade processes the text of a user’s prompt. This may block the action if the hook exits with code 2. **Use cases**: Log all user prompts for auditing, block potentially harmful or policy-violating prompts **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "pre_user_prompt",
  "tool_info": {
    "user_prompt": "can you run the echo hello command"
  }
}
```

The `show_output` configuration option does not apply to this hook.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#post_cascade_response)

post\_cascade\_response

Triggered asynchronously **after** Cascade completes a response to a user’s prompt. This hook receives the full Cascade response ever since the last user input. **Use cases**: Log all Cascade responses for auditing, analyze response patterns, send responses to external systems for compliance review **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "post_cascade_response",
  "tool_info": {
    "response": "### Planner Response\n\nI'll help you create that file.\n\n*Created file `/path/to/file.py`*\n\n### Planner Response\n\nThe file has been created successfully."
  }
}
```

The `response` field contains the markdown-formatted content of Cascade’s response since the last user input. This includes planner responses, tool actions (file reads, writes, commands), and any other steps Cascade took. It also includes information about which [rules](https://docs.windsurf.com/windsurf/cascade/memories-and-rules) were triggered. See the [Tracking Triggered Rules](https://docs.windsurf.com/windsurf/cascade/hooks#tracking-triggered-rules) example for how to parse rule usage. The `show_output` configuration option does not apply to this hook.

The `response` content is derived from trajectory data and may contain sensitive information from your codebase or conversations. Handle this data according to your organization’s security and privacy policies.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#post_cascade_response_with_transcript)

post\_cascade\_response\_with\_transcript

Triggered asynchronously **after** Cascade completes a response to a user’s prompt, similar to `post_cascade_response`. Instead of providing a markdown summary inline, this hook writes the full conversation transcript (from the beginning of the conversation) to a local JSONL file and provides the file path. **Use cases**: Enterprise audit and compliance logging, tracking AI-generated contributions, feeding transcripts to external observability or analytics tools **Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "post_cascade_response_with_transcript",
  "tool_info": {
    "transcript_path": "/Users/yourname/.windsurf/transcripts/{trajectory_id}.jsonl"
  }
}
```

The `transcript_path` points to a [JSONL](https://jsonlines.org/) file at `~/.windsurf/transcripts/{trajectory_id}.jsonl`. Each line is a JSON object representing a single step in the conversation, with a `type` and `status` field plus step-specific data. For example:

Copy

Ask AI

```
{"status":"done","type":"user_input","user_input":{"rules_applied":{"always_on":["my-rule.md"]},"user_response":"create a hello world file"}}
{"planner_response":{"response":"I'll create a hello world file for you."},"status":"done","type":"planner_response"}
{"code_action":{"new_content":"print('hello world')\n","path":"/path/to/file.py"},"status":"done","type":"code_action"}
{"planner_response":{"response":"I created the file for you."},"status":"done","type":"planner_response"}
```

The transcript includes detailed, customer-owned data such as file contents, command outputs, tool arguments, search results, and [rules](https://docs.windsurf.com/windsurf/cascade/memories-and-rules) that were applied. Please note that the exact structure of each step may change in future versions, so please build any hook consumers to be resilient. Transcript files are written with `0600` permissions. Windsurf automatically limits the transcripts directory to 100 files, pruning the oldest by modification time. The `show_output` configuration option does not apply to this hook. This table shows the key differences between `post_cascade_response` and `post_cascade_response_with_transcript` hooks:

|  | `post_cascade_response` | `post_cascade_response_with_transcript` |
| - | - | - |
| **Data scope** | Only the steps since the last user input | The full conversation from the beginning |
| **Format** | Markdown summary in `tool_info.response` | Structured JSONL file at `tool_info.transcript_path` |
| **Detail level** | Condensed, human-readable summary | Detailed, machine-readable data (file contents, command output, etc.) |
| **Delivery** | Inline via stdin JSON | File on disk (`~/.windsurf/transcripts/`) |

Transcript files will contain sensitive information from your codebase including file contents, command outputs, and conversation history. Handle these files according to your organization’s security and privacy policies.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#post_setup_worktree)

post\_setup\_worktree

Triggered **after** a new [git worktree](https://docs.windsurf.com/windsurf/cascade/worktrees) is created and configured. The hook is executed inside the new **worktree** directory. **Use cases**: Copy `.env` files or other untracked files into the worktree, install dependencies, run setup scripts **Environment Variables**:

| Variable | Description |
| - | - |
| `$ROOT_WORKSPACE_PATH` | The absolute path to the original workspace. Use this to access files or run commands relative to the original repository. |

**Input JSON**:

Copy

Ask AI

```
{
  "agent_action_name": "post_setup_worktree",
  "tool_info": {
    "worktree_path": "/Users/me/.windsurf/worktrees/my-repo/abmy-repo-c123",
    "root_workspace_path": "/Users/me/projects/my-repo"
  }
}
```

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#exit-codes)

Exit Codes

Your hook scripts communicate results through exit codes:

| Exit Code | Meaning | Effect |
| - | - | - |
| `0` | Success | Action proceeds normally |
| `2` | Blocking Error | The Cascade agent will see the error message from stderr. For pre-hooks, this **blocks** the action. |
| Any other | Error | Action proceeds normally |

Only **pre-hooks** (pre\_user\_prompt, pre\_read\_code, pre\_write\_code, pre\_run\_command, pre\_mcp\_tool\_use) can block actions using exit code 2. Post-hooks cannot block since the action has already occurred.

Keep in mind that the user can see any hook-generated standard output and standard error in the Cascade UI if `show_output` is true.

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#example-use-cases)

Example Use Cases

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#logging-all-cascade-actions)

Logging All Cascade Actions

Track every action Cascade takes for auditing purposes. **Config**:

Copy

Ask AI

```
{
  "hooks": {
    "post_read_code": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py",
        "show_output": true
      }
    ],
    "post_write_code": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py",
        "show_output": true
      }
    ],
    "post_run_command": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py",
        "show_output": true
      }
    ],
    "post_mcp_tool_use": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py",
        "show_output": true
      }
    ],
    "post_cascade_response": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py"
      }
    ]
  }
}
```

**Script** (`log_input.py`):

Copy

Ask AI

```
#!/usr/bin/env python3

import sys
import json

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()
    
    # Parse the JSON
    try:
        data = json.loads(input_data)
        
        # Write formatted JSON to file
        with open("/Users/yourname/hooks/input.txt", "a") as f:
            f.write('\n' + '='*80 + '\n')
            f.write(json.dumps(data, indent=2, separators=(',', ': ')))
            f.write('\n')
    
        print(json.dumps(data, indent=2))
    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```

This script appends every hook invocation to a log file, creating an audit trail of all Cascade actions. You may transform the input data or perform custom logic as you see fit.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#restricting-file-access)

Restricting File Access

Prevent Cascade from reading files outside a specific directory. **Config**:

Copy

Ask AI

```
{
  "hooks": {
    "pre_read_code": [
      {
        "command": "python3 /Users/yourname/hooks/block_read_access.py",
        "show_output": true
      }
    ]
  }
}
```

**Script** (`block_read_access.py`):

Copy

Ask AI

```
#!/usr/bin/env python3

import sys
import json

ALLOWED_PREFIX = "/Users/yourname/my-project/"

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()

    # Parse the JSON
    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "pre_read_code":
            tool_info = data.get("tool_info", {})
            file_path = tool_info.get("file_path", "")
            
            if not file_path.startswith(ALLOWED_PREFIX):
                print(f"Access denied: Cascade is only allowed to read files under {ALLOWED_PREFIX}", file=sys.stderr)
                sys.exit(2)  # Exit code 2 blocks the action
            
            print(f"Access granted: {file_path}", file=sys.stdout)

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```

When Cascade attempts to read a file outside the allowed directory, this hook blocks the operation and displays an error message.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#blocking-dangerous-commands)

Blocking Dangerous Commands

Prevent Cascade from executing potentially harmful commands. **Config**:

Copy

Ask AI

```
{
  "hooks": {
    "pre_run_command": [
      {
        "command": "python3 /Users/yourname/hooks/block_dangerous_commands.py",
        "show_output": true
      }
    ]
  }
}
```

**Script** (`block_dangerous_commands.py`):

Copy

Ask AI

```
#!/usr/bin/env python3

import sys
import json

DANGEROUS_COMMANDS = ["rm -rf", "sudo rm", "format", "del /f"]

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()

    # Parse the JSON
    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "pre_run_command":
            tool_info = data.get("tool_info", {})
            command = tool_info.get("command_line", "")

            for dangerous_cmd in DANGEROUS_COMMANDS:
                if dangerous_cmd in command:
                    print(f"Command blocked: '{dangerous_cmd}' is not allowed for safety reasons.", file=sys.stderr)
                    sys.exit(2)  # Exit code 2 blocks the command
            
            print(f"Command approved: {command}", file=sys.stdout)

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```

This hook scans commands for dangerous patterns and blocks them before execution.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#blocking-policy-violating-prompts)

Blocking Policy-Violating Prompts

Prevent users from submitting prompts that violate organizational policies. **Config**:

Copy

Ask AI

```
{
  "hooks": {
    "pre_user_prompt": [
      {
        "command": "python3 /Users/yourname/hooks/block_bad_prompts.py"
      }
    ]
  }
}
```

**Script** (`block_bad_prompts.py`):

Copy

Ask AI

```
#!/usr/bin/env python3

import sys
import json

BLOCKED_PATTERNS = [
    "something dangerous",
    "bypass security",
    "ignore previous instructions"
]

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()

    # Parse the JSON
    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "pre_user_prompt":
            tool_info = data.get("tool_info", {})
            user_prompt = tool_info.get("user_prompt", "").lower()

            for pattern in BLOCKED_PATTERNS:
                if pattern in user_prompt:
                    print(f"Prompt blocked: Contains prohibited content. The user cannot ask the agent to do bad things.", file=sys.stderr)
                    sys.exit(2)  # Exit code 2 blocks the prompt

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```

This hook examines user prompts before they are processed and blocks any that contain prohibited patterns. When a prompt is blocked, the user sees an error message in the Cascade UI.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#logging-cascade-responses)

Logging Cascade Responses

Track all Cascade responses for compliance auditing or analytics. **Config**:

Copy

Ask AI

```
{
  "hooks": {
    "post_cascade_response": [
      {
        "command": "python3 /Users/yourname/hooks/log_cascade_response.py"
      }
    ]
  }
}
```

**Script** (`log_cascade_response.py`):

Copy

Ask AI

```
#!/usr/bin/env python3

import sys
import json
from datetime import datetime

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()

    # Parse the JSON
    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "post_cascade_response":
            tool_info = data.get("tool_info", {})
            cascade_response = tool_info.get("response", "")
            trajectory_id = data.get("trajectory_id", "unknown")
            timestamp = data.get("timestamp", datetime.now().isoformat())

            # Log to file
            with open("/Users/yourname/hooks/cascade_responses.log", "a") as f:
                f.write(f"\n{'='*80}\n")
                f.write(f"Timestamp: {timestamp}\n")
                f.write(f"Trajectory ID: {trajectory_id}\n")
                f.write(f"Response:\n{cascade_response}\n")

            print(f"Logged Cascade response for trajectory {trajectory_id}")

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```

This hook logs every Cascade response to a file, creating an audit trail of all AI-generated content. You can extend this to send data to external logging systems, databases, or compliance platforms.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#tracking-triggered-rules)

Tracking Triggered Rules

Track which [rules](https://docs.windsurf.com/windsurf/cascade/memories-and-rules) were applied during Cascade interactions for observability and metrics. **Config**:

Copy

Ask AI

```
{
  "hooks": {
    "post_cascade_response": [
      {
        "command": "python3 /Users/yourname/hooks/track_rules.py"
      }
    ]
  }
}
```

**Script** (`track_rules.py`):

Copy

Ask AI

```
#!/usr/bin/env python3

import sys
import json
import re
from datetime import datetime

def extract_triggered_rules(response: str) -> dict:
    """
    Parse triggered rules from the Cascade response.
    Rules appear as: - (Rule-Type) Triggered Rule: rule-filename.md
    """
    pattern = r"- \(([^)]+)\) Triggered Rule: (.+?)(?:\s*$)"
    rules = {}

    for match in re.finditer(pattern, response, re.MULTILINE):
        rule_type, rule_name = match.groups()
        if rule_type not in rules:
            rules[rule_type] = []
        rules[rule_type].append(rule_name)

    return rules

def main():
    input_data = sys.stdin.read()

    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "post_cascade_response":
            response = data.get("tool_info", {}).get("response", "")
            trajectory_id = data.get("trajectory_id", "unknown")
            timestamp = data.get("timestamp", datetime.now().isoformat())

            rules = extract_triggered_rules(response)
            total_rules = sum(len(v) for v in rules.values())

            # Log to file
            with open("/Users/yourname/hooks/rules_usage.log", "a") as f:
                f.write(f"\n{'='*60}\n")
                f.write(f"Timestamp: {timestamp}\n")
                f.write(f"Trajectory: {trajectory_id}\n")
                f.write(f"Total rules triggered: {total_rules}\n")
                for rule_type, rule_list in rules.items():
                    if rule_list:
                        f.write(f"  {rule_type}: {', '.join(rule_list)}\n")

            print(f"Tracked {total_rules} triggered rules")

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```

**Rule types:**

* `Always On` - Rules that are always included
* `Model Decision` - Rules whose descriptions were shown to the model for conditional application
* `Manual` - Rules explicitly @-mentioned in user input
* `Global` - Global rules from `global_rules.md`
* `Glob` - Rules triggered by file access matching glob patterns

This tracks which rules were *presented* to the model or *triggered* by file access, but does not indicate whether the model actually *followed* a rule. Rules that have already been shown recently in the conversation are deduplicated and may not appear again until later.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#running-code-formatters-after-edits)

Running Code Formatters After Edits

Automatically format code files after Cascade modifies them. **Config**:

Copy

Ask AI

```
{
  "hooks": {
    "post_write_code": [
      {
        "command": "bash /Users/yourname/hooks/format_code.sh",
        "show_output": false
      }
    ]
  }
}
```

**Script** (`format_code.sh`):

Copy

Ask AI

```
#!/bin/bash

# Read JSON from stdin
input=$(cat)

# Extract file path using jq
file_path=$(echo "$input" | jq -r '.tool_info.file_path')

# Format based on file extension
if [[ "$file_path" == *.py ]]; then
    black "$file_path" 2>&1
    echo "Formatted Python file: $file_path"
elif [[ "$file_path" == *.js ]] || [[ "$file_path" == *.ts ]]; then
    prettier --write "$file_path" 2>&1
    echo "Formatted JS/TS file: $file_path"
elif [[ "$file_path" == *.go ]]; then
    gofmt -w "$file_path" 2>&1
    echo "Formatted Go file: $file_path"
fi

exit 0
```

This hook automatically runs the appropriate formatter based on the file type after each edit.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#setting-up-worktrees)

Setting Up Worktrees

Copy environment files and install dependencies when a new worktree is created. **Config** (in `.windsurf/hooks.json`):

Copy

Ask AI

```
{
  "hooks": {
    "post_setup_worktree": [
      {
        "command": "bash $ROOT_WORKSPACE_PATH/hooks/setup_worktree.sh",
        "show_output": true
      }
    ]
  }
}
```

**Script** (`hooks/setup_worktree.sh`):

Copy

Ask AI

```
#!/bin/bash

# Copy environment files from the original workspace
if [ -f "$ROOT_WORKSPACE_PATH/.env" ]; then
    cp "$ROOT_WORKSPACE_PATH/.env" .env
    echo "Copied .env file"
fi

if [ -f "$ROOT_WORKSPACE_PATH/.env.local" ]; then
    cp "$ROOT_WORKSPACE_PATH/.env.local" .env.local
    echo "Copied .env.local file"
fi

# Install dependencies
if [ -f "package.json" ]; then
    npm install
    echo "Installed npm dependencies"
fi

exit 0
```

This hook ensures each worktree has the necessary environment configuration and dependencies installed automatically.

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#best-practices)

Best Practices

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#security)

Security

**Use Cascade Hooks at Your Own Risk**: Hooks execute shell commands automatically with your user account’s full permissions. You are entirely responsible for the code you configure. Poorly designed or malicious hooks can modify files, delete data, expose credentials, or compromise your system.

* **Validate all inputs**: Never trust the input JSON without validation, especially for file paths and commands.
* **Use absolute paths**: Always use absolute paths in your hook configurations to avoid ambiguity.
* **Protect sensitive data**: Avoid logging sensitive information like API keys or credentials.
* **Review permissions**: Ensure your hook scripts have appropriate file system permissions.
* **Audit before deployment**: Review every hook command and script before adding to your configuration.
* **Test in isolation**: Run hooks in a test environment before enabling them on your primary development machine.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#performance-considerations)

Performance Considerations

* **Keep hooks fast**: Slow hooks will impact Cascade’s responsiveness. Aim for sub-100ms execution times.
* **Use async operations**: For non-blocking hooks, consider logging to a queue or database asynchronously.
* **Filter early**: Check the action type at the start of your script to avoid unnecessary processing.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#error-handling)

Error Handling

* **Always validate JSON**: Use try-catch blocks to handle malformed input gracefully.
* **Log errors properly**: Write errors to `stderr` so they’re visible when `show_output` is enabled.
* **Fail safely**: If your hook encounters an error, consider whether it should block the action or allow it to proceed.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#testing-your-hooks)

Testing Your Hooks

1. **Start with logging**: Begin by implementing a simple logging hook to understand the data flow.
2. **Use `show_output: true`**: Enable output during development to see what your hooks are doing.
3. **Test blocking behavior**: Verify that exit code 2 properly blocks actions in pre-hooks.
4. **Check all code paths**: Test both success and failure scenarios in your scripts.

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#enterprise-distribution)

Enterprise Distribution

Enterprise organizations need to enforce security policies, compliance requirements, and development standards that individual users cannot bypass. Cascade Hooks supports two enterprise distribution methods:

1. **Cloud Dashboard** - Configure hooks via Team Settings in the Windsurf dashboard
2. **System-Level Files** - Deploy hooks via MDM or configuration management tools

Both methods can be used together — hooks from all sources are combined and executed in order.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#cloud-dashboard-configuration)

Cloud Dashboard Configuration

Team admins can configure Cascade Hooks directly from the Windsurf dashboard. **Requirements:**

* Enterprise plan
* `TEAM_SETTINGS_UPDATE` permission

**To configure:**

1. Navigate to **Team Settings** in the Windsurf dashboard
2. Find the **Cascade Hooks** section
3. Enter your hooks configuration in JSON format
4. Save your changes

Hooks configured through the dashboard are automatically distributed to all team members and loaded when Windsurf starts. Cloud-configured hooks are loaded first, followed by system-level, user-level, and workspace-level hooks.

When multiple team configurations are merged, hooks are combined per action rather than overwritten. This means hooks from all applicable team configs will run together.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#system-level-file-deployment)

System-Level File Deployment

For organizations that prefer file-based configuration or need hooks to work offline, deploy your mandatory `hooks.json` configuration to these OS-specific locations: **macOS:**

Copy

Ask AI

```
/Library/Application Support/Windsurf/hooks.json
```

**Linux/WSL:**

Copy

Ask AI

```
/etc/windsurf/hooks.json
```

**Windows:**

Copy

Ask AI

```
C:\ProgramData\Windsurf\hooks.json
```

Place your hook scripts in a corresponding system directory (e.g., `/usr/local/share/windsurf-hooks/` on Unix systems). System-level hooks take precedence over user and workspace hooks, and cannot be disabled by end users without root permissions.

#### [​](https://docs.windsurf.com/windsurf/cascade/hooks#mdm-and-configuration-management)

MDM and Configuration Management

Enterprise IT teams can deploy system-level hooks using standard tools: **Mobile Device Management (MDM)**

* **Jamf Pro** (macOS) - Deploy via configuration profiles or scripts
* **Microsoft Intune** (Windows/macOS) - Use PowerShell scripts or policy deployment
* **Workspace ONE**, **Google Endpoint Management**, and other MDM solutions

**Configuration Management**

* **Ansible**, **Puppet**, **Chef**, **SaltStack** - Use your existing infrastructure automation
* **Custom deployment scripts** - Shell scripts, PowerShell, or your preferred tooling

#### [​](https://docs.windsurf.com/windsurf/cascade/hooks#verification-and-auditing)

Verification and Auditing

After deployment, verify that hooks are properly installed:

Copy

Ask AI

```
# Verify system hooks are present
ls -la /etc/windsurf/hooks.json  # Linux
ls -la "/Library/Application Support/Windsurf/hooks.json"  # macOS

# Test hook execution (should see hook output in Cascade)
# Have a developer trigger the relevant Cascade action

# Verify users cannot modify system hooks
sudo chown root:root /etc/windsurf/hooks.json
sudo chmod 644 /etc/windsurf/hooks.json
```

**Important**: System-level hooks are entirely managed by your IT or security team. Windsurf does not deploy or manage files at system-level paths. Ensure your internal teams handle deployment, updates, and compliance according to your organization’s policies.

### [​](https://docs.windsurf.com/windsurf/cascade/hooks#workspace-hooks-for-team-projects)

Workspace Hooks for Team Projects

For project-specific conventions, teams can use workspace-level hooks in version control:

Copy

Ask AI

```
# Add to your repository
.windsurf/
├── hooks.json
└── scripts/
    └── format-check.py

# Commit to git
git add .windsurf/
git commit -m "Add workspace hooks for code formatting"
```

This allows teams to standardize development practices. Keep security-critical policies at the cloud or system level, and avoid checking sensitive information into version control.

## [​](https://docs.windsurf.com/windsurf/cascade/hooks#additional-resources)

Additional Resources

* **MCP Integration**: Learn more about [Model Context Protocol in Windsurf](https://docs.windsurf.com/windsurf/cascade/mcp)
* **Workflows**: Discover how to combine hooks with [Cascade Workflows](https://docs.windsurf.com/windsurf/cascade/workflows)
* **Analytics**: Track Cascade usage with [Team Analytics](https://docs.windsurf.com/windsurf/accounts/analytics)

[Model Context Protocol (MCP)](https://docs.windsurf.com/windsurf/cascade/mcp)[Usage](https://docs.windsurf.com/windsurf/accounts/usage)

⌘I

[twitter](https://x.com/windsurf)[discord](https://discord.com/invite/3XFf78nAx5)

[Powered by](https://www.mintlify.com/?utm_campaign=poweredBy&utm_medium=referral&utm_source=codeium)
