use crate::config::{CodexOutput, RuntimeAgent, agent_runs_cli, command_basename};

use super::OutputPipeline;

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
/// codex の top-level オプションのうち値を取るもの（codex-cli 0.161.0 の `codex --help` で確認）。
///
/// サブコマンドを探すとき、これらの値（`-p exec` のプロファイル名など）を位置引数と取り違えない
/// よう、オプションと値をまとめて読み飛ばす。`-i` / `--image` は複数の値を取り得るが 1 つとして
/// 扱う（`-i a.png,b.png` のカンマ区切りなら正しく読める）。読み違えてもサブコマンドが見つからない
/// だけで、`codex_output = "auto"` は従来どおりの素通しへ倒れる。
const CODEX_VALUE_OPTIONS: [&str; 20] = [
    "-c",
    "--config",
    "-m",
    "--model",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "-C",
    "--cd",
    "-i",
    "--image",
    "--enable",
    "--disable",
    "--local-provider",
    "--add-dir",
    "--remote",
    "--remote-auth-token-env",
];

/// provider が明示されていればそれを優先し、無ければ実行ファイル名から推論する。
pub(super) fn agent_is_claude(agent: &RuntimeAgent) -> bool {
    agent_runs_cli(agent.provider.as_deref(), &agent.command, "claude")
}

/// RuntimeAgent が codex provider かを判定する。
fn agent_is_codex(agent: &RuntimeAgent) -> bool {
    agent_runs_cli(agent.provider.as_deref(), &agent.command, "codex")
}

/// 既知エージェントに必要なフラグを自動付与し、タスクの出力の処理方法を返す。
/// provider（または実行ファイル名）が `claude` か `codex` かを判定し、それぞれの
/// 無人実行に必要なフラグを付与する。いずれにも該当しない場合は何もせず、出力は素通しにする。
pub(super) fn ensure_required_flags(agent: &mut RuntimeAgent) -> OutputPipeline {
    if agent_is_claude(agent) {
        ensure_claude_required_flags(agent);
        OutputPipeline::ClaudeStreamJson
    } else if agent_is_codex(agent) {
        ensure_codex_unattended_flags(&mut agent.command);
        ensure_codex_output_flags(&mut agent.command, agent.codex_output)
    } else {
        OutputPipeline::Raw
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
    let executable = env_command_index(command);
    if has_codex_approval_override(&command[executable..]) {
        return;
    }
    // env のオプションや代入を保ち、内側の実行ファイル直後に挿入する。
    let insert_at = command.len().min(executable + 1);
    command.insert(insert_at, CODEX_APPROVAL_OVERRIDE.to_string());
    command.insert(insert_at, "-c".to_string());
}

/// env の前置きがある場合、実行ファイルが独立した引数になっている位置を返す。
fn env_command_index(command: &[String]) -> usize {
    if command_basename(command) != "env" {
        return 0;
    }
    let mut idx = 1;
    let mut options = true;
    while let Some(arg) = command.get(idx) {
        if options && arg == "--" {
            idx += 1;
            options = false;
        } else if options && matches!(arg.as_str(), "-u" | "--unset" | "-C" | "--chdir" | "-P") {
            idx += 2;
        } else if options && (arg.starts_with("-S") || arg.starts_with("--split-string")) {
            // split-string 内の実行ファイルは独立した引数ではない。既存の扱いを維持する。
            return 0;
        } else if options && arg.starts_with('-') {
            idx += 1;
        } else if arg.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty()
                && key.bytes().enumerate().all(|(i, b)| {
                    b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                })
        }) {
            options = false;
            idx += 1;
        } else {
            return idx;
        }
    }
    0
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

/// codex の出力モードに応じて `--json` を付与し、タスクの出力の処理方法を返す。
///
/// 人間向けの出力は、ファイルを変更するたびにそのターンの累積 diff 全体を出し直す
/// （実測で 141MB のログのうち 126MB、1 回あたり 100〜500KB）。ペインが常に diff の末尾を
/// 映して止まって見えるため、`codex exec --json` の JSONL を `format-codex` で整形する。
///
/// `--json` は `exec` サブコマンドの直後に挿入する。`exec resume ...` でも `exec` の直後に
/// 置けば受け付けられ、`e` は `exec` の別名。既に `--json`（旧名 `--experimental-json`）が
/// あれば二重に付けない。
fn ensure_codex_output_flags(command: &mut Vec<String>, mode: CodexOutput) -> OutputPipeline {
    if mode == CodexOutput::Human {
        return OutputPipeline::CodexRaw;
    }
    match codex_exec_index(command) {
        Some(exec) => {
            let has_json = command[exec + 1..]
                .iter()
                .take_while(|arg| arg.as_str() != "--")
                .any(|arg| arg == "--json" || arg == "--experimental-json");
            if !has_json {
                command.insert(exec + 1, "--json".to_string());
            }
            OutputPipeline::CodexJson
        }
        // `exec` を見つけられない（サブコマンドを自分で足すラッパー等）。`json` は「JSONL を出す」
        // というユーザーの申告に従って整形する。`auto` は出力の形が分からないので、従来どおり
        // 何も足さずに素通しする（JSONL でない出力を整形器へ流すと読めなくなる）。
        None if mode == CodexOutput::Json => OutputPipeline::CodexJson,
        None => OutputPipeline::CodexRaw,
    }
}

/// command の中の codex の `exec` サブコマンド（別名 `e`）の位置を返す。見つからなければ `None`。
///
/// 実行ファイルより後ろを codex の argv として読み、最初の位置引数をサブコマンドとみなす。
/// env の前置きは読み飛ばす。値を取る top-level オプション（[`CODEX_VALUE_OPTIONS`]）は値ごと
/// 飛ばし、`--opt=value` や `-mVALUE` のような結合形式を含むそれ以外の `-…` は 1 要素として
/// 飛ばす。`--` 以降は位置引数なのでサブコマンドではない（clap も `--` の後ろをサブコマンドと
/// して解釈しない）。単独の `-` は clap と同じく位置引数として扱う。
fn codex_exec_index(command: &[String]) -> Option<usize> {
    let mut idx = env_command_index(command) + 1;
    while let Some(arg) = command.get(idx) {
        if arg == "--" {
            return None;
        }
        if CODEX_VALUE_OPTIONS.contains(&arg.as_str()) {
            idx += 2;
        } else if arg.starts_with('-') && arg != "-" {
            idx += 1;
        } else {
            return matches!(arg.as_str(), "exec" | "e").then_some(idx);
        }
    }
    None
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

/// どのセッションで起動するかを決める claude のフラグ（値付き・値なしの両方）。
///
/// これらがエージェントの command に既にあると、token-burn が足す `--resume <id>` と
/// 衝突する（`--continue` は直近の別セッションを、`--session-id` は固定 ID を選び、
/// `--no-session-persistence` はそもそも再開できるセッションを残さない）。
const CLAUDE_SESSION_FLAGS: [&str; 8] = [
    "--resume",
    "-r",
    "--continue",
    "-c",
    "--session-id",
    "--fork-session",
    "--no-session-persistence",
    "--from-pr",
];

/// command に既にあるセッション系フラグを返す（最初の 1 つ）。
///
/// シェル文字列の部分一致ではなく argv の要素として判定する（`--resume=<id>` 形式も拾う）。
/// `--` 以降は位置引数なので見ない。
pub(super) fn claude_session_flag(command: &[String]) -> Option<&str> {
    command
        .iter()
        .skip(1)
        .take_while(|arg| arg.as_str() != "--")
        .find_map(|arg| {
            let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
            CLAUDE_SESSION_FLAGS.contains(&flag).then_some(flag)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_session_flag_detects_flags_as_argv_elements() {
        let cmd = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(claude_session_flag(&cmd(&["claude", "-p"])), None);
        assert_eq!(
            claude_session_flag(&cmd(&["claude", "--continue", "-p"])),
            Some("--continue")
        );
        assert_eq!(claude_session_flag(&cmd(&["claude", "-c"])), Some("-c"));
        assert_eq!(
            claude_session_flag(&cmd(&["claude", "--session-id=0633fa18"])),
            Some("--session-id")
        );
        assert_eq!(
            claude_session_flag(&cmd(&["claude", "--no-session-persistence"])),
            Some("--no-session-persistence")
        );
        // 部分一致では拾わない（別オプションの値やモデル名に含まれる文字列）
        assert_eq!(
            claude_session_flag(&cmd(&["claude", "--model=resume-model", "--resumes"])),
            None
        );
        // `--` 以降は位置引数
        assert_eq!(
            claude_session_flag(&cmd(&["claude", "-p", "--", "--resume"])),
            None
        );
        // 実行ファイル自体の名前は見ない
        assert_eq!(claude_session_flag(&cmd(&["-c"])), None);
    }

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
        // 実行ファイル直後に top-level config override が、exec 直後に --json が挿入される
        assert_eq!(
            agent.command,
            vec![
                "codex".to_string(),
                "-c".to_string(),
                "approval_policy=never".to_string(),
                "exec".to_string(),
                "--json".to_string(),
            ]
        );
    }

    #[test]
    fn ensure_required_flags_ignores_unknown_agent() {
        // claude でも codex でもないエージェントは一切変更せず、出力も素通しにする
        let mut agent = RuntimeAgent {
            name: "aider".to_string(),
            command: vec!["aider".to_string(), "--yes".to_string()],
            ..Default::default()
        };
        let original = agent.command.clone();
        assert_eq!(ensure_required_flags(&mut agent), OutputPipeline::Raw);
        assert_eq!(agent.command, original);
    }

    #[test]
    fn ensure_required_flags_selects_the_claude_pipeline() {
        let mut agent = make_agent(vec!["claude"]);
        assert_eq!(
            ensure_required_flags(&mut agent),
            OutputPipeline::ClaudeStreamJson
        );
    }

    // ─────────── codex の出力（codex_output と --json の付与） ───────────

    fn codex_agent(command: &[&str], mode: CodexOutput) -> RuntimeAgent {
        RuntimeAgent {
            name: "codex".to_string(),
            provider: Some("codex".to_string()),
            command: command.iter().map(|s| s.to_string()).collect(),
            codex_output: mode,
            ..Default::default()
        }
    }

    /// 実運用の構成（provider = "codex" のラッパースクリプトで exec を渡す）。承認方針は
    /// 実行ファイルの直後、--json は exec の直後に入り、既存の引数の順序は変わらない。
    #[test]
    fn codex_wrapper_with_exec_gets_json_right_after_exec() {
        let mut agent = codex_agent(
            &[
                "/opt/bin/codex-wrapper.sh",
                "exec",
                "--sandbox",
                "danger-full-access",
                "-c",
                "model='gpt-6.1-sol'",
                "-c",
                "model_reasoning_effort='xhigh'",
            ],
            CodexOutput::Auto,
        );
        assert_eq!(ensure_required_flags(&mut agent), OutputPipeline::CodexJson);
        assert_eq!(
            agent.command,
            [
                "/opt/bin/codex-wrapper.sh",
                "-c",
                "approval_policy=never",
                "exec",
                "--json",
                "--sandbox",
                "danger-full-access",
                "-c",
                "model='gpt-6.1-sol'",
                "-c",
                "model_reasoning_effort='xhigh'",
            ]
        );
        // 2 回適用しても増えない（approval_policy も --json も既存として検出される）
        let once = agent.command.clone();
        ensure_required_flags(&mut agent);
        assert_eq!(agent.command, once);
    }

    #[test]
    fn codex_exec_alias_e_gets_json() {
        let mut agent = codex_agent(&["codex", "e", "--skip-git-repo-check"], CodexOutput::Auto);
        assert_eq!(ensure_required_flags(&mut agent), OutputPipeline::CodexJson);
        assert_eq!(
            agent.command,
            [
                "codex",
                "-c",
                "approval_policy=never",
                "e",
                "--json",
                "--skip-git-repo-check"
            ]
        );
    }

    #[test]
    fn codex_json_follows_exec_inside_env_prefix() {
        let mut agent = codex_agent(
            &["env", "-u", "OLD_TOKEN", "TOKEN=value", "codex", "exec"],
            CodexOutput::Auto,
        );
        assert_eq!(ensure_required_flags(&mut agent), OutputPipeline::CodexJson);
        assert_eq!(
            agent.command,
            [
                "env",
                "-u",
                "OLD_TOKEN",
                "TOKEN=value",
                "codex",
                "-c",
                "approval_policy=never",
                "exec",
                "--json",
            ]
        );
    }

    /// exec より前の top-level オプションは値ごと読み飛ばす。値（ここではプロファイル名 `exec`）を
    /// サブコマンドと取り違えない。`--opt=value` と `-mVALUE` は 1 要素で、未知の `--flag` は値を取らない。
    #[test]
    fn codex_exec_is_found_after_global_options_and_their_values() {
        let mut command: Vec<String> = [
            "codex",
            "-p",
            "exec",
            "--model=gpt-6.1-sol",
            "-mgpt-6.1-sol",
            "--oss",
            "-C",
            "/tmp/repo",
            "--add-dir",
            "/tmp/shared",
            "--enable",
            "web_search",
            "--remote",
            "ws://127.0.0.1:1",
            "-a",
            "never",
            "exec",
            "review this",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(codex_exec_index(&command), Some(16));
        assert_eq!(
            ensure_codex_output_flags(&mut command, CodexOutput::Auto),
            OutputPipeline::CodexJson
        );
        assert_eq!(&command[16..], ["exec", "--json", "review this"]);
    }

    #[test]
    fn codex_existing_json_flag_is_not_duplicated() {
        for flag in ["--json", "--experimental-json"] {
            let mut command: Vec<String> = ["codex", "exec", "--sandbox", "read-only", flag]
                .map(String::from)
                .to_vec();
            let original = command.clone();
            assert_eq!(
                ensure_codex_output_flags(&mut command, CodexOutput::Auto),
                OutputPipeline::CodexJson,
                "{flag}"
            );
            assert_eq!(command, original, "{flag} を二重に付けてはいけない");
        }
    }

    #[test]
    fn codex_exec_resume_gets_json_right_after_exec() {
        let mut command: Vec<String> = ["codex", "exec", "resume", "--last"]
            .map(String::from)
            .to_vec();
        ensure_codex_output_flags(&mut command, CodexOutput::Auto);
        assert_eq!(command, ["codex", "exec", "--json", "resume", "--last"]);
    }

    /// `--` 以降は位置引数。そこに `exec` や `--json` があってもサブコマンドや既存フラグとは扱わない。
    #[test]
    fn codex_double_dash_ends_the_option_scan() {
        // `--` の後ろの exec はプロンプトの一部（サブコマンドではない）
        let mut command: Vec<String> = ["codex", "--", "exec"].map(String::from).to_vec();
        let original = command.clone();
        assert_eq!(
            ensure_codex_output_flags(&mut command, CodexOutput::Auto),
            OutputPipeline::CodexRaw
        );
        assert_eq!(command, original);

        // exec の後ろでも、`--` より後ろの `--json` はプロンプトの文字列
        let mut command: Vec<String> = ["codex", "exec", "--", "--json"].map(String::from).to_vec();
        assert_eq!(
            ensure_codex_output_flags(&mut command, CodexOutput::Auto),
            OutputPipeline::CodexJson
        );
        assert_eq!(command, ["codex", "exec", "--json", "--", "--json"]);
    }

    /// exec を渡さないラッパー（中でサブコマンドを足すもの）は出力の形が分からないので、
    /// `auto` では何も足さずに従来どおり素通しする。
    #[test]
    fn codex_wrapper_without_exec_stays_raw_in_auto_mode() {
        for command in [
            vec!["/opt/bin/codex-wrapper.sh"],
            vec!["/opt/bin/codex-wrapper.sh", "--sandbox", "read-only"],
            // 最初の位置引数が exec 以外（プロンプトや別のサブコマンド）
            vec!["codex", "review"],
            vec!["codex", "-", "exec"],
        ] {
            let mut agent = codex_agent(&command, CodexOutput::Auto);
            assert_eq!(
                ensure_required_flags(&mut agent),
                OutputPipeline::CodexRaw,
                "{command:?}"
            );
            assert!(
                !agent.command.iter().any(|arg| arg == "--json"),
                "{:?}",
                agent.command
            );
        }
    }

    /// `json` は exec を見つけられなくても JSONL を出すというユーザーの申告に従う（何も足さない）。
    #[test]
    fn codex_json_mode_trusts_the_command_without_exec() {
        let mut agent = codex_agent(&["/opt/bin/codex-json.sh"], CodexOutput::Json);
        assert_eq!(ensure_required_flags(&mut agent), OutputPipeline::CodexJson);
        assert_eq!(
            agent.command,
            ["/opt/bin/codex-json.sh", "-c", "approval_policy=never"]
        );

        // exec を見つけられれば auto と同じく --json を付ける
        let mut agent = codex_agent(&["codex", "exec"], CodexOutput::Json);
        assert_eq!(ensure_required_flags(&mut agent), OutputPipeline::CodexJson);
        assert!(
            agent
                .command
                .ends_with(&["exec".to_string(), "--json".to_string()])
        );
    }

    #[test]
    fn codex_human_mode_never_adds_json() {
        let mut agent = codex_agent(&["codex", "exec"], CodexOutput::Human);
        assert_eq!(ensure_required_flags(&mut agent), OutputPipeline::CodexRaw);
        assert_eq!(
            agent.command,
            ["codex", "-c", "approval_policy=never", "exec"]
        );
    }

    /// provider 未設定でも実行ファイル名が codex なら codex として扱う（従来の推論と同じ）。
    #[test]
    fn codex_is_inferred_from_the_executable_name_without_provider() {
        let mut agent = RuntimeAgent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        assert_eq!(ensure_required_flags(&mut agent), OutputPipeline::CodexJson);
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
    fn codex_flags_follow_executable_inside_env_prefix() {
        for prefix in [
            vec!["env", "-u", "OLD_TOKEN", "TOKEN=value"],
            vec!["/usr/bin/env", "--unset=OLD_TOKEN", "TOKEN=value"],
            vec!["env", "-C", "/tmp", "TOKEN=value"],
            vec!["env", "--", "TOKEN=value"],
        ] {
            let mut command: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
            command.extend(["codex", "exec"].map(String::from));
            ensure_codex_unattended_flags(&mut command);
            assert_eq!(&command[..prefix.len()], &prefix);
            assert_eq!(
                &command[prefix.len()..],
                &["codex", "-c", "approval_policy=never", "exec"]
            );
            let once = command.clone();
            ensure_codex_unattended_flags(&mut command);
            assert_eq!(command, once);
        }
    }

    #[test]
    fn codex_flags_respect_explicit_policy_after_env_delimiter() {
        let mut command = [
            "env",
            "--",
            "TOKEN=value",
            "codex",
            "-a",
            "on-request",
            "exec",
        ]
        .map(String::from)
        .to_vec();
        let original = command.clone();
        ensure_codex_unattended_flags(&mut command);
        assert_eq!(command, original);
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
