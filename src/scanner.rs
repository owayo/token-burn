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
    // 優先順位: target prompt > agent prompt > global default
    let effective_default = agent.prompt.as_deref().unwrap_or(&config.prompts.default);
    let default_prompt = config.resolve_prompt(effective_default)?;
    let mut targets: Vec<ResolvedTarget> = Vec::new();

    for scan in &config.scan {
        let mut scanned = scan_directories(scan).await?;
        for mut target in scanned.drain(..) {
            if target.prompt.is_empty() {
                target.prompt.clone_from(&default_prompt);
            }
            if let Some(existing) = targets.iter_mut().find(|t| t.directory == target.directory) {
                // 同一ディレクトリが複数の scan から見つかった場合は重複追加しない。
                // 可視性だけは Unknown より具体的な値を優先して更新する。
                if existing.visibility == Visibility::Unknown
                    && target.visibility != Visibility::Unknown
                {
                    existing.visibility = target.visibility;
                }
                continue;
            }
            targets.push(target);
        }
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

        let prompt_value = target.prompt.as_deref().unwrap_or(effective_default);
        let prompt = config.resolve_prompt(prompt_value)?;

        let new_target = ResolvedTarget {
            directory: path.clone(),
            display_name,
            prompt,
            visibility: Visibility::Unknown,
        };

        if let Some(pos) = targets.iter().position(|t| t.directory == path) {
            // スキャンで得た可視性を維持したまま上書きする
            let existing_visibility = targets[pos].visibility.clone();
            targets[pos] = ResolvedTarget {
                visibility: existing_visibility,
                ..new_target
            };
        } else {
            targets.push(new_target);
        }
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
    let remote_url = std::process::Command::new("git")
        .args(["-C", &path.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());

    if let Some(username) = &scan.username {
        let remote_url = remote_url.as_deref()?;
        if !remote_belongs_to_username(remote_url, username) {
            return None;
        }
    }

    // `username` 未指定の通常スキャンでは `origin` がなくても対象に含める。
    let visibility = if let Some(remote_url) = remote_url.as_deref() {
        let visibility_key = extract_remote_repo(remote_url)
            .unwrap_or_else(|| dir_name.to_string())
            .to_ascii_lowercase();
        visibility_map
            .get(&visibility_key)
            .cloned()
            .unwrap_or(Visibility::Unknown)
    } else {
        Visibility::Unknown
    };

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
    extract_remote_owner_and_repo(remote_url).map(|(owner, _)| owner)
}

fn extract_remote_repo(remote_url: &str) -> Option<String> {
    extract_remote_owner_and_repo(remote_url).map(|(_, repo)| repo)
}

fn extract_remote_owner_and_repo(remote_url: &str) -> Option<(String, String)> {
    let trimmed = remote_url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    if let Some((host_part, path_part)) = trimmed.split_once(':')
        && host_part.contains('@')
        && !host_part.contains("://")
    {
        let mut parts = path_part.split('/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        if !owner.is_empty() && !repo.is_empty() {
            return Some((owner.to_string(), repo.to_string()));
        }
    }

    let (_, after_scheme) = trimmed.split_once("://")?;
    let (_, path) = after_scheme.split_once('/')?;
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
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
            (r.name.to_ascii_lowercase(), vis)
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
    fn extract_remote_repo_parses_remote_url() {
        let repo = extract_remote_repo("https://github.com/owayo/token-burn.git");
        assert_eq!(repo.as_deref(), Some("token-burn"));
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

    #[test]
    fn extract_remote_owner_without_git_suffix() {
        // .git サフィックスなしでも正しくパースされる
        let owner = extract_remote_owner("https://github.com/owayo/token-burn");
        assert_eq!(owner.as_deref(), Some("owayo"));
    }

    #[test]
    fn extract_remote_repo_without_git_suffix() {
        let repo = extract_remote_repo("https://github.com/owayo/token-burn");
        assert_eq!(repo.as_deref(), Some("token-burn"));
    }

    #[test]
    fn extract_remote_owner_with_trailing_slash() {
        let owner = extract_remote_owner("https://github.com/owayo/token-burn/");
        assert_eq!(owner.as_deref(), Some("owayo"));
    }

    #[test]
    fn extract_remote_owner_scp_style_without_git_suffix() {
        let owner = extract_remote_owner("git@github.com:owayo/token-burn");
        assert_eq!(owner.as_deref(), Some("owayo"));
    }

    #[test]
    fn extract_remote_returns_none_for_scp_missing_repo() {
        // owner のみでリポジトリ部分がない場合は None
        assert!(extract_remote_owner_and_repo("git@github.com:owner").is_none());
    }

    #[test]
    fn extract_remote_returns_none_for_empty_owner() {
        assert!(extract_remote_owner_and_repo("https://github.com//repo.git").is_none());
    }

    #[test]
    fn check_repo_uses_remote_repo_name_for_visibility_lookup() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("token-burn-check-repo-test-{unique}"));
        std::fs::create_dir_all(&temp_dir).expect("test temp dir should be created");
        let repo_dir = temp_dir.join("local-dir-name");
        std::fs::create_dir_all(&repo_dir).expect("repo dir should be created");

        let status = std::process::Command::new("git")
            .args(["-C", &repo_dir.to_string_lossy(), "init", "--quiet"])
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init should succeed");

        let status = std::process::Command::new("git")
            .args([
                "-C",
                &repo_dir.to_string_lossy(),
                "remote",
                "add",
                "origin",
                "https://github.com/owayo/Token-Burn.git",
            ])
            .status()
            .expect("git remote add should run");
        assert!(status.success(), "git remote add should succeed");

        let scan = Scan {
            base_dirs: vec![],
            recursive: false,
            username: Some("owayo".to_string()),
            public_first: true,
            exclude: vec![],
        };
        let visibility_map = HashMap::from([(String::from("token-burn"), Visibility::Public)]);

        let target = check_repo(&repo_dir, "local-dir-name", &scan, &visibility_map)
            .expect("repository should be detected");
        assert_eq!(target.visibility, Visibility::Public);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn check_repo_without_origin_is_included_when_username_is_not_set() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir =
            std::env::temp_dir().join(format!("token-burn-check-no-origin-test-{unique}"));
        std::fs::create_dir_all(&temp_dir).expect("test temp dir should be created");
        let repo_dir = temp_dir.join("local-only-repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir should be created");

        let status = std::process::Command::new("git")
            .args(["-C", &repo_dir.to_string_lossy(), "init", "--quiet"])
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init should succeed");

        let scan = Scan {
            base_dirs: vec![],
            recursive: false,
            username: None,
            public_first: true,
            exclude: vec![],
        };

        let target = check_repo(&repo_dir, "local-only-repo", &scan, &HashMap::new())
            .expect("repository without origin should still be detected");
        assert_eq!(target.directory, repo_dir);
        assert_eq!(target.display_name, "local-only-repo");
        assert_eq!(target.visibility, Visibility::Unknown);

        let _ = std::fs::remove_dir_all(&temp_dir);
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

    #[tokio::test]
    async fn resolve_targets_preserves_scan_order_when_target_overrides() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("token-burn-scanner-order-test-{unique}"));
        std::fs::create_dir_all(&temp_dir).expect("test temp dir should be created");

        // スキャン結果を模擬するための3ディレクトリを作成
        let dir_a = temp_dir.join("aaa");
        let dir_b = temp_dir.join("bbb");
        let dir_c = temp_dir.join("ccc");
        std::fs::create_dir_all(&dir_a).expect("dir_a should be created");
        std::fs::create_dir_all(&dir_b).expect("dir_b should be created");
        std::fs::create_dir_all(&dir_c).expect("dir_c should be created");

        // scan 由来の順序（a, b, c）を明示ターゲットで再現して検証する
        let config = Config {
            config_dir: temp_dir.clone(),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
            },
            prompts: Prompts {
                default: "default prompt".to_string(),
            },
            agents: vec![Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: "monday".to_string(),
                reset_time: "09:00".to_string(),
                timezone: "UTC".to_string(),
                prompt: Some("agent prompt".to_string()),
            }],
            scan: vec![],
            targets: vec![
                Target {
                    directory: dir_a.to_string_lossy().to_string(),
                    prompt: None,
                },
                Target {
                    directory: dir_b.to_string_lossy().to_string(),
                    prompt: Some("override prompt".to_string()),
                },
                Target {
                    directory: dir_c.to_string_lossy().to_string(),
                    prompt: None,
                },
            ],
        };

        let resolved = resolve_targets(&config, &config.agents[0])
            .await
            .expect("three targets should resolve");

        assert_eq!(resolved.len(), 3);
        // 順序は a, b, c のまま維持される
        assert_eq!(resolved[0].directory, dir_a);
        assert_eq!(resolved[1].directory, dir_b);
        assert_eq!(resolved[2].directory, dir_c);
        // bbb は上書きプロンプト、他は agent prompt を使う
        assert_eq!(resolved[0].prompt, "agent prompt");
        assert_eq!(resolved[1].prompt, "override prompt");
        assert_eq!(resolved[2].prompt, "agent prompt");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn resolve_targets_inplace_override_preserves_visibility() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("token-burn-scanner-vis-test-{unique}"));
        std::fs::create_dir_all(&temp_dir).expect("test temp dir should be created");

        let dir_a = temp_dir.join("aaa");
        let dir_b = temp_dir.join("bbb");
        std::fs::create_dir_all(&dir_a).expect("dir_a should be created");
        std::fs::create_dir_all(&dir_b).expect("dir_b should be created");

        let config = Config {
            config_dir: temp_dir.clone(),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
            },
            prompts: Prompts {
                default: "default".to_string(),
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
            // bbb を2回追加し、2回目でプロンプトを上書きする
            targets: vec![
                Target {
                    directory: dir_a.to_string_lossy().to_string(),
                    prompt: None,
                },
                Target {
                    directory: dir_b.to_string_lossy().to_string(),
                    prompt: None,
                },
                Target {
                    directory: dir_b.to_string_lossy().to_string(),
                    prompt: Some("overridden".to_string()),
                },
            ],
        };

        let resolved = resolve_targets(&config, &config.agents[0])
            .await
            .expect("targets should resolve");

        // bbb は重複排除されて1件になり、上書きプロンプトが反映される
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].directory, dir_a);
        assert_eq!(resolved[1].directory, dir_b);
        assert_eq!(resolved[1].prompt, "overridden");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn resolve_targets_deduplicates_overlapping_scan_results() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("token-burn-scanner-dedup-test-{unique}"));
        std::fs::create_dir_all(&temp_dir).expect("test temp dir should be created");

        let repo_dir = temp_dir.join("dup-repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir should be created");

        let status = std::process::Command::new("git")
            .args(["-C", &repo_dir.to_string_lossy(), "init", "--quiet"])
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init should succeed");

        let status = std::process::Command::new("git")
            .args([
                "-C",
                &repo_dir.to_string_lossy(),
                "remote",
                "add",
                "origin",
                "https://github.com/owayo/dup-repo.git",
            ])
            .status()
            .expect("git remote add should run");
        assert!(status.success(), "git remote add should succeed");

        let config = Config {
            config_dir: temp_dir.clone(),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
            },
            prompts: Prompts {
                default: "default prompt".to_string(),
            },
            agents: vec![Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: "monday".to_string(),
                reset_time: "09:00".to_string(),
                timezone: "UTC".to_string(),
                prompt: None,
            }],
            scan: vec![
                Scan {
                    base_dirs: vec![temp_dir.to_string_lossy().to_string()],
                    recursive: false,
                    username: None,
                    public_first: true,
                    exclude: vec![],
                },
                Scan {
                    base_dirs: vec![temp_dir.to_string_lossy().to_string()],
                    recursive: false,
                    username: None,
                    public_first: true,
                    exclude: vec![],
                },
            ],
            targets: vec![],
        };

        let resolved = resolve_targets(&config, &config.agents[0])
            .await
            .expect("targets should resolve");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].directory, repo_dir);
        assert_eq!(resolved[0].display_name, "dup-repo");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn resolve_targets_deduplicates_relative_target_and_scan_path() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir =
            std::env::temp_dir().join(format!("token-burn-scanner-relative-test-{unique}"));
        std::fs::create_dir_all(&temp_dir).expect("test temp dir should be created");

        let repo_dir = temp_dir.join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir should be created");

        let status = std::process::Command::new("git")
            .args(["-C", &repo_dir.to_string_lossy(), "init", "--quiet"])
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init should succeed");

        let old_cwd = std::env::current_dir().expect("cwd should be available");
        std::env::set_current_dir(&temp_dir).expect("should switch cwd");
        let expected_repo_dir = std::env::current_dir()
            .expect("cwd should be available")
            .join("repo");

        let config = Config {
            config_dir: temp_dir.clone(),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
            },
            prompts: Prompts {
                default: "default prompt".to_string(),
            },
            agents: vec![Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: "monday".to_string(),
                reset_time: "09:00".to_string(),
                timezone: "UTC".to_string(),
                prompt: None,
            }],
            scan: vec![Scan {
                base_dirs: vec![".".to_string()],
                recursive: false,
                username: None,
                public_first: true,
                exclude: vec![],
            }],
            targets: vec![Target {
                directory: "repo".to_string(),
                prompt: Some("target prompt".to_string()),
            }],
        };

        let resolved = resolve_targets(&config, &config.agents[0]).await;
        std::env::set_current_dir(old_cwd).expect("should restore cwd");
        let resolved = resolved.expect("same directory should be deduplicated");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].directory, expected_repo_dir);
        assert_eq!(resolved[0].display_name, "repo");
        assert_eq!(resolved[0].prompt, "target prompt");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
