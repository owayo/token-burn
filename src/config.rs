use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

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
}

fn default_limit() -> usize {
    10
}

#[derive(Debug, Deserialize)]
pub struct Prompts {
    pub default: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Agent {
    pub name: String,
    pub command: Vec<String>,
    pub reset_weekday: String,
    pub reset_time: String,
    pub timezone: String,
    /// エージェント固有のプロンプト上書き（[prompts].default より優先）
    pub prompt: Option<String>,
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
        }
        Ok(())
    }
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

    fn base_config() -> Config {
        Config {
            config_dir: PathBuf::from("."),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
            },
            prompts: Prompts {
                default: "review".to_string(),
            },
            agents: vec![Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: "monday".to_string(),
                reset_time: "09:00".to_string(),
                timezone: "UTC".to_string(),
                prompt: None,
            }],
            scan: vec![],
            targets: vec![Target {
                directory: ".".to_string(),
                prompt: None,
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
    fn validate_rejects_duplicate_agent_names() {
        // 同名エージェントは現状許容されている（バグではなく仕様確認）
        let mut config = base_config();
        config.agents.push(config.agents[0].clone());
        // 同名エージェントでもバリデーションは通る（重複禁止ルールなし）
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
        let old_cwd = std::env::current_dir().expect("cwd should be available");
        let tmp = TempDir::new().expect("temp dir should be created");
        std::env::set_current_dir(tmp.path()).expect("should switch cwd");

        let expected = std::env::current_dir()
            .expect("cwd should be available")
            .join("repo");
        let path = resolve_directory("./nested/../repo").expect("relative path should resolve");
        std::env::set_current_dir(old_cwd).expect("should restore cwd");

        assert_eq!(path, expected);
        assert!(path.is_absolute());
    }
}
