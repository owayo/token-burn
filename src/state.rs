use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Per-agent map of directory path → last processed timestamp
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(flatten)]
    pub agents: HashMap<String, HashMap<String, DateTime<Utc>>>,
}

impl State {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn last_processed(&self, agent_name: &str, directory: &Path) -> Option<DateTime<Utc>> {
        self.agents
            .get(agent_name)
            .and_then(|m| m.get(&directory.to_string_lossy().to_string()))
            .copied()
    }

    pub fn mark_completed(&mut self, agent_name: &str, directory: &Path) {
        self.agents
            .entry(agent_name.to_string())
            .or_default()
            .insert(directory.to_string_lossy().to_string(), Utc::now());
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

pub fn state_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("state.json")
}

/// Parse a duration string like "7d", "24h", "30m", "1d12h"
pub fn parse_duration(s: &str) -> Result<chrono::Duration> {
    let mut total_secs: i64 = 0;
    let mut num_buf = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            num_buf.push(c);
        } else {
            let n: i64 = num_buf
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid duration number: {}", num_buf))?;
            num_buf.clear();
            match c {
                'd' => total_secs += n * 86400,
                'h' => total_secs += n * 3600,
                'm' => total_secs += n * 60,
                's' => total_secs += n,
                _ => anyhow::bail!("Invalid duration unit '{}' in: {}", c, s),
            }
        }
    }

    if !num_buf.is_empty() {
        anyhow::bail!("Duration must end with a unit (d/h/m/s): {}", s);
    }
    if total_secs == 0 {
        anyhow::bail!("Duration must be positive: {}", s);
    }

    Ok(chrono::Duration::seconds(total_secs))
}

#[cfg(test)]
mod tests {
    use super::parse_duration;

    #[test]
    fn parse_duration_supports_compound_values() {
        let d = parse_duration("1d12h30m15s").expect("duration should parse");
        assert_eq!(d.num_seconds(), 131_415);
    }

    #[test]
    fn parse_duration_rejects_missing_unit() {
        let err = parse_duration("30").expect_err("duration without unit must fail");
        assert!(err.to_string().contains("Duration must end with a unit"));
    }

    #[test]
    fn parse_duration_rejects_invalid_unit() {
        let err = parse_duration("5w").expect_err("unsupported unit must fail");
        assert!(err.to_string().contains("Invalid duration unit"));
    }

    #[test]
    fn parse_duration_rejects_zero_duration() {
        let err = parse_duration("0s").expect_err("zero duration must fail");
        assert!(err.to_string().contains("Duration must be positive"));
    }
}
