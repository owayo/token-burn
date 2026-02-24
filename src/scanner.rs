use anyhow::{Context, Result};
use colored::Colorize;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::config::{Config, Scan};

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub directory: std::path::PathBuf,
    pub display_name: String,
    pub prompt: String,
    pub visibility: Visibility,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Visibility {
    Public,
    Private,
    Unknown,
}

impl std::fmt::Display for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Visibility::Public => write!(f, "PUBLIC"),
            Visibility::Private => write!(f, "PRIVATE"),
            Visibility::Unknown => write!(f, "UNKNOWN"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GhRepo {
    name: String,
    visibility: String,
}

pub async fn resolve_targets(
    config: &Config,
    agent: &crate::config::Agent,
) -> Result<Vec<ResolvedTarget>> {
    // Priority: target prompt > agent prompt > global default
    let effective_default = agent.prompt.as_deref().unwrap_or(&config.prompts.default);
    let default_prompt = config.resolve_prompt(effective_default)?;
    let mut targets = Vec::new();

    for scan in &config.scan {
        let mut scanned = scan_directories(scan).await?;
        for target in &mut scanned {
            if target.prompt.is_empty() {
                target.prompt.clone_from(&default_prompt);
            }
        }
        targets.extend(scanned);
    }

    for target in &config.targets {
        let path = crate::config::resolve_directory(&target.directory)?;
        if !path.exists() {
            eprintln!(
                "{}: {} does not exist, skipping",
                "Warning".yellow(),
                target.directory
            );
            continue;
        }
        if !path.is_dir() {
            eprintln!(
                "{}: {} is not a directory, skipping",
                "Warning".yellow(),
                target.directory
            );
            continue;
        }
        let display_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| target.directory.clone());

        targets.retain(|t| t.directory != path);

        let prompt_value = target.prompt.as_deref().unwrap_or(effective_default);
        let prompt = config.resolve_prompt(prompt_value)?;

        targets.push(ResolvedTarget {
            directory: path,
            display_name,
            prompt,
            visibility: Visibility::Unknown,
        });
    }

    if targets.is_empty() {
        anyhow::bail!("No targets found");
    }

    Ok(targets)
}

async fn scan_directories(scan: &Scan) -> Result<Vec<ResolvedTarget>> {
    let visibility_map = if let Some(username) = &scan.username {
        fetch_visibility_map(username).await.unwrap_or_else(|e| {
            eprintln!(
                "{}: Failed to fetch repo visibility: {}",
                "Warning".yellow(),
                e
            );
            HashMap::new()
        })
    } else {
        HashMap::new()
    };

    let mut results = Vec::new();

    for base_dir in &scan.base_dirs {
        let base_path = crate::config::resolve_directory(base_dir)?;
        if !base_path.exists() {
            eprintln!(
                "{}: {} does not exist, skipping",
                "Warning".yellow(),
                base_dir
            );
            continue;
        }
        find_repos(&base_path, scan, &visibility_map, &mut results)?;
    }

    if scan.public_first {
        results.sort_by(|a, b| a.visibility.cmp(&b.visibility));
    }

    Ok(results)
}

fn find_repos(
    dir: &Path,
    scan: &Scan,
    visibility_map: &HashMap<String, Visibility>,
    results: &mut Vec<ResolvedTarget>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory: {}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if scan.exclude.contains(&dir_name) || dir_name.starts_with('.') {
            continue;
        }

        if path.join(".git").exists() {
            if let Some(target) = check_repo(&path, &dir_name, scan, visibility_map) {
                results.push(target);
            }
        } else if scan.recursive {
            find_repos(&path, scan, visibility_map, results)?;
        }
    }

    Ok(())
}

fn check_repo(
    path: &Path,
    dir_name: &str,
    scan: &Scan,
    visibility_map: &HashMap<String, Visibility>,
) -> Option<ResolvedTarget> {
    let output = std::process::Command::new("git")
        .args(["-C", &path.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let remote_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Some(username) = &scan.username {
        if !remote_belongs_to_username(&remote_url, username) {
            return None;
        }
    }

    let visibility = visibility_map
        .get(dir_name)
        .cloned()
        .unwrap_or(Visibility::Unknown);

    Some(ResolvedTarget {
        directory: path.to_path_buf(),
        display_name: dir_name.to_string(),
        prompt: String::new(),
        visibility,
    })
}

fn remote_belongs_to_username(remote_url: &str, username: &str) -> bool {
    extract_remote_owner(remote_url)
        .map(|owner| owner.eq_ignore_ascii_case(username))
        .unwrap_or(false)
}

fn extract_remote_owner(remote_url: &str) -> Option<String> {
    let trimmed = remote_url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    // SCP-like SSH URL: git@host:owner/repo
    if let Some((host_part, path_part)) = trimmed.split_once(':') {
        if host_part.contains('@') && !host_part.contains("://") {
            let mut parts = path_part.split('/');
            let owner = parts.next()?;
            let repo = parts.next()?;
            if !owner.is_empty() && !repo.is_empty() {
                return Some(owner.to_string());
            }
        }
    }

    // URL with scheme: https://host/owner/repo or ssh://git@host/owner/repo
    let (_, after_scheme) = trimmed.split_once("://")?;
    let (_, path) = after_scheme.split_once('/')?;
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(owner.to_string())
}

async fn fetch_visibility_map(username: &str) -> Result<HashMap<String, Visibility>> {
    let output = tokio::process::Command::new("gh")
        .args([
            "repo",
            "list",
            username,
            "--json",
            "name,visibility",
            "--limit",
            "1000",
        ])
        .output()
        .await
        .context("Failed to run gh CLI (is gh installed and authenticated?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh repo list failed: {}", stderr.trim());
    }

    let repos: Vec<GhRepo> =
        serde_json::from_slice(&output.stdout).context("Failed to parse gh output")?;

    Ok(repos
        .into_iter()
        .map(|r| {
            let vis = match r.visibility.to_uppercase().as_str() {
                "PUBLIC" => Visibility::Public,
                "PRIVATE" => Visibility::Private,
                _ => Visibility::Unknown,
            };
            (r.name, vis)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Agent, Config, Prompts, Settings, Target};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn extract_remote_owner_parses_https_url() {
        let owner = extract_remote_owner("https://github.com/owayo/token-burn.git");
        assert_eq!(owner.as_deref(), Some("owayo"));
    }

    #[test]
    fn extract_remote_owner_parses_scp_style_ssh_url() {
        let owner = extract_remote_owner("git@github.com:owayo/token-burn.git");
        assert_eq!(owner.as_deref(), Some("owayo"));
    }

    #[test]
    fn remote_belongs_to_username_requires_exact_owner_match() {
        let remote = "https://github.com/some-user/token-burn.git";
        assert!(remote_belongs_to_username(remote, "some-user"));
        assert!(remote_belongs_to_username(remote, "Some-User"));
        assert!(!remote_belongs_to_username(remote, "user"));
    }

    #[test]
    fn remote_belongs_to_username_returns_false_for_unparseable_remote() {
        assert!(!remote_belongs_to_username(
            "/home/user/projects/my-repo",
            "some-user",
        ));
    }

    #[tokio::test]
    async fn resolve_targets_skips_file_targets() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("token-burn-scanner-test-{unique}"));
        std::fs::create_dir_all(&temp_dir).expect("test temp dir should be created");

        let repo_dir = temp_dir.join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir should be created");
        let file_target = temp_dir.join("not-a-dir.txt");
        std::fs::write(&file_target, "dummy").expect("file target should be created");

        let config = Config {
            config_dir: temp_dir.clone(),
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
            targets: vec![
                Target {
                    directory: file_target.to_string_lossy().to_string(),
                    prompt: None,
                },
                Target {
                    directory: repo_dir.to_string_lossy().to_string(),
                    prompt: Some("target prompt".to_string()),
                },
            ],
        };

        let resolved = resolve_targets(&config, &config.agents[0])
            .await
            .expect("one valid directory target should remain");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].directory, repo_dir);
        assert_eq!(resolved[0].display_name, "repo");
        assert_eq!(resolved[0].prompt, "target prompt");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
