mod classify;
mod cleanup;
mod config;
mod display;
mod executor;
mod format_stream;
mod init;
mod scanner;
mod schedule;
mod state;
mod usage;

#[cfg(test)]
mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    pub struct CwdGuard {
        original: PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl CwdGuard {
        pub fn switch_to(path: &Path) -> Self {
            // カレントディレクトリはプロセス全体の状態なので、変更するテストは必ず直列化する。
            let lock = CWD_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let original = std::env::current_dir().expect("cwd should be available");
            std::env::set_current_dir(path).expect("should switch cwd");
            Self {
                original,
                _lock: lock,
            }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }
}

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::collections::HashMap;
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
    /// 処理する順番でターゲットディレクトリを一覧する（limit 無視、実行しない）
    List {
        /// 強制的に対象とするディレクトリパス（指定時はスキャン・状態フィルタをスキップ）
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
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
        /// レート制限閾値超過時に作成する停止ファイルのパス
        #[arg(long)]
        stop_file: Option<PathBuf>,
        /// レート制限使用率の自動停止閾値（%）
        #[arg(long, default_value_t = 95)]
        threshold: u8,
    },
    /// jsonl を分類して終了コード (0=success,1=failed,2=rate-limited,3=retryable) を返す（ワーカースクリプト専用）
    #[command(hide = true, name = "classify-result")]
    ClassifyResult {
        /// 分類対象の jsonl ファイル
        jsonl: PathBuf,
    },
    /// ai-usage の使用率をチェックし閾値超過なら stop file を作成する（ワーカースクリプト専用）
    #[command(hide = true, name = "usage-gate")]
    UsageGate {
        /// ai-usage --json の profile と照合する値
        #[arg(long)]
        profile: String,
        /// ai-usage --json の provider と照合する値
        #[arg(long)]
        provider: String,
        /// 使用率の停止閾値（%）
        #[arg(long)]
        threshold: u8,
        /// 閾値超過時に作成する stop file のパス
        #[arg(long)]
        stop_file: PathBuf,
        /// ai-usage 出力の短 TTL キャッシュファイルのパス
        #[arg(long)]
        cache_file: PathBuf,
        /// ai-usage コマンド（`--` 以降）
        #[arg(last = true)]
        command: Vec<String>,
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

    if let Commands::FormatStream {
        raw_output,
        stop_file,
        threshold,
    } = &command
    {
        return format_stream::run(raw_output.as_deref(), stop_file.as_deref(), *threshold);
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

    if let Commands::ClassifyResult { jsonl } = &command {
        let class = classify::classify_jsonl(jsonl);
        if let Some(msg) = class.message() {
            println!("{msg}");
        }
        std::process::exit(class.exit_code());
    }

    if let Commands::UsageGate {
        profile,
        provider,
        threshold,
        stop_file,
        cache_file,
        command,
    } = &command
    {
        usage::run_usage_gate(
            profile, provider, *threshold, stop_file, cache_file, command,
        )
        .await?;
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
            let runtime_agents = config.expand_runtime_agents()?;
            let resolver = usage::ScheduleResolver::load(&config).await;
            display::print_status(&runtime_agents, &resolver)?;
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
        Commands::List { paths } => {
            list(ListOptions {
                config,
                config_path,
                agent_name,
                fresh,
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
        Commands::ClassifyResult { .. } => unreachable!(),
        Commands::UsageGate { .. } => unreachable!(),
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

struct ListOptions {
    config: config::Config,
    config_path: PathBuf,
    agent_name: Option<String>,
    fresh: bool,
    public_only: bool,
    force_paths: Vec<PathBuf>,
}

async fn list(opts: ListOptions) -> Result<()> {
    let ListOptions {
        config,
        config_path,
        agent_name,
        fresh,
        public_only,
        force_paths,
    } = opts;
    let runtime_agents = config.expand_runtime_agents()?;
    let resolver = usage::ScheduleResolver::load(&config).await;
    let (agent_idx, sched) = if let Some(name) = &agent_name {
        let idx = runtime_agents
            .iter()
            .position(|a| a.name == *name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {}", name))?;
        let s = resolver
            .schedule_for(&runtime_agents[idx])?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Selected agent '{}' is skipped (ai-usage fallback=skip)",
                    name
                )
            })?;
        (idx, s)
    } else {
        resolver.select_nearest(&runtime_agents)?
    };

    let agent = &runtime_agents[agent_idx];
    println!(
        "{} {} (reset in {}, source: {})",
        "Selected agent:".bold(),
        sched.agent_name.cyan(),
        display::format_duration(sched.time_until_reset).red(),
        sched.source.label().dimmed(),
    );
    println!();

    let targets = if force_paths.is_empty() {
        scanner::resolve_targets(&config, agent).await?
    } else {
        resolve_force_paths(&config, agent, &force_paths)?
    };

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

    let state_file = state::state_path(&config_path);
    let run_state = state::State::load(&state_file);
    let (mut targets, skipped) = if fresh || !force_paths.is_empty() {
        (targets, 0usize)
    } else {
        filter_by_state(targets, &run_state, agent, &config, &sched)
    };

    // 最終ファイル変更日時が古い順に並べ替える（force_paths 指定時は CLI 指定順を尊重）
    let modified = if force_paths.is_empty() {
        let dirs: Vec<_> = targets.iter().map(|t| t.directory.clone()).collect();
        let modified = scanner::repo_last_modified_map(&dirs).await;
        sort_by_least_recent(&mut targets, &modified, public_first_enabled(&config));
        modified
    } else {
        HashMap::new()
    };

    display::print_targets(&targets, &modified);

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

    Ok(())
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
    let runtime_agents = config.expand_runtime_agents()?;
    let resolver = usage::ScheduleResolver::load(&config).await;
    let (agent_idx, sched) = if let Some(name) = &agent_name {
        let idx = runtime_agents
            .iter()
            .position(|a| a.name == *name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {}", name))?;
        let s = resolver
            .schedule_for(&runtime_agents[idx])?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Selected agent '{}' is skipped (ai-usage fallback=skip)",
                    name
                )
            })?;
        (idx, s)
    } else {
        resolver.select_nearest(&runtime_agents)?
    };

    let agent = &runtime_agents[agent_idx];
    println!(
        "{} {} (reset in {}, source: {})",
        "Selected agent:".bold(),
        sched.agent_name.cyan(),
        display::format_duration(sched.time_until_reset).red(),
        sched.source.label().dimmed(),
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
    let (mut targets, skipped) = if fresh || !force_paths.is_empty() {
        (targets, 0usize)
    } else {
        filter_by_state(targets, &run_state, agent, &config, &sched)
    };

    // 最終ファイル変更日時が古い順に並べ替えてから limit を適用する。
    // これをしないと limit で毎回リストの先頭だけが選ばれ、末尾のリポジトリに到達しない。
    // force_paths 指定時は CLI 指定順を尊重する。
    let modified = if force_paths.is_empty() {
        let dirs: Vec<_> = targets.iter().map(|t| t.directory.clone()).collect();
        let modified = scanner::repo_last_modified_map(&dirs).await;
        sort_by_least_recent(&mut targets, &modified, public_first_enabled(&config));
        modified
    } else {
        HashMap::new()
    };

    // 制限適用: CLIオプションが設定値を上書き
    let limit = limit_override.unwrap_or(config.settings.limit);
    let truncated = if targets.len() > limit {
        targets.len() - limit
    } else {
        0
    };
    let targets: Vec<_> = targets.into_iter().take(limit).collect();

    display::print_targets(&targets, &modified);

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

    let ai_usage_command = config
        .ai_usage
        .as_ref()
        .filter(|g| g.enabled)
        .map(|g| g.command.clone());
    let plan = executor::build_plan(agent, targets, ai_usage_command);
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
        config.settings.rate_limit_threshold,
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
    agent: &config::RuntimeAgent,
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

        // [[targets]] に定義された専用プロンプトがあればそちらを優先する
        let prompt = config
            .targets
            .iter()
            .filter_map(|t| {
                let t_resolved = config::resolve_directory(&t.directory).ok()?;
                if t_resolved == resolved {
                    t.prompt.as_deref()
                } else {
                    None
                }
            })
            .next()
            .map(|p| config.resolve_prompt(p))
            .transpose()?
            .unwrap_or_else(|| default_prompt.clone());

        // CLI で明示的に指定されたパスはユーザー指定順を維持するため、
        // [[targets]] の `defer` フラグは反映しない。
        targets.push(scanner::ResolvedTarget {
            directory: resolved,
            display_name,
            prompt,
            visibility: scanner::Visibility::Unknown,
            defer: false,
        });
    }

    if targets.is_empty() {
        anyhow::bail!("No valid paths specified");
    }

    Ok(targets)
}

/// 「しばらく触っていないリポジトリ」を先頭に寄せる。
///
/// 処理済みカットオフ (`skip_within` / 前回リセット) は絶対時刻の窓なので、窓をまたいだ
/// 時点で処理済み履歴が一斉に無効化される。ターゲット順が固定のままだと、そのたびに
/// リストの先頭 `limit` 件だけが再処理され、末尾のリポジトリには永遠に到達しない。
/// 最終ファイル変更日時が古い順に並べ替えることで、カットオフが切れても前回処理した分は
/// 後ろへ回り、放置されているリポジトリから消化される。
///
/// 順序の基準は `state.json` の処理時刻ではなく、リポジトリ自身の最終ファイル変更日時
/// (`scanner::repo_last_modified`)。実際に変更が入ったかどうかを見るため、レート制限で
/// 中断されて何も変更できなかった実行を「処理済み」と数えてしまうことがない。
///
/// `defer` の優先度は従来どおり維持し、その内側だけを並べ替える。安定ソートなので、
/// 変更日時が同じターゲット同士の順序も変わらない。
/// 変更日時を取得できなかったリポジトリは、判断材料が無いので各グループの末尾に置く。
///
/// 可視性 (`public_first`) でグループ化するかどうかは `public_first` 引数で切り替える。
/// 無条件に `visibility` をソートキーへ入れると、`public_first = false` を指定しても
/// 公開リポジトリが必ず先頭に寄り、設定が黙って無視される（`limit` と併用すると
/// 公開リポジトリが limit 件以上ある限り非公開リポジトリに永久に到達しない）。
fn sort_by_least_recent(
    targets: &mut [scanner::ResolvedTarget],
    modified: &HashMap<PathBuf, DateTime<Utc>>,
    public_first: bool,
) {
    targets.sort_by_cached_key(|t| {
        let last_modified = modified.get(&t.directory).copied();
        // public_first = false のときは全要素が None になり、可視性はキーとして効かない。
        let visibility = public_first.then(|| t.visibility.clone());
        (t.defer, visibility, last_modified.is_none(), last_modified)
    });
}

/// いずれかの `[[scan]]` が `public_first` を有効にしているか。
/// 有効な scan が 1 つでもあれば、最終的な実行順も可視性でグループ化する。
fn public_first_enabled(config: &config::Config) -> bool {
    config.scan.iter().any(|scan| scan.public_first)
}

fn filter_by_state(
    targets: Vec<scanner::ResolvedTarget>,
    run_state: &state::State,
    agent: &config::RuntimeAgent,
    config: &config::Config,
    sched: &schedule::AgentSchedule,
) -> (Vec<scanner::ResolvedTarget>, usize) {
    use chrono::Utc;

    // カットオフ時刻を決定: この時刻以降に処理済みのディレクトリをスキップ
    let cutoff = if let Some(ref skip_within) = config.settings.skip_within {
        match state::parse_duration(skip_within) {
            Ok(dur) => match Utc::now().checked_sub_signed(dur) {
                Some(cutoff) => cutoff,
                None => {
                    eprintln!(
                        "{}: skip_within '{}' is too large; using the previous reset",
                        "Warning".yellow(),
                        skip_within
                    );
                    sched.state_cutoff.with_timezone(&Utc)
                }
            },
            Err(e) => {
                eprintln!(
                    "{}: Invalid skip_within '{}': {}",
                    "Warning".yellow(),
                    skip_within,
                    e
                );
                // 前回リセット時刻にフォールバック
                sched.state_cutoff.with_timezone(&Utc)
            }
        }
    } else {
        // デフォルト: 前回リセット以降に処理済みのディレクトリをスキップ
        sched.state_cutoff.with_timezone(&Utc)
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
            rate_limit_threshold: 95,
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
            rate_limit_threshold: 95,
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
            reset_weekday: Some("monday".to_string()),
            reset_time: Some("09:00".to_string()),
            timezone: Some("UTC".to_string()),
            ..Default::default()
        };
        let conf = config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
            },
            agents: vec![agent.clone()],
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        };
        let runtime = conf.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];
        let sched = schedule::calculate_fixed_reset(runtime_agent).unwrap();

        // 2つのターゲットを用意: 1つは処理済み、1つは未処理
        let targets = vec![
            scanner::ResolvedTarget {
                directory: std::path::PathBuf::from("/tmp/processed-repo"),
                display_name: "processed-repo".to_string(),
                prompt: "review".to_string(),
                visibility: scanner::Visibility::Unknown,
                defer: false,
            },
            scanner::ResolvedTarget {
                directory: std::path::PathBuf::from("/tmp/new-repo"),
                display_name: "new-repo".to_string(),
                prompt: "review".to_string(),
                visibility: scanner::Visibility::Unknown,
                defer: false,
            },
        ];

        let mut run_state = state::State::default();
        run_state
            .agents
            .entry("claude".to_string())
            .or_default()
            .insert("/tmp/processed-repo".to_string(), Utc::now());

        let (kept, skipped) = filter_by_state(targets, &run_state, runtime_agent, &conf, &sched);
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
            defer: false,
        }];
        // fresh=true の場合はスキップ数0、全ターゲット保持
        let original_len = targets.len();
        // filter_by_state は fresh=true では呼ばれない（main.rs で分岐）
        // ここでは空の State で全ターゲット保持を確認
        let agent = config::Agent {
            name: "claude".to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: Some("monday".to_string()),
            reset_time: Some("09:00".to_string()),
            timezone: Some("UTC".to_string()),
            ..Default::default()
        };
        let conf = config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
            },
            agents: vec![agent.clone()],
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        };
        let runtime = conf.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];
        let sched = schedule::calculate_fixed_reset(runtime_agent).unwrap();
        let empty_state = state::State::default();

        let (kept, skipped) = filter_by_state(targets, &empty_state, runtime_agent, &conf, &sched);
        assert_eq!(skipped, 0);
        assert_eq!(kept.len(), original_len);
    }

    fn target_at(
        name: &str,
        visibility: scanner::Visibility,
        defer: bool,
    ) -> scanner::ResolvedTarget {
        scanner::ResolvedTarget {
            directory: std::path::PathBuf::from(format!("/tmp/{name}")),
            display_name: name.to_string(),
            prompt: "review".to_string(),
            visibility,
            defer,
        }
    }

    fn modified_at(entries: &[(&str, i64)]) -> HashMap<PathBuf, DateTime<Utc>> {
        entries
            .iter()
            .map(|(name, days_ago)| {
                (
                    std::path::PathBuf::from(format!("/tmp/{name}")),
                    Utc::now() - chrono::Duration::days(*days_ago),
                )
            })
            .collect()
    }

    #[test]
    fn sort_by_least_recent_puts_stale_repos_first() {
        use scanner::Visibility;
        // 実行順が固定だと limit で先頭だけが繰り返し処理されるため、
        // 長く触られていないリポジトリが先に来ることを確認する
        let mut targets = vec![
            target_at("fresh", Visibility::Public, false),
            target_at("stale", Visibility::Public, false),
            target_at("middle", Visibility::Public, false),
        ];
        let modified = modified_at(&[("fresh", 1), ("stale", 60), ("middle", 10)]);

        sort_by_least_recent(&mut targets, &modified, true);

        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(order, vec!["stale", "middle", "fresh"]);
    }

    #[test]
    fn sort_by_least_recent_keeps_visibility_and_defer_priority() {
        use scanner::Visibility;
        // public_first と defer の優先度は並べ替え後も維持される
        let mut targets = vec![
            target_at("deferred-stale", Visibility::Public, true),
            target_at("private-stale", Visibility::Private, false),
            target_at("public-fresh", Visibility::Public, false),
        ];
        let modified = modified_at(&[
            ("deferred-stale", 90),
            ("private-stale", 90),
            ("public-fresh", 1),
        ]);

        sort_by_least_recent(&mut targets, &modified, true);

        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(
            order,
            vec!["public-fresh", "private-stale", "deferred-stale"]
        );
    }

    #[test]
    fn sort_by_least_recent_sorts_within_visibility_groups() {
        use scanner::Visibility;
        // public 群を変更日時順、private 群を変更日時順に並べ、それを連結した順序になる。
        // public 優先は変更日時に負けないので、public の最新 (1日前) は
        // private の最古 (200日前) よりも先に来る。
        let mut targets = vec![
            target_at("private-fresh", Visibility::Private, false),
            target_at("public-fresh", Visibility::Public, false),
            target_at("unknown-stale", Visibility::Unknown, false),
            target_at("private-stale", Visibility::Private, false),
            target_at("public-stale", Visibility::Public, false),
            target_at("private-middle", Visibility::Private, false),
            target_at("public-middle", Visibility::Public, false),
        ];
        let modified = modified_at(&[
            ("private-fresh", 2),
            ("public-fresh", 1),
            ("unknown-stale", 300),
            ("private-stale", 200),
            ("public-stale", 60),
            ("private-middle", 100),
            ("public-middle", 30),
        ]);

        sort_by_least_recent(&mut targets, &modified, true);

        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "public-stale",
                "public-middle",
                "public-fresh",
                "private-stale",
                "private-middle",
                "private-fresh",
                "unknown-stale",
            ]
        );
    }

    #[test]
    fn sort_by_least_recent_puts_unknown_modified_last() {
        use scanner::Visibility;
        // 変更日時を取得できなかったリポジトリは判断材料が無いので末尾へ
        let mut targets = vec![
            target_at("unknown", Visibility::Public, false),
            target_at("known-fresh", Visibility::Public, false),
        ];
        let modified = modified_at(&[("known-fresh", 1)]);

        sort_by_least_recent(&mut targets, &modified, true);

        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(order, vec!["known-fresh", "unknown"]);
    }

    #[test]
    fn sort_by_least_recent_is_stable_for_equal_timestamps() {
        use scanner::Visibility;
        // 変更日時が同じなら元の順序（scan / [[targets]] の並び）を保つ
        let mut targets = vec![
            target_at("first", Visibility::Public, false),
            target_at("second", Visibility::Public, false),
            target_at("third", Visibility::Public, false),
        ];
        let same = Utc::now() - chrono::Duration::days(3);
        let modified: HashMap<PathBuf, DateTime<Utc>> = targets
            .iter()
            .map(|t| (t.directory.clone(), same))
            .collect();

        sort_by_least_recent(&mut targets, &modified, true);

        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(order, vec!["first", "second", "third"]);
    }

    /// `public_first = false` を指定したら可視性でグループ化しない。
    ///
    /// 以前は `sort_by_least_recent` が無条件に `visibility` をソートキーへ入れていたため、
    /// `public_first` を読むのは scanner の 1 箇所だけなのに最終順序が必ず
    /// public 優先になり、設定が黙って無視されていた。`limit` と併用すると
    /// 公開リポジトリが limit 件以上ある限り非公開リポジトリへ永久に到達しない。
    #[test]
    fn sort_by_least_recent_ignores_visibility_when_public_first_disabled() {
        use scanner::Visibility;
        let mut targets = vec![
            target_at("public-fresh", Visibility::Public, false),
            target_at("private-stale", Visibility::Private, false),
            target_at("unknown-middle", Visibility::Unknown, false),
        ];
        let modified = modified_at(&[
            ("public-fresh", 1),
            ("private-stale", 90),
            ("unknown-middle", 30),
        ]);

        sort_by_least_recent(&mut targets, &modified, false);

        // 可視性は無視され、純粋に最終更新の古い順になる
        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(
            order,
            vec!["private-stale", "unknown-middle", "public-fresh"]
        );
    }

    /// `public_first = false` でも `defer` の優先度は維持される。
    #[test]
    fn sort_by_least_recent_keeps_defer_when_public_first_disabled() {
        use scanner::Visibility;
        let mut targets = vec![
            target_at("deferred-stale", Visibility::Public, true),
            target_at("normal-fresh", Visibility::Private, false),
        ];
        let modified = modified_at(&[("deferred-stale", 300), ("normal-fresh", 1)]);

        sort_by_least_recent(&mut targets, &modified, false);

        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(order, vec!["normal-fresh", "deferred-stale"]);
    }

    /// `public_first = false` でも変更日時不明は末尾、同時刻は元順序で安定。
    #[test]
    fn sort_by_least_recent_without_public_first_keeps_unknown_last_and_is_stable() {
        use scanner::Visibility;
        let mut targets = vec![
            target_at("no-mtime-a", Visibility::Public, false),
            target_at("known", Visibility::Private, false),
            target_at("no-mtime-b", Visibility::Public, false),
        ];
        let modified = modified_at(&[("known", 5)]);

        sort_by_least_recent(&mut targets, &modified, false);

        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(order, vec!["known", "no-mtime-a", "no-mtime-b"]);
    }

    fn scan_with(public_first: bool) -> config::Scan {
        config::Scan {
            base_dirs: vec!["/tmp".to_string()],
            recursive: false,
            username: None,
            public_first,
            exclude: vec![],
        }
    }

    fn config_with_scans(scans: Vec<config::Scan>) -> config::Config {
        config::Config {
            config_dir: std::path::PathBuf::from("/tmp"),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
            },
            agents: vec![],
            scan: scans,
            targets: vec![],
            ai_usage: None,
        }
    }

    #[test]
    fn public_first_enabled_reflects_scan_settings() {
        // scan が無い構成（[[targets]] のみ）は可視性でグループ化しない
        assert!(!public_first_enabled(&config_with_scans(vec![])));
        assert!(!public_first_enabled(&config_with_scans(vec![scan_with(
            false
        )])));
        assert!(public_first_enabled(&config_with_scans(vec![scan_with(
            true
        )])));
        // 1 つでも有効な scan があればグループ化する
        assert!(public_first_enabled(&config_with_scans(vec![
            scan_with(false),
            scan_with(true),
        ])));
    }

    #[test]
    fn cli_limit_rejects_zero() {
        let result = Cli::try_parse_from(["token-burn", "--limit", "0"]);
        assert!(result.is_err(), "limit=0 は CLI で拒否されるべき");
    }

    #[test]
    fn filter_by_state_with_skip_within_uses_duration_cutoff() {
        use chrono::Utc;

        let agent = config::Agent {
            name: "claude".to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: Some("monday".to_string()),
            reset_time: Some("09:00".to_string()),
            timezone: Some("UTC".to_string()),
            ..Default::default()
        };
        // skip_within を 1 時間に設定
        let conf = config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                skip_within: Some("1h".to_string()),
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
            },
            agents: vec![agent.clone()],
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        };
        let runtime = conf.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];
        let sched = schedule::calculate_fixed_reset(runtime_agent).unwrap();

        let targets = vec![
            scanner::ResolvedTarget {
                directory: std::path::PathBuf::from("/tmp/recent-repo"),
                display_name: "recent-repo".to_string(),
                prompt: "review".to_string(),
                visibility: scanner::Visibility::Unknown,
                defer: false,
            },
            scanner::ResolvedTarget {
                directory: std::path::PathBuf::from("/tmp/old-repo"),
                display_name: "old-repo".to_string(),
                prompt: "review".to_string(),
                visibility: scanner::Visibility::Unknown,
                defer: false,
            },
        ];

        let mut run_state = state::State::default();
        // 30分前に処理済み → skip_within=1h 以内なのでスキップ
        run_state
            .agents
            .entry("claude".to_string())
            .or_default()
            .insert(
                "/tmp/recent-repo".to_string(),
                Utc::now() - chrono::Duration::minutes(30),
            );
        // 2時間前に処理済み → skip_within=1h を超えているので再処理
        run_state
            .agents
            .entry("claude".to_string())
            .or_default()
            .insert(
                "/tmp/old-repo".to_string(),
                Utc::now() - chrono::Duration::hours(2),
            );

        let (kept, skipped) = filter_by_state(targets, &run_state, runtime_agent, &conf, &sched);
        assert_eq!(
            skipped, 1,
            "1時間以内に処理済みのターゲットはスキップされるべき"
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].display_name, "old-repo");
    }

    #[test]
    fn filter_by_state_with_out_of_range_skip_within_uses_schedule_cutoff() {
        use chrono::Utc;

        let agent = config::Agent {
            name: "claude".to_string(),
            command: vec!["echo".to_string()],
            reset_weekday: Some("monday".to_string()),
            reset_time: Some("09:00".to_string()),
            timezone: Some("UTC".to_string()),
            ..Default::default()
        };
        let conf = config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                // chrono::Duration では表現できるが、現在時刻からの減算は日時範囲を超える。
                skip_within: Some("9223372036854775s".to_string()),
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
            },
            agents: vec![agent.clone()],
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        };
        let runtime = conf.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];
        let sched = schedule::calculate_fixed_reset(runtime_agent).unwrap();
        let targets = vec![scanner::ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/processed-repo"),
            display_name: "processed-repo".to_string(),
            prompt: "review".to_string(),
            visibility: scanner::Visibility::Unknown,
            defer: false,
        }];
        let mut run_state = state::State::default();
        run_state
            .agents
            .entry("claude".to_string())
            .or_default()
            .insert("/tmp/processed-repo".to_string(), Utc::now());

        let (kept, skipped) = filter_by_state(targets, &run_state, runtime_agent, &conf, &sched);

        assert_eq!(skipped, 1, "スケジュールのカットオフへ戻るべき");
        assert!(kept.is_empty());
    }

    #[test]
    fn resolve_force_paths_rejects_nonexistent_directory() {
        let config = config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "default".to_string(),
            },
            agents: vec![config::Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: Some("monday".to_string()),
                reset_time: Some("09:00".to_string()),
                timezone: Some("UTC".to_string()),
                ..Default::default()
            }],
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        };
        let runtime = config.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];

        let result = resolve_force_paths(
            &config,
            runtime_agent,
            &[PathBuf::from("/nonexistent/path/that/does/not/exist")],
        );
        assert!(result.is_err(), "存在しないパスはエラーになるべき");
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn resolve_force_paths_rejects_file_path() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be monotonic")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("token-burn-file-test-{unique}"));
        std::fs::create_dir_all(&temp_dir).expect("temp dir should be created");
        let file_path = temp_dir.join("not-a-dir.txt");
        std::fs::write(&file_path, "dummy").expect("file should be created");

        let config = config::Config {
            config_dir: temp_dir.clone(),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "default".to_string(),
            },
            agents: vec![config::Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: Some("monday".to_string()),
                reset_time: Some("09:00".to_string()),
                timezone: Some("UTC".to_string()),
                ..Default::default()
            }],
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        };
        let runtime = config.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];

        let result = resolve_force_paths(&config, runtime_agent, &[file_path]);
        assert!(result.is_err(), "ファイルパスはエラーになるべき");
        assert!(result.unwrap_err().to_string().contains("Not a directory"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn resolve_force_paths_empty_paths_returns_error() {
        let config = config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "default".to_string(),
            },
            agents: vec![config::Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: Some("monday".to_string()),
                reset_time: Some("09:00".to_string()),
                timezone: Some("UTC".to_string()),
                ..Default::default()
            }],
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        };
        let runtime = config.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];

        let result = resolve_force_paths(&config, runtime_agent, &[]);
        assert!(result.is_err(), "空のパスリストはエラーになるべき");
        assert!(result.unwrap_err().to_string().contains("No valid paths"));
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

        let resolved = {
            let _cwd_guard = crate::test_support::CwdGuard::switch_to(&temp_dir);
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
                    rate_limit_threshold: 95,
                },
                prompts: config::Prompts {
                    default: "default prompt".to_string(),
                },
                agents: vec![config::Agent {
                    name: "agent".to_string(),
                    command: vec!["echo".to_string()],
                    reset_weekday: Some("monday".to_string()),
                    reset_time: Some("09:00".to_string()),
                    timezone: Some("UTC".to_string()),
                    ..Default::default()
                }],
                scan: vec![],
                targets: vec![],
                ai_usage: None,
            };
            let runtime = config.expand_runtime_agents().expect("expand");
            let runtime_agent = &runtime[0];

            let resolved = resolve_force_paths(
                &config,
                runtime_agent,
                &[PathBuf::from("repo"), PathBuf::from("./repo")],
            );
            (expected_repo_dir, resolved)
        };
        let (expected_repo_dir, resolved) = resolved;
        let resolved = resolved.expect("same directory should be deduplicated");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].directory, expected_repo_dir);
        assert_eq!(resolved[0].display_name, "repo");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn resolve_force_paths_uses_target_prompt() {
        let temp_dir = std::env::temp_dir().join("token-burn-test-target-prompt");
        let repo_dir = temp_dir.join("my-repo");
        let _ = std::fs::create_dir_all(&repo_dir);

        let config = config::Config {
            config_dir: std::path::PathBuf::from("/tmp"),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "default prompt".to_string(),
            },
            agents: vec![config::Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: Some("monday".to_string()),
                reset_time: Some("09:00".to_string()),
                timezone: Some("UTC".to_string()),
                ..Default::default()
            }],
            scan: vec![],
            targets: vec![config::Target {
                directory: repo_dir.to_string_lossy().to_string(),
                prompt: Some("custom target prompt".to_string()),
                defer: false,
            }],
            ai_usage: None,
        };
        let runtime = config.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];

        let resolved = resolve_force_paths(&config, runtime_agent, std::slice::from_ref(&repo_dir))
            .expect("should resolve");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].prompt, "custom target prompt");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn resolve_force_paths_falls_back_to_default_without_target() {
        let temp_dir = std::env::temp_dir().join("token-burn-test-no-target-prompt");
        let repo_dir = temp_dir.join("other-repo");
        let _ = std::fs::create_dir_all(&repo_dir);

        let config = config::Config {
            config_dir: std::path::PathBuf::from("/tmp"),
            settings: config::Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
            },
            prompts: config::Prompts {
                default: "default prompt".to_string(),
            },
            agents: vec![config::Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: Some("monday".to_string()),
                reset_time: Some("09:00".to_string()),
                timezone: Some("UTC".to_string()),
                ..Default::default()
            }],
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        };
        let runtime = config.expand_runtime_agents().expect("expand");
        let runtime_agent = &runtime[0];

        let resolved = resolve_force_paths(&config, runtime_agent, std::slice::from_ref(&repo_dir))
            .expect("should resolve");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].prompt, "default prompt");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
