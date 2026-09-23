mod classify;
mod cleanup;
mod config;
mod display;
mod executor;
mod format_stream;
mod init;
mod rate_control;
mod resume;
mod scanner;
mod schedule;
mod state;
mod tui;
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
use std::collections::{BTreeMap, HashMap, HashSet};
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

    /// 保存済み状態（処理済み履歴と中断セッション）を無視して全ターゲットを最初から処理する
    #[arg(long, global = true)]
    fresh: bool,

    /// レート制限で中断したセッションを再開せず、新しいセッションで処理する
    #[arg(long, global = true)]
    no_resume: bool,

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

    /// 並列実行するワーカー数（デフォルト: 設定値の parallelism）
    #[arg(
        short = 'w',
        long,
        global = true,
        value_name = "N",
        value_parser = parse_positive_workers
    )]
    workers: Option<usize>,

    /// 実行対象と実行順を TUI で選んでから実行する（run のみ）
    #[arg(short = 'i', long, global = true)]
    interactive: bool,

    /// 公開リポジトリのみ処理する
    #[arg(long, global = true)]
    public_only: bool,

    /// 処理済み判定を共有する範囲（デフォルト: 設定値の dedup_scope）。
    /// agent を指定すると他アカウントの処理済み履歴を引き継がない
    #[arg(long, global = true, value_name = "SCOPE")]
    dedup_scope: Option<config::DedupScope>,
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
    /// jsonl を分類して終了コード (0=success,1=failed,2=rate-limited,3=retryable,4=resume-unavailable) を返す（ワーカースクリプト専用）
    #[command(hide = true, name = "classify-result")]
    ClassifyResult {
        /// 分類対象の jsonl ファイル
        jsonl: PathBuf,
    },
    /// 中断したセッションの再開記録（resume.json）を更新する（ワーカースクリプト専用）
    #[command(hide = true, name = "resume-entry")]
    ResumeEntry {
        #[command(subcommand)]
        action: ResumeEntryAction,
    },
    /// 停止判定とキューからの claim を同じロックの下で行う（ワーカースクリプト専用）
    ///
    /// 一時停止中なら再開時刻まで待つ。終了コードは 0=claim 成功（番号を stdout へ出力）、
    /// 10=恒久停止、20=処理できる pending 無し。
    #[command(hide = true, name = "gate-claim")]
    GateClaim {
        /// 恒久停止シグナルのファイルパス（一時停止ファイルはこの隣に置かれる）
        #[arg(long)]
        stop_file: PathBuf,
        /// `pending-*` / `claimed-*` を置くキューディレクトリ
        #[arg(long)]
        queue_dir: PathBuf,
        /// 実行全体のデッドライン（Unix epoch 秒）。これを越える待機はせず停止する
        #[arg(long)]
        deadline_epoch: Option<i64>,
        /// 一時停止から再開する直前に実行する再検証コマンド（usage-gate）
        #[arg(long)]
        revalidate: Option<String>,
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

/// `resume-entry` の操作。いずれも `resume.json` は `state_file` と同じディレクトリに置く。
#[derive(Subcommand)]
enum ResumeEntryAction {
    /// レート制限で中断したセッションを保存する
    Save {
        /// エージェント名
        agent: String,
        /// 処理したディレクトリ
        directory: PathBuf,
        /// state.json のパス
        state_file: PathBuf,
        /// 中断した実行の jsonl（session_id を取り出す）
        #[arg(long)]
        jsonl: PathBuf,
        /// タスクの元のプロンプト（再開してよいかの照合に使うハッシュを取る）
        #[arg(long)]
        prompt_file: PathBuf,
    },
    /// 再開できなかったセッションの記録を消す（保存済みの ID と一致する場合だけ）
    Forget {
        agent: String,
        directory: PathBuf,
        state_file: PathBuf,
        #[arg(long)]
        session: String,
    },
    /// 再開したセッションの失敗を数える（上限に達したら記録を捨てる）
    Fail {
        agent: String,
        directory: PathBuf,
        state_file: PathBuf,
        #[arg(long)]
        session: String,
    },
}

/// `resume-entry` を実行する。表示はワーカーペインに出る（stdout）。
fn run_resume_entry(action: ResumeEntryAction) -> Result<()> {
    match action {
        ResumeEntryAction::Save {
            agent,
            directory,
            state_file,
            jsonl,
            prompt_file,
        } => {
            let path = resume::resume_path(&state_file);
            let entry = resume::save_from_jsonl(&path, &agent, &directory, &jsonl, &prompt_file)?;
            let after = entry
                .retry_after
                .map(|at| {
                    format!(
                        " after {}",
                        at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")
                    )
                })
                .unwrap_or_default();
            println!(
                "{}",
                format!(
                    "  ↻ Saved session {} — the next run resumes it{after}",
                    entry.short_id()
                )
                .cyan()
            );
        }
        ResumeEntryAction::Forget {
            agent,
            directory,
            state_file,
            session,
        } => {
            resume::forget_atomic(
                &resume::resume_path(&state_file),
                &agent,
                &directory,
                Some(&session),
            )?;
        }
        ResumeEntryAction::Fail {
            agent,
            directory,
            state_file,
            session,
        } => {
            let outcome = resume::record_failure_atomic(
                &resume::resume_path(&state_file),
                &agent,
                &directory,
                &session,
            )?;
            let short = resume::short_session_id(&session);
            match outcome {
                resume::FailureOutcome::Counted(n) => println!(
                    "{}",
                    format!(
                        "  ↻ Resuming session {short} failed ({n}/{}); the next run tries it again",
                        resume::MAX_FAILED_RESUMES
                    )
                    .yellow()
                ),
                resume::FailureOutcome::Dropped => println!(
                    "{}",
                    format!(
                        "  ↻ Gave up resuming session {short} after {} failures; the next run starts a new session",
                        resume::MAX_FAILED_RESUMES
                    )
                    .yellow()
                ),
                resume::FailureOutcome::Missing => {}
            }
        }
    }
    Ok(())
}

fn parse_positive_usize(value: &str, option: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("無効な数値です: {value}"))?;
    if parsed == 0 {
        return Err(format!("{option} には 1 以上を指定してください"));
    }
    Ok(parsed)
}

fn parse_positive_limit(value: &str) -> Result<usize, String> {
    parse_positive_usize(value, "limit")
}

fn parse_positive_workers(value: &str) -> Result<usize, String> {
    parse_positive_usize(value, "workers")
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
        // 停止シグナルを書けなかった場合だけ非ゼロ。タスクスクリプトはこの終了コードを
        // 見て、後続を止められない状態のままワーカーが走り続けるのを防ぐ。
        let code = format_stream::run(raw_output.as_deref(), stop_file.as_deref(), *threshold)?;
        std::process::exit(code);
    }

    if let Commands::Mark {
        agent,
        directory,
        state_file,
    } = command
    {
        state::mark_completed_atomic(&state_file, &agent, &directory)?;
        // 完了したので、中断セッションの記録（あれば）はもう要らない。処理済み記録を先に
        // 確定させてから消すので、間で落ちても二重処理にはならない（次回は処理済みとして
        // スキップされ、残った記録は使われない）。消せなくても完了の記録は済んでいるため、
        // 警告に留めてタスクを失敗にしない。
        if let Err(e) =
            resume::forget_atomic(&resume::resume_path(&state_file), &agent, &directory, None)
        {
            eprintln!(
                "{}: failed to clear the resume entry for {}: {:#}",
                "Warning".yellow(),
                directory.display(),
                e
            );
        }
        return Ok(());
    }

    if let Commands::ResumeEntry { action } = command {
        return run_resume_entry(action);
    }

    if let Commands::ClassifyResult { jsonl } = &command {
        let class = classify::classify_jsonl(jsonl);
        if let Some(msg) = class.message() {
            println!("{msg}");
        }
        std::process::exit(class.exit_code());
    }

    if let Commands::GateClaim {
        stop_file,
        queue_dir,
        deadline_epoch,
        revalidate,
    } = &command
    {
        let outcome = rate_control::run_gate_claim(
            stop_file,
            queue_dir,
            *deadline_epoch,
            revalidate.as_deref(),
        )
        .await?;
        // claim できた番号だけを stdout に出す（進捗や警告は stderr）。ワーカーは
        // これをタスクスクリプト名の組み立てに使い、終了コードで分岐する。
        if let rate_control::ClaimOutcome::Claimed(index) = &outcome {
            println!("{index}");
        }
        std::process::exit(outcome.exit_code());
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
    let no_resume = cli.no_resume;
    let limit = if cli.no_limit {
        Some(usize::MAX)
    } else {
        cli.limit
    };
    let public_only = cli.public_only;
    let interactive = cli.interactive;
    let workers = cli.workers;
    match command {
        Commands::Status => {
            let runtime_agents = config.expand_runtime_agents()?;
            let resolver = usage::ScheduleResolver::load(&config).await;
            display::print_status(&runtime_agents, &resolver)?;
        }
        Commands::Run { paths } => {
            // 検証はここで行う。共通部で解決すると、処理済み判定を使わない
            // `status` / `clean` まで `--dedup-scope global` でエラーになる。
            let dedup_scope = resolve_dedup_scope(&config, cli.dedup_scope)?;
            run(RunOptions {
                config,
                config_path,
                agent_name,
                dry_run,
                fresh,
                no_resume,
                limit_override: limit,
                workers_override: workers,
                public_only,
                interactive,
                dedup_scope,
                force_paths: paths,
            })
            .await?;
        }
        Commands::List { paths } => {
            let dedup_scope = resolve_dedup_scope(&config, cli.dedup_scope)?;
            list(ListOptions {
                config,
                config_path,
                agent_name,
                fresh,
                no_resume,
                public_only,
                dedup_scope,
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
        Commands::ResumeEntry { .. } => unreachable!(),
        Commands::UsageGate { .. } | Commands::GateClaim { .. } => unreachable!(),
    }

    Ok(())
}

struct RunOptions {
    config: config::Config,
    config_path: PathBuf,
    agent_name: Option<String>,
    dry_run: bool,
    fresh: bool,
    /// 保存済みの中断セッションを今回は再開しない（新しい中断は保存する）。
    no_resume: bool,
    limit_override: Option<usize>,
    workers_override: Option<usize>,
    public_only: bool,
    /// 実行対象と実行順を TUI で確定してから実行する。
    interactive: bool,
    dedup_scope: config::DedupScope,
    force_paths: Vec<PathBuf>,
}

struct ListOptions {
    config: config::Config,
    config_path: PathBuf,
    agent_name: Option<String>,
    fresh: bool,
    no_resume: bool,
    public_only: bool,
    dedup_scope: config::DedupScope,
    force_paths: Vec<PathBuf>,
}

/// 実行時に適用する dedup scope を決める。CLI 指定（`--dedup-scope`）は設定値より優先。
///
/// 共有 scope は「実行中のエージェントの前回リセット時刻」をカットオフに使えないため、
/// `skip_within` が無ければここで弾く。設定側は `Config::validate` が同じ検査をするが、
/// CLI で `agent` から `global` へ引き上げた場合はそこを通らないので二重に置く。
fn resolve_dedup_scope(
    config: &config::Config,
    override_scope: Option<config::DedupScope>,
) -> Result<config::DedupScope> {
    let scope = override_scope.unwrap_or(config.settings.dedup_scope);
    if scope.is_shared() && config.settings.skip_within.is_none() {
        anyhow::bail!(
            "--dedup-scope {} requires settings.skip_within in the config (the per-agent reset cutoff cannot be shared across agents)",
            scope.label()
        );
    }
    Ok(scope)
}

async fn list(opts: ListOptions) -> Result<()> {
    let ListOptions {
        config,
        config_path,
        agent_name,
        fresh,
        no_resume,
        public_only,
        dedup_scope,
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
    let run_state = state::State::load(&state_file)?;
    let (mut targets, skipped) = if fresh || !force_paths.is_empty() {
        (targets, SkipSummary::default())
    } else {
        filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime_agents,
            &config,
            &sched,
            dedup_scope,
        )
    };

    let resumes = resolve_resumes(
        &targets,
        agent,
        &runtime_agents,
        &run_state,
        &resume::ResumeStore::load_or_warn(&resume::resume_path(&state_file)),
        dedup_scope,
        resume_disabled_reason(&config, agent, fresh, no_resume).as_deref(),
    );

    // 最終ファイル変更日時が古い順に並べ替える（force_paths 指定時は CLI 指定順を尊重）
    let modified = if force_paths.is_empty() {
        let dirs: Vec<_> = targets.iter().map(|t| t.directory.clone()).collect();
        let modified = scanner::repo_last_modified_map(&dirs).await;
        sort_by_least_recent(
            &mut targets,
            &modified,
            public_first_enabled(&config),
            &resumable_directories(&resumes),
        );
        modified
    } else {
        HashMap::new()
    };

    display::print_targets(&targets, &modified, &resumes);

    if public_filtered > 0 {
        println!(
            "  {} {} targets (non-public)",
            "Filtered:".dimmed(),
            public_filtered
        );
    }

    print_skip_summary(&skipped, dedup_scope, &config);

    if public_filtered > 0 || skipped.total > 0 {
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
        no_resume,
        limit_override,
        workers_override,
        public_only,
        interactive,
        dedup_scope,
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
    let run_state = state::State::load(&state_file)?;
    let (mut targets, skipped) = if fresh || !force_paths.is_empty() {
        (targets, SkipSummary::default())
    } else {
        filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime_agents,
            &config,
            &sched,
            dedup_scope,
        )
    };

    // レート制限で中断したセッションのうち、今回再開するものを決める。並べ替えより前に
    // 判定するのは、再開するターゲットを各グループの先頭へ寄せるため。
    let resumes = resolve_resumes(
        &targets,
        agent,
        &runtime_agents,
        &run_state,
        &resume::ResumeStore::load_or_warn(&resume::resume_path(&state_file)),
        dedup_scope,
        resume_disabled_reason(&config, agent, fresh, no_resume).as_deref(),
    );
    let resumable = resumable_directories(&resumes);

    // 最終ファイル変更日時が古い順に並べ替えてから limit を適用する。
    // これをしないと limit で毎回リストの先頭だけが選ばれ、末尾のリポジトリに到達しない。
    // force_paths 指定時は CLI 指定順を尊重する。
    let modified = if force_paths.is_empty() {
        let dirs: Vec<_> = targets.iter().map(|t| t.directory.clone()).collect();
        let modified = scanner::repo_last_modified_map(&dirs).await;
        sort_by_least_recent(
            &mut targets,
            &modified,
            public_first_enabled(&config),
            &resumable,
        );
        modified
    } else {
        HashMap::new()
    };

    // 制限適用: CLIオプションが設定値を上書き
    let limit = limit_override.unwrap_or(config.settings.limit);
    // ワーカー数: CLI オプションが設定値を上書き（TUI のヘッダーにも出す）
    let parallelism = workers_override.unwrap_or(config.settings.parallelism);

    let (targets, truncated) = if interactive && !targets.is_empty() {
        // TUI では候補を limit で切らず、先頭 limit 件を初期選択にして全候補を見せる。
        // そのまま決定すれば非対話実行と同じ対象になり、必要なら limit の外側
        // （11 件目以降）も選べる。確定後に limit を再適用すると、選んだのに実行
        // されないターゲットが黙って落ちるため、選択結果をそのまま実行対象にする。
        let ctx = tui::RunContext {
            agent_name: sched.agent_name.clone(),
            reset_in: display::format_duration(sched.time_until_reset),
            schedule_source: sched.source.label().to_string(),
            workers: parallelism,
        };
        match tui::select_targets(targets, &modified, &resumable, limit, &ctx)? {
            tui::Outcome::Confirmed(selected) => (selected, 0),
            tui::Outcome::Cancelled => {
                println!("{}", "Cancelled - nothing was executed.".yellow());
                return Ok(());
            }
        }
    } else {
        let truncated = targets.len().saturating_sub(limit);
        (targets.into_iter().take(limit).collect(), truncated)
    };

    display::print_targets(&targets, &modified, &resumes);

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

    print_skip_summary(&skipped, dedup_scope, &config);

    if public_filtered > 0 || skipped.total > 0 {
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
    let resume_options = resume_options(&config, agent, resumes)?;
    let plan = executor::build_plan(agent, targets, ai_usage_command, resume_options);
    executor::print_plan(&plan, parallelism);

    if dry_run {
        println!(
            "{}",
            "Dry run mode - no commands will be executed.".yellow()
        );
        return Ok(());
    }

    let reset_info = sched.next_reset.format("%Y/%m/%d %H:%M").to_string();
    // デッドラインは起動時の残り時間ではなく、実行直前の現在時刻から引き直す。
    // TUI での選択は人手なので分単位で止まり、スキャン（gh CLI / git ls-files）にも
    // 時間がかかる。起動時の値をそのまま渡すと、そこに費やした分だけモニターの
    // デッドラインが後ろへずれ、実際のリセット後まで新規タスクを開始してしまう。
    let time_until_reset = remaining_until(sched.next_reset);
    let report_dir = resolve_report_dir(&config.settings);
    executor::execute_plan_tmux(
        plan,
        parallelism,
        time_until_reset,
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

/// レポート出力先を解決する。設定値は必ず絶対パスへ正規化する。
///
/// 相対パスのまま返すと、レポートディレクトリの作成（`executor` 側。プロセスの cwd で
/// 解決される）と、そこへ書き込むタスクスクリプトの `tee` / `--raw-output`
/// （対象リポジトリへ `cd` した後で解決される）が別ディレクトリを指す。結果として
/// ログのパイプラインが `No such file or directory` で失敗し、全ターゲットが
/// `failed-N` になって `state.json` に 1 件も記録されない。`cleanup` も別の場所を
/// 見に行くことになる。`report_dir = "reports"` のような素直な設定で踏むため、
/// 読み込み時点で絶対パスへ寄せる。
fn resolve_report_dir(settings: &config::Settings) -> PathBuf {
    if let Some(ref dir) = settings.report_dir {
        // current_dir() の取得に失敗する環境（cwd が削除済み等）では、
        // 展開だけした従来の値へフォールバックする。
        return config::resolve_directory(dir).unwrap_or_else(|_| {
            let expanded = shellexpand::tilde(dir);
            PathBuf::from(expanded.as_ref())
        });
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("Documents")
        .join("token-burn")
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
/// 指定時刻までの残り時間。すでに過ぎている場合は 0（デッドライン到達扱い）。
///
/// `AgentSchedule::time_until_reset` は起動時のスナップショットなので、スキャンや TUI 操作に
/// かかった時間の分だけ実際より長くなる。実行直前にここで引き直す。
fn remaining_until(reset: DateTime<chrono::FixedOffset>) -> std::time::Duration {
    (reset - chrono::Local::now().fixed_offset())
        .to_std()
        .unwrap_or(std::time::Duration::ZERO)
}

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
///
/// 中断セッションを再開するターゲット（`resumable`）は、同じグループ内の先頭に寄せる。
/// 中断したリポジトリは直前まで編集されていて変更日時が最も新しいので、古い順だけで
/// 並べると末尾へ回り、`limit` から外れて再開されないまま放置される。一方で明示的な
/// `defer` と `public_first` は再開より優先する（再開のためにユーザーの設定を覆さない）。
fn sort_by_least_recent(
    targets: &mut [scanner::ResolvedTarget],
    modified: &HashMap<PathBuf, DateTime<Utc>>,
    public_first: bool,
    resumable: &HashSet<PathBuf>,
) {
    targets.sort_by_cached_key(|t| {
        let last_modified = modified.get(&t.directory).copied();
        // public_first = false のときは全要素が None になり、可視性はキーとして効かない。
        let visibility = public_first.then(|| t.visibility.clone());
        let fresh_start = !resumable.contains(&t.directory);
        (
            t.defer,
            visibility,
            fresh_start,
            last_modified.is_none(),
            last_modified,
        )
    });
}

/// 今回は保存済みの中断セッションを再開しない理由。`None` なら再開してよい。
///
/// `--fresh` は「保存済み状態を無視して最初から」という意味なので、処理済み履歴と
/// 同じく中断セッションも見ない。`--no-resume` は処理済みスキップを保ったまま再開だけを
/// 止める。どちらも今回の再開を止めるだけで、新しい中断は保存する。
fn resume_disabled_reason(
    config: &config::Config,
    agent: &config::RuntimeAgent,
    fresh: bool,
    no_resume: bool,
) -> Option<String> {
    if fresh {
        return Some("--fresh".to_string());
    }
    if no_resume {
        return Some("--no-resume".to_string());
    }
    if !config.settings.resume_interrupted {
        return Some("settings.resume_interrupted = false".to_string());
    }
    executor::resume_support(agent).unsupported_reason()
}

/// 保存済みの中断セッションを、ターゲットごとに再開するかどうか判定する。
///
/// 記録はエージェント展開名ごとに引く（transcript はそのアカウントの config dir にあり、
/// 別アカウントからは再開できない）。処理済み判定と同じ dedup scope で「中断より新しい
/// 処理記録」を探し、あれば古いと見なす（共有 scope で別エージェントが仕上げた等）。
/// 記録の無いターゲットは結果に含めない。
fn resolve_resumes(
    targets: &[scanner::ResolvedTarget],
    agent: &config::RuntimeAgent,
    runtime_agents: &[config::RuntimeAgent],
    run_state: &state::State,
    store: &resume::ResumeStore,
    scope: config::DedupScope,
    disabled: Option<&str>,
) -> HashMap<PathBuf, resume::ResumeDecision> {
    let peers = dedup_peers(scope, agent, runtime_agents);
    targets
        .iter()
        .filter_map(|target| {
            let entry = store.get(&agent.name, &target.directory)?;
            let decision = match disabled {
                Some(why) => resume::ResumeDecision::Skip {
                    session_id: entry.session_id.clone(),
                    reason: resume::SkipReason::Disabled(why.to_string()),
                },
                None => {
                    let latest = run_state
                        .last_processed_in_scope(&target.directory, |name| peers.contains(name));
                    resume::decide(entry, &target.prompt, latest.as_ref())
                }
            };
            Some((target.directory.clone(), decision))
        })
        .collect()
}

/// 再開すると判定したターゲットのディレクトリ。
fn resumable_directories(resumes: &HashMap<PathBuf, resume::ResumeDecision>) -> HashSet<PathBuf> {
    resumes
        .iter()
        .filter(|(_, decision)| decision.resume_entry().is_some())
        .map(|(dir, _)| dir.clone())
        .collect()
}

/// 実行計画へ渡す再開の設定を組み立てる。
///
/// 継続プロンプトは再開するタスクがあるときだけ解決する。使わない実行で `[prompts] resume`
/// の `.md` が読めないことを理由に止めない。
fn resume_options(
    config: &config::Config,
    agent: &config::RuntimeAgent,
    resumes: HashMap<PathBuf, resume::ResumeDecision>,
) -> Result<executor::ResumeOptions> {
    let sessions: HashMap<PathBuf, resume::ResumeEntry> = resumes
        .into_iter()
        .filter_map(|(dir, decision)| match decision {
            resume::ResumeDecision::Resume(entry) => Some((dir, entry)),
            resume::ResumeDecision::Skip { .. } => None,
        })
        .collect();
    let prompt = if sessions.is_empty() {
        String::new()
    } else {
        config.resume_prompt()?
    };
    Ok(executor::ResumeOptions {
        save_interrupted: config.settings.resume_interrupted
            && executor::resume_support(agent) == executor::ResumeSupport::Supported,
        sessions,
        prompt,
    })
}

/// いずれかの `[[scan]]` が `public_first` を有効にしているか。
/// 有効な scan が 1 つでもあれば、最終的な実行順も可視性でグループ化する。
fn public_first_enabled(config: &config::Config) -> bool {
    config.scan.iter().any(|scan| scan.public_first)
}

/// 処理済みスキップの内訳。
///
/// 共有 scope では自分以外のエージェントの履歴が理由になり得るため、件数だけでなく
/// 「どのエージェントの記録で弾かれたか」を持つ。件数しか出さないと、統合が効いた結果
/// スキップされたのか、ターゲット探索そのものが壊れているのか区別できない。
#[derive(Debug, Default)]
struct SkipSummary {
    total: usize,
    /// スキップ理由になったエージェント展開名 → 件数。
    by_agent: BTreeMap<String, usize>,
}

impl SkipSummary {
    fn record(&mut self, agent_name: &str) {
        self.total += 1;
        *self.by_agent.entry(agent_name.to_string()).or_default() += 1;
    }

    /// `codex=5, codex-alt=2` 形式の内訳。件数降順、同数はエージェント名昇順。
    fn breakdown(&self) -> String {
        let mut entries: Vec<_> = self.by_agent.iter().collect();
        entries.sort_by(|(a_name, a_count), (b_name, b_count)| {
            b_count.cmp(a_count).then_with(|| a_name.cmp(b_name))
        });
        entries
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// 処理済みスキップの内訳を表示する。
///
/// scope と窓を必ず併記する。件数だけでは「統合が効いてスキップされた」のか
/// 「ターゲット探索が壊れて候補が消えた」のかを実行ログから切り分けられない。
fn print_skip_summary(skipped: &SkipSummary, scope: config::DedupScope, config: &config::Config) {
    if skipped.total == 0 {
        return;
    }
    let window = config
        .settings
        .skip_within
        .as_deref()
        .unwrap_or("since last reset");
    println!(
        "  {} {} targets (already processed; scope: {}, window: {})",
        "Skipped:".dimmed(),
        skipped.total,
        scope.label(),
        window
    );
    // 自分の履歴しか見ない agent scope では内訳が自明なので出さない。
    if scope.is_shared() {
        println!("    {} {}", "by agent:".dimmed(), skipped.breakdown());
    }
}

/// dedup scope に応じた「処理済み履歴を共有するエージェント」の判定。
enum DedupPeers {
    /// 全エージェント。`state.json` にあって現在の設定に無いもの（削除・改名済み）も含む。
    All,
    /// 列挙した展開名のみ。
    Named(HashSet<String>),
}

impl DedupPeers {
    fn contains(&self, agent_name: &str) -> bool {
        match self {
            DedupPeers::All => true,
            DedupPeers::Named(names) => names.contains(agent_name),
        }
    }
}

/// 実行中のエージェントと同じ処理済み履歴を共有する相手を決める。
///
/// `provider` scope では現在の `RuntimeAgent` 一覧からしか provider を復元できない。
/// `state.json` は provider を持たないため、設定から消えた・改名されたエージェントの記録は
/// provider 不明として対象外になる。別 provider の履歴を誤って引き当てて実行を握り潰すより、
/// 取りこぼして再処理する方が安全なため、この向きに倒している。
fn dedup_peers(
    scope: config::DedupScope,
    agent: &config::RuntimeAgent,
    runtime_agents: &[config::RuntimeAgent],
) -> DedupPeers {
    match scope {
        config::DedupScope::Global => DedupPeers::All,
        config::DedupScope::Agent => DedupPeers::Named(HashSet::from([agent.name.clone()])),
        config::DedupScope::Provider => {
            // provider 未設定のエージェントはグルーピングの手掛かりが無いので自分自身のみ。
            // 空文字（`provider = ""`）も未設定と同じに扱う。有効な provider として
            // 扱うと、空文字を書いたエージェント同士が互いの履歴でスキップし合う。
            let Some(provider) = agent
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|provider| !provider.is_empty())
            else {
                return DedupPeers::Named(HashSet::from([agent.name.clone()]));
            };
            // 照合は**両辺**を trim する。左辺だけ trim していた頃は、
            // `provider = "claude "` のように空白付きで書いたエージェントが自分自身を
            // 照合から落とし（`"claude " != "claude"`）、同じ provider の相手も居なければ
            // peers が空集合になって重複排除が無言で無効化されていた。毎回同じ先頭
            // `limit` 件を再処理してクォータを二重消費する一方、ログには
            // 「スキップ 0 件」としか出ないので原因を追えない。
            let names = runtime_agents
                .iter()
                .filter(|candidate| {
                    candidate
                        .provider
                        .as_deref()
                        .map(str::trim)
                        .is_some_and(|p| p.eq_ignore_ascii_case(provider))
                })
                .map(|candidate| candidate.name.clone())
                // 実行中のエージェントは provider の表記に関わらず必ず自分の履歴を見る。
                // 書き込み側は常に実行したエージェント名のキーへ記録するため、参照側が
                // 自分を外すと「自分が処理したのに未処理」と判定してしまう。
                .chain(std::iter::once(agent.name.clone()))
                .collect();
            DedupPeers::Named(names)
        }
    }
}

fn filter_by_state(
    targets: Vec<scanner::ResolvedTarget>,
    run_state: &state::State,
    agent: &config::RuntimeAgent,
    runtime_agents: &[config::RuntimeAgent],
    config: &config::Config,
    sched: &schedule::AgentSchedule,
    scope: config::DedupScope,
) -> (Vec<scanner::ResolvedTarget>, SkipSummary) {
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

    let peers = dedup_peers(scope, agent, runtime_agents);

    let mut kept = Vec::new();
    let mut skipped = SkipSummary::default();
    for target in targets {
        if let Some(last) =
            run_state.last_processed_in_scope(&target.directory, |name| peers.contains(name))
            && last.at >= cutoff
        {
            skipped.record(&last.agent_name);
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
            dedup_scope: crate::config::DedupScope::Agent,
            resume_interrupted: true,
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
            dedup_scope: crate::config::DedupScope::Agent,
            resume_interrupted: true,
        };
        let dir = resolve_report_dir(&settings);
        // チルダが展開されていることを確認
        assert!(!dir.to_string_lossy().contains('~'));
        assert!(dir.to_string_lossy().ends_with("custom-reports"));
    }

    #[test]
    fn resolve_report_dir_absolutizes_relative_path() {
        // 相対パスのまま返すと、レポートディレクトリの作成（プロセスの cwd 基準）と
        // タスクスクリプトの tee / --raw-output（対象リポジトリへ cd した後の基準）が
        // 別の場所を指し、ログのパイプラインが必ず失敗して全ターゲットが failed になる。
        let tmp = tempfile::TempDir::new().expect("temp dir should be created");
        let _cwd_guard = crate::test_support::CwdGuard::switch_to(tmp.path());

        let settings = config::Settings {
            parallelism: 1,
            skip_within: None,
            report_dir: Some("reports".to_string()),
            cleanup_after: None,
            limit: 10,
            rate_limit_threshold: 95,
            dedup_scope: crate::config::DedupScope::Agent,
            resume_interrupted: true,
        };
        let dir = resolve_report_dir(&settings);
        assert!(
            dir.is_absolute(),
            "相対パスは絶対パスへ正規化されるべき: {dir:?}"
        );
        assert!(dir.ends_with("reports"), "{dir:?}");
        assert_eq!(
            dir,
            std::env::current_dir()
                .expect("cwd should be available")
                .join("reports")
        );
    }

    #[test]
    fn resolve_report_dir_normalizes_relative_segments() {
        // `./` や `..` を含む設定値も 1 つの絶対パスへ畳み込む。
        let tmp = tempfile::TempDir::new().expect("temp dir should be created");
        let _cwd_guard = crate::test_support::CwdGuard::switch_to(tmp.path());

        let settings = config::Settings {
            parallelism: 1,
            skip_within: None,
            report_dir: Some("./nested/../logs".to_string()),
            cleanup_after: None,
            limit: 10,
            rate_limit_threshold: 95,
            dedup_scope: crate::config::DedupScope::Agent,
            resume_interrupted: true,
        };
        let dir = resolve_report_dir(&settings);
        assert_eq!(
            dir,
            std::env::current_dir()
                .expect("cwd should be available")
                .join("logs")
        );
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
                resume: None,
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

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            runtime_agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Agent,
        );
        assert_eq!(skipped.total, 1);
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
                resume: None,
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

        let (kept, skipped) = filter_by_state(
            targets,
            &empty_state,
            runtime_agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Agent,
        );
        assert_eq!(skipped.total, 0);
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

        sort_by_least_recent(&mut targets, &modified, true, &HashSet::new());

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

        sort_by_least_recent(&mut targets, &modified, true, &HashSet::new());

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

        sort_by_least_recent(&mut targets, &modified, true, &HashSet::new());

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

        sort_by_least_recent(&mut targets, &modified, true, &HashSet::new());

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

        sort_by_least_recent(&mut targets, &modified, true, &HashSet::new());

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

        sort_by_least_recent(&mut targets, &modified, false, &HashSet::new());

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

        sort_by_least_recent(&mut targets, &modified, false, &HashSet::new());

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

        sort_by_least_recent(&mut targets, &modified, false, &HashSet::new());

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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
                resume: None,
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
    fn cli_workers_rejects_zero() {
        let result = Cli::try_parse_from(["token-burn", "--workers", "0"]);
        assert!(result.is_err(), "workers=0 は CLI で拒否されるべき");
    }

    #[test]
    fn cli_workers_parses_long_and_short_forms() {
        let long = Cli::try_parse_from(["token-burn", "run", "--workers", "5"])
            .expect("--workers は run で受け付けられるべき");
        assert_eq!(long.workers, Some(5));

        // サブコマンド省略時（デフォルト run）でも global オプションとして解釈される
        let short = Cli::try_parse_from(["token-burn", "-w", "2"])
            .expect("-w はサブコマンド省略時も受け付けられるべき");
        assert_eq!(short.workers, Some(2));

        let absent = Cli::try_parse_from(["token-burn", "run"]).expect("run は解釈できるべき");
        assert_eq!(absent.workers, None, "未指定なら設定値を使うため None");
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
                resume: None,
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

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            runtime_agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Agent,
        );
        assert_eq!(
            skipped.total, 1,
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
                resume: None,
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

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            runtime_agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Agent,
        );

        assert_eq!(skipped.total, 1, "スケジュールのカットオフへ戻るべき");
        assert!(kept.is_empty());
    }

    /// dedup scope 検証用の設定。`agents` は (エージェント名, provider) の並び。
    fn dedup_test_config(
        agents: &[(&str, Option<&str>)],
        skip_within: Option<&str>,
    ) -> config::Config {
        config::Config {
            config_dir: std::path::PathBuf::from("."),
            settings: config::Settings {
                parallelism: 1,
                skip_within: skip_within.map(|s| s.to_string()),
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
                dedup_scope: config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "review".to_string(),
                resume: None,
            },
            agents: agents
                .iter()
                .map(|(name, provider)| config::Agent {
                    name: (*name).to_string(),
                    provider: provider.map(|p| p.to_string()),
                    command: vec!["echo".to_string()],
                    reset_weekday: Some("monday".to_string()),
                    reset_time: Some("09:00".to_string()),
                    timezone: Some("UTC".to_string()),
                    ..Default::default()
                })
                .collect(),
            scan: vec![],
            targets: vec![],
            ai_usage: None,
        }
    }

    /// (エージェント名, ディレクトリ) を「たった今処理した」状態にする。
    fn state_with(entries: &[(&str, &str)]) -> state::State {
        let mut run_state = state::State::default();
        for (agent, dir) in entries {
            run_state
                .agents
                .entry((*agent).to_string())
                .or_default()
                .insert((*dir).to_string(), chrono::Utc::now());
        }
        run_state
    }

    /// テスト対象のエージェントを展開名で取り出し、固定スケジュールとともに返す。
    fn dedup_runtime(
        conf: &config::Config,
        agent_name: &str,
    ) -> (Vec<config::RuntimeAgent>, schedule::AgentSchedule) {
        let runtime = conf.expand_runtime_agents().expect("expand");
        let agent = runtime
            .iter()
            .find(|a| a.name == agent_name)
            .unwrap_or_else(|| panic!("agent {agent_name} が展開されるべき"));
        let sched = schedule::calculate_fixed_reset(agent).expect("schedule");
        (runtime, sched)
    }

    /// dedup_scope = global では、別アカウント（別エージェント）の処理済み記録でもスキップする。
    /// アカウント A の codex で一巡したリポジトリを、アカウント B の codex が拾い直さないための挙動。
    #[test]
    fn filter_by_state_global_scope_shares_history_across_agents() {
        let conf = dedup_test_config(
            &[("codex", Some("codex")), ("codex-alt", Some("codex"))],
            Some("2d"),
        );
        let (runtime, sched) = dedup_runtime(&conf, "codex-alt");
        let agent = runtime.iter().find(|a| a.name == "codex-alt").unwrap();

        // 記録があるのは別エージェント (codex) の側だけ
        let run_state = state_with(&[("codex", "/tmp/repo-a")]);
        let targets = vec![target_at("repo-a", scanner::Visibility::Unknown, false)];

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Global,
        );

        assert!(
            kept.is_empty(),
            "他エージェントの処理済み記録でもスキップされるべき"
        );
        assert_eq!(skipped.total, 1);
        assert_eq!(
            skipped.by_agent.get("codex"),
            Some(&1),
            "どのエージェントの記録で弾いたかを残すべき"
        );
    }

    /// dedup_scope = agent（既定）は他エージェントの記録を一切見ない。
    #[test]
    fn filter_by_state_agent_scope_ignores_other_agents() {
        let conf = dedup_test_config(
            &[("codex", Some("codex")), ("codex-alt", Some("codex"))],
            Some("2d"),
        );
        let (runtime, sched) = dedup_runtime(&conf, "codex-alt");
        let agent = runtime.iter().find(|a| a.name == "codex-alt").unwrap();

        let run_state = state_with(&[("codex", "/tmp/repo-a")]);
        let targets = vec![target_at("repo-a", scanner::Visibility::Unknown, false)];

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Agent,
        );

        assert_eq!(kept.len(), 1, "自分の履歴に無いなら処理対象のまま残すべき");
        assert_eq!(skipped.total, 0);
    }

    /// dedup_scope = global でも窓（skip_within）は効く。窓の外の記録では弾かない。
    /// 「統合したのに再処理される」ときの切り分けに必要な境界。
    #[test]
    fn filter_by_state_global_scope_respects_skip_within_window() {
        let conf = dedup_test_config(
            &[("codex", Some("codex")), ("codex-alt", Some("codex"))],
            Some("2d"),
        );
        let (runtime, sched) = dedup_runtime(&conf, "codex-alt");
        let agent = runtime.iter().find(|a| a.name == "codex-alt").unwrap();

        // 別エージェントの記録はあるが 3 日前 = skip_within("2d") の外
        let mut run_state = state::State::default();
        run_state
            .agents
            .entry("codex".to_string())
            .or_default()
            .insert(
                "/tmp/repo-a".to_string(),
                chrono::Utc::now() - chrono::Duration::days(3),
            );
        let targets = vec![target_at("repo-a", scanner::Visibility::Unknown, false)];

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Global,
        );

        assert_eq!(kept.len(), 1, "窓の外の記録では弾かないべき");
        assert_eq!(skipped.total, 0);
    }

    /// dedup_scope = provider は同じ provider 同士でだけ共有する。
    #[test]
    fn filter_by_state_provider_scope_shares_within_same_provider() {
        let conf = dedup_test_config(
            &[
                ("codex", Some("codex")),
                ("codex-alt", Some("codex")),
                ("claude", Some("claude")),
            ],
            Some("2d"),
        );
        let (runtime, sched) = dedup_runtime(&conf, "codex-alt");
        let agent = runtime.iter().find(|a| a.name == "codex-alt").unwrap();

        // repo-a は同 provider の codex が、repo-b は別 provider の claude が処理済み
        let run_state = state_with(&[("codex", "/tmp/repo-a"), ("claude", "/tmp/repo-b")]);
        let targets = vec![
            target_at("repo-a", scanner::Visibility::Unknown, false),
            target_at("repo-b", scanner::Visibility::Unknown, false),
        ];

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Provider,
        );

        assert_eq!(skipped.total, 1);
        assert_eq!(skipped.by_agent.get("codex"), Some(&1));
        assert_eq!(
            kept.iter()
                .map(|t| t.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["repo-b"],
            "別 provider の履歴では弾かないべき"
        );
    }

    /// provider 未設定のエージェントは、provider scope でも自分の記録だけを見る。
    /// 手掛かりが無いのに他エージェントと同一視すると、無関係な履歴で実行を握り潰す。
    #[test]
    fn filter_by_state_provider_scope_without_provider_falls_back_to_self() {
        let conf = dedup_test_config(&[("codex", Some("codex")), ("legacy", None)], Some("2d"));
        let (runtime, sched) = dedup_runtime(&conf, "legacy");
        let agent = runtime.iter().find(|a| a.name == "legacy").unwrap();

        let run_state = state_with(&[("codex", "/tmp/repo-a")]);
        let targets = vec![target_at("repo-a", scanner::Visibility::Unknown, false)];

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Provider,
        );

        assert_eq!(kept.len(), 1);
        assert_eq!(skipped.total, 0);
    }

    /// global scope は state.json にしか残っていないエージェント名（削除・改名済み）も参照する。
    #[test]
    fn filter_by_state_global_scope_includes_agents_absent_from_config() {
        let conf = dedup_test_config(&[("codex", Some("codex"))], Some("2d"));
        let (runtime, sched) = dedup_runtime(&conf, "codex");
        let agent = &runtime[0];

        let run_state = state_with(&[("retired-agent", "/tmp/repo-a")]);
        let targets = vec![target_at("repo-a", scanner::Visibility::Unknown, false)];

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Global,
        );

        assert!(kept.is_empty());
        assert_eq!(skipped.by_agent.get("retired-agent"), Some(&1));
    }

    /// provider scope は provider を復元できないエージェント名（設定に無い）を対象外にする。
    #[test]
    fn filter_by_state_provider_scope_excludes_agents_absent_from_config() {
        let conf = dedup_test_config(&[("codex", Some("codex"))], Some("2d"));
        let (runtime, sched) = dedup_runtime(&conf, "codex");
        let agent = &runtime[0];

        let run_state = state_with(&[("retired-agent", "/tmp/repo-a")]);
        let targets = vec![target_at("repo-a", scanner::Visibility::Unknown, false)];

        let (kept, skipped) = filter_by_state(
            targets,
            &run_state,
            agent,
            &runtime,
            &conf,
            &sched,
            config::DedupScope::Provider,
        );

        assert_eq!(
            kept.len(),
            1,
            "provider を復元できない記録で弾くより取りこぼす方に倒すべき"
        );
        assert_eq!(skipped.total, 0);
    }

    /// provider の照合は大文字小文字を無視する（"Codex" と "codex" を別グループにしない）。
    #[test]
    fn dedup_peers_matches_provider_case_insensitively() {
        let conf = dedup_test_config(
            &[("codex", Some("codex")), ("codex-alt", Some("Codex"))],
            Some("2d"),
        );
        let runtime = conf.expand_runtime_agents().expect("expand");
        let agent = runtime.iter().find(|a| a.name == "codex").unwrap();

        let peers = dedup_peers(config::DedupScope::Provider, agent, &runtime);
        assert!(peers.contains("codex-alt"));
    }

    /// `provider = ""`（空文字）は未設定と同じに扱い、自分自身の記録だけを見る。
    /// 有効な provider として扱うと、空文字を書いたエージェント同士が互いの履歴で
    /// スキップし合い、片方が一巡したリポジトリをもう片方が処理できなくなる。
    #[test]
    fn dedup_peers_treats_blank_provider_as_unset() {
        let conf = dedup_test_config(&[("a1", Some("")), ("a2", Some("  "))], Some("2d"));
        let runtime = conf.expand_runtime_agents().expect("expand");
        let agent = runtime.iter().find(|a| a.name == "a1").unwrap();

        let peers = dedup_peers(config::DedupScope::Provider, agent, &runtime);
        assert!(peers.contains("a1"), "自分自身は含む");
        assert!(
            !peers.contains("a2"),
            "空 provider 同士をグルーピングしてはいけない"
        );
    }

    /// `provider` の前後空白は両辺で無視する。片側だけ trim していた頃は、
    /// `provider = "claude "` のように空白付きで書いたエージェントが**自分自身**を
    /// 照合から落とし、同じ provider の相手も居なければ peers が空集合になって
    /// 重複排除が無言で無効化されていた（毎回同じ先頭 limit 件を再処理し、
    /// スキップ 0 件としか出ないので原因も追えない）。
    #[test]
    fn dedup_peers_ignores_surrounding_whitespace_in_provider() {
        let conf = dedup_test_config(
            &[("claude", Some("claude ")), ("claude-alt", Some(" CLAUDE"))],
            Some("2d"),
        );
        let runtime = conf.expand_runtime_agents().expect("expand");
        let agent = runtime.iter().find(|a| a.name == "claude").unwrap();

        let peers = dedup_peers(config::DedupScope::Provider, agent, &runtime);
        assert!(peers.contains("claude"), "自分自身を必ず含む");
        assert!(
            peers.contains("claude-alt"),
            "空白違いは同じ provider として扱う"
        );
    }

    /// 実行中のエージェントは、provider の表記がどうであれ自分の記録を必ず見る。
    /// 書き込み側は常に実行したエージェント名のキーへ記録するため、参照側が
    /// 自分を外すと「自分が処理したのに未処理と判定する」状態になる。
    #[test]
    fn dedup_peers_always_includes_the_running_agent() {
        for provider in ["claude", "", "  ", " claude "] {
            let conf = dedup_test_config(&[("solo", Some(provider))], Some("2d"));
            let runtime = conf.expand_runtime_agents().expect("expand");
            let agent = runtime.iter().find(|a| a.name == "solo").unwrap();

            let peers = dedup_peers(config::DedupScope::Provider, agent, &runtime);
            assert!(
                peers.contains("solo"),
                "provider = {provider:?} で自分自身が外れている"
            );
        }
    }

    /// スキップ内訳は件数降順・同数はエージェント名昇順で安定して並ぶ。
    #[test]
    fn skip_summary_breakdown_orders_by_count_then_name() {
        let mut summary = SkipSummary::default();
        for _ in 0..2 {
            summary.record("zeta");
        }
        for _ in 0..2 {
            summary.record("alpha");
        }
        for _ in 0..5 {
            summary.record("codex");
        }

        assert_eq!(summary.total, 9);
        assert_eq!(summary.breakdown(), "codex=5, alpha=2, zeta=2");
    }

    /// --dedup-scope は設定値を上書きする（「今回は引き継がない」を実行ごとに選べる）。
    #[test]
    fn resolve_dedup_scope_cli_overrides_config() {
        let mut conf = dedup_test_config(&[("codex", Some("codex"))], Some("2d"));
        conf.settings.dedup_scope = config::DedupScope::Global;

        assert_eq!(
            resolve_dedup_scope(&conf, Some(config::DedupScope::Agent))
                .expect("CLI 指定は通るべき"),
            config::DedupScope::Agent
        );
        assert_eq!(
            resolve_dedup_scope(&conf, None).expect("未指定なら設定値"),
            config::DedupScope::Global
        );
    }

    /// 共有 scope を CLI で指定しても skip_within が無ければエラーにする。
    /// エージェント固有のリセット時刻を他エージェントの履歴へ当てはめないため。
    #[test]
    fn resolve_dedup_scope_shared_requires_skip_within() {
        let conf = dedup_test_config(&[("codex", Some("codex"))], None);

        let err = resolve_dedup_scope(&conf, Some(config::DedupScope::Global))
            .expect_err("skip_within 無しの共有 scope は拒否されるべき");
        assert!(err.to_string().contains("skip_within"), "{err}");

        assert!(
            resolve_dedup_scope(&conf, Some(config::DedupScope::Agent)).is_ok(),
            "agent scope は skip_within 無しでも従来どおり動くべき"
        );
    }

    /// CLI の --dedup-scope をパースできる。
    #[test]
    fn cli_parses_dedup_scope_override() {
        let cli = Cli::parse_from(["token-burn", "run", "--dedup-scope", "agent"]);
        assert_eq!(cli.dedup_scope, Some(config::DedupScope::Agent));

        let cli = Cli::parse_from(["token-burn", "run", "--dedup-scope", "global"]);
        assert_eq!(cli.dedup_scope, Some(config::DedupScope::Global));

        let cli = Cli::parse_from(["token-burn", "run"]);
        assert_eq!(cli.dedup_scope, None, "未指定なら設定値を使う");
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "default".to_string(),
                resume: None,
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "default".to_string(),
                resume: None,
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "default".to_string(),
                resume: None,
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
                    dedup_scope: crate::config::DedupScope::Agent,
                    resume_interrupted: true,
                },
                prompts: config::Prompts {
                    default: "default prompt".to_string(),
                    resume: None,
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "default prompt".to_string(),
                resume: None,
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
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: config::Prompts {
                default: "default prompt".to_string(),
                resume: None,
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

    // ─────────── 中断セッションの再開 ───────────

    const SESSION: &str = "0633fa18-8643-40f7-9106-a35330ed247d";

    /// 中断を 1 時間前に記録した resume.json を作り、読み込んで返す。
    fn store_with(entries: &[(&str, &str, &str)]) -> resume::ResumeStore {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join("resume.json");
        for (agent, dir, prompt) in entries {
            resume::save_atomic(
                &path,
                agent,
                std::path::Path::new(dir),
                resume::ResumeEntry {
                    session_id: SESSION.to_string(),
                    interrupted_at: (chrono::Local::now() - chrono::Duration::hours(1))
                        .fixed_offset(),
                    retry_after: None,
                    prompt_hash: resume::prompt_hash(prompt),
                    message: None,
                    log: None,
                    failed_resumes: 0,
                },
            )
            .expect("save");
        }
        resume::ResumeStore::load_or_warn(&path)
    }

    #[test]
    fn sort_by_least_recent_puts_resumable_targets_first_within_groups() {
        use scanner::Visibility;
        let mut targets = vec![
            target_at("stale", Visibility::Public, false),
            target_at("interrupted", Visibility::Public, false),
            target_at("private-interrupted", Visibility::Private, false),
            target_at("deferred-interrupted", Visibility::Public, true),
        ];
        // 中断したリポジトリは直前まで編集されていて変更日時が最も新しい
        let modified = modified_at(&[
            ("stale", 90),
            ("interrupted", 0),
            ("private-interrupted", 30),
            ("deferred-interrupted", 0),
        ]);
        let resumable: HashSet<PathBuf> = [
            "/tmp/interrupted",
            "/tmp/private-interrupted",
            "/tmp/deferred-interrupted",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();

        // 再開するものはグループの先頭へ寄せるが、defer と public_first は覆さない
        let mut public_first = targets.clone();
        sort_by_least_recent(&mut public_first, &modified, true, &resumable);
        let order: Vec<_> = public_first
            .iter()
            .map(|t| t.display_name.as_str())
            .collect();
        assert_eq!(
            order,
            vec![
                "interrupted",
                "stale",
                "private-interrupted",
                "deferred-interrupted"
            ]
        );

        // public_first が無ければ、再開するもの同士は変更日時の古い順
        sort_by_least_recent(&mut targets, &modified, false, &resumable);
        let order: Vec<_> = targets.iter().map(|t| t.display_name.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "private-interrupted",
                "interrupted",
                "stale",
                "deferred-interrupted"
            ]
        );
    }

    #[test]
    fn resolve_resumes_resumes_only_the_running_agents_sessions_with_the_same_prompt() {
        let conf = dedup_test_config(
            &[("claude", Some("claude")), ("claude-home", Some("claude"))],
            Some("2d"),
        );
        let (runtime, _) = dedup_runtime(&conf, "claude");
        let agent = runtime.iter().find(|a| a.name == "claude").unwrap();
        let store = store_with(&[
            ("claude", "/tmp/same-prompt", "review"),
            ("claude", "/tmp/edited-prompt", "old instructions"),
            // 別アカウントの transcript は再開できない
            ("claude-home", "/tmp/other-account", "review"),
        ]);
        let targets = vec![
            target_at("same-prompt", scanner::Visibility::Unknown, false),
            target_at("edited-prompt", scanner::Visibility::Unknown, false),
            target_at("other-account", scanner::Visibility::Unknown, false),
            target_at("never-interrupted", scanner::Visibility::Unknown, false),
        ];

        let resumes = resolve_resumes(
            &targets,
            agent,
            &runtime,
            &state::State::default(),
            &store,
            config::DedupScope::Agent,
            None,
        );

        assert!(matches!(
            resumes.get(std::path::Path::new("/tmp/same-prompt")),
            Some(resume::ResumeDecision::Resume(_))
        ));
        assert!(matches!(
            resumes.get(std::path::Path::new("/tmp/edited-prompt")),
            Some(resume::ResumeDecision::Skip {
                reason: resume::SkipReason::PromptChanged,
                ..
            })
        ));
        assert!(!resumes.contains_key(std::path::Path::new("/tmp/other-account")));
        assert!(!resumes.contains_key(std::path::Path::new("/tmp/never-interrupted")));
        assert_eq!(
            resumable_directories(&resumes),
            HashSet::from([PathBuf::from("/tmp/same-prompt")])
        );
    }

    /// 共有 scope で別エージェントが中断後に処理済みにしたら、そのセッションは古い。
    /// agent scope（既定）では他エージェントの記録を見ないので再開する。
    #[test]
    fn resolve_resumes_follows_the_dedup_scope_for_newer_processed_records() {
        let conf = dedup_test_config(
            &[("claude", Some("claude")), ("claude-home", Some("claude"))],
            Some("2d"),
        );
        let (runtime, _) = dedup_runtime(&conf, "claude");
        let agent = runtime.iter().find(|a| a.name == "claude").unwrap();
        let store = store_with(&[("claude", "/tmp/repo-a", "review")]);
        // 中断（1 時間前）より後に、別エージェントが処理した
        let run_state = state_with(&[("claude-home", "/tmp/repo-a")]);
        let targets = vec![target_at("repo-a", scanner::Visibility::Unknown, false)];

        let global = resolve_resumes(
            &targets,
            agent,
            &runtime,
            &run_state,
            &store,
            config::DedupScope::Global,
            None,
        );
        assert_eq!(
            global.get(std::path::Path::new("/tmp/repo-a")),
            Some(&resume::ResumeDecision::Skip {
                session_id: SESSION.to_string(),
                reason: resume::SkipReason::Superseded {
                    agent_name: "claude-home".to_string()
                },
            })
        );

        let agent_scope = resolve_resumes(
            &targets,
            agent,
            &runtime,
            &run_state,
            &store,
            config::DedupScope::Agent,
            None,
        );
        assert!(matches!(
            agent_scope.get(std::path::Path::new("/tmp/repo-a")),
            Some(resume::ResumeDecision::Resume(_))
        ));
    }

    #[test]
    fn resolve_resumes_reports_why_resuming_is_disabled() {
        let conf = dedup_test_config(&[("claude", Some("claude"))], None);
        let (runtime, _) = dedup_runtime(&conf, "claude");
        let agent = &runtime[0];
        let store = store_with(&[("claude", "/tmp/repo-a", "review")]);
        let targets = vec![target_at("repo-a", scanner::Visibility::Unknown, false)];

        let resumes = resolve_resumes(
            &targets,
            agent,
            &runtime,
            &state::State::default(),
            &store,
            config::DedupScope::Agent,
            Some("--no-resume"),
        );
        assert_eq!(
            resumes.get(std::path::Path::new("/tmp/repo-a")),
            Some(&resume::ResumeDecision::Skip {
                session_id: SESSION.to_string(),
                reason: resume::SkipReason::Disabled("--no-resume".to_string()),
            })
        );
        assert!(resumable_directories(&resumes).is_empty());
    }

    #[test]
    fn resume_disabled_reason_covers_flags_settings_and_agent_support() {
        let mut conf = dedup_test_config(&[("claude", Some("claude"))], None);
        let claude = config::RuntimeAgent {
            name: "claude".to_string(),
            command: vec!["claude".to_string()],
            ..Default::default()
        };
        assert_eq!(resume_disabled_reason(&conf, &claude, false, false), None);
        assert_eq!(
            resume_disabled_reason(&conf, &claude, true, false).as_deref(),
            Some("--fresh")
        );
        assert_eq!(
            resume_disabled_reason(&conf, &claude, false, true).as_deref(),
            Some("--no-resume")
        );
        let codex = config::RuntimeAgent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        assert_eq!(
            resume_disabled_reason(&conf, &codex, false, false).as_deref(),
            Some("not a claude agent")
        );
        conf.settings.resume_interrupted = false;
        assert_eq!(
            resume_disabled_reason(&conf, &claude, false, false).as_deref(),
            Some("settings.resume_interrupted = false")
        );
    }

    #[test]
    fn resume_options_resolve_the_prompt_only_when_something_is_resumed() {
        let mut conf = dedup_test_config(&[("claude", Some("claude"))], None);
        // 読めない .md を指していても、再開しない実行は止めない
        conf.prompts.resume = Some("missing-resume-prompt.md".to_string());
        let claude = config::RuntimeAgent {
            name: "claude".to_string(),
            command: vec!["claude".to_string()],
            ..Default::default()
        };
        let options = resume_options(&conf, &claude, HashMap::new()).expect("no prompt needed");
        assert!(options.sessions.is_empty());
        assert!(options.save_interrupted);

        let store = store_with(&[("claude", "/tmp/repo-a", "review")]);
        let entry = store
            .get("claude", std::path::Path::new("/tmp/repo-a"))
            .unwrap()
            .clone();
        let resumes = HashMap::from([(
            PathBuf::from("/tmp/repo-a"),
            resume::ResumeDecision::Resume(entry),
        )]);
        assert!(
            resume_options(&conf, &claude, resumes.clone()).is_err(),
            "再開するなら継続プロンプトが読めないことはエラー"
        );

        conf.prompts.resume = None;
        let options = resume_options(&conf, &claude, resumes).expect("default prompt");
        assert_eq!(options.prompt, resume::DEFAULT_RESUME_PROMPT);
        assert_eq!(options.sessions.len(), 1);

        // 再開に対応しないエージェントでは中断を保存しない
        let codex = config::RuntimeAgent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        assert!(
            !resume_options(&conf, &codex, HashMap::new())
                .unwrap()
                .save_interrupted
        );
    }

    #[test]
    fn cli_parses_no_resume() {
        let cli = Cli::try_parse_from(["token-burn", "run", "--no-resume"]).expect("parse");
        assert!(cli.no_resume);
        assert!(!cli.fresh);
    }
}
