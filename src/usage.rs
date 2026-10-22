use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local, Utc};
use colored::Colorize;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

use crate::config::{
    Config, RuntimeAgent, RuntimeAiUsage, StateWindowPolicy, UsageFallback, UsageWindowPolicy,
};
use crate::rate_control::{self, Decision, WindowObservation};
use crate::schedule::{AgentSchedule, ScheduleSource, UsageWindow, calculate_fixed_reset};

/// ai-usage --json のトップレベル出力。
#[derive(Debug, Deserialize)]
struct AiUsageOutput {
    #[serde(default)]
    accounts: Vec<AiUsageAccount>,
}

/// ai-usage --json の 1 アカウント（profile × provider）。
#[derive(Debug, Deserialize, Clone)]
struct AiUsageAccount {
    profile: String,
    provider: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    weekly: Option<UsageWindowData>,
    #[serde(default)]
    five_hour: Option<UsageWindowData>,
    #[serde(default)]
    error: Option<String>,
}

/// 各枠（weekly / five_hour）のリセット情報。
#[derive(Debug, Deserialize, Clone)]
struct UsageWindowData {
    /// ai-usage が返す実際の枠種別（`five_hour` / `daily` / `weekly` / `monthly` 等）。
    ///
    /// スロット名（`weekly` / `five_hour`）と一致しないことがある。実データでは
    /// antigravity が `five_hour` スロットに `kind:"daily"`（24 時間枠）を、pixellab が
    /// `weekly` スロットに `kind:"monthly"`（月次枠）を返す。周期の導出はスロット名では
    /// なくこちらを優先しないと `state_cutoff` が実際の枠開始点からずれる。
    #[serde(default)]
    kind: Option<String>,
    resets_at: Option<String>,
    /// 使用率（%）。usage-gate の閾値判定に使う。
    used_percent: Option<f64>,
}

impl UsageWindowData {
    /// この枠の 1 周期の長さ。`kind` が未知・欠損ならスロット名の既定値へ落とす。
    fn period(&self, slot: UsageWindow) -> chrono::Duration {
        match self.kind.as_deref() {
            Some("five_hour") => chrono::Duration::hours(5),
            Some("daily") => chrono::Duration::days(1),
            Some("weekly") => chrono::Duration::days(7),
            Some("monthly") => chrono::Duration::days(30),
            _ => slot.period(),
        }
    }
}

/// ai-usage --json の取得結果スナップショット。
#[derive(Debug, Clone)]
pub struct AiUsageSnapshot {
    accounts: Vec<AiUsageAccount>,
}

impl AiUsageSnapshot {
    /// (profile, provider) に一致するアカウントを探す。
    /// claude/codex は (profile, provider) で一意。antigravity は group_label 違いで
    /// 複数行になりうるが、その場合は最初の一致を採用する。
    fn find(&self, profile: &str, provider: &str) -> Option<&AiUsageAccount> {
        self.accounts
            .iter()
            .find(|a| a.profile == profile && a.provider == provider)
    }
}

/// ai-usage 連携の状態。
#[derive(Debug, Clone)]
pub enum UsageState {
    /// 連携無効（[ai_usage] 未設定 or enabled=false）。
    Disabled,
    /// 取得成功。
    Loaded(AiUsageSnapshot),
    /// 取得失敗（理由）。
    Failed(String),
}

/// スケジュール解決器。ai-usage --json を 1 プロセス 1 回だけ取得し、
/// RuntimeAgent ごとに ai-usage / fixed を統合してスケジュールを返す。
pub struct ScheduleResolver {
    usage: UsageState,
    state_window: StateWindowPolicy,
}

impl ScheduleResolver {
    /// ai-usage --json を（有効なら）1 回だけ実行してリゾルバを構築する。
    pub async fn load(config: &Config) -> Self {
        let Some(global) = config.ai_usage.as_ref() else {
            return Self {
                usage: UsageState::Disabled,
                state_window: StateWindowPolicy::Weekly,
            };
        };
        if !global.enabled {
            return Self {
                usage: UsageState::Disabled,
                state_window: global.state_window,
            };
        }
        let usage = match run_ai_usage(&global.command).await {
            Ok(snapshot) => UsageState::Loaded(snapshot),
            Err(e) => UsageState::Failed(e.to_string()),
        };
        Self {
            usage,
            state_window: global.state_window,
        }
    }

    /// ai-usage の取得に失敗したか（status 表示用）。
    pub fn failure(&self) -> Option<&str> {
        match &self.usage {
            UsageState::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// RuntimeAgent のスケジュールを解決する。
    /// `Ok(None)` は fallback=skip により候補から外れたことを表す。
    pub fn schedule_for(&self, agent: &RuntimeAgent) -> Result<Option<AgentSchedule>> {
        match &agent.ai_usage {
            None => Ok(Some(calculate_fixed_reset(agent)?)),
            Some(rt) => {
                let account = match &self.usage {
                    UsageState::Loaded(s) => s.find(&rt.profile, &rt.provider),
                    _ => None,
                };
                match account {
                    Some(acc) if acc.ok => match self.build_from_account(agent, acc) {
                        Ok(Some(sched)) => Ok(Some(sched)),
                        Ok(None) => self.fallback(agent, "ai-usage has no usable reset window"),
                        Err(e) => self.fallback(agent, &e.to_string()),
                    },
                    Some(acc) => {
                        let reason = acc
                            .error
                            .clone()
                            .unwrap_or_else(|| "ai-usage account not ok".to_string());
                        self.fallback(agent, &reason)
                    }
                    None => self.fallback(agent, &self.miss_reason(rt)),
                }
            }
        }
    }

    /// 全 RuntimeAgent のうち、最も近いリセットのものを選ぶ。
    pub fn select_nearest(&self, agents: &[RuntimeAgent]) -> Result<(usize, AgentSchedule)> {
        anyhow::ensure!(!agents.is_empty(), "No agents configured");
        let mut best: Option<(usize, AgentSchedule)> = None;
        for (i, agent) in agents.iter().enumerate() {
            if let Some(sched) = self.schedule_for(agent)? {
                let replace = match &best {
                    None => true,
                    Some((_, b)) => sched.time_until_reset < b.time_until_reset,
                };
                if replace {
                    best = Some((i, sched));
                }
            }
        }
        best.ok_or_else(|| {
            anyhow::anyhow!("No agent could be scheduled (all skipped via fallback=skip)")
        })
    }

    fn miss_reason(&self, rt: &RuntimeAiUsage) -> String {
        match &self.usage {
            UsageState::Failed(e) => format!("ai-usage command failed: {e}"),
            UsageState::Disabled => "ai-usage disabled".to_string(),
            UsageState::Loaded(_) => {
                format!("no ai-usage entry for ({}, {})", rt.profile, rt.provider)
            }
        }
    }

    fn fallback(&self, agent: &RuntimeAgent, reason: &str) -> Result<Option<AgentSchedule>> {
        match agent.fallback {
            UsageFallback::Fixed => {
                let mut sched = calculate_fixed_reset(agent).with_context(|| {
                    format!(
                        "ai-usage failed for '{}' ({}) and fixed fallback is unavailable",
                        agent.name, reason
                    )
                })?;
                sched.source = ScheduleSource::FixedFallback(reason.to_string());
                Ok(Some(sched))
            }
            UsageFallback::Skip => Ok(None),
            UsageFallback::Error => {
                anyhow::bail!(
                    "ai-usage resolution failed for '{}': {}",
                    agent.name,
                    reason
                )
            }
        }
    }

    fn build_from_account(
        &self,
        agent: &RuntimeAgent,
        acc: &AiUsageAccount,
    ) -> Result<Option<AgentSchedule>> {
        let chosen = match agent.window {
            UsageWindowPolicy::Weekly => acc.weekly.as_ref().map(|w| (UsageWindow::Weekly, w)),
            UsageWindowPolicy::FiveHour => {
                acc.five_hour.as_ref().map(|w| (UsageWindow::FiveHour, w))
            }
            UsageWindowPolicy::Nearest => nearest_window(acc),
        };
        let Some((window, data)) = chosen else {
            return Ok(None);
        };
        let Some(next_reset) = parse_resets_at(data) else {
            return Ok(None);
        };

        // state_cutoff の枠を決める。state_window=weekly なら weekly 基準を優先し、
        // weekly が無い場合だけ選択枠に落とす。deadline が five_hour 由来でも、
        // 処理済みカットオフは週次基準を保つのが安全。
        let (cutoff_anchor, cutoff_period) = match self.state_window {
            StateWindowPolicy::Weekly => acc
                .weekly
                .as_ref()
                .and_then(|w| parse_resets_at(w).map(|at| (at, w.period(UsageWindow::Weekly))))
                .unwrap_or((next_reset, data.period(window))),
            StateWindowPolicy::Selected => (next_reset, data.period(window)),
        };
        // 周期は枠データの `kind` から求める。スロット名で決め打ちすると、実データの
        // `five_hour` スロットに入る 24 時間枠（`kind:"daily"`）を 5 時間として扱い、
        // カットオフ（= 直前の枠の開始点）が未来へ飛ぶ。すると `filter_by_state` の
        // `last >= cutoff` が恒偽になって処理済みフィルタが黙って無効化され、毎回同じ
        // 先頭ターゲットだけを再処理し続ける。
        let state_cutoff = cutoff_anchor - cutoff_period;

        let time_until_reset = (next_reset.with_timezone(&Utc) - Utc::now())
            .to_std()
            .unwrap_or(Duration::from_secs(0));

        Ok(Some(AgentSchedule {
            agent_name: agent.name.clone(),
            next_reset,
            state_cutoff,
            time_until_reset,
            source: ScheduleSource::AiUsage(window),
        }))
    }
}

/// weekly / five_hour のうち、resets_at が最も近い方を選ぶ。
/// deadline 算出に使う resets_at と同じ基準で選択し、選択枠と締切の不整合を防ぐ
/// （resets_in_seconds は resets_at と独立に欠損し得るため選択基準には使わない）。
/// resets_at がパースできない枠は候補から除外する（採用すると build_from_account が
/// 丸ごと None を返し、スケジュール解決全体が失敗してしまうため）。
fn nearest_window(acc: &AiUsageAccount) -> Option<(UsageWindow, &UsageWindowData)> {
    [
        acc.weekly.as_ref().map(|w| (UsageWindow::Weekly, w)),
        acc.five_hour.as_ref().map(|w| (UsageWindow::FiveHour, w)),
    ]
    .into_iter()
    .flatten()
    .filter_map(|(win, w)| parse_resets_at(w).map(|r| (win, w, r)))
    .min_by_key(|(_, _, r)| r.with_timezone(&Utc))
    .map(|(win, w, _)| (win, w))
}

fn parse_resets_at(data: &UsageWindowData) -> Option<DateTime<FixedOffset>> {
    let s = data.resets_at.as_deref()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local).fixed_offset())
}

/// ai-usage --json のハング検知タイムアウト。
/// ai-usage は通常数秒で完了するため、30 秒を超えたら異常と見なして子プロセスを
/// キルしエラーを返す。タイムアウト時は usage-gate の fail-closed 経路が発火し、
/// 後続タスクの停止・監視ペインの停止判定が正しく伝わる。
const AI_USAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// clap が使い方の誤り（未知のフラグ等）で終わるときの終了コード。
///
/// `--refresh` は 2026-10 版の ai-usage から使える。それより古い版は、token-burn が足した
/// フラグを知らずにこのコードで終わる。
const USAGE_ERROR_EXIT: i32 = 2;

/// 全アカウント分の `ai-usage --json` を、ai-usage の共有キャッシュを通さずに取り直すコマンド。
///
/// ai-usage は取得結果をプロセス間で既定 120 秒共有する（`--cache-ttl`）。手動リセット
/// （claude.ai の "Reset for free" や Codex の banked reset）の直後でもリセット前の値を返し
/// 得るので、リセット時刻や使用率で何かを決める取得（スケジュールの解決・実行開始時の判定）は
/// `--refresh` で取り直す。出力は statusline の表示用キャッシュにも使うため、[`live_command`]
/// と違って対象のアカウントを絞らない。利用者の command に既にあれば足さない（二重指定は
/// ai-usage の CLI が拒否する）。
pub fn refresh_command(base: &[String]) -> Vec<String> {
    let mut command = base.to_vec();
    if !base
        .iter()
        .any(|arg| arg == "--refresh" || arg.starts_with("--refresh="))
    {
        command.push("--refresh".to_string());
    }
    command
}

/// フラグを足したコマンド（`sent`）が失敗したとき、設定どおりの command（`base`）で
/// 取り直すか。
///
/// 足したフラグを知らない古い ai-usage は clap の使い方エラー（終了コード 2）で終わる。
/// 終了コードだけでは足したフラグが原因とまでは言い切れないが、ai-usage が 2 で終わるのは
/// 使い方エラーのときだけなので、足した引数付きの呼び出しが使い方エラーだったときは設定
/// どおりの command で取り直す。共有キャッシュを通る分だけ値は古くなり得る（既定 120 秒）が、
/// 使用率を確認できないとして fail-closed で実行全体を止めるより、従来どおりの値で判定する
/// 方がよい。何も足していない（同じコマンド）なら、取り直しても同じ結果になるので取り直さない。
pub fn retry_without_added_flags(exit_code: Option<i32>, sent: &[String], base: &[String]) -> bool {
    exit_code == Some(USAGE_ERROR_EXIT) && sent != base
}

/// 非ゼロで終了した ai-usage のエラー。
pub fn exit_error(output: &std::process::Output) -> anyhow::Error {
    anyhow::anyhow!(
        "ai-usage exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// `ai-usage --json` の出力と、それを共有キャッシュを通さずに取り直せたか。
struct Fetched {
    stdout: Vec<u8>,
    /// `--refresh` で取り直せたか。古い ai-usage で設定どおりの command へ落ちたら false。
    fresh: bool,
}

/// `sent`（`base` に `--refresh` 等を足したコマンド）で ai-usage を実行する。古い ai-usage が
/// 足したフラグを知らずに使い方エラーで終わったら、`base` で取り直す
/// （[`retry_without_added_flags`]）。それ以外の失敗はエラーとして返す。
async fn fetch_ai_usage(sent: &[String], base: &[String]) -> Result<Fetched> {
    // sent は base にフラグを足したものなので、空の command でも空にならない。base で検査する。
    anyhow::ensure!(!base.is_empty(), "ai_usage.command is empty");
    let output = spawn_ai_usage_with_timeout(sent).await?;
    if output.status.success() {
        return Ok(Fetched {
            stdout: output.stdout,
            fresh: true,
        });
    }
    if !retry_without_added_flags(output.status.code(), sent, base) {
        return Err(exit_error(&output));
    }
    let output = spawn_ai_usage_with_timeout(base).await?;
    if !output.status.success() {
        return Err(exit_error(&output));
    }
    Ok(Fetched {
        stdout: output.stdout,
        fresh: false,
    })
}

fn parse_snapshot(stdout: &[u8]) -> Result<AiUsageSnapshot> {
    let parsed: AiUsageOutput =
        serde_json::from_slice(stdout).context("failed to parse ai-usage --json output")?;
    Ok(AiUsageSnapshot {
        accounts: parsed.accounts,
    })
}

/// スケジュールの解決に使う、全アカウント分の使用状況を取り直す。
///
/// `--refresh` を付ける。ai-usage の共有キャッシュを通すと、手動リセットで Codex の週次の
/// リセット時刻が動いた直後に、動く前の時刻でデッドラインを決めてしまう。
async fn run_ai_usage(command: &[String]) -> Result<AiUsageSnapshot> {
    let fetched = fetch_ai_usage(&refresh_command(command), command).await?;
    if !fetched.fresh {
        eprintln!(
            "{}: ai-usage rejected --refresh as a usage error; retried without it, so the reset times may come from its shared cache (update ai-usage)",
            "warning".yellow()
        );
    }
    parse_snapshot(&fetched.stdout)
}

/// usage-gate の判定に使う使用状況を、ai-usage の共有キャッシュを通さずに取り直す。
///
/// まず実行中のアカウントだけを取り直す（[`live_command`]。ほかのアカウント・サービスへ
/// 問い合わせない）。足した引数付きの呼び出しが使い方エラーなら、古い ai-usage とみなして
/// 設定どおりの command で取り直す。
///
/// 絞り込んだ取得が使い方エラー以外で失敗したら、全アカウント分を `--refresh` で取り直す。
/// ai-usage は、絞り込んだ対象に Chrome のセッションが無いと「対象が無い」として非ゼロで
/// 終わる。そのまま取得の失敗として fail-closed にすると、以前は「エントリが無ければ続行」
/// だったアカウント（Chrome で claude.ai からログアウトしている等。エージェント自身は
/// 別の認証で動ける）で、実行全体が止まってしまう。全アカウント分でも取れなければ、従来
/// どおり取得の失敗として扱う。起動できない・期限切れは取り直しても変わらないので取り直さない。
async fn fetch_for_gate(command: &[String], profile: &str, provider: &str) -> Result<Fetched> {
    anyhow::ensure!(!command.is_empty(), "ai_usage.command is empty");
    let narrowed = live_command(command, profile, provider);
    let output = spawn_ai_usage_with_timeout(&narrowed).await?;
    if output.status.success() {
        return Ok(Fetched {
            stdout: output.stdout,
            fresh: true,
        });
    }
    if retry_without_added_flags(output.status.code(), &narrowed, command) {
        eprintln!(
            "\x1b[2m  usage-gate: ai-usage が --refresh 等の追加の引数を使い方エラーで拒んだため、設定どおりのコマンドで取り直します（共有キャッシュの値の可能性があります。ai-usage を更新してください）\x1b[0m"
        );
        let output = spawn_ai_usage_with_timeout(command).await?;
        if !output.status.success() {
            return Err(exit_error(&output));
        }
        return Ok(Fetched {
            stdout: output.stdout,
            fresh: false,
        });
    }
    let everything = refresh_command(command);
    if everything == narrowed {
        // 利用者の command が既に絞り込んでいる。取り直しても同じ結果になる。
        return Err(exit_error(&output));
    }
    eprintln!(
        "\x1b[2m  usage-gate: {profile}/{provider} に絞った取得が失敗したため、全アカウント分で取り直します（{}）\x1b[0m",
        exit_error(&output)
    );
    fetch_ai_usage(&everything, command).await
}

/// `tokio::process::Command` で ai-usage を実行し、`AI_USAGE_TIMEOUT` を超えたら
/// `kill_on_drop` により子プロセスをキルしてエラーを返す。
/// 呼び出し側で status / stdout を判定する。
async fn spawn_ai_usage_with_timeout(command: &[String]) -> Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new(&command[0]);
    cmd.args(&command[1..]).kill_on_drop(true);
    match tokio::time::timeout(AI_USAGE_TIMEOUT, cmd.output()).await {
        Ok(res) => res.with_context(|| {
            format!(
                "failed to run ai-usage command: {}",
                crate::display::format_command(command)
            )
        }),
        Err(_) => anyhow::bail!(
            "ai-usage timed out after {}s: {}",
            AI_USAGE_TIMEOUT.as_secs(),
            crate::display::format_command(command)
        ),
    }
}

/// 使用率ゲート: `(profile, provider)` の weekly / five_hour 使用率のうち、いずれかが
/// `threshold`（%）以上なら `stop_file` を作成し、後続タスクの実行を止める。
///
/// - ai-usage の取得に失敗したときは fail-closed（使用率を確認できない以上、安全側で停止）。
/// - 該当エントリが無い / 使用率欠損のときは過剰停止を避けて続行する。
/// - `stop_file` の作成は `create_new` で冪等（並列 worker から同時に呼ばれても安全）。
/// - 使用率は呼ぶたびに `--refresh` で取り直す（[`live_command`]。理由は本体のコメント）。
///
/// **通知は必ず stderr へ書く。** このゲートは `gate-claim` の `--revalidate` からも
/// 起動され、そのとき stdout はワーカーの `CLAIMED=$(... gate-claim ...)` に直結して
/// いる。1 行でも stdout へ書くと claim 番号に連結され、タスクスクリプト名が壊れて、
/// 既に `claimed-*` へ rename 済みのタスクがマーカーも残さず失われる。
pub async fn run_usage_gate(
    profile: &str,
    provider: &str,
    threshold: u8,
    stop_file: &Path,
    command: &[String],
) -> Result<()> {
    // 既に停止シグナルがあれば何もしない。
    if stop_file.exists() {
        return Ok(());
    }

    // 判定に使う使用率は、ai-usage の共有キャッシュ（既定 120 秒）を通さずに、実行中の
    // アカウントだけを取り直す。共有キャッシュを通すと、手動リセットの直後にリセット前の
    // 値で止めたり、閾値を越えた後も古い値のまま走り続けたりする。並列ワーカーが同時に
    // 取り直しても、ai-usage 側が 1 回の取得にまとめる（`--refresh` の仕様）ので、
    // こちらで重複を抑える必要は無い（以前は TTL 20 秒の独自キャッシュで抑えていた）。
    let fetched = fetch_for_gate(command, profile, provider).await;
    // 根拠の観測時刻の上限（取得を終えた時刻）。一時停止に記録する。モニターの早期解除より
    // 前に取得を終えたデータは、write_pause が書き戻さない。取得の途中で解除された場合は
    // 解除より後の時刻になるので書き戻し得るが、その一時停止も同じ取得元（ai-usage）どうしの
    // 比較なので、モニターが次の取得で確かめ直して解除できる。
    let observed_at = rate_control::now_epoch();
    let snapshot = match fetched.and_then(|fetched| parse_snapshot(&fetched.stdout)) {
        Ok(s) => s,
        Err(e) => {
            // fail-closed: ai-usage の取得に失敗したら stop_file を作って停止する。
            // 既に他ワーカーが stop_file を作成済みなら write_stop_file は false を返す
            // が、その場合も他経路で停止シグナルは伝わるため正常終了でよい。
            if write_stop_file(stop_file, &format!("usage-gate failed: {e}")) {
                eprintln!(
                    "\x1b[31m  \u{26d4} usage-gate: ai-usage を確認できないため停止 ({e})\x1b[0m"
                );
            } else if !stop_file.exists() {
                // 書き込みに失敗し、かつ stop_file が存在しない（既存ではなく作成失敗）。
                // フェイルオープン状態を避けるため、エラーを伝搬してワーカーを止める。
                anyhow::bail!("usage-gate: failed to write stop file: {e}");
            }
            return Ok(());
        }
    };

    let Some(acc) = snapshot.find(profile, provider) else {
        eprintln!("usage-gate: ({profile}, {provider}) のエントリが無いため続行します");
        return Ok(());
    };

    // アカウントが ok:false（認証切れ等で ai-usage がエラーを報告）の場合は、使用率を
    // 信頼できないため fail-closed で停止する。ai-usage の取得自体には成功していても、
    // 「使用率を確認できない」という点は load 失敗と同じであり、ScheduleResolver も
    // ok:false を失敗として fallback している（一貫性）。この検査が無いと、ok:false かつ
    // used_percent 欠損のとき max_used=None となり stop_file を作らず走り続ける（fail-open）。
    if !acc.ok {
        let reason = acc.error.as_deref().unwrap_or("ai-usage account not ok");
        if write_stop_file(stop_file, &format!("usage-gate account not ok: {reason}")) {
            eprintln!(
                "\x1b[31m  \u{26d4} usage-gate: {profile}/{provider} の使用率を確認できないため停止 ({reason})\x1b[0m"
            );
        } else if !stop_file.exists() {
            // 作成失敗かつ既存でもない（ENOSPC・権限不足等）。フェイルオープンを避ける
            // ため、取得失敗パスと同様にエラーを伝搬してワーカーを止める。
            anyhow::bail!("usage-gate: failed to write stop file (account not ok: {reason})");
        }
        return Ok(());
    }

    // 枠ごとに周期で分類して判定する。最大値へ潰すと「どの枠が閾値に触れたか」と
    // 「その枠のリセット時刻」が失われ、5 時間枠のように待てば回復する枠でも恒久停止に
    // なってしまう（週次デッドラインまでの実行余力を丸ごと捨てる）。
    let observations = gate_observations(acc);
    match rate_control::evaluate(&observations, threshold, rate_control::now_epoch()) {
        Decision::Proceed => {}
        Decision::Stop { basis, detail } => {
            let reason = basis.reason(threshold);
            let detail = detail.map(|d| format!(" ({d})")).unwrap_or_default();
            if write_stop_file(stop_file, &format!("usage-gate: {reason}")) {
                eprintln!(
                    "\x1b[31m  \u{26d4} usage-gate: {profile}/{provider} {reason}{detail} のため後続を停止\x1b[0m"
                );
            } else if !stop_file.exists() {
                // 閾値超過なのに stop_file を作成できず、既存でもない（ENOSPC・権限不足
                // 等の作成失敗）。fail-closed パスと同様、フェイルオープンを避けるため
                // エラーを伝搬してワーカーを止める。既存（別ワーカーが作成済み）の場合は
                // 停止シグナルが既にあるため正常終了でよい。
                anyhow::bail!("usage-gate: failed to write stop file ({reason})");
            }
        }
        Decision::Pause { resume_at, basis } => {
            let reason = basis.reason(threshold);
            let state = rate_control::PauseState::new(
                resume_at,
                basis.window.clone(),
                format!("usage-gate: {reason}"),
            )
            .observed(
                rate_control::PauseSource::AiUsage,
                rate_control::PauseCause::Threshold,
                observed_at,
            )
            .with_basis(basis.used_percent, basis.reset_at);
            // pause を書けない場合は恒久停止へ倒す。待機の根拠を共有できない以上、
            // 走り続けるより止める側が安全（fail-closed）。
            if let Err(e) =
                rate_control::write_pause(&rate_control::pause_path_for(stop_file), &state)
            {
                if write_stop_file(stop_file, &format!("usage-gate: {reason}")) {
                    eprintln!(
                        "\x1b[31m  \u{26d4} usage-gate: pause を記録できないため停止 ({e})\x1b[0m"
                    );
                } else if !stop_file.exists() {
                    anyhow::bail!("usage-gate: failed to write pause file: {e}");
                }
            } else {
                eprintln!(
                    "\x1b[33m  \u{23f8} usage-gate: {profile}/{provider} {reason} のため {} の枠がリセットされるまで待機\x1b[0m",
                    basis.window
                );
            }
        }
    }
    Ok(())
}

/// usage-gate の閾値判定に渡す枠ごとの観測値を組み立てる。
///
/// 周期はスロット名ではなく `kind`（`five_hour` / `daily` / `weekly` / `monthly`）から
/// 導く。実データでは antigravity が `five_hour` スロットに 24 時間枠を、pixellab が
/// `weekly` スロットに月次枠を返すため、スロット名で決め打ちすると「待てば回復するか」の
/// 判断を取り違える。使用率が欠損している枠は判定材料にならないので落とす（該当枠が
/// 無いときの続行は呼び出し側の従来方針どおり）。
fn gate_observations(acc: &AiUsageAccount) -> Vec<WindowObservation> {
    [
        (UsageWindow::Weekly, acc.weekly.as_ref()),
        (UsageWindow::FiveHour, acc.five_hour.as_ref()),
    ]
    .into_iter()
    .filter_map(|(slot, data)| {
        let data = data?;
        let used_percent = data.used_percent?;
        let kind = rate_control::WindowKind::from_period_secs(data.period(slot).num_seconds());
        let reset_at = parse_resets_at(data).map(|at| at.timestamp());
        Some(WindowObservation::new(kind, slot.label(), used_percent).with_reset_at(reset_at))
    })
    .collect()
}

/// 実行中のアカウントだけを、ai-usage の共有キャッシュを通さずに取り直すコマンド。
///
/// ai-usage は取得結果をプロセス間で既定 120 秒共有する（`--cache-ttl`）。手動リセットの
/// 直後でもリセット前の値を返し得るので、一時停止中の回復の確認と停止の判定では
/// `--refresh` で取り直す。`--profile` / `--only` で対象を 1 アカウントに絞り、待機中に
/// 繰り返し取っても、ほかのアカウント（ほかのサービス）へ問い合わせないようにする。
/// 利用者の command に既にあるフラグは足さない（二重指定は ai-usage の CLI が拒否する）。
pub fn live_command(base: &[String], profile: &str, provider: &str) -> Vec<String> {
    let has = |long: &str, short: Option<&str>| {
        base.iter().any(|arg| {
            arg == long
                || arg.starts_with(&format!("{long}="))
                || short.is_some_and(|s| arg == s || (arg.starts_with(s) && !arg.starts_with("--")))
        })
    };
    let mut command = base.to_vec();
    if !has("--refresh", None) {
        command.push("--refresh".to_string());
    }
    if !has("--profile", Some("-p")) {
        command.push("--profile".to_string());
        command.push(profile.to_string());
    }
    if !has("--only", None) {
        command.push("--only".to_string());
        command.push(provider.to_string());
    }
    command
}

/// `ai-usage --json` の出力から、1 アカウントの枠の実測値を取り出す（一時停止中の回復の
/// 確認に使う）。
///
/// `observed_from` はデータが取得された時刻の下限、`fresh` は `--refresh` で取り直したか
/// （[`rate_control::LiveObservation`]）。アカウントが無い・`ok:false`・JSON として読めない
/// ときは、判定できない理由を返す（使用率を確認できない状態を「余裕あり」へ潰さない）。
pub fn live_observation(
    json: &[u8],
    profile: &str,
    provider: &str,
    observed_from: i64,
    fresh: bool,
) -> std::result::Result<rate_control::LiveObservation, LiveError> {
    let parsed: AiUsageOutput = serde_json::from_slice(json)
        .map_err(|e| LiveError::Unavailable(format!("cannot parse the ai-usage output: {e}")))?;
    let snapshot = AiUsageSnapshot {
        accounts: parsed.accounts,
    };
    let account = snapshot.find(profile, provider).ok_or_else(|| {
        LiveError::NoEntry(format!("no ai-usage entry for ({profile}, {provider})"))
    })?;
    if !account.ok {
        return Err(LiveError::Unavailable(
            account
                .error
                .clone()
                .unwrap_or_else(|| "ai-usage account not ok".to_string()),
        ));
    }
    Ok(rate_control::LiveObservation {
        windows: gate_observations(account),
        observed_from,
        fresh,
    })
}

/// [`live_observation`] で実行中のアカウントの使用率を確認できなかった理由。
///
/// usage-gate と同じく 2 つを分ける。使用率を確認できない（取得・解釈の失敗、`ok:false`）
/// ときは止める側に倒し（fail-closed）、出力に該当アカウントが無いときは過剰停止を避けて
/// 続行する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveError {
    /// 出力に実行中のアカウント（`profile` と `provider` の組）が無い。
    NoEntry(String),
    /// 取得・解釈に失敗した、または ai-usage がそのアカウントのエラー（`ok:false`）を報告した。
    Unavailable(String),
}

impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveError::NoEntry(reason) | LiveError::Unavailable(reason) => f.write_str(reason),
        }
    }
}

/// stop_file を冪等に作成する。新規作成できたら true、既存または作成失敗なら false。
fn write_stop_file(stop_file: &Path, reason: &str) -> bool {
    // 実装は `rate_control::write_stop` へ寄せる。ここに複製を持つと、停止発行を
    // claim と同じロックで直列化する等の変更が片方だけに入って再発する。
    rate_control::write_stop(stop_file, reason).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AiUsageConfig, Prompts, Settings};

    fn config_for_resolver(ai_usage: Option<AiUsageConfig>) -> Config {
        Config {
            config_dir: Default::default(),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 1,
                rate_limit_threshold: 95,
                dedup_scope: crate::config::DedupScope::Agent,
                resume_interrupted: true,
            },
            prompts: Prompts {
                default: String::new(),
                resume: None,
            },
            agents: Vec::new(),
            scan: Vec::new(),
            targets: Vec::new(),
            ai_usage,
        }
    }

    fn ai_usage_config(enabled: bool, command: Vec<String>) -> AiUsageConfig {
        AiUsageConfig {
            enabled,
            command,
            window: UsageWindowPolicy::Weekly,
            fallback: UsageFallback::Fixed,
            state_window: StateWindowPolicy::Selected,
            profiles: Vec::new(),
        }
    }

    #[tokio::test]
    async fn spawn_ai_usage_redacts_secret_on_spawn_failure() {
        let secret = "actual-secret-value";
        let command = vec![
            "token-burn-command-that-does-not-exist".to_string(),
            "--api-key".to_string(),
            secret.to_string(),
        ];

        let error = spawn_ai_usage_with_timeout(&command)
            .await
            .expect_err("存在しないコマンドの起動は失敗するべき");
        let message = error.to_string();

        assert!(message.contains("--api-key <redacted>"));
        assert!(!message.contains(secret));
    }

    #[tokio::test]
    async fn schedule_resolver_load_skips_command_when_integration_is_disabled() {
        let config = config_for_resolver(Some(ai_usage_config(
            false,
            vec!["token-burn-command-that-must-not-run".to_string()],
        )));

        let resolver = ScheduleResolver::load(&config).await;

        assert!(matches!(resolver.usage, UsageState::Disabled));
        assert!(matches!(resolver.state_window, StateWindowPolicy::Selected));
        assert_eq!(resolver.failure(), None);
    }

    /// 偽の ai-usage（`sh -c`）。受け取った引数を 1 回 1 行で記録し、`json` を出力する。
    ///
    /// `refuse_refresh` なら、`--refresh` を知らない古い ai-usage と同じく clap の使い方
    /// エラー（終了コード 2）で終わる。
    struct FakeAiUsage {
        command: Vec<String>,
        log: std::path::PathBuf,
    }

    impl FakeAiUsage {
        fn new(dir: &Path, json: &str, refuse_refresh: bool) -> Self {
            let refuse = if refuse_refresh {
                "case \" $* \" in *' --refresh '*) echo \"error: unexpected argument '--refresh' found\" >&2; exit 2;; esac; "
            } else {
                ""
            };
            Self::with_guard(dir, json, refuse)
        }

        /// 絞り込んだ対象（`--only`）に Chrome のセッションが無いときの ai-usage。対象が無い
        /// として終了コード 1 で終わる。絞り込まなければ `json` を出力する。
        fn without_a_session_for_the_narrowed_account(dir: &Path, json: &str) -> Self {
            Self::with_guard(
                dir,
                json,
                "case \" $* \" in *' --only '*) echo 'Error: No signed-in Claude/Codex/PixelLab sessions or Antigravity/Grok token found.' >&2; exit 1;; esac; ",
            )
        }

        /// `guard` は出力の前に実行するシェルの断片（引数を見て終了するなど）。
        fn with_guard(dir: &Path, json: &str, guard: &str) -> Self {
            let output = dir.join("ai-usage-output.json");
            std::fs::write(&output, json).unwrap();
            let log = dir.join("ai-usage-args.log");
            let script = format!(
                "printf '%s\\n' \"$*\" >> {}; {guard}cat {}",
                sh_quote(&log),
                sh_quote(&output)
            );
            Self {
                // `sh -c <script> ai-usage --json ...` の `$*` は `--json` 以降の引数になる。
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    script,
                    "ai-usage".into(),
                    "--json".into(),
                ],
                log,
            }
        }

        /// 起動されたときの引数（1 回 1 行）。
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .map(|log| log.lines().map(String::from).collect())
                .unwrap_or_default()
        }
    }

    fn sh_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
    }

    #[tokio::test]
    async fn schedule_resolver_load_parses_successful_command_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        let output = r#"{"accounts":[{"profile":"P","provider":"claude","ok":true,"weekly":{"resets_at":"2099-01-01T00:00:00+00:00","used_percent":42.0}}]}"#;
        let fake = FakeAiUsage::new(tmp.path(), output, false);
        let config = config_for_resolver(Some(ai_usage_config(true, fake.command.clone())));

        let resolver = ScheduleResolver::load(&config).await;

        assert!(matches!(
            &resolver.usage,
            UsageState::Loaded(snapshot) if snapshot.accounts.len() == 1
        ));
        assert_eq!(resolver.failure(), None);
        let schedule = resolver
            .schedule_for(&rt_agent(
                "P",
                "claude",
                UsageWindowPolicy::Weekly,
                UsageFallback::Error,
            ))
            .expect("読み込んだ使用状況からスケジュールを解決できるべき")
            .expect("weekly 枠があるためスケジュールを返すべき");
        assert!(matches!(schedule.source, ScheduleSource::AiUsage { .. }));
        // リセット時刻は ai-usage の共有キャッシュを通さずに取り直す。全アカウント分が要るので
        // 対象は絞らない。
        assert_eq!(fake.calls(), ["--json --refresh"]);
    }

    #[tokio::test]
    async fn schedule_resolver_load_retries_without_refresh_on_an_old_ai_usage() {
        // `--refresh` を知らない古い ai-usage は使い方エラー（終了コード 2）で終わる。
        // 失敗として fixed へ落とさず、設定どおりの command で取り直す。
        let tmp = tempfile::TempDir::new().unwrap();
        let output = r#"{"accounts":[{"profile":"P","provider":"claude","ok":true,"weekly":{"resets_at":"2099-01-01T00:00:00+00:00","used_percent":42.0}}]}"#;
        let fake = FakeAiUsage::new(tmp.path(), output, true);
        let config = config_for_resolver(Some(ai_usage_config(true, fake.command.clone())));

        let resolver = ScheduleResolver::load(&config).await;

        assert!(matches!(&resolver.usage, UsageState::Loaded(_)));
        assert_eq!(fake.calls(), ["--json --refresh", "--json"]);
    }

    #[test]
    fn refresh_command_adds_refresh_once() {
        let base = vec!["ai-usage".to_string(), "--json".to_string()];
        assert_eq!(
            refresh_command(&base),
            ["ai-usage", "--json", "--refresh"],
            "全アカウント分を取るので --profile / --only は足さない"
        );
        let already = refresh_command(&base);
        assert_eq!(refresh_command(&already), already, "二重に足さない");
    }

    #[test]
    fn only_a_usage_error_caused_by_added_flags_is_retried() {
        let base = vec!["ai-usage".to_string(), "--json".to_string()];
        let sent = refresh_command(&base);
        assert!(retry_without_added_flags(Some(2), &sent, &base));
        // 使い方エラー以外の失敗は取り直さない（fail-closed のまま）。
        assert!(!retry_without_added_flags(Some(1), &sent, &base));
        assert!(!retry_without_added_flags(None, &sent, &base));
        // 何も足していなければ、取り直しても同じ結果になる。
        assert!(!retry_without_added_flags(Some(2), &sent, &sent));
    }

    #[tokio::test]
    async fn schedule_resolver_load_records_command_failure() {
        let config = config_for_resolver(Some(ai_usage_config(
            true,
            vec!["token-burn-command-that-does-not-exist".to_string()],
        )));

        let resolver = ScheduleResolver::load(&config).await;

        assert!(matches!(resolver.usage, UsageState::Failed(_)));
        let failure = resolver.failure().expect("起動失敗理由を保持するべき");
        assert!(failure.contains("token-burn-command-that-does-not-exist"));
    }

    fn window_data(resets_at: Option<&str>) -> UsageWindowData {
        UsageWindowData {
            kind: None,
            resets_at: resets_at.map(String::from),
            used_percent: None,
        }
    }

    /// `kind` を明示した枠データ（実データの `daily` / `monthly` 再現用）。
    fn window_data_with_kind(kind: &str, resets_at: Option<&str>) -> UsageWindowData {
        UsageWindowData {
            kind: Some(kind.to_string()),
            resets_at: resets_at.map(String::from),
            used_percent: None,
        }
    }

    fn account(
        profile: &str,
        provider: &str,
        ok: bool,
        weekly: Option<UsageWindowData>,
        five_hour: Option<UsageWindowData>,
        error: Option<&str>,
    ) -> AiUsageAccount {
        AiUsageAccount {
            profile: profile.to_string(),
            provider: provider.to_string(),
            ok,
            weekly,
            five_hour,
            error: error.map(String::from),
        }
    }

    fn rt_agent(
        profile: &str,
        provider: &str,
        window: UsageWindowPolicy,
        fb: UsageFallback,
    ) -> RuntimeAgent {
        RuntimeAgent {
            name: format!("claude-{profile}"),
            provider: Some(provider.to_string()),
            command: vec!["claude".to_string()],
            reset_weekday: Some("monday".to_string()),
            reset_time: Some("09:00".to_string()),
            timezone: Some("UTC".to_string()),
            ai_usage: Some(RuntimeAiUsage {
                profile: profile.to_string(),
                provider: provider.to_string(),
            }),
            window,
            fallback: fb,
            ..Default::default()
        }
    }

    fn resolver(state: UsageState) -> ScheduleResolver {
        ScheduleResolver {
            usage: state,
            state_window: StateWindowPolicy::Weekly,
        }
    }

    #[test]
    fn parse_resets_at_parses_rfc3339() {
        let d = window_data(Some("2099-01-02T03:04:05+00:00"));
        let parsed = parse_resets_at(&d).expect("RFC3339 should parse");
        assert_eq!(parsed.to_utc().to_rfc3339(), "2099-01-02T03:04:05+00:00");
        let expected = DateTime::parse_from_rfc3339("2099-01-02T03:04:05+00:00")
            .unwrap()
            .with_timezone(&Local)
            .fixed_offset();
        assert_eq!(parsed, expected);
        assert_eq!(parsed.naive_local(), expected.naive_local());
        assert_eq!(
            parsed.offset().local_minus_utc(),
            expected.offset().local_minus_utc()
        );
    }

    #[test]
    fn parse_resets_at_none_for_missing() {
        assert!(parse_resets_at(&window_data(None)).is_none());
        assert!(parse_resets_at(&window_data(Some("not-a-date"))).is_none());
    }

    #[test]
    fn nearest_window_picks_nearer_resets_at() {
        // deadline 算出に使う resets_at 基準で、最も近い枠を選ぶ。
        let acc = account(
            "P",
            "claude",
            true,
            Some(window_data(Some("2099-01-10T00:00:00+00:00"))),
            Some(window_data(Some("2099-01-05T00:00:00+00:00"))),
            None,
        );
        let (w, _) = nearest_window(&acc).expect("should pick one");
        // five_hour(1/05) が weekly(1/10) より近い。
        assert_eq!(w, UsageWindow::FiveHour);
    }

    #[test]
    fn nearest_window_ignores_unparseable_resets_at() {
        // resets_at がパース不能な枠は候補から除外する。
        // （採用すると build_from_account が None を返してスケジュール解決全体が失敗するため）
        let acc = account(
            "P",
            "claude",
            true,
            Some(window_data(Some("not-a-date"))),
            Some(window_data(Some("2099-01-10T00:00:00+00:00"))),
            None,
        );
        let (w, _) = nearest_window(&acc).expect("should pick the parseable window");
        assert_eq!(w, UsageWindow::FiveHour);
    }

    #[test]
    fn schedule_for_uses_ai_usage_weekly() {
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "Work",
                "claude",
                true,
                Some(window_data(Some("2099-01-01T00:00:00+00:00"))),
                None,
                None,
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "Work",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("should resolve");
        assert_eq!(sched.source, ScheduleSource::AiUsage(UsageWindow::Weekly));
        // 状態カットオフは次回リセットの 7 日前
        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn schedule_for_uses_daily_period_for_daily_kind() {
        // 実データの antigravity は `five_hour` スロットに 24 時間枠（kind:"daily"）を返す。
        // スロット名どおり 5 時間として扱うと state_cutoff が未来へ飛び、処理済み
        // フィルタが恒久的に無効化される（毎回同じ先頭ターゲットを再処理し続ける）。
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "Antigravity",
                "antigravity",
                true,
                None,
                Some(window_data_with_kind(
                    "daily",
                    Some("2099-01-02T00:00:00+00:00"),
                )),
                None,
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "Antigravity",
            "antigravity",
            UsageWindowPolicy::FiveHour,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("should resolve");
        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(
            diff.num_hours(),
            24,
            "daily 枠のカットオフは 24 時間前であるべき"
        );
    }

    #[test]
    fn schedule_for_uses_monthly_period_for_monthly_kind() {
        // 実データの pixellab は `weekly` スロットに月次枠（kind:"monthly"）を返す。
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "Yohei",
                "pixellab",
                true,
                Some(window_data_with_kind(
                    "monthly",
                    Some("2099-02-01T00:00:00+00:00"),
                )),
                None,
                None,
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "Yohei",
            "pixellab",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("should resolve");
        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(
            diff.num_days(),
            30,
            "monthly 枠のカットオフは 30 日前であるべき"
        );
    }

    #[test]
    fn schedule_for_unknown_kind_falls_back_to_slot_period() {
        // 未知の kind はスロット名の既定周期へ落とす（従来動作を維持）。
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "Work",
                "claude",
                true,
                Some(window_data_with_kind(
                    "fortnightly",
                    Some("2099-01-01T00:00:00+00:00"),
                )),
                None,
                None,
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "Work",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("should resolve");
        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn schedule_for_prefers_explicit_kind_over_the_slot_name() {
        // `weekly` スロットに 5 時間枠（kind:"five_hour"）が入る向きの食い違いでも、
        // 周期は kind 側で決める。daily / monthly のテストは「スロットより長い枠」
        // しか見ておらず、短い枠へ倒れる `Some("five_hour")` の分岐は素通りしていた。
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "Work",
                "claude",
                true,
                Some(window_data_with_kind(
                    "five_hour",
                    Some("2099-01-01T05:00:00+00:00"),
                )),
                None,
                None,
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "Work",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("should resolve");
        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(
            diff.num_hours(),
            5,
            "five_hour kind のカットオフは 5 時間前であるべき"
        );
    }

    #[test]
    fn schedule_for_weekly_kind_in_five_hour_slot_uses_seven_days() {
        // 逆向き（`five_hour` スロットに週次枠）でも kind を優先する。スロット名で
        // 決め打ちすると 7 日枠を 5 時間として扱い、カットオフが実際の枠開始点より
        // はるかに新しくなって、同じ週に処理済みのターゲットを何度も拾い直す。
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "Work",
                "claude",
                true,
                None,
                Some(window_data_with_kind(
                    "weekly",
                    Some("2099-01-08T00:00:00+00:00"),
                )),
                None,
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "Work",
            "claude",
            UsageWindowPolicy::FiveHour,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("should resolve");
        let diff = sched.next_reset - sched.state_cutoff;
        assert_eq!(
            diff.num_days(),
            7,
            "weekly kind のカットオフは 7 日前であるべき"
        );
    }

    #[test]
    fn schedule_for_falls_back_to_fixed_when_entry_missing() {
        let snapshot = AiUsageSnapshot { accounts: vec![] };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "UNKNOWN",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().expect("fixed fallback");
        match sched.source {
            ScheduleSource::FixedFallback(reason) => {
                assert!(reason.contains("no ai-usage entry"));
            }
            other => panic!("expected FixedFallback, got {other:?}"),
        }
    }

    #[test]
    fn schedule_for_skip_returns_none() {
        let r = resolver(UsageState::Failed("boom".to_string()));
        let agent = rt_agent(
            "P",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Skip,
        );
        assert!(r.schedule_for(&agent).unwrap().is_none());
    }

    #[test]
    fn schedule_for_error_propagates() {
        let r = resolver(UsageState::Failed("boom".to_string()));
        let agent = rt_agent(
            "P",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Error,
        );
        assert!(r.schedule_for(&agent).is_err());
    }

    #[test]
    fn schedule_for_ok_false_falls_back_with_error_message() {
        let snapshot = AiUsageSnapshot {
            accounts: vec![account(
                "P",
                "claude",
                false,
                None,
                None,
                Some("rate limited"),
            )],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agent = rt_agent(
            "P",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Fixed,
        );
        let sched = r.schedule_for(&agent).unwrap().unwrap();
        match sched.source {
            ScheduleSource::FixedFallback(reason) => assert!(reason.contains("rate limited")),
            other => panic!("expected FixedFallback, got {other:?}"),
        }
    }

    #[test]
    fn schedule_for_non_ai_usage_agent_uses_fixed() {
        let r = resolver(UsageState::Disabled);
        let agent = RuntimeAgent {
            name: "claude".to_string(),
            command: vec!["claude".to_string()],
            reset_weekday: Some("monday".to_string()),
            reset_time: Some("09:00".to_string()),
            timezone: Some("UTC".to_string()),
            ..Default::default()
        };
        let sched = r.schedule_for(&agent).unwrap().unwrap();
        assert_eq!(sched.source, ScheduleSource::Fixed);
    }

    #[test]
    fn select_nearest_skips_none_and_picks_closest() {
        let snapshot = AiUsageSnapshot {
            accounts: vec![
                account(
                    "FAR",
                    "claude",
                    true,
                    Some(window_data(Some("2099-12-31T00:00:00+00:00"))),
                    None,
                    None,
                ),
                account(
                    "NEAR",
                    "claude",
                    true,
                    Some(window_data(Some("2099-01-01T00:00:00+00:00"))),
                    None,
                    None,
                ),
            ],
        };
        let r = resolver(UsageState::Loaded(snapshot));
        let agents = vec![
            rt_agent(
                "FAR",
                "claude",
                UsageWindowPolicy::Weekly,
                UsageFallback::Skip,
            ),
            rt_agent(
                "NEAR",
                "claude",
                UsageWindowPolicy::Weekly,
                UsageFallback::Skip,
            ),
        ];
        let (idx, sched) = r.select_nearest(&agents).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(sched.agent_name, "claude-NEAR");
    }

    #[test]
    fn select_nearest_errors_when_all_skipped() {
        let r = resolver(UsageState::Failed("boom".to_string()));
        let agents = vec![rt_agent(
            "P",
            "claude",
            UsageWindowPolicy::Weekly,
            UsageFallback::Skip,
        )];
        assert!(r.select_nearest(&agents).is_err());
    }

    /// 偽の ai-usage を `json` で用意して usage-gate を 1 回動かす。
    async fn gate_with(
        dir: &Path,
        stop: &Path,
        provider: &str,
        json: &str,
    ) -> (Result<()>, FakeAiUsage) {
        let fake = FakeAiUsage::new(dir, json, false);
        let result = run_usage_gate("Work", provider, 90, stop, &fake.command).await;
        (result, fake)
    }

    #[tokio::test]
    async fn usage_gate_stops_when_over_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "claude",
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":true,"weekly":{"used_percent":95.0}}]}"#,
        )
        .await;
        result.unwrap();
        assert!(stop.exists(), "閾値超過で stop_file が作られるべき");
    }

    #[tokio::test]
    async fn usage_gate_refreshes_only_the_running_account_every_time() {
        // 判定は ai-usage の共有キャッシュ（既定 120 秒）を通さずに取り直す。以前は独自の
        // キャッシュ（TTL 20 秒）も挟んでいたため、手動リセットの直後でも最大 2 分以上前の
        // 使用率で判定していた。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let fake = FakeAiUsage::new(
            tmp.path(),
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":true,"weekly":{"used_percent":50.0}}]}"#,
            false,
        );
        for _ in 0..2 {
            run_usage_gate("Work", "claude", 90, &stop, &fake.command)
                .await
                .unwrap();
        }
        assert_eq!(
            fake.calls(),
            [
                "--json --refresh --profile Work --only claude",
                "--json --refresh --profile Work --only claude"
            ],
            "呼ぶたびに実行中のアカウントだけを取り直すべき"
        );
        assert!(!stop.exists());
    }

    #[tokio::test]
    async fn usage_gate_falls_back_to_the_configured_command_on_an_old_ai_usage() {
        // `--refresh` を知らない古い ai-usage は使い方エラー（終了コード 2）で終わる。
        // 使用率を確認できないとして実行全体を止めず、設定どおりの command（絞り込みも
        // --refresh も無し）で取り直して判定する。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let fake = FakeAiUsage::new(
            tmp.path(),
            r#"{"accounts":[{"profile":"Other","provider":"claude","ok":true,"weekly":{"used_percent":10.0}},{"profile":"Work","provider":"claude","ok":true,"weekly":{"used_percent":95.0}}]}"#,
            true,
        );
        run_usage_gate("Work", "claude", 90, &stop, &fake.command)
            .await
            .unwrap();
        assert_eq!(
            fake.calls(),
            ["--json --refresh --profile Work --only claude", "--json"]
        );
        assert!(stop.exists(), "取り直した値で閾値超過を判定するべき");
        let reason = std::fs::read_to_string(&stop).unwrap();
        assert!(reason.contains("weekly 95%"), "{reason}");
    }

    #[tokio::test]
    async fn usage_gate_does_not_retry_a_command_that_already_refreshes() {
        // 利用者の command に既に --refresh・絞り込みがあれば何も足していない。使い方エラーは
        // 足したフラグのせいではないので取り直さず、従来どおり fail-closed で止める。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let fake = FakeAiUsage::new(tmp.path(), "{}", true);
        let mut command = fake.command.clone();
        command.extend(
            ["--refresh", "--profile", "Work", "--only", "claude"]
                .iter()
                .map(|s| s.to_string()),
        );
        run_usage_gate("Work", "claude", 90, &stop, &command)
            .await
            .unwrap();
        assert_eq!(fake.calls().len(), 1);
        assert!(stop.exists());
    }

    #[tokio::test]
    async fn usage_gate_fail_closed_when_the_fallback_also_fails() {
        // 終了コード 2 で取り直した設定どおりの command も失敗したら、使用率を確認できない
        // ので fail-closed で止める。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "exit 2".to_string(),
            "ai-usage".to_string(),
        ];
        run_usage_gate("Work", "claude", 90, &stop, &command)
            .await
            .unwrap();
        assert!(
            stop.exists(),
            "取り直しも失敗したら fail-closed で停止すべき"
        );
    }

    #[tokio::test]
    async fn usage_gate_continues_when_the_running_account_has_no_session() {
        // 絞り込んだ対象に Chrome のセッションが無いと ai-usage は終了コード 1 で終わる。
        // 全アカウント分で取り直してエントリが無いと分かれば、絞り込む前と同じく続行する
        // （エージェント自身は別の認証で動けるので、過剰に止めない）。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let fake = FakeAiUsage::without_a_session_for_the_narrowed_account(
            tmp.path(),
            r#"{"accounts":[{"profile":"Home","provider":"claude","ok":true,"weekly":{"used_percent":99.0}}]}"#,
        );
        run_usage_gate("Work", "claude", 90, &stop, &fake.command)
            .await
            .unwrap();
        assert_eq!(
            fake.calls(),
            [
                "--json --refresh --profile Work --only claude",
                "--json --refresh"
            ]
        );
        assert!(!stop.exists(), "エントリ無しは続行（過剰停止しない）");
    }

    #[tokio::test]
    async fn usage_gate_judges_with_every_account_when_the_narrowed_fetch_fails() {
        // 絞り込んだ取得だけが失敗し、全アカウント分では実行中のアカウントが取れた。
        // その値で判定する（閾値超過なら止める）。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let fake = FakeAiUsage::without_a_session_for_the_narrowed_account(
            tmp.path(),
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":true,"weekly":{"used_percent":95.0}}]}"#,
        );
        run_usage_gate("Work", "claude", 90, &stop, &fake.command)
            .await
            .unwrap();
        assert_eq!(fake.calls().len(), 2);
        assert!(stop.exists(), "取り直した値で閾値超過を判定するべき");
    }

    #[tokio::test]
    async fn usage_gate_fail_closed_when_ai_usage_cannot_start() {
        // 起動できないのは取り直しても変わらない。使用率を確認できないので止める。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        run_usage_gate(
            "Work",
            "claude",
            90,
            &stop,
            &["token-burn-command-that-does-not-exist".to_string()],
        )
        .await
        .unwrap();
        assert!(stop.exists(), "起動できなければ fail-closed で停止すべき");
    }

    #[tokio::test]
    async fn usage_gate_errors_when_stop_file_write_fails_over_threshold() {
        // 閾値超過だが stop_file を作成できず（親ディレクトリが存在しない）、
        // 既存でもない場合は、フェイルオープンせずエラーを伝搬する。
        let tmp = tempfile::TempDir::new().unwrap();
        // 存在しないサブディレクトリ配下を指すため create_new が必ず失敗する。
        let stop = tmp.path().join("missing-dir").join("stop");
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "claude",
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":true,"weekly":{"used_percent":95.0}}]}"#,
        )
        .await;
        assert!(result.is_err(), "stop_file 作成失敗時はエラーを返すべき");
        assert!(!stop.exists());
    }

    #[tokio::test]
    async fn usage_gate_continues_when_under_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "claude",
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":true,"weekly":{"used_percent":50.0},"five_hour":{"used_percent":10.0}}]}"#,
        )
        .await;
        result.unwrap();
        assert!(!stop.exists(), "閾値未満では stop_file は作られない");
    }

    #[tokio::test]
    async fn usage_gate_uses_max_of_weekly_and_five_hour() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        // weekly は低いが five_hour が閾値超過 → 停止する。
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "codex",
            r#"{"accounts":[{"profile":"Work","provider":"codex","ok":true,"weekly":{"used_percent":20.0},"five_hour":{"used_percent":92.0}}]}"#,
        )
        .await;
        result.unwrap();
        // リセット時刻が読めないので、いつ再開してよいか確定できず恒久停止に倒れる。
        assert!(stop.exists(), "five_hour が閾値超過なら停止すべき");
    }

    #[tokio::test]
    async fn usage_gate_pauses_when_only_the_five_hour_window_is_over() {
        // 5 時間枠だけが閾値を超え、その枠のリセット時刻が分かっていて、週次枠に余裕が
        // あるケース。待てば回復するので恒久停止にはしない（実際に起きた事故の再現）。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let reset = Utc::now() + chrono::Duration::minutes(10);
        let resets_at = reset.to_rfc3339();
        let before = rate_control::now_epoch();
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "claude",
            &format!(
                r#"{{"accounts":[{{"profile":"Work","provider":"claude","ok":true,"weekly":{{"used_percent":43.0}},"five_hour":{{"kind":"five_hour","used_percent":90.0,"resets_at":"{resets_at}"}}}}]}}"#
            ),
        )
        .await;
        result.unwrap();
        let after = rate_control::now_epoch();
        assert!(
            !stop.exists(),
            "5 時間枠の枯渇で恒久停止してはいけない（待てば回復する）"
        );
        let state = rate_control::read_pause(&rate_control::pause_path_for(&stop))
            .unwrap()
            .expect("pause file が作られるべき");
        assert_eq!(state.window, "five_hour");
        assert!(state.resume_at > rate_control::now_epoch());
        // モニターが手動リセット等での回復を確かめられるよう、根拠と観測時刻を残す。
        assert_eq!(state.source, rate_control::PauseSource::AiUsage);
        assert_eq!(state.cause, rate_control::PauseCause::Threshold);
        assert_eq!(state.used_percent, Some(90.0));
        assert_eq!(state.window_reset_at, Some(reset.timestamp()));
        assert!(
            (before..=after).contains(&state.observed_at),
            "観測時刻の上限は取得を終えた時刻: {} not in {before}..={after}",
            state.observed_at
        );
    }

    #[tokio::test]
    async fn usage_gate_stops_permanently_when_the_weekly_window_is_over() {
        // 週次枠は実行全体のデッドラインと同じ周期なので、待っても回復しない。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let resets_at = (Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "claude",
            &format!(
                r#"{{"accounts":[{{"profile":"Work","provider":"claude","ok":true,"weekly":{{"kind":"weekly","used_percent":95.0,"resets_at":"{resets_at}"}},"five_hour":{{"kind":"five_hour","used_percent":10.0,"resets_at":"{resets_at}"}}}}]}}"#
            ),
        )
        .await;
        result.unwrap();
        assert!(stop.exists(), "週次枠の枯渇は恒久停止すべき");
        assert!(
            !rate_control::pause_path_for(&stop).exists(),
            "恒久停止のときに一時停止を書いてはいけない"
        );
    }

    #[tokio::test]
    async fn usage_gate_stops_when_a_daily_window_reports_a_weekly_reset() {
        // antigravity は `five_hour` スロットに 24 時間枠を返す。スロット名で 5 時間と
        // 決め打ちすると、週次相当のリセット時刻を短周期枠のものとして受け入れてしまう。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let resets_at = (Utc::now() + chrono::Duration::days(3)).to_rfc3339();
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "antigravity",
            &format!(
                r#"{{"accounts":[{{"profile":"Work","provider":"antigravity","ok":true,"five_hour":{{"kind":"daily","used_percent":95.0,"resets_at":"{resets_at}"}}}}]}}"#
            ),
        )
        .await;
        result.unwrap();
        assert!(
            stop.exists(),
            "24 時間枠が 3 日先のリセットを報告したら、待機の根拠にせず停止すべき"
        );
    }

    #[tokio::test]
    async fn usage_gate_fail_closed_on_fetch_failure() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        // command が失敗する（false は exit 1）→ 取り直さずに fail-closed で停止。
        run_usage_gate("Work", "claude", 90, &stop, &["false".to_string()])
            .await
            .unwrap();
        assert!(stop.exists(), "取得失敗時は fail-closed で停止すべき");
    }

    #[tokio::test]
    async fn usage_gate_fail_closed_on_unparseable_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let (result, _) = gate_with(tmp.path(), &stop, "claude", "not json").await;
        result.unwrap();
        assert!(
            stop.exists(),
            "読めない出力は使用率を確認できないので停止すべき"
        );
    }

    #[tokio::test]
    async fn usage_gate_fail_closed_when_account_not_ok() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        // 該当エントリは存在するが ok:false（認証切れ等）かつ used_percent 欠損。
        // acc.ok を見ずに used_percent だけで判定すると max_used=None で走り続ける
        // （fail-open）ため、ok:false を fail-closed で停止することを確認する。
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "claude",
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":false,"error":"auth expired"}]}"#,
        )
        .await;
        result.unwrap();
        assert!(
            stop.exists(),
            "ok:false のアカウントは使用率を確認できないため fail-closed で停止すべき"
        );
    }

    #[tokio::test]
    async fn usage_gate_errors_when_stop_file_write_fails_for_not_ok_account() {
        // ok:false かつ stop_file を作成できず既存でもない場合はフェイルオープンせず
        // エラーを伝搬する（取得失敗パスと同じ扱い）。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("missing-dir").join("stop");
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "claude",
            r#"{"accounts":[{"profile":"Work","provider":"claude","ok":false,"error":"auth expired"}]}"#,
        )
        .await;
        assert!(
            result.is_err(),
            "ok:false で stop_file 作成失敗時はエラーを返すべき"
        );
        assert!(!stop.exists());
    }

    #[tokio::test]
    async fn usage_gate_continues_when_entry_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let (result, _) = gate_with(
            tmp.path(),
            &stop,
            "claude",
            r#"{"accounts":[{"profile":"Other","provider":"claude","ok":true,"weekly":{"used_percent":99.0}}]}"#,
        )
        .await;
        result.unwrap();
        assert!(!stop.exists(), "該当エントリ無しは続行（過剰停止しない）");
    }

    #[tokio::test]
    async fn usage_gate_noop_when_stop_file_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        std::fs::write(&stop, "pre-existing").unwrap();
        let fake = FakeAiUsage::new(tmp.path(), "{}", false);
        // stop_file が既存なら ai-usage を呼ばず即 return（fail-closed もしない）。
        run_usage_gate("Work", "claude", 90, &stop, &fake.command)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&stop).unwrap(), "pre-existing");
        assert!(fake.calls().is_empty(), "ai-usage を呼んではいけない");
    }
}
