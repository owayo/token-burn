mod cleanup;
mod config;
mod display;
mod executor;
mod format_stream;
mod init;
mod scanner;
mod schedule;
mod state;

use anyhow::Result;
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "token-burn")]
#[command(
    version,
    about = "週次リセット前に AI コーディングアシスタントのトークンを消費する"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// 設定ファイルのパス
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// 使用するエージェントを固定する
    #[arg(long, global = true)]
    agent: Option<String>,

    /// 実行せずに計画だけ表示する
    #[arg(short = 'n', long, global = true)]
    dry_run: bool,

    /// 保存済み状態を無視して全ターゲットを処理する
    #[arg(long, global = true)]
    fresh: bool,

    /// 処理するターゲット数の上限（デフォルト: 設定値または 10）
    #[arg(
        short,
        long,
        global = true,
        conflicts_with = "no_limit",
        value_parser = parse_positive_limit
    )]
    limit: Option<usize>,

    /// 上限なしで全ターゲットを処理する
    #[arg(long, global = true, conflicts_with = "limit")]
    no_limit: bool,

    /// 公開リポジトリのみ処理する
    #[arg(long, global = true)]
    public_only: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// トークン消費を実行する
    Run {
        /// 強制実行するディレクトリパス（指定時はスキャン・状態フィルタリングをスキップ）
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// エージェントのリセット状況を表示する
    Status,
    /// 設定ファイルとプロンプト雛形を初期化する
    Init {
        /// 確認なしで既存ファイルを上書きする
        #[arg(short, long)]
        force: bool,
    },
    /// 古いレポートディレクトリを削除する
    Clean {
        /// この期間より古いレポートを削除する（config の cleanup_after より優先）
        #[arg(long)]
        older_than: Option<String>,
    },
    /// タスク完了を記録する（ワーカースクリプト専用）
    #[command(hide = true)]
    Mark {
        /// エージェント名
        agent: String,
        /// 処理したディレクトリ
        directory: PathBuf,
        /// state.json のパス
        state_file: PathBuf,
    },
    /// stream-json 出力を読みやすいテキストに整形する（ワーカースクリプト専用）
    #[command(hide = true, name = "format-stream")]
    FormatStream {
        /// 受け取った生の stream-json 入力をそのまま保存するパス
        #[arg(long)]
        raw_output: Option<PathBuf>,
    },
}

fn parse_positive_limit(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("無効な数値です: {value}"))?;
    if parsed == 0 {
        return Err("limit には 1 以上を指定してください".to_string());
    }
    Ok(parsed)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let command = cli.command.unwrap_or(Commands::Run { paths: vec![] });

    if let Commands::Init { force } = command {
        let config_path = cli.config.unwrap_or_else(config::default_config_path);
        return init::run_init(&config_path, force);
    }

    if let Commands::FormatStream { raw_output } = &command {
        return format_stream::run(raw_output.as_deref());
    }

    if let Commands::Mark {
        agent,
        directory,
        state_file,
    } = command
    {
        state::mark_completed_atomic(&state_file, &agent, &directory)?;
        return Ok(());
    }

    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let config = config::Config::load(&config_path)?;

    let agent_name = cli.agent;
    let dry_run = cli.dry_run;
    let fresh = cli.fresh;
    let limit = if cli.no_limit {
        Some(usize::MAX)
    } else {
        cli.limit
    };
    let public_only = cli.public_only;

    match command {
        Commands::Status => {
            display::print_status(&config)?;
        }
        Commands::Run { paths } => {
            run(RunOptions {
                config,
                config_path,
                agent_name,
                dry_run,
                fresh,
                limit_override: limit,
                public_only,
                force_paths: paths,
            })
            .await?;
        }
        Commands::Clean { older_than } => {
            run_clean(&config, older_than)?;
        }
        Commands::Mark { .. } => unreachable!(),
        Commands::Init { .. } => unreachable!(),
        Commands::FormatStream { .. } => unreachable!(),
    }

    Ok(())
}

struct RunOptions {
    config: config::Config,
    config_path: PathBuf,
    agent_name: Option<String>,
    dry_run: bool,
    fresh: bool,
    limit_override: Option<usize>,
    public_only: bool,
    force_paths: Vec<PathBuf>,
}

async fn run(opts: RunOptions) -> Result<()> {
    let RunOptions {
        config,
        config_path,
        agent_name,
        dry_run,
        fresh,
        limit_override,
        public_only,
        force_paths,
    } = opts;
    let (agent_idx, sched) = if let Some(name) = &agent_name {
        let idx = config
            .agents
            .iter()
            .position(|a| a.name == *name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {}", name))?;
        let s = schedule::calculate_next_reset(&config.agents[idx])?;
        (idx, s)
    } else {
        schedule::select_nearest_agent(&config.agents)?
    };

    let agent = &config.agents[agent_idx];
    println!(
        "{} {} (reset in {})",
        "Selected agent:".bold(),
        agent.name.cyan(),
        display::format_duration(sched.time_until_reset).red()
    );
    println!();

    let targets = if force_paths.is_empty() {
        scanner::resolve_targets(&config, agent).await?
    } else {
        resolve_force_paths(&config, agent, &force_paths)?
    };

    // 公開リポジトリのみにフィルタリング
    let (targets, public_filtered) = if public_only {
        let before = targets.len();
        let filtered: Vec<_> = targets
            .into_iter()
            .filter(|t| t.visibility == scanner::Visibility::Public)
            .collect();
        let removed = before - filtered.len();
        (filtered, removed)
    } else {
        (targets, 0usize)
    };

    if public_only && targets.is_empty() {
        println!(
            "{}",
            "No public repositories found. Ensure scan.username is set for visibility detection."
                .yellow()
        );
        return Ok(());
    }

    // 保存済み状態でフィルタリング（処理済みディレクトリをスキップ）
    // force_paths 指定時は状態フィルタリングをスキップ
    let state_file = state::state_path(&config_path);
    let run_state = state::State::load(&state_file);
    let (targets, skipped) = if fresh || !force_paths.is_empty() {
        (targets, 0usize)
    } else {
        filter_by_state(targets, &run_state, agent, &config, &sched)
    };

    // 制限適用: CLIオプションが設定値を上書き
    let limit = limit_override.unwrap_or(config.settings.limit);
    let truncated = if targets.len() > limit {
        targets.len() - limit
    } else {
        0
    };
    let targets: Vec<_> = targets.into_iter().take(limit).collect();

    display::print_targets(&targets);

    if truncated > 0 {
        println!(
            "  {} {} targets (limit: {})",
            "Truncated:".dimmed(),
            truncated,
            limit
        );
    }

    if public_filtered > 0 {
        println!(
            "  {} {} targets (non-public)",
            "Filtered:".dimmed(),
            public_filtered
        );
    }

    if skipped > 0 {
        println!(
            "  {} {} targets (already processed)",
            "Skipped:".dimmed(),
            skipped
        );
    }

    if public_filtered > 0 || skipped > 0 {
        println!();
    }

    if targets.is_empty() {
        println!(
            "{}",
            "All targets already processed. Use --fresh to re-process.".yellow()
        );
        return Ok(());
    }

    let plan = executor::build_plan(agent, targets);
    executor::print_plan(&plan);

    if dry_run {
        println!(
            "{}",
            "Dry run mode - no commands will be executed.".yellow()
        );
        return Ok(());
    }

    let reset_info = sched.next_reset.format("%Y/%m/%d %H:%M").to_string();
    let report_dir = resolve_report_dir(&config.settings);
    executor::execute_plan_tmux(
        plan,
        config.settings.parallelism,
        sched.time_until_reset,
        &state_file,
        &reset_info,
        &report_dir,
    )?;

    // 古いレポートディレクトリを自動クリーンアップ
    let max_age = config.settings.cleanup_after.as_deref().unwrap_or("7d");
    println!();
    match cleanup::cleanup_old_reports(&report_dir, max_age) {
        Ok(deleted) => cleanup::print_cleanup_result(&deleted),
        Err(e) => eprintln!("{}: cleanup failed: {}", "Warning".yellow(), e),
    }

    Ok(())
}

fn run_clean(config: &config::Config, older_than: Option<String>) -> Result<()> {
    let report_dir = resolve_report_dir(&config.settings);
    let max_age = older_than
        .as_deref()
        .or(config.settings.cleanup_after.as_deref())
        .unwrap_or("7d");
    let deleted = cleanup::cleanup_old_reports(&report_dir, max_age)?;
    cleanup::print_cleanup_result(&deleted);
    Ok(())
}

fn resolve_report_dir(settings: &config::Settings) -> PathBuf {
    if let Some(ref dir) = settings.report_dir {
        let expanded = shellexpand::tilde(dir);
        PathBuf::from(expanded.as_ref())
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join("Documents")
            .join("token-burn")
    }
}

fn resolve_force_paths(
    config: &config::Config,
    agent: &config::Agent,
    paths: &[PathBuf],
) -> Result<Vec<scanner::ResolvedTarget>> {
    let effective_default = agent.prompt.as_deref().unwrap_or(&config.prompts.default);
    let default_prompt = config.resolve_prompt(effective_default)?;

    let mut seen = std::collections::HashSet::new();
    let mut targets = Vec::new();
    for path in paths {
        let dir_str = path.to_string_lossy();
        let resolved = config::resolve_directory(&dir_str)?;
        if !resolved.exists() {
            anyhow::bail!("Directory does not exist: {}", resolved.display());
        }
        if !resolved.is_dir() {
            anyhow::bail!("Not a directory: {}", resolved.display());
        }
        // 等価なパスが複数指定されても同一ターゲットは 1 回だけ処理する。
        if !seen.insert(resolved.clone()) {
            continue;
        }
        let display_name = resolved
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| dir_str.to_string());
        targets.push(scanner::ResolvedTarget {
            directory: resolved,
            display_name,
            prompt: default_prompt.clone(),
            visibility: scanner::Visibility::Unknown,
        });
    }

    if targets.is_empty() {
        anyhow::bail!("No valid paths specified");
    }

    Ok(targets)
}

fn filter_by_state(
    targets: Vec<scanner::ResolvedTarget>,
    run_state: &state::State,
    agent: &config::Agent,
    config: &config::Config,
    sched: &schedule::AgentSchedule,
) -> (Vec<scanner::ResolvedTarget>, usize) {
    use chrono::Utc;

    // カットオフ時刻を決定: この時刻以降に処理済みのディレクトリをスキップ
    let cutoff = if let Some(ref skip_within) = config.settings.skip_within {
        match state::parse_duration(skip_within) {
            Ok(dur) => Utc::now() - dur,
            Err(e) => {
                eprintln!(
                    "{}: Invalid skip_within '{}': {}",
                    "Warning".yellow(),
                    skip_within,
                    e
                );
                // 前回リセット時刻にフォールバック
                sched.previous_reset.with_timezone(&Utc)
            }
        }
    } else {
        // デフォルト: 前回リセット以降に処理済みのディレクトリをスキップ
        sched.previous_reset.with_timezone(&Utc)
    };

    let mut kept = Vec::new();
    let mut skipped = 0usize;
    for target in targets {
        if let Some(last) = run_state.last_processed(&agent.name, &target.directory)
            && last >= cutoff
        {
            skipped += 1;
            continue;
        }
        kept.push(target);
    }
    (kept, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolve_report_dir_uses_default_when_none() {
        let settings = config::Settings {
            parallelism: 1,
            skip_within: None,
            report_dir: None,
            cleanup_after: None,
            limit: 10,
        };
        let dir = resolve_report_dir(&settings);
        assert!(dir.ends_with("Documents/token-burn"));
    }

    #[test]
    fn resolve_report_dir_expands_tilde() {
        let settings = config::Settings {
            parallelism: 1,
            skip_within: None,
            report_dir: Some("~/custom-reports".to_string()),
            cleanup_after: None,
            limit: 10,
        };
        let dir = resolve_report_dir(&settings);
        // チルダが展開されていることを確認
        assert!(!dir.to_string_lossy().contains('~'));
        assert!(dir.to_string_lossy().ends_with("custom-reports"));
    }

    #[test]
    fn filter_by_state_skips_processed_targets() {
        use chrono::Utc;

        let agent = config::Agent {
            name: "claude".to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        };
        let conf = config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
            },
            agents: vec![agent.clone()],
            scan: vec![],
            targets: vec![],
        };
        let sched = schedule::calculate_next_reset(&agent).unwrap();

        // 2つのターゲットを用意: 1つは処理済み、1つは未処理
        let targets = vec![
            scanner::ResolvedTarget {
                directory: std::path::PathBuf::from("/tmp/processed-repo"),
                display_name: "processed-repo".to_string(),
                prompt: "review".to_string(),
                visibility: scanner::Visibility::Unknown,
            },
            scanner::ResolvedTarget {
                directory: std::path::PathBuf::from("/tmp/new-repo"),
                display_name: "new-repo".to_string(),
                prompt: "review".to_string(),
                visibility: scanner::Visibility::Unknown,
            },
        ];

        let mut run_state = state::State::default();
        run_state
            .agents
            .entry("claude".to_string())
            .or_default()
            .insert("/tmp/processed-repo".to_string(), Utc::now());

        let (kept, skipped) = filter_by_state(targets, &run_state, &agent, &conf, &sched);
        assert_eq!(skipped, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].display_name, "new-repo");
    }

    #[test]
    fn filter_by_state_fresh_keeps_all() {
        let targets = vec![scanner::ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: scanner::Visibility::Unknown,
        }];
        // fresh=true の場合はスキップ数0、全ターゲット保持
        let original_len = targets.len();
        // filter_by_state は fresh=true では呼ばれない（main.rs で分岐）
        // ここでは空の State で全ターゲット保持を確認
        let agent = config::Agent {
            name: "claude".to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: "monday".to_string(),
            reset_time: "09:00".to_string(),
            timezone: "UTC".to_string(),
            prompt: None,
        };
        let conf = config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
            },
            agents: vec![agent.clone()],
            scan: vec![],
            targets: vec![],
        };
        let sched = schedule::calculate_next_reset(&agent).unwrap();
        let empty_state = state::State::default();

        let (kept, skipped) = filter_by_state(targets, &empty_state, &agent, &conf, &sched);
        assert_eq!(skipped, 0);
        assert_eq!(kept.len(), original_len);
    }

    #[test]
    fn cli_limit_rejects_zero() {
        let result = Cli::try_parse_from(["token-burn", "--limit", "0"]);
        assert!(result.is_err(), "limit=0 は CLI で拒否されるべき");
    }

    #[test]
    fn resolve_force_paths_deduplicates_equivalent_relative_paths() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("token-burn-force-paths-test-{unique}"));
        let repo_dir = temp_dir.join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir should be created");

        let old_cwd = std::env::current_dir().expect("cwd should be available");
        std::env::set_current_dir(&temp_dir).expect("should switch cwd");
        let expected_repo_dir =
            config::resolve_directory("repo").expect("repo path should resolve");

        let config = config::Config {
            config_dir: temp_dir.clone(),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
            },
            prompts: config::Prompts {
                default: "default prompt".to_string(),
            },
            agents: vec![config::Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: "monday".to_string(),
                reset_time: "09:00".to_string(),
                timezone: "UTC".to_string(),
                prompt: None,
            }],
            scan: vec![],
            targets: vec![],
        };

        let resolved = resolve_force_paths(
            &config,
            &config.agents[0],
            &[PathBuf::from("repo"), PathBuf::from("./repo")],
        );

        std::env::set_current_dir(old_cwd).expect("should restore cwd");
        let resolved = resolved.expect("same directory should be deduplicated");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].directory, expected_repo_dir);
        assert_eq!(resolved[0].display_name, "repo");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
