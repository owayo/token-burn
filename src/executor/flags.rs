use crate::config::RuntimeAgent;

const CLAUDE_BLOCKED_INTERACTIVE_TOOL: &str = "AskUserQuestion";
/// `claude -p` はメインターン終了後にバックグラウンドタスク（Agent
/// run_in_background / Workflow 等）が残っていると、既定 600 秒で
/// "Background tasks still running after 600s; terminating." を出して全タスクを
/// 強制終了し、仕事が未完のまま `is_error:false` の result で正常終了してしまう。
/// 無期限待機（=0）に切り替えて、サブエージェントの完了通知でメインループが
/// 再開し実際に完走できるようにする。agent/profile の env で明示済みなら尊重する。
pub(super) const CLAUDE_PRINT_BG_WAIT_ENV: &str = "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS";
/// codex を無人実行する際、コマンド承認待ちで停止しないための config override。
/// `codex exec` には `--ask-for-approval` フラグが無い（0.136.0）ため、サブコマンドの
/// オプション表面に依存しない top-level の `-c approval_policy=never` を使う。
const CODEX_APPROVAL_OVERRIDE: &str = "approval_policy=never";

/// command の先頭要素（実行ファイル）の basename（拡張子なし）を返す。
/// 空 command やパス解決不能な場合は空文字列を返す。
fn command_basename(command: &[String]) -> &str {
    let Some(first) = command.first() else {
        return "";
    };
    std::path::Path::new(first.as_str())
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
}

/// command の先頭要素が claude 実行ファイル（ラッパースクリプト含む）かを判定する。
/// ファイル名（basename）が "claude" そのもの、または "claude-" / "claude_" で始まる場合に true。
fn is_claude_command(command: &[String]) -> bool {
    let basename = command_basename(command);
    basename == "claude" || basename.starts_with("claude-") || basename.starts_with("claude_")
}

/// command の先頭要素が codex 実行ファイル（ラッパースクリプト含む）かを判定する。
/// ファイル名（basename）が "codex" そのもの、または "codex-" / "codex_" で始まる場合に true。
fn is_codex_command(command: &[String]) -> bool {
    let basename = command_basename(command);
    basename == "codex" || basename.starts_with("codex-") || basename.starts_with("codex_")
}

/// provider が明示されていればそれを優先し、無ければ実行ファイル名から推論する。
pub(super) fn agent_is_claude(agent: &RuntimeAgent) -> bool {
    match agent.provider.as_deref() {
        Some(p) => p.eq_ignore_ascii_case("claude"),
        None => is_claude_command(&agent.command),
    }
}

/// RuntimeAgent が codex provider かを判定する。
fn agent_is_codex(agent: &RuntimeAgent) -> bool {
    match agent.provider.as_deref() {
        Some(p) => p.eq_ignore_ascii_case("codex"),
        None => is_codex_command(&agent.command),
    }
}

/// 既知エージェントに必要なフラグを自動付与する。
/// provider（または実行ファイル名）が `claude` か `codex` かを判定し、それぞれの
/// 無人実行に必要なフラグを付与する。いずれにも該当しない場合は何もしない。
pub(super) fn ensure_required_flags(agent: &mut RuntimeAgent) {
    if agent_is_claude(agent) {
        ensure_claude_required_flags(agent);
    } else if agent_is_codex(agent) {
        ensure_codex_unattended_flags(&mut agent.command);
    }
}

/// `claude` の場合、`-p`、`--verbose`、`--output-format stream-json`、
/// `--include-partial-messages` はログ取得に必須であり、常に存在しなければならない。
/// また、token-burn は無人実行のため、
/// ユーザー回答待ちで停止する `AskUserQuestion` を禁止する。
/// さらにバックグラウンドタスクの 600 秒強制終了を無効化する env を注入する
/// （[`CLAUDE_PRINT_BG_WAIT_ENV`] のドキュメント参照）。
fn ensure_claude_required_flags(agent: &mut RuntimeAgent) {
    agent
        .env
        .entry(CLAUDE_PRINT_BG_WAIT_ENV.to_string())
        .or_insert_with(|| "0".to_string());
    let needs_print = !agent.command.iter().any(|s| s == "-p" || s == "--print");
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

    if needs_print {
        agent.command.push("-p".to_string());
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
    ensure_disallowed_tool(&mut agent.command, CLAUDE_BLOCKED_INTERACTIVE_TOOL);
}

/// codex は無人バッチ実行のため、コマンド承認待ちで停止しないよう
/// `-c approval_policy=never` を付与する。
///
/// `codex exec` には `--ask-for-approval` フラグが存在しない（0.136.0 で確認）ため、
/// サブコマンドのオプション表面に依存しない top-level の config override を
/// 実行ファイル直後に挿入する（`codex -c approval_policy=never exec ...`）。
/// `--sandbox` とは独立した軸なので、サンドボックス指定の有無に関わらず付与する。
///
/// ユーザーが承認方針を明示済みの場合（`-a` / `--ask-for-approval` /
/// `-c approval_policy=...` / `--dangerously-bypass-approvals-and-sandbox`）は
/// その意図を尊重し、何も付与しない。
fn ensure_codex_unattended_flags(command: &mut Vec<String>) {
    if has_codex_approval_override(command) {
        return;
    }
    // 実行ファイル（command[0]）の直後に top-level config override を挿入する。
    let insert_at = command.len().min(1);
    command.insert(insert_at, CODEX_APPROVAL_OVERRIDE.to_string());
    command.insert(insert_at, "-c".to_string());
}

/// ユーザーが codex の承認方針を明示済みかを判定する。
/// 明示済みなら token-burn は `approval_policy` を上書きしない。
///
/// clap の short option は値結合形式（`-aVALUE` / `-a=VALUE`）も受理するため、
/// スペース区切り・結合・`=` 形式のすべてを検出する。`--` 以降は位置引数
/// （prompt 等）なのでオプションとして解釈しない。
fn has_codex_approval_override(command: &[String]) -> bool {
    let mut idx = 0usize;
    while idx < command.len() {
        let arg = &command[idx];

        // `--` 以降は位置引数。オプション走査を打ち切る。
        if arg == "--" {
            break;
        }

        // 承認フラグ: --ask-for-approval / -a（単体・結合・= 形式）、
        // および承認とサンドボックスを一括無効化する bypass フラグ。
        if arg == "--dangerously-bypass-approvals-and-sandbox"
            || arg == "-a"
            || arg == "--ask-for-approval"
            || arg.starts_with("--ask-for-approval=")
            || arg.strip_prefix("-a").is_some_and(|rest| !rest.is_empty())
        {
            return true;
        }

        // -c / --config のスペース区切り形式（値は次要素）
        if arg == "-c" || arg == "--config" {
            if command
                .get(idx + 1)
                .is_some_and(|value| is_approval_policy_override(value))
            {
                return true;
            }
            idx += 2;
            continue;
        }

        // --config=key=value 形式
        if let Some(value) = arg.strip_prefix("--config=")
            && is_approval_policy_override(value)
        {
            return true;
        }

        // -c 値結合形式（-cKEY=VALUE / -c=KEY=VALUE）
        if let Some(rest) = arg.strip_prefix("-c").filter(|rest| !rest.is_empty()) {
            let value = rest.strip_prefix('=').unwrap_or(rest);
            if is_approval_policy_override(value) {
                return true;
            }
        }

        idx += 1;
    }
    false
}

/// `key=value` 形式の config override が `approval_policy` キーかを判定する。
fn is_approval_policy_override(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(key, _)| key == "approval_policy")
}

fn ensure_disallowed_tool(command: &mut Vec<String>, tool: &str) {
    let mut idx = 0usize;
    while idx < command.len() {
        if let Some((flag, tools)) = command[idx].split_once('=')
            && is_disallowed_tools_flag(flag)
        {
            let flag = flag.to_string();
            if !tool_list_contains(tools, tool) {
                command[idx] = format!("{flag}={}", append_tool(tools, tool));
            }
            return;
        }

        if is_disallowed_tools_flag(&command[idx]) {
            let flag = command[idx].clone();
            let value_start = idx + 1;
            let mut value_end = value_start;
            while value_end < command.len() && !command[value_end].starts_with('-') {
                value_end += 1;
            }

            let mut tools = command[value_start..value_end].join(",");
            if !tool_list_contains(&tools, tool) {
                tools = append_tool(&tools, tool);
            }
            command[idx] = format!("{flag}={tools}");
            command.drain(value_start..value_end);
            return;
        }

        idx += 1;
    }

    command.push(format!("--disallowedTools={tool}"));
}

fn is_disallowed_tools_flag(flag: &str) -> bool {
    flag == "--disallowedTools" || flag == "--disallowed-tools"
}

fn tool_list_contains(tools: &str, tool: &str) -> bool {
    tools
        .split(|c: char| c == ',' || c.is_whitespace())
        .any(|part| part == tool)
}

fn append_tool(tools: &str, tool: &str) -> String {
    let tools = tools.trim_end();
    if tools.is_empty() {
        tool.to_string()
    } else {
        format!("{tools},{tool}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent(command: Vec<&str>) -> RuntimeAgent {
        RuntimeAgent {
            name: "claude".to_string(),
            command: command.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn ensure_required_flags_adds_missing_claude_flags() {
        let mut agent = make_agent(vec!["claude"]);
        ensure_required_flags(&mut agent);
        assert!(agent.command.contains(&"-p".to_string()));
        assert!(agent.command.contains(&"--verbose".to_string()));
        assert!(agent.command.contains(&"--output-format".to_string()));
        assert!(agent.command.contains(&"stream-json".to_string()));
        assert!(
            agent
                .command
                .contains(&"--include-partial-messages".to_string())
        );
        assert!(
            agent
                .command
                .contains(&"--disallowedTools=AskUserQuestion".to_string())
        );
    }

    #[test]
    fn ensure_required_flags_injects_bg_wait_env_for_claude() {
        let mut agent = make_agent(vec!["claude"]);
        ensure_required_flags(&mut agent);
        assert_eq!(
            agent.env.get(CLAUDE_PRINT_BG_WAIT_ENV).map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn ensure_required_flags_respects_user_bg_wait_env() {
        let mut agent = make_agent(vec!["claude"]);
        agent
            .env
            .insert(CLAUDE_PRINT_BG_WAIT_ENV.to_string(), "600000".to_string());
        ensure_required_flags(&mut agent);
        assert_eq!(
            agent.env.get(CLAUDE_PRINT_BG_WAIT_ENV).map(String::as_str),
            Some("600000")
        );

        // 空文字（= unset 指定）も上書きしない。env -u で claude 既定に戻せる余地を残す。
        let mut agent = make_agent(vec!["claude"]);
        agent
            .env
            .insert(CLAUDE_PRINT_BG_WAIT_ENV.to_string(), String::new());
        ensure_required_flags(&mut agent);
        assert_eq!(
            agent.env.get(CLAUDE_PRINT_BG_WAIT_ENV).map(String::as_str),
            Some("")
        );
    }

    #[test]
    fn ensure_required_flags_does_not_inject_bg_wait_env_for_codex() {
        let mut agent = RuntimeAgent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        ensure_required_flags(&mut agent);
        assert!(!agent.env.contains_key(CLAUDE_PRINT_BG_WAIT_ENV));
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
            "--disallowedTools=AskUserQuestion",
        ]);
        let original_len = agent.command.len();
        ensure_required_flags(&mut agent);
        assert_eq!(agent.command.len(), original_len);
    }

    #[test]
    fn ensure_required_flags_accepts_long_print_flag_without_duplicate() {
        let mut agent = make_agent(vec![
            "claude",
            "--print",
            "--verbose",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
            "--disallowedTools=AskUserQuestion",
        ]);
        let original_len = agent.command.len();
        ensure_required_flags(&mut agent);

        assert_eq!(agent.command.len(), original_len);
        assert!(!agent.command.iter().any(|s| s == "-p"));
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
        assert_eq!(agent.command.len(), original_len + 3);
        assert!(
            agent
                .command
                .contains(&"--output-format=stream-json".to_string())
        );
        assert!(!agent.command.iter().any(|s| s == "--output-format"));
        assert!(
            agent
                .command
                .contains(&"--disallowedTools=AskUserQuestion".to_string())
        );
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
    fn ensure_required_flags_appends_ask_user_question_to_existing_disallowed_tools() {
        let mut agent = make_agent(vec![
            "claude",
            "-p",
            "--disallowedTools",
            "Bash,Edit",
            "--verbose",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
        ]);
        ensure_required_flags(&mut agent);

        assert!(
            agent
                .command
                .contains(&"--disallowedTools=Bash,Edit,AskUserQuestion".to_string())
        );
        assert!(!agent.command.iter().any(|s| s == "--disallowedTools"));
    }

    #[test]
    fn ensure_required_flags_accepts_kebab_disallowed_tools_flag_without_duplicate() {
        let mut agent = make_agent(vec![
            "claude",
            "-p",
            "--disallowed-tools",
            "Bash",
            "AskUserQuestion",
            "--verbose",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
        ]);
        ensure_required_flags(&mut agent);

        assert!(
            agent
                .command
                .contains(&"--disallowed-tools=Bash,AskUserQuestion".to_string())
        );
        assert_eq!(
            agent
                .command
                .iter()
                .filter(|arg| arg.contains("AskUserQuestion"))
                .count(),
            1
        );
    }

    #[test]
    fn ensure_required_flags_appends_to_equals_style_disallowed_tools() {
        let mut agent = make_agent(vec![
            "claude",
            "-p",
            "--disallowedTools=Bash",
            "--verbose",
            "--output-format",
            "stream-json",
            "--include-partial-messages",
        ]);
        ensure_required_flags(&mut agent);

        assert!(
            agent
                .command
                .contains(&"--disallowedTools=Bash,AskUserQuestion".to_string())
        );
    }

    #[test]
    fn ensure_required_flags_empty_command_returns_early() {
        // command が空の場合（executable が空文字列にならない）
        let mut agent = RuntimeAgent {
            name: "test".to_string(),
            command: vec![],
            ..Default::default()
        };
        let original_len = agent.command.len();
        ensure_required_flags(&mut agent);
        // 空のcommandは "claude" ではないので何も変更されない
        assert_eq!(agent.command.len(), original_len);
    }

    #[test]
    fn ensure_required_flags_adds_codex_approval_policy() {
        let mut agent = RuntimeAgent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        ensure_required_flags(&mut agent);
        // 実行ファイル直後に top-level config override が挿入される
        assert_eq!(
            agent.command,
            vec![
                "codex".to_string(),
                "-c".to_string(),
                "approval_policy=never".to_string(),
                "exec".to_string(),
            ]
        );
    }

    #[test]
    fn ensure_required_flags_ignores_unknown_agent() {
        // claude でも codex でもないエージェントは一切変更しない
        let mut agent = RuntimeAgent {
            name: "aider".to_string(),
            command: vec!["aider".to_string(), "--yes".to_string()],
            ..Default::default()
        };
        let original = agent.command.clone();
        ensure_required_flags(&mut agent);
        assert_eq!(agent.command, original);
    }

    #[test]
    fn is_claude_command_detects_bare_claude() {
        assert!(is_claude_command(&["claude".to_string()]));
        assert!(is_claude_command(&["claude".to_string(), "-p".to_string()]));
    }

    #[test]
    fn is_claude_command_detects_wrapper_script() {
        assert!(is_claude_command(&[
            "/opt/tools/claude-wrapper.sh".to_string()
        ]));
        assert!(is_claude_command(&[
            "./claude-wrapper.sh".to_string(),
            "-p".to_string(),
        ]));
        assert!(is_claude_command(&["claude-code.sh".to_string()]));
        assert!(is_claude_command(&["claude_custom".to_string()]));
    }

    #[test]
    fn is_claude_command_rejects_non_claude() {
        assert!(!is_claude_command(&["codex".to_string()]));
        assert!(!is_claude_command(&["my-claude-fork".to_string()]));
        assert!(!is_claude_command(&[]));
    }

    #[test]
    fn is_codex_command_detects_bare_and_wrapper() {
        assert!(is_codex_command(&["codex".to_string()]));
        assert!(is_codex_command(&["codex".to_string(), "exec".to_string()]));
        assert!(is_codex_command(&[
            "/opt/tools/codex-wrapper.sh".to_string()
        ]));
        assert!(is_codex_command(&["codex_custom".to_string()]));
    }

    #[test]
    fn is_codex_command_rejects_non_codex() {
        assert!(!is_codex_command(&["claude".to_string()]));
        assert!(!is_codex_command(&["my-codex-fork".to_string()]));
        assert!(!is_codex_command(&[]));
    }

    #[test]
    fn ensure_codex_unattended_flags_inserts_after_executable() {
        // ユーザーの実際の構成に近いコマンド
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "--sandbox".to_string(),
            "danger-full-access".to_string(),
            "-c".to_string(),
            "model='gpt-5.5'".to_string(),
        ];
        ensure_codex_unattended_flags(&mut command);
        // 実行ファイル直後に -c approval_policy=never が入る
        assert_eq!(
            &command[..3],
            &[
                "codex".to_string(),
                "-c".to_string(),
                "approval_policy=never".to_string(),
            ]
        );
        // 既存の引数は保持される
        assert!(command.contains(&"--sandbox".to_string()));
        assert!(command.contains(&"model='gpt-5.5'".to_string()));
        // 二重付与しない
        assert_eq!(
            command
                .iter()
                .filter(|s| *s == "approval_policy=never")
                .count(),
            1
        );
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_short_approval_flag() {
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "-a".to_string(),
            "on-request".to_string(),
        ];
        let original = command.clone();
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(command, original);
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_long_approval_flag() {
        let mut command = vec![
            "codex".to_string(),
            "--ask-for-approval".to_string(),
            "never".to_string(),
            "exec".to_string(),
        ];
        let original = command.clone();
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(command, original);
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_equals_approval_flag() {
        let mut command = vec![
            "codex".to_string(),
            "--ask-for-approval=never".to_string(),
            "exec".to_string(),
        ];
        let original = command.clone();
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(command, original);
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_existing_approval_policy() {
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "-c".to_string(),
            "approval_policy=on-request".to_string(),
        ];
        let original = command.clone();
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(command, original);
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_config_equals_approval_policy() {
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "--config=approval_policy=untrusted".to_string(),
        ];
        let original = command.clone();
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(command, original);
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_bypass_flag() {
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
        ];
        let original = command.clone();
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(command, original);
    }

    #[test]
    fn ensure_codex_unattended_flags_ignores_unrelated_config() {
        // -c model=... のような無関係な config override は尊重判定にならず付与される
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "-c".to_string(),
            "model='gpt-5.5'".to_string(),
        ];
        ensure_codex_unattended_flags(&mut command);
        assert!(command.contains(&"approval_policy=never".to_string()));
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_attached_short_approval() {
        // -a=never / -anever（clap の short option 値結合形式）も尊重する
        for arg in ["-a=never", "-anever"] {
            let mut command = vec!["codex".to_string(), arg.to_string(), "exec".to_string()];
            let original = command.clone();
            ensure_codex_unattended_flags(&mut command);
            assert_eq!(command, original, "should respect {arg}");
        }
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_attached_config_approval() {
        // -capproval_policy=... / -c=approval_policy=...（-c 値結合形式）も尊重する
        for arg in [
            "-capproval_policy=on-request",
            "-c=approval_policy=on-request",
        ] {
            let mut command = vec!["codex".to_string(), "exec".to_string(), arg.to_string()];
            let original = command.clone();
            ensure_codex_unattended_flags(&mut command);
            assert_eq!(command, original, "should respect {arg}");
        }
    }

    #[test]
    fn ensure_codex_unattended_flags_respects_spaced_config_approval() {
        // --config approval_policy=...（スペース区切りの long config）も尊重する
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "--config".to_string(),
            "approval_policy=never".to_string(),
        ];
        let original = command.clone();
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(command, original);
    }

    #[test]
    fn ensure_codex_unattended_flags_attached_unrelated_config_still_adds() {
        // -cmodel=... のような無関係な結合 config では尊重判定にならず付与される
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "-cmodel=gpt-5.5".to_string(),
        ];
        ensure_codex_unattended_flags(&mut command);
        assert!(command.contains(&"approval_policy=never".to_string()));
    }

    #[test]
    fn ensure_codex_unattended_flags_stops_at_double_dash() {
        // `--` 以降は位置引数。承認フラグとして誤検出せず付与する
        let mut command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "--".to_string(),
            "-a".to_string(),
        ];
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(
            &command[..3],
            &[
                "codex".to_string(),
                "-c".to_string(),
                "approval_policy=never".to_string(),
            ]
        );
        // `--` 以降はそのまま保持される
        assert!(command.contains(&"--".to_string()));
        assert!(command.contains(&"-a".to_string()));
    }

    #[test]
    fn ensure_required_flags_works_with_wrapper() {
        let mut agent = RuntimeAgent {
            name: "claude".to_string(),
            command: vec!["/opt/tools/claude-wrapper.sh".to_string()],
            ..Default::default()
        };
        ensure_required_flags(&mut agent);
        assert!(agent.command.contains(&"-p".to_string()));
        assert!(agent.command.contains(&"--verbose".to_string()));
        assert!(agent.command.contains(&"--output-format".to_string()));
        assert!(agent.command.contains(&"stream-json".to_string()));
        assert!(
            agent
                .command
                .contains(&"--include-partial-messages".to_string())
        );
        assert!(
            agent
                .command
                .contains(&"--disallowedTools=AskUserQuestion".to_string())
        );
    }

    #[test]
    fn ensure_required_flags_rewrites_equals_style_non_stream_json() {
        // --output-format=text のequals形式は --output-format=stream-json に書き換えられる
        let mut agent = make_agent(vec!["claude", "-p", "--output-format=text"]);
        ensure_required_flags(&mut agent);
        assert!(
            agent
                .command
                .contains(&"--output-format=stream-json".to_string()),
            "equals形式の値が stream-json に書き換えられるべき: {:?}",
            agent.command
        );
        assert!(
            !agent.command.contains(&"--output-format=text".to_string()),
            "元の値が残るべきでない: {:?}",
            agent.command
        );
    }
}
