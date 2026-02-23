mod config;
mod display;
mod executor;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let command = cli.command.unwrap_or(Commands::Run);

    if let Commands::Init { force } = command {
        let config_path = cli.config.unwrap_or_else(config::default_config_path);
        return init::run_init(&config_path, force);
    }

    if let Commands::Mark {
        agent,
        directory,
        state_file,
    } = command
    {
        let mut run_state = state::State::load(&state_file);
        run_state.mark_completed(&agent, &directory);
        run_state.save(&state_file)?;
        return Ok(());
    }

    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let config = config::Config::load(&config_path)?;

    let agent_name = cli.agent;
    let dry_run = cli.dry_run;
    let fresh = cli.fresh;

    match command {
        Commands::Status => {
            display::print_status(&config)?;
        }
        Commands::Run => {
            run(config, &config_path, agent_name, dry_run, fresh).await?;
        }
        Commands::Mark { .. } => unreachable!(),
        Commands::Init { .. } => unreachable!(),
    }

    Ok(())
}

async fn run(
    config: config::Config,
    config_path: &std::path::Path,
    agent_name: Option<String>,
    dry_run: bool,
    fresh: bool,
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

    let targets = scanner::resolve_targets(&config).await?;

    // Filter targets by saved state (skip already-processed directories)
    let state_file = state::state_path(config_path);
    let run_state = state::State::load(&state_file);
    let (targets, skipped) = if fresh {
        (targets, 0usize)
    } else {
        filter_by_state(targets, &run_state, agent, &config, &sched)
    };

    display::print_targets(&targets);

    if skipped > 0 {
        println!(
            "  {} {} targets (already processed)",
            "Skipped:".dimmed(),
            skipped
        );
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
    executor::execute_plan_tmux(
        plan,
        config.settings.parallelism,
        sched.time_until_reset,
        &state_file,
        &reset_info,
    )?;

    Ok(())
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
