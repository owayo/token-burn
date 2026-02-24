use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

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
    /// Skip directories processed within this duration (e.g., "7d", "24h", "1d12h").
    /// If omitted, defaults to skipping directories processed since the previous reset.
    pub skip_within: Option<String>,
    /// Directory to save execution logs (default: ~/Documents/token-burn)
    pub report_dir: Option<String>,
    /// Auto-delete report directories older than this duration (default: "7d").
    pub cleanup_after: Option<String>,
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
    /// Agent-specific prompt override (takes precedence over [prompts].default)
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

    /// Resolve a prompt value: if it ends with `.md`, read file contents; otherwise use as literal string.
    /// Relative paths are resolved from the config file's directory.
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

pub fn resolve_directory(dir: &str) -> Result<PathBuf> {
    let expanded = shellexpand::tilde(dir);
    Ok(PathBuf::from(expanded.as_ref()))
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

    fn base_config() -> Config {
        Config {
            config_dir: PathBuf::from("."),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
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
}
