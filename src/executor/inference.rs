//! 表示の初期値は明示指定だけから取り、CLI 内部の既定値はセッション記録に任せる。
use std::path::PathBuf;

use crate::config::{RuntimeAgent, agent_runs_cli};
use crate::monitor::inference::{Configuration, Provider, Settings};

pub(super) fn configuration(agent: &RuntimeAgent) -> Configuration {
    let provider = if agent_runs_cli(agent.provider.as_deref(), &agent.command, "claude") {
        Some(Provider::Claude)
    } else if agent_runs_cli(agent.provider.as_deref(), &agent.command, "codex") {
        Some(Provider::Codex)
    } else {
        None
    };
    let mut config = Configuration {
        provider,
        started_at: Some(chrono::Utc::now()),
        ..Default::default()
    };
    let Some(provider) = provider else {
        return config;
    };
    config.settings = explicit_settings(&agent.command, provider);
    let (variable, prefix) = match provider {
        Provider::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
        Provider::Codex => ("CODEX_HOME", ".codex"),
    };
    if let Some(root) = agent
        .env
        .get(variable)
        .cloned()
        .or_else(|| std::env::var(variable).ok())
        .filter(|value| !value.is_empty())
    {
        config.session_roots.push(PathBuf::from(root));
    }
    if let Some(home) = dirs::home_dir() {
        config.session_roots.push(home.join(prefix));
        // ラッパーが切り替えた設定も、実行中の session ID と完全一致する記録だけを読む。
        if let Ok(entries) = std::fs::read_dir(&home) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&format!("{prefix}-"))
                    || name.starts_with(&format!("{prefix}_"))
                {
                    config.session_roots.push(entry.path());
                }
            }
        }
    }
    config.session_roots.sort();
    config.session_roots.dedup();
    config
}

fn explicit_settings(command: &[String], provider: Provider) -> Settings {
    let mut settings = Settings::default();
    let Some(start) = super::flags::env_command_index(command) else {
        return settings;
    };
    let mut args = command.iter().skip(start + 1);
    let mut model_flag = None;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let (option, attached) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
        if option == "--model" || (provider == Provider::Codex && option == "-m") {
            model_flag = attached
                .or_else(|| args.next().map(String::as_str))
                .map(str::to_owned);
        } else if provider == Provider::Claude && option == "--effort" {
            settings.effort = attached
                .or_else(|| args.next().map(String::as_str))
                .map(str::to_owned);
        } else if provider == Provider::Codex && matches!(option, "-c" | "--config") {
            if let Some(value) = attached.or_else(|| args.next().map(String::as_str)) {
                config_value(value, &mut settings);
            }
        } else if provider == Provider::Codex && arg.starts_with("-c") && arg.len() > 2 {
            config_value(&arg[2..], &mut settings);
        } else if provider == Provider::Codex && arg.starts_with("-m") && arg.len() > 2 {
            model_flag = Some(arg[2..].trim_start_matches('=').into());
        } else if takes_value(option, provider) && attached.is_none() {
            args.next();
        } else if !arg.starts_with('-')
            && !matches!(arg.as_str(), "exec" | "e" | "resume" | "fork" | "review")
        {
            break;
        }
    }
    settings.model = model_flag.or(settings.model);
    settings
}

fn config_value(value: &str, settings: &mut Settings) {
    let Some((key, raw)) = value.trim_start_matches('=').split_once('=') else {
        return;
    };
    let slot = match key.trim() {
        "model" => &mut settings.model,
        "model_reasoning_effort" => &mut settings.effort,
        _ => return,
    };
    // Codex の -c と同じく、TOML 文字列なら引用符を外し、それ以外は生の指定を使う。
    let parsed = toml::from_str::<toml::Value>(&format!("value = {}", raw.trim())).ok();
    *slot = Some(
        parsed
            .as_ref()
            .and_then(|v| v.get("value")?.as_str())
            .unwrap_or(raw.trim())
            .into(),
    );
}

fn takes_value(option: &str, provider: Provider) -> bool {
    match provider {
        Provider::Codex => matches!(
            option,
            "-p" | "--profile"
                | "-s"
                | "--sandbox"
                | "-a"
                | "--ask-for-approval"
                | "-C"
                | "--cd"
                | "-i"
                | "--image"
                | "--enable"
                | "--disable"
                | "--local-provider"
                | "--add-dir"
                | "--remote"
                | "--remote-auth-token-env"
                | "--output-schema"
                | "--color"
                | "-o"
                | "--output-last-message"
                | "--thread-source"
                | "--cyber-access-program"
        ),
        Provider::Claude => matches!(
            option,
            "--agent"
                | "--agents"
                | "--append-system-prompt"
                | "--append-system-prompt-file"
                | "--system-prompt"
                | "--system-prompt-file"
                | "--settings"
                | "--setting-sources"
                | "--mcp-config"
                | "--permission-mode"
                | "--output-format"
                | "--input-format"
                | "--fallback-model"
                | "--max-turns"
                | "--max-budget-usd"
                | "--session-id"
                | "--resume"
                | "-r"
                | "--allowedTools"
                | "--allowed-tools"
                | "--disallowedTools"
                | "--disallowed-tools"
                | "--tools"
                | "--plugin-dir"
                | "--add-dir"
                | "--debug-file"
                | "--json-schema"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(args: &[&str], provider: Provider) -> Settings {
        explicit_settings(
            &args.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
            provider,
        )
    }

    #[test]
    fn reads_explicit_claude_and_codex_settings_without_scanning_option_values() {
        let claude = settings(
            &[
                "claude",
                "-c",
                "--system-prompt",
                "--model=not-a-model",
                "--model=opus[1m]",
                "--effort",
                "max",
            ],
            Provider::Claude,
        );
        assert_eq!(claude.model.as_deref(), Some("opus[1m]"));
        assert_eq!(claude.effort.as_deref(), Some("max"));
        let codex = settings(
            &[
                "env",
                "KEY=private",
                "codex",
                "exec",
                "-c",
                "model='example-model'",
                "-cmodel_reasoning_effort=\"xhigh\"",
            ],
            Provider::Codex,
        );
        assert_eq!(codex.model.as_deref(), Some("example-model"));
        assert_eq!(codex.effort.as_deref(), Some("xhigh"));
        assert_eq!(
            settings(&["codex", "exec", "--", "--model=prompt"], Provider::Codex),
            Settings::default()
        );
        assert_eq!(
            settings(&["env", "-S", "codex exec -m hidden"], Provider::Codex),
            Settings::default()
        );
        let codex = settings(
            &[
                "codex",
                "-mexplicit",
                "exec",
                "-c",
                "model='config-model'",
                "--config=model_reasoning_effort='high'",
            ],
            Provider::Codex,
        );
        assert_eq!(codex.model.as_deref(), Some("explicit"));
        assert_eq!(codex.effort.as_deref(), Some("high"));
    }
}
