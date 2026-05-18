use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

/// `claude` interactive モードで使用してはならないフラグ。
/// これらが指定された場合、出力が `--print` 経路になりプラン枠ではなく Agent SDK クレジット側を消費する。
const CLAUDE_PRINT_ONLY_FLAGS: &[&str] = &[
    "-p",
    "--print",
    "--output-format",
    "--input-format",
    "--include-partial-messages",
    "--max-budget-usd",
    "--no-session-persistence",
    "--include-hook-events",
    "--json-schema",
];

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub config_dir: PathBuf,
    pub settings: Settings,
    pub prompts: Prompts,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub scan: Vec<Scan>,
    #[serde(default)]
    pub targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub parallelism: usize,
    /// この期間以内に処理済みのディレクトリをスキップ（例: "7d", "24h", "1d12h"）。
    /// 省略時は前回リセット以降に処理済みのディレクトリをスキップ。
    pub skip_within: Option<String>,
    /// 実行ログの保存先ディレクトリ（デフォルト: ~/Documents/token-burn）
    pub report_dir: Option<String>,
    /// この期間より古いレポートディレクトリを自動削除（デフォルト: "7d"）。
    pub cleanup_after: Option<String>,
    /// 1回の実行で処理する最大ターゲット数（デフォルト: 10）。
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// レート制限使用率がこの閾値（%）を超えたら自動停止する（デフォルト: 95）。
    #[serde(default = "default_rate_limit_threshold")]
    pub rate_limit_threshold: u8,
}

fn default_limit() -> usize {
    10
}

fn default_rate_limit_threshold() -> u8 {
    95
}

#[derive(Debug, Deserialize)]
pub struct Prompts {
    pub default: String,
}

/// エージェントの起動・分類モード。
/// 2026-06-15 以降、Anthropic は `claude -p` を Agent SDK 専用クレジットに分離し、
/// 通常プラン枠は interactive Claude Code 経路でのみ消費可能になる。
/// token-burn は「プラン枠を使い切る」のが目的なので `claude-interactive` 経路をデフォルトとし、
/// 既存の `claude -p` 経路は `claude-print` として保持する（段階的移行のため）。
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgentMode {
    /// command の内容から自動判定する: `claude` 実行ファイルかつ `-p` / `--print` を含まなければ
    /// `ClaudeInteractive`、含めば `ClaudePrint`、それ以外は `Generic` として扱う。
    #[default]
    Auto,
    /// claude / codex / その他、特別な扱いをしない汎用エージェント。
    Generic,
    /// 既存の `claude -p` (stream-json) 経路。2026-06-15 以降は Agent SDK クレジット消費になるため明示 opt-in。
    ClaudePrint,
    /// `claude "prompt"` の対話モード起動経路。tmux 実 TTY + Stop/StopFailure hooks で分類。
    ClaudeInteractive,
}

/// `claude --settings` に渡すための settings JSON ソース。
/// 複数指定可能で、定義順に解決後 deep merge され、token-burn の Stop / StopFailure hooks を
/// prepend した最終 JSON が 1 つだけ `--settings` で claude に渡される。
///
/// **TOML 例:**
/// ```toml
/// claude_settings = [
///     { file = "~/.config/claude/plugin-settings.json" },
///     { command = ["bash", "-lc", "~/bin/claude-plugin-settings.sh"] },
///     { inline = { enabledPlugins = { "plugin@example" = true } } },
/// ]
/// ```
///
/// `file`: パス（`~` 展開対応）。中身は valid な JSON object でなければならない。
/// `command`: shell コマンド（実行ファイル + args 配列、shell が必要なら `["bash", "-lc", "..."]`）。
///   stdout が JSON object である必要がある。動的判定（cwd 依存等）はこの経路で実現する。
/// `inline`: TOML 上で直接書く JSON object（TOML テーブルとして表現可能なもの）。
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged, deny_unknown_fields)]
pub enum ClaudeSettingsSource {
    File { file: String },
    Command { command: Vec<String> },
    Inline { inline: toml::Value },
}

#[derive(Debug, Deserialize, Clone)]
pub struct Agent {
    pub name: String,
    pub command: Vec<String>,
    /// エージェントの起動・分類モード（省略時は Auto）。
    #[serde(default)]
    pub mode: AgentMode,
    /// `claude --settings` で渡す settings JSON ソース（複数指定可、定義順に deep merge）。
    /// claude-interactive モードで wrapper の `--settings` を廃止して token-burn 側に集約するための仕組み。
    /// claude-print でも user settings として渡せる。generic / Auto(非claude) では空でなければエラー。
    #[serde(default)]
    pub claude_settings: Vec<ClaudeSettingsSource>,
    pub reset_weekday: String,
    pub reset_time: String,
    pub timezone: String,
    /// エージェント固有のプロンプト上書き（[prompts].default より優先）
    pub prompt: Option<String>,
}

impl Agent {
    /// `Auto` モードを実コマンドから具体モードへ解決する。
    /// claude 実行ファイルでなければ `Generic`、`-p` / `--print` / `--output-format` のいずれかを
    /// 含めば `ClaudePrint`、それ以外は `ClaudeInteractive`。
    pub fn resolved_mode(&self) -> AgentMode {
        if self.mode != AgentMode::Auto {
            return self.mode;
        }
        if !is_claude_executable(&self.command) {
            return AgentMode::Generic;
        }
        let has_print_flag = self.command.iter().any(|s| {
            s == "-p"
                || s == "--print"
                || s == "--output-format"
                || s.starts_with("--output-format=")
                || s == "--input-format"
                || s.starts_with("--input-format=")
                || s == "--include-partial-messages"
        });
        if has_print_flag {
            AgentMode::ClaudePrint
        } else {
            AgentMode::ClaudeInteractive
        }
    }
}

/// command の先頭要素が claude 実行ファイル（ラッパースクリプト含む）かを判定する。
/// ファイル名（basename）が "claude" そのもの、または "claude-" / "claude_" で始まる場合に true。
pub fn is_claude_executable(command: &[String]) -> bool {
    let Some(first) = command.first() else {
        return false;
    };
    let basename = Path::new(first.as_str())
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    basename == "claude" || basename.starts_with("claude-") || basename.starts_with("claude_")
}

#[derive(Debug, Deserialize)]
pub struct Scan {
    pub base_dirs: Vec<String>,
    #[serde(default)]
    pub recursive: bool,
    pub username: Option<String>,
    #[serde(default = "default_true")]
    pub public_first: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct Target {
    pub directory: String,
    pub prompt: Option<String>,
    /// true の場合、このターゲットを実行リストの末尾に回す（既存順序は安定ソートで維持）
    #[serde(default)]
    pub defer: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config: {}", path.display()))?;
        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config: {}", path.display()))?;
        config.config_dir = path
            .canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        config.validate()?;
        Ok(config)
    }

    /// プロンプト値を解決: `.md` で終わる場合はファイル内容を読み込み、それ以外はそのまま使用。
    /// 相対パスは設定ファイルのディレクトリから解決される。
    pub fn resolve_prompt(&self, value: &str) -> Result<String> {
        if value.ends_with(".md") {
            let expanded = shellexpand::tilde(value);
            let path = PathBuf::from(expanded.as_ref());
            let path = if path.is_absolute() {
                path
            } else {
                self.config_dir.join(path)
            };
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read prompt file: {}", path.display()))?;
            Ok(content.trim().to_string())
        } else {
            Ok(value.to_string())
        }
    }

    fn validate(&self) -> Result<()> {
        if self.agents.is_empty() {
            anyhow::bail!("At least one agent must be configured");
        }
        if self.scan.is_empty() && self.targets.is_empty() {
            anyhow::bail!("Either [[scan]] or [[targets]] must be configured");
        }
        if self.settings.parallelism == 0 {
            anyhow::bail!("parallelism must be at least 1");
        }
        if self.settings.limit == 0 {
            anyhow::bail!("limit must be at least 1");
        }
        if self.settings.rate_limit_threshold == 0 || self.settings.rate_limit_threshold > 100 {
            anyhow::bail!("rate_limit_threshold must be between 1 and 100");
        }
        validate_optional_duration("skip_within", self.settings.skip_within.as_deref())?;
        validate_optional_duration("cleanup_after", self.settings.cleanup_after.as_deref())?;
        for agent in &self.agents {
            if agent.name.trim().is_empty() {
                anyhow::bail!("Agent name must not be empty");
            }
            if agent.command.is_empty() {
                anyhow::bail!(
                    "Agent '{}' command must include at least one element",
                    agent.name
                );
            }
            if agent.command[0].trim().is_empty() {
                anyhow::bail!("Agent '{}' executable must not be empty", agent.name);
            }
            parse_weekday(&agent.reset_weekday)?;
            parse_time(&agent.reset_time)?;
            agent
                .timezone
                .parse::<chrono_tz::Tz>()
                .map_err(|_| anyhow::anyhow!("Invalid timezone: {}", agent.timezone))?;
            validate_agent_mode(agent)?;
        }
        Ok(())
    }
}

fn validate_agent_mode(agent: &Agent) -> Result<()> {
    let resolved = agent.resolved_mode();
    match agent.mode {
        AgentMode::ClaudeInteractive => {
            if !is_claude_executable(&agent.command) {
                anyhow::bail!(
                    "Agent '{}': mode='claude-interactive' は claude 実行ファイルでのみ使用できます",
                    agent.name
                );
            }
            for flag in agent.command.iter() {
                let is_print_flag = CLAUDE_PRINT_ONLY_FLAGS.iter().any(|deny| {
                    flag == deny
                        || (deny.starts_with("--") && flag.starts_with(&format!("{deny}=")))
                });
                if is_print_flag {
                    anyhow::bail!(
                        "Agent '{}': mode='claude-interactive' では --print 系フラグ '{}' を含められません。\n  2026-06-15 以降、これらを含むと Agent SDK クレジット経路になり token-burn の目的を達成できません。",
                        agent.name,
                        flag
                    );
                }
            }
        }
        AgentMode::ClaudePrint => {
            if !is_claude_executable(&agent.command) {
                anyhow::bail!(
                    "Agent '{}': mode='claude-print' は claude 実行ファイルでのみ使用できます",
                    agent.name
                );
            }
        }
        AgentMode::Auto | AgentMode::Generic => {}
    }

    // command 内に `--settings` / `--settings=...` を直接書くことは、token-burn が `--settings` を
    // 自前で渡す方針（claude_settings に集約）と衝突するため拒否する。
    // claude エージェント（claude-print / claude-interactive）のみ拒否対象。generic は通過。
    let claude_modes = matches!(
        resolved,
        AgentMode::ClaudeInteractive | AgentMode::ClaudePrint
    );
    if claude_modes {
        let has_explicit_settings = agent
            .command
            .iter()
            .any(|s| s == "--settings" || s.starts_with("--settings="));
        if has_explicit_settings {
            anyhow::bail!(
                "Agent '{}': command に `--settings` を直接書けません。代わりに [[agents]].claude_settings を使用してください。\n  token-burn は --settings を必ず 1 個だけ渡す方針（user settings と token-burn hooks を deep merge）です。",
                agent.name
            );
        }
    }

    // claude_settings は claude 経路でのみ使用可能。generic / 非 claude では空でなければエラー。
    if !agent.claude_settings.is_empty() && !claude_modes {
        anyhow::bail!(
            "Agent '{}': claude_settings は claude エージェント (mode = 'claude-print' / 'claude-interactive' / Auto with claude 実行ファイル) でのみ指定できます",
            agent.name
        );
    }

    Ok(())
}

fn validate_optional_duration(field_name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        crate::state::parse_duration(value)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Invalid {field_name}: {e}"))?;
    }
    Ok(())
}

pub fn resolve_directory(dir: &str) -> Result<PathBuf> {
    let expanded = shellexpand::tilde(dir);
    let path = PathBuf::from(expanded.as_ref());
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize_path(&absolute))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

pub fn default_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".config")
        .join("token-burn")
        .join("config.toml")
}

pub fn parse_weekday(s: &str) -> Result<chrono::Weekday> {
    match s.to_lowercase().as_str() {
        "monday" | "mon" => Ok(chrono::Weekday::Mon),
        "tuesday" | "tue" => Ok(chrono::Weekday::Tue),
        "wednesday" | "wed" => Ok(chrono::Weekday::Wed),
        "thursday" | "thu" => Ok(chrono::Weekday::Thu),
        "friday" | "fri" => Ok(chrono::Weekday::Fri),
        "saturday" | "sat" => Ok(chrono::Weekday::Sat),
        "sunday" | "sun" => Ok(chrono::Weekday::Sun),
        _ => anyhow::bail!("Invalid weekday: {}", s),
    }
}

pub fn parse_time(s: &str) -> Result<(u32, u32)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid time format: {} (expected HH:MM)", s);
    }
    let hour: u32 = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid hour: {}", parts[0]))?;
    let minute: u32 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid minute: {}", parts[1]))?;
    if hour > 23 || minute > 59 {
        anyhow::bail!("Invalid time: {}", s);
    }
    Ok((hour, minute))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn agent_with_mode(name: &str, command: Vec<&str>, mode: AgentMode) -> Agent {
        Agent {
            name: name.to_string(),
            command: command.into_iter().map(String::from).collect(),
            mode,
            claude_settings: Vec::new(),
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        }
    }

    #[test]
    fn resolved_mode_auto_claude_with_p_is_print() {
        let a = agent_with_mode("claude", vec!["claude", "-p"], AgentMode::Auto);
        assert_eq!(a.resolved_mode(), AgentMode::ClaudePrint);
    }

    #[test]
    fn resolved_mode_auto_claude_without_p_is_interactive() {
        let a = agent_with_mode("claude", vec!["claude"], AgentMode::Auto);
        assert_eq!(a.resolved_mode(), AgentMode::ClaudeInteractive);
    }

    #[test]
    fn resolved_mode_auto_claude_with_output_format_is_print() {
        let a = agent_with_mode(
            "claude",
            vec!["claude", "--output-format", "stream-json"],
            AgentMode::Auto,
        );
        assert_eq!(a.resolved_mode(), AgentMode::ClaudePrint);
    }

    #[test]
    fn resolved_mode_auto_codex_is_generic() {
        let a = agent_with_mode("codex", vec!["codex", "exec"], AgentMode::Auto);
        assert_eq!(a.resolved_mode(), AgentMode::Generic);
    }

    #[test]
    fn resolved_mode_explicit_overrides_auto_detection() {
        // 明示指定された mode は command 内容に関わらず尊重される
        let a = agent_with_mode("claude", vec!["claude"], AgentMode::ClaudePrint);
        assert_eq!(a.resolved_mode(), AgentMode::ClaudePrint);
    }

    #[test]
    fn validate_agent_mode_rejects_claude_interactive_with_p() {
        let mut config = base_config();
        config.agents[0] =
            agent_with_mode("claude", vec!["claude", "-p"], AgentMode::ClaudeInteractive);
        let err = config
            .validate()
            .expect_err("claude-interactive + -p は拒否されるべき");
        assert!(err.to_string().contains("--print 系フラグ"));
    }

    #[test]
    fn validate_agent_mode_rejects_claude_interactive_with_output_format() {
        let mut config = base_config();
        config.agents[0] = agent_with_mode(
            "claude",
            vec!["claude", "--output-format", "stream-json"],
            AgentMode::ClaudeInteractive,
        );
        let err = config
            .validate()
            .expect_err("claude-interactive + --output-format は拒否されるべき");
        assert!(err.to_string().contains("--print 系フラグ"));
    }

    #[test]
    fn validate_agent_mode_rejects_claude_interactive_with_equals_output_format() {
        let mut config = base_config();
        config.agents[0] = agent_with_mode(
            "claude",
            vec!["claude", "--output-format=stream-json"],
            AgentMode::ClaudeInteractive,
        );
        let err = config
            .validate()
            .expect_err("equals 形式の --output-format も拒否されるべき");
        assert!(err.to_string().contains("--print 系フラグ"));
    }

    #[test]
    fn validate_agent_mode_rejects_claude_interactive_on_non_claude_executable() {
        let mut config = base_config();
        config.agents[0] = agent_with_mode("custom", vec!["codex"], AgentMode::ClaudeInteractive);
        let err = config
            .validate()
            .expect_err("非 claude 実行ファイル + claude-interactive は拒否されるべき");
        assert!(err.to_string().contains("claude 実行ファイル"));
    }

    #[test]
    fn validate_agent_mode_allows_claude_interactive_with_clean_command() {
        let mut config = base_config();
        config.agents[0] = agent_with_mode(
            "claude",
            vec!["claude", "--model", "opus"],
            AgentMode::ClaudeInteractive,
        );
        config
            .validate()
            .expect("--model など print-only でない flag は許可されるべき");
    }

    #[test]
    fn validate_agent_mode_rejects_claude_print_on_non_claude_executable() {
        let mut config = base_config();
        config.agents[0] = agent_with_mode("foo", vec!["codex"], AgentMode::ClaudePrint);
        let err = config
            .validate()
            .expect_err("非 claude + claude-print は拒否されるべき");
        assert!(err.to_string().contains("claude 実行ファイル"));
    }

    #[test]
    fn validate_rejects_command_with_explicit_settings_flag() {
        // command に `--settings` を直接書くと token-burn の集約方針と衝突するため拒否
        let mut config = base_config();
        config.agents[0] = agent_with_mode(
            "claude",
            vec!["claude", "--settings", "/etc/foo.json"],
            AgentMode::ClaudeInteractive,
        );
        let err = config
            .validate()
            .expect_err("command 内 --settings は拒否されるべき");
        assert!(err.to_string().contains("`--settings` を直接書けません"));
        assert!(err.to_string().contains("claude_settings"));
    }

    #[test]
    fn validate_rejects_command_with_equals_style_settings_flag() {
        let mut config = base_config();
        config.agents[0] = agent_with_mode(
            "claude",
            vec!["claude", "--settings=/etc/foo.json"],
            AgentMode::ClaudeInteractive,
        );
        let err = config
            .validate()
            .expect_err("equals 形式の --settings も拒否されるべき");
        assert!(err.to_string().contains("`--settings` を直接書けません"));
    }

    #[test]
    fn validate_rejects_command_with_settings_flag_in_claude_print() {
        // claude-print でも同じ理由で拒否（user settings は claude_settings 経由で）
        let mut config = base_config();
        config.agents[0] = agent_with_mode(
            "claude",
            vec!["claude", "-p", "--settings", "/etc/foo.json"],
            AgentMode::ClaudePrint,
        );
        let err = config
            .validate()
            .expect_err("claude-print でも --settings 直書きは拒否されるべき");
        assert!(err.to_string().contains("`--settings` を直接書けません"));
    }

    #[test]
    fn validate_allows_command_with_settings_in_generic_mode() {
        // generic では --settings 直書きを許す（汎用エージェントは token-burn の hook 制約とは無関係）
        let mut config = base_config();
        config.agents[0] = agent_with_mode(
            "codex",
            vec!["codex", "--settings", "/etc/foo.json"],
            AgentMode::Generic,
        );
        config
            .validate()
            .expect("generic では command 内 --settings を許可するべき");
    }

    #[test]
    fn validate_rejects_claude_settings_in_generic_mode() {
        let mut config = base_config();
        let mut agent = agent_with_mode("codex", vec!["codex"], AgentMode::Generic);
        agent.claude_settings = vec![ClaudeSettingsSource::File {
            file: "/etc/foo.json".to_string(),
        }];
        config.agents[0] = agent;
        let err = config
            .validate()
            .expect_err("generic で claude_settings を使うのは拒否されるべき");
        assert!(err.to_string().contains("claude_settings は claude"));
    }

    #[test]
    fn validate_allows_claude_settings_in_claude_interactive() {
        let mut config = base_config();
        let mut agent = agent_with_mode("claude", vec!["claude"], AgentMode::ClaudeInteractive);
        agent.claude_settings = vec![ClaudeSettingsSource::File {
            file: "/etc/foo.json".to_string(),
        }];
        config.agents[0] = agent;
        config
            .validate()
            .expect("claude-interactive で claude_settings は許可されるべき");
    }

    #[test]
    fn claude_settings_source_deserialize_file_variant() {
        let toml_str = r#"claude_settings = [{ file = "~/foo.json" }]"#;
        #[derive(serde::Deserialize)]
        struct Wrap {
            claude_settings: Vec<ClaudeSettingsSource>,
        }
        let w: Wrap = toml::from_str(toml_str).expect("file variant should deserialize");
        assert_eq!(w.claude_settings.len(), 1);
        match &w.claude_settings[0] {
            ClaudeSettingsSource::File { file } => assert_eq!(file, "~/foo.json"),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn claude_settings_source_deserialize_command_variant() {
        let toml_str = r#"claude_settings = [{ command = ["bash", "-lc", "echo {}"] }]"#;
        #[derive(serde::Deserialize)]
        struct Wrap {
            claude_settings: Vec<ClaudeSettingsSource>,
        }
        let w: Wrap = toml::from_str(toml_str).expect("command variant should deserialize");
        match &w.claude_settings[0] {
            ClaudeSettingsSource::Command { command } => {
                assert_eq!(command, &vec!["bash", "-lc", "echo {}"]);
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn claude_settings_source_deserialize_inline_variant() {
        let toml_str = r#"
[[claude_settings]]
inline = { enabledPlugins = { "p1" = true } }
"#;
        #[derive(serde::Deserialize)]
        struct Wrap {
            claude_settings: Vec<ClaudeSettingsSource>,
        }
        let w: Wrap = toml::from_str(toml_str).expect("inline variant should deserialize");
        match &w.claude_settings[0] {
            ClaudeSettingsSource::Inline { inline } => {
                let t = inline.as_table().expect("inline should be a TOML table");
                let ep = t.get("enabledPlugins").and_then(|v| v.as_table()).unwrap();
                assert_eq!(ep.get("p1").and_then(|v| v.as_bool()), Some(true));
            }
            other => panic!("expected Inline, got {other:?}"),
        }
    }

    #[test]
    fn claude_settings_source_rejects_mixed_keys() {
        // deny_unknown_fields により、file/command/inline 以外のキーは拒否される
        let toml_str = r#"claude_settings = [{ file = "x.json", command = ["a"] }]"#;
        #[derive(serde::Deserialize, Debug)]
        struct Wrap {
            #[allow(dead_code)]
            claude_settings: Vec<ClaudeSettingsSource>,
        }
        assert!(
            toml::from_str::<Wrap>(toml_str).is_err(),
            "file と command を同時指定するのは拒否されるべき"
        );
    }

    #[test]
    fn agent_mode_default_is_auto() {
        assert_eq!(AgentMode::default(), AgentMode::Auto);
    }

    #[test]
    fn agent_mode_deserializes_kebab_case() {
        let a: AgentMode = toml::from_str("mode = \"claude-interactive\"\n")
            .ok()
            .and_then(|t: toml::Table| {
                let v = t.get("mode")?.clone();
                v.try_into().ok()
            })
            .expect("kebab-case でデシリアライズできるべき");
        assert_eq!(a, AgentMode::ClaudeInteractive);
    }

    fn base_config() -> Config {
        Config {
            config_dir: PathBuf::from("."),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: Prompts {
                default: "review".to_string(),
            },
            agents: vec![Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                mode: AgentMode::default(),
                claude_settings: Vec::new(),
                reset_weekday: "monday".to_string(),
                reset_time: "09:00".to_string(),
                timezone: "UTC".to_string(),
                prompt: None,
            }],
            scan: vec![],
            targets: vec![Target {
                directory: ".".to_string(),
                prompt: None,
                defer: false,
            }],
        }
    }

    #[test]
    fn validate_rejects_empty_agent_command() {
        let mut config = base_config();
        config.agents[0].command = vec![];

        let err = config
            .validate()
            .expect_err("empty command must be rejected");
        assert!(err.to_string().contains("include at least one element"));
    }

    #[test]
    fn validate_rejects_empty_agent_executable() {
        let mut config = base_config();
        config.agents[0].command = vec!["".to_string(), "-p".to_string()];

        let err = config
            .validate()
            .expect_err("empty executable must be rejected");
        assert!(err.to_string().contains("executable must not be empty"));
    }

    #[test]
    fn validate_accepts_non_empty_agent_command() {
        let config = base_config();
        config
            .validate()
            .expect("valid agent command should pass validation");
    }

    #[test]
    fn parse_weekday_accepts_full_and_short_names() {
        assert_eq!(parse_weekday("monday").unwrap(), chrono::Weekday::Mon);
        assert_eq!(parse_weekday("Mon").unwrap(), chrono::Weekday::Mon);
        assert_eq!(parse_weekday("FRIDAY").unwrap(), chrono::Weekday::Fri);
        assert_eq!(parse_weekday("sun").unwrap(), chrono::Weekday::Sun);
    }

    #[test]
    fn parse_weekday_rejects_invalid() {
        assert!(parse_weekday("funday").is_err());
        assert!(parse_weekday("").is_err());
    }

    #[test]
    fn parse_time_valid() {
        assert_eq!(parse_time("09:00").unwrap(), (9, 0));
        assert_eq!(parse_time("23:59").unwrap(), (23, 59));
        assert_eq!(parse_time("00:00").unwrap(), (0, 0));
    }

    #[test]
    fn parse_time_rejects_invalid() {
        assert!(parse_time("24:00").is_err());
        assert!(parse_time("09:60").is_err());
        assert!(parse_time("9").is_err());
        assert!(parse_time("09:00:00").is_err());
    }

    #[test]
    fn resolve_prompt_literal_string() {
        let config = base_config();
        let result = config.resolve_prompt("review code").unwrap();
        assert_eq!(result, "review code");
    }

    #[test]
    fn resolve_prompt_reads_md_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prompt_path = tmp.path().join("test.md");
        std::fs::write(&prompt_path, "  file content  ").unwrap();
        let mut config = base_config();
        config.config_dir = tmp.path().to_path_buf();
        let result = config.resolve_prompt("test.md").unwrap();
        assert_eq!(result, "file content");
    }

    #[test]
    fn resolve_prompt_missing_md_file_returns_error() {
        let config = base_config();
        assert!(config.resolve_prompt("nonexistent.md").is_err());
    }

    #[test]
    fn validate_rejects_empty_agents() {
        let mut config = base_config();
        config.agents = vec![];
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_parallelism() {
        let mut config = base_config();
        config.settings.parallelism = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_rate_limit_threshold() {
        let mut config = base_config();
        config.settings.rate_limit_threshold = 0;
        let err = config.validate().expect_err("threshold=0 は拒否されるべき");
        assert!(
            err.to_string()
                .contains("rate_limit_threshold must be between 1 and 100")
        );
    }

    #[test]
    fn validate_rejects_over_100_rate_limit_threshold() {
        // u8 なので最大 255 だが、101 以上は拒否される
        let mut config = base_config();
        config.settings.rate_limit_threshold = 101;
        let err = config
            .validate()
            .expect_err("threshold=101 は拒否されるべき");
        assert!(
            err.to_string()
                .contains("rate_limit_threshold must be between 1 and 100")
        );
    }

    #[test]
    fn validate_rejects_zero_limit() {
        let mut config = base_config();
        config.settings.limit = 0;
        let err = config.validate().expect_err("limit=0 は拒否されるべき");
        assert!(err.to_string().contains("limit must be at least 1"));
    }

    #[test]
    fn validate_rejects_no_scan_or_targets() {
        let mut config = base_config();
        config.scan = vec![];
        config.targets = vec![];
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_whitespace_only_agent_name() {
        let mut config = base_config();
        config.agents[0].name = "   ".to_string();
        let err = config
            .validate()
            .expect_err("空白のみのエージェント名は拒否されるべき");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_rejects_whitespace_only_executable() {
        let mut config = base_config();
        config.agents[0].command = vec!["  ".to_string()];
        let err = config
            .validate()
            .expect_err("空白のみの実行ファイル名は拒否されるべき");
        assert!(err.to_string().contains("executable must not be empty"));
    }

    #[test]
    fn validate_rejects_invalid_timezone() {
        let mut config = base_config();
        config.agents[0].timezone = "Invalid/Zone".to_string();
        let err = config
            .validate()
            .expect_err("無効なタイムゾーンは拒否されるべき");
        assert!(err.to_string().contains("Invalid timezone"));
    }

    #[test]
    fn validate_rejects_invalid_skip_within() {
        let mut config = base_config();
        config.settings.skip_within = Some("broken".to_string());
        let err = config
            .validate()
            .expect_err("無効な skip_within は拒否されるべき");
        assert!(err.to_string().contains("Invalid skip_within"));
    }

    #[test]
    fn validate_rejects_invalid_cleanup_after() {
        let mut config = base_config();
        config.settings.cleanup_after = Some("broken".to_string());
        let err = config
            .validate()
            .expect_err("無効な cleanup_after は拒否されるべき");
        assert!(err.to_string().contains("Invalid cleanup_after"));
    }

    #[test]
    fn parse_time_rejects_whitespace_padded() {
        assert!(parse_time(" 09:00").is_err());
        assert!(parse_time("09:00 ").is_err());
    }

    #[test]
    fn resolve_prompt_empty_string_returns_empty() {
        let config = base_config();
        let result = config.resolve_prompt("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn resolve_directory_absolute_path_unchanged() {
        let path = resolve_directory("/tmp/some-repo").expect("絶対パスは解決されるべき");
        assert_eq!(path, std::path::PathBuf::from("/tmp/some-repo"));
        assert!(path.is_absolute());
    }

    #[test]
    fn resolve_directory_tilde_expansion() {
        let path = resolve_directory("~/test-dir").expect("チルダは展開されるべき");
        assert!(path.is_absolute());
        assert!(!path.to_string_lossy().contains('~'));
        assert!(path.to_string_lossy().ends_with("test-dir"));
    }

    #[test]
    fn normalize_path_handles_parent_dir() {
        let path = normalize_path(Path::new("/a/b/../c"));
        assert_eq!(path, PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_path_handles_current_dir() {
        let path = normalize_path(Path::new("/a/./b/./c"));
        assert_eq!(path, PathBuf::from("/a/b/c"));
    }

    #[test]
    fn normalize_path_parent_at_root() {
        // ルートを超える .. はルートに留まる
        let path = normalize_path(Path::new("/a/../.."));
        assert_eq!(path, PathBuf::from("/"));
    }

    #[test]
    fn default_config_path_is_absolute() {
        let path = default_config_path();
        // ホームディレクトリが取得できない場合は "~" になるが、
        // 通常は絶対パスになる
        assert!(path.to_string_lossy().contains("config.toml"));
        assert!(path.to_string_lossy().contains("token-burn"));
    }

    #[test]
    fn parse_time_rejects_negative_values() {
        // 負の値は数値パースで弾かれる
        assert!(parse_time("-1:00").is_err());
        assert!(parse_time("09:-5").is_err());
    }

    #[test]
    fn parse_time_rejects_non_numeric() {
        assert!(parse_time("ab:cd").is_err());
        assert!(parse_time("9.5:00").is_err());
    }

    #[test]
    fn parse_weekday_case_insensitive_mixed() {
        // 大文字小文字混在でも受け付ける
        assert_eq!(parse_weekday("MoNdAy").unwrap(), chrono::Weekday::Mon);
        assert_eq!(parse_weekday("TUESDAY").unwrap(), chrono::Weekday::Tue);
    }

    #[test]
    fn parse_weekday_rejects_partial_names() {
        // 2文字の短縮形は受け付けない
        assert!(parse_weekday("mo").is_err());
        assert!(parse_weekday("fr").is_err());
    }

    #[test]
    fn validate_allows_duplicate_agent_names() {
        // 同名エージェントは許容されている（重複禁止ルールなし）
        let mut config = base_config();
        config.agents.push(config.agents[0].clone());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let result = Config::load(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn load_invalid_toml_returns_error() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "not valid toml {{{{").unwrap();
        let result = Config::load(&config_path);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_prompt_absolute_md_path() {
        let tmp = TempDir::new().unwrap();
        let prompt_path = tmp.path().join("absolute.md");
        std::fs::write(&prompt_path, "absolute content").unwrap();

        let config = base_config();
        let result = config
            .resolve_prompt(&prompt_path.to_string_lossy())
            .unwrap();
        assert_eq!(result, "absolute content");
    }

    #[test]
    fn resolve_directory_normalizes_relative_segments() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let _cwd_guard = crate::test_support::CwdGuard::switch_to(tmp.path());

        let expected = std::env::current_dir()
            .expect("cwd should be available")
            .join("repo");
        let path = resolve_directory("./nested/../repo").expect("relative path should resolve");

        assert_eq!(path, expected);
        assert!(path.is_absolute());
    }

    #[test]
    // normalize_path に空パス（""）を渡した場合は空の PathBuf を返す
    fn normalize_path_empty_string_returns_empty() {
        let path = normalize_path(Path::new(""));
        assert_eq!(path, PathBuf::new());
    }

    #[test]
    // parse_time の境界値 "00:59" は有効な時刻として受け付ける
    fn parse_time_boundary_00_59_is_valid() {
        let result = parse_time("00:59").unwrap();
        assert_eq!(result, (0, 59));
    }

    #[test]
    // scan と targets が両方存在する場合は validate が成功する（正常系の網羅）
    fn validate_accepts_both_scan_and_targets() {
        let mut config = base_config();
        config.scan = vec![Scan {
            base_dirs: vec![".".to_string()],
            recursive: false,
            username: None,
            public_first: true,
            exclude: vec![],
        }];
        config.targets = vec![Target {
            directory: ".".to_string(),
            prompt: None,
            defer: false,
        }];
        config
            .validate()
            .expect("scan と targets が両方ある場合は成功するべき");
    }

    #[test]
    fn validate_rejects_invalid_reset_weekday() {
        let mut config = base_config();
        config.agents[0].reset_weekday = "notaday".to_string();
        let err = config.validate().expect_err("無効な曜日は拒否されるべき");
        assert!(err.to_string().contains("Invalid weekday"));
    }

    #[test]
    fn validate_rejects_invalid_reset_time() {
        let mut config = base_config();
        config.agents[0].reset_time = "25:00".to_string();
        let err = config.validate().expect_err("無効な時刻は拒否されるべき");
        assert!(err.to_string().contains("Invalid time"));
    }

    #[test]
    fn resolve_prompt_trims_whitespace_from_md_file() {
        let tmp = TempDir::new().unwrap();
        let prompt_path = tmp.path().join("whitespace.md");
        std::fs::write(&prompt_path, "\n  content with whitespace  \n\n").unwrap();
        let mut config = base_config();
        config.config_dir = tmp.path().to_path_buf();
        let result = config.resolve_prompt("whitespace.md").unwrap();
        assert_eq!(result, "content with whitespace");
    }

    #[test]
    fn normalize_path_multiple_parent_dirs() {
        let path = normalize_path(Path::new("/a/b/c/../../d"));
        assert_eq!(path, PathBuf::from("/a/d"));
    }

    #[test]
    fn normalize_path_only_root() {
        let path = normalize_path(Path::new("/"));
        assert_eq!(path, PathBuf::from("/"));
    }

    #[test]
    // resolve_prompt で .md 以外の拡張子（.txt）はファイル読み込みではなくリテラルとして扱われる
    fn resolve_prompt_non_md_extension_is_treated_as_literal() {
        let tmp = TempDir::new().unwrap();
        let txt_path = tmp.path().join("prompt.txt");
        std::fs::write(&txt_path, "should not be read").unwrap();

        let mut config = base_config();
        config.config_dir = tmp.path().to_path_buf();

        // .txt ファイルへのパス文字列をそのままリテラルとして返す
        let value = txt_path.to_string_lossy().to_string();
        let result = config.resolve_prompt(&value).unwrap();
        assert_eq!(result, value);
    }
}
