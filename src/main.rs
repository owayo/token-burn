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
    about = "Consume AI coding assistant tokens before weekly reset"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Config file path
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Force specific agent
    #[arg(long, global = true)]
    agent: Option<String>,

    /// Dry run mode
    #[arg(short = 'n', long, global = true)]
    dry_run: bool,

    /// Ignore saved state and process all targets
    #[arg(long, global = true)]
    fresh: bool,

    /// Maximum number of targets to process (default: from config or 10)
    #[arg(short, long, global = true, conflicts_with = "no_limit")]
    limit: Option<usize>,

    /// Process all targets without limit
    #[arg(long, global = true, conflicts_with = "limit")]
    no_limit: bool,

    /// Only process public repositories
    #[arg(long, global = true)]
    public_only: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Execute token consumption
    Run,
    /// Show agent reset status
    Status,
    /// Initialize config file and prompt templates
    Init {
        /// Overwrite existing files without confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Clean up old report directories
    Clean {
        /// Delete reports older than this duration (overrides config cleanup_after)
        #[arg(long)]
        older_than: Option<String>,
    },
    /// Record task completion (internal use by worker scripts)
    #[command(hide = true)]
    Mark {
        /// Agent name
        agent: String,
        /// Directory that was processed
        directory: PathBuf,
        /// State file path
        state_file: PathBuf,
    },
    /// Format stream-json output into readable text (internal use by worker scripts)
    #[command(hide = true, name = "format-stream")]
    FormatStream,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let command = cli.command.unwrap_or(Commands::Run);

    if let Commands::Init { force } = command {
        let config_path = cli.config.unwrap_or_else(config::default_config_path);
        return init::run_init(&config_path, force);
    }

    if let Commands::FormatStream = command {
        return format_stream::run();
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
        Commands::Run => {
            run(
                config,
                &config_path,
                agent_name,
                dry_run,
                fresh,
                limit,
                public_only,
            )
            .await?;
        }
        Commands::Clean { older_than } => {
            run_clean(&config, older_than)?;
        }
        Commands::Mark { .. } => unreachable!(),
        Commands::Init { .. } => unreachable!(),
        Commands::FormatStream => unreachable!(),
    }

    Ok(())
}

async fn run(
    config: config::Config,
    config_path: &std::path::Path,
    agent_name: Option<String>,
    dry_run: bool,
    fresh: bool,
    limit_override: Option<usize>,
    public_only: bool,
) -> Result<()> {
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

    let targets = scanner::resolve_targets(&config, agent).await?;

    // Filter to public repositories only if requested
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

    // Filter targets by saved state (skip already-processed directories)
    let state_file = state::state_path(config_path);
    let run_state = state::State::load(&state_file);
    let (targets, skipped) = if fresh {
        (targets, 0usize)
    } else {
        filter_by_state(targets, &run_state, agent, &config, &sched)
    };

    // Apply limit: CLI option overrides config value
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

    // Auto-cleanup old report directories
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

fn filter_by_state(
    targets: Vec<scanner::ResolvedTarget>,
    run_state: &state::State,
    agent: &config::Agent,
    config: &config::Config,
    sched: &schedule::AgentSchedule,
) -> (Vec<scanner::ResolvedTarget>, usize) {
    use chrono::Utc;

    // Determine the cutoff time: skip directories processed after this time
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
                // Fall back to previous reset
                sched.previous_reset.with_timezone(&Utc)
            }
        }
    } else {
        // Default: skip directories processed since the previous reset
        sched.previous_reset.with_timezone(&Utc)
    };

    let mut kept = Vec::new();
    let mut skipped = 0usize;
    for target in targets {
        if let Some(last) = run_state.last_processed(&agent.name, &target.directory) {
            if last >= cutoff {
                skipped += 1;
                continue;
            }
        }
        kept.push(target);
    }
    (kept, skipped)
}
