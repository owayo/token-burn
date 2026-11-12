//! 一時停止中の回復の確認と早期解除。
//!
//! pause file の `resume_at` は、根拠の観測から見積もった回復時刻にすぎない。利用者は
//! 手動リセット（claude.ai の "Reset for free"、Codex の banked reset）で枠を任意の時刻に
//! 戻せるため、見積もりまで待つだけだと、戻った枠を何時間も使わずに過ごす（実例: 23:52 に
//! 5 時間枠 100% で止まり、直後に手動リセットしたのに 01:30 まで待ち続けた）。
//!
//! ai-usage 連携がある構成では、一時停止中にここが実行中のアカウントだけを ai-usage の
//! 共有キャッシュを通さずに（`--refresh`）取り直し、根拠の枠が回復したと判定できたら
//! （[`rate_control::check_recovery`]）一時停止を早期解除する（[`rate_control::lift_pause`]）。
//! 待機中のワーカー（gate-claim）は pause file の世代の変化を毎秒読み、解除を見て再開する。
//!
//! 取得をモニター（1 プロセス）に置くのは、ワーカー数ぶん重複させず、claude.ai への
//! 問い合わせを 1 本にまとめるため。取得の失敗・判定できないデータ・まだ回復していない
//! ときは何もしない（fail-soft）。モニターが死んでも、ワーカーは見積もった時刻まで待つだけで、
//! 安全側に倒れる。
//!
//! 解除を読んだワーカーは、再開の前に usage-gate で使用率の裏を取る。usage-gate も
//! `--refresh` で取り直すので、解除の根拠と同じ新しさのデータで判定する（statusline の
//! 表示用キャッシュは読まない。あれは ai-usage の共有キャッシュを通った値で、解除の時点では
//! 手動リセットより前の値のままのことがある）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use super::Event;
use super::manifest::AiUsageCommands;
use super::process::Runner;
use crate::rate_control::{self, PauseSource, PauseState, Recovery};

/// 確認の間隔と期限。テストでは短くする。
#[derive(Debug, Clone, Copy)]
struct Timing {
    /// pause file を読む間隔。確かめられるようになってから最初の確認までの遅れはこれだけ
    /// （書かれた直後の一時停止は、根拠の観測から `LIVE_FRESHNESS_MARGIN_SECS` が過ぎてから
    /// 確かめる。[`Checker::poll`]）。
    poll: Duration,
    /// 取得が終わってから次の取得を始めるまでの間隔。開始ではなく完了から数える
    /// （取得が長引いたときに休みなく取らない。monitor::usage と同じ考え方）。
    ///
    /// 一時停止が替わっても数え直さない。早期解除の直後に止まり直した場合（再開前の
    /// 再検証が解除前の値を読んだ等）に、解除と停止の繰り返しへ合わせて claude.ai へ
    /// 問い合わせ続けないため。
    interval: Duration,
    /// 1 回の取得の期限。
    timeout: Duration,
    /// 解除の書き込みでロックを待つ上限。
    lock_timeout: Duration,
}

const TIMING: Timing = Timing {
    poll: Duration::from_secs(1),
    interval: Duration::from_secs(30),
    timeout: super::usage::TIMEOUT,
    // ロックを握る処理（pause の読み書きと claim）は短いので通常は待たない。取れなければ
    // 解除せずに次の確認へ回すだけ（待ち続けるだけで安全側）。
    lock_timeout: super::STOP_LOCK_TIMEOUT,
};

/// 表示に添える説明の上限（文字数）。ai-usage のエラー文は長くなり得るので、通知の欄を
/// 埋め尽くさないように切る。
const MAX_DETAIL: usize = 120;

/// ai-usage が `--refresh` 等を知らない（古い版）ため確認をやめたときに、モニター自身の
/// 警告として出す文言。
pub const UNSUPPORTED_ALERT: &str = "ai-usage does not support --refresh, so a manual reset \
     will not be detected while paused (update ai-usage)";

/// 確認の結果。メインのループへ送る。
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// 確かめたが解除しなかった（まだ回復していない・判定できない・取得に失敗した）。
    Checked(Check),
    /// 一時停止を早期解除した。通知は pause file の解除の記録からコントローラーが組み立てる。
    Lifted { window: String, detail: String },
    /// ai-usage が `--refresh` / `--profile` / `--only` を知らない（clap の使い方の誤りで
    /// 終了コード 2）。この実行の残りでは確かめない。
    Unsupported,
}

/// 解除に至らなかった確認の 1 回分（表示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// 確認を終えた時刻（Unix epoch 秒）。
    pub at: i64,
    /// どの一時停止についての確認か（[`PauseState::since`]）。別の一時停止に古い結果を
    /// 添えないよう、表示はこれが一致するときだけにする。
    pub since: i64,
    /// [`rate_control::check_recovery`] の説明か、取得に失敗した理由。
    pub detail: String,
}

/// 回復の確認の表示用の状態。メインのループが持つ。
#[derive(Debug, Default)]
pub struct Status {
    /// 回復を確かめている（ai-usage 連携があり、ai-usage が `--refresh` に対応している）。
    /// 確かめていないときに「リセットされれば早めに再開」と案内しない。
    pub active: bool,
    /// 直近の確認のうち、解除に至らなかったもの。
    pub last: Option<Check>,
}

impl Status {
    pub fn new(active: bool) -> Self {
        Self { active, last: None }
    }

    /// 確認の結果を反映する。モニター自身の警告として出すことがあれば返す。
    pub fn apply(&mut self, update: Update) -> Option<String> {
        match update {
            Update::Checked(check) => {
                self.last = Some(check);
                None
            }
            Update::Lifted { .. } => {
                self.last = None;
                None
            }
            Update::Unsupported => {
                self.active = false;
                self.last = None;
                Some(UNSUPPORTED_ALERT.to_string())
            }
        }
    }
}

/// 実データを取り直しても早期解除できない一時停止なら、その理由。
///
/// 判定の本体は [`rate_control::check_recovery`] で、そのうち取り直したデータに関係なく
/// 一時停止だけで決まる条件をここで先に見る。結果の分かっている問い合わせを claude.ai へ
/// 送らないためと、「リセットされれば早めに再開」と案内してよいかを決めるため。判定の本体と
/// 食い違っても、取り直して判定できないか、取り直さずに待つかのどちらかで、解除を誤って
/// 作ることはない。
pub fn not_liftable(pause: &PauseState) -> Option<&'static str> {
    let Some(basis) = pause.used_percent else {
        // 旧形式の pause file や、根拠の使用率を持たない一時停止。比べる元が無い。
        return Some("the pause has no recorded usage");
    };
    if !rate_control::is_recovery_drop(basis, 0.0) {
        // 根拠の使用率が低い（例: 8% で拒否された）。取り直して得られる最良の値の 0% まで
        // 下がっても、回復と認める下がり幅に届かない。
        return Some("the recorded usage is too low to confirm a reset");
    }
    if pause.relapse && pause.source != PauseSource::AiUsage {
        // 早期解除の直後に API がまた拒否した。ai-usage が別アカウントを見ている構成では、
        // 同じ証拠で解除すると拒否と解除を繰り返す。
        return Some("the API rejected requests again right after an early resume");
    }
    None
}

/// 一時停止のあいだ回復を確かめ続ける常駐タスク。ai-usage 連携があるときだけ起動する。
///
/// 取得は別タスクで行うので、待っている間も停止判定・Ctrl-C・描画は止まらない。
pub async fn run(
    commands: AiUsageCommands,
    stop_file: PathBuf,
    threshold: u8,
    events: mpsc::Sender<Event>,
) {
    let checker = Checker::new(commands, stop_file, threshold, TIMING);
    run_with(checker, events).await;
}

async fn run_with(mut checker: Checker, events: mpsc::Sender<Event>) {
    let mut poll = tokio::time::interval(checker.timing.poll);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        poll.tick().await;
        let Some(update) = checker.poll().await else {
            continue;
        };
        // 古い ai-usage は何度試しても変わらないので、この実行の残りでは確かめない
        // （monitor::disk が古い ai-usage を statvfs へ切り替えるのと同じ考え方）。
        let unsupported = update == Update::Unsupported;
        if events.send(Event::Recovery(update)).await.is_err() || unsupported {
            return;
        }
    }
}

struct Checker {
    commands: AiUsageCommands,
    stop_file: PathBuf,
    pause_file: PathBuf,
    threshold: u8,
    timing: Timing,
    runner: Runner,
    /// 直近の取得が終わった時刻。
    last_fetch: Option<Instant>,
}

impl Checker {
    fn new(commands: AiUsageCommands, stop_file: PathBuf, threshold: u8, timing: Timing) -> Self {
        Self {
            pause_file: rate_control::pause_path_for(&stop_file),
            commands,
            stop_file,
            threshold,
            timing,
            runner: Runner::default(),
            last_fetch: None,
        }
    }

    /// pause file を読み、効いている一時停止があれば回復を確かめる。送る結果があれば返す。
    async fn poll(&mut self) -> Option<Update> {
        // 恒久停止の後は、一時停止を解いてもワーカーは止まる（stop file が先に効く）。
        // 実行が終わるまで問い合わせ続けない。
        if super::controller::signal_present(&self.stop_file) {
            return None;
        }
        // 無い・読めない・壊れているときは確かめない（壊れた場合の扱いはワーカーが決める）。
        let pause = rate_control::read_pause(&self.pause_file).ok().flatten()?;
        let now = rate_control::now_epoch();
        if !pause.blocks(now) || not_liftable(&pause).is_some() {
            return None;
        }
        // 取り直したデータの取得時刻の下限は「取得開始 − LIVE_FRESHNESS_MARGIN_SECS」
        // （[`Self::check`]）。それが根拠の観測より前になるうちは、取っても check_recovery が
        // 必ず判定できない。書かれた直後の一時停止を見つけてすぐ取ると、claude.ai への
        // 問い合わせが無駄になり、早期再開も次の取得まで遅れる。取得の間隔の数え始めには
        // しないので、条件を満たした次の確認ですぐ取る（前回の実行の中断記録から保留した
        // 一時停止は根拠が前回の中断の時刻なので、見つけたらすぐ確かめる）。
        if now - rate_control::LIVE_FRESHNESS_MARGIN_SECS < pause.observed_at {
            return None;
        }
        if self
            .last_fetch
            .is_some_and(|done| done.elapsed() < self.timing.interval)
        {
            return None;
        }
        let update = self.check().await;
        self.last_fetch = Some(Instant::now());
        update
    }

    /// 実データを取り直し、回復していれば解除する。
    async fn check(&mut self) -> Option<Update> {
        let started = rate_control::now_epoch();
        let live = match self
            .runner
            .run(&self.commands.live, self.timing.timeout)
            .await
        {
            Ok(output) if output.status.code() == Some(2) => return Some(Update::Unsupported),
            Ok(output) if output.status.success() => crate::usage::live_observation(
                &output.stdout,
                &self.commands.profile,
                &self.commands.provider,
                // `--refresh` は、ほかのプロセスが先に始めた取得に合流することがある。
                // 取得された時刻の下限は、こちらの開始よりその分だけ前になり得る。
                started - rate_control::LIVE_FRESHNESS_MARGIN_SECS,
                true,
            )
            // 回復の確認では、どちらの理由でも解除しない（待ち続けるだけ）。
            .map_err(|error| error.to_string()),
            Ok(output) => Err(format!("ai-usage failed ({})", output.status)),
            Err(error) => Err(format!("ai-usage {error}")),
        };

        // 取得の間に延長・再停止・解除され得るので、判定は読み直した状態で行う。
        // 解除済み・期限切れになっていれば、知らせることは無い。
        let pause = rate_control::read_pause(&self.pause_file).ok().flatten()?;
        let now = rate_control::now_epoch();
        if !pause.blocks(now) {
            return None;
        }
        let live = match live {
            Ok(live) => live,
            Err(reason) => return Some(checked(&pause, now, &reason)),
        };
        match rate_control::check_recovery(&pause, &live, self.threshold, now) {
            Recovery::Recovered(detail) => Some(self.lift(pause, detail).await),
            Recovery::NotYet(detail) | Recovery::Undecidable(detail) => {
                Some(checked(&pause, now, &detail))
            }
        }
    }

    /// 判定に使った世代のままのときだけ解除する（判定の最中に届いた新しい拒否を消さない）。
    async fn lift(&mut self, pause: PauseState, detail: String) -> Update {
        let path = self.pause_file.clone();
        let reason = detail.clone();
        let revision = pause.revision;
        let lock_timeout = self.timing.lock_timeout;
        // lift_pause は同期 I/O でロックを待つので、ランタイムのスレッドを塞がないよう
        // 別スレッドで呼ぶ（rate_control::write_stop_within の呼び方と同じ）。
        let lifted = tokio::task::spawn_blocking(move || {
            rate_control::lift_pause(&path, revision, &reason, lock_timeout)
                .map_err(|error| error.to_string())
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        let now = rate_control::now_epoch();
        match lifted {
            Ok(true) => Update::Lifted {
                window: pause.window,
                detail,
            },
            // 世代が変わった・ロックを取れなかった。次の確認で取り直す。
            Ok(false) => checked(
                &pause,
                now,
                &format!("{detail} (not lifted: the pause changed or was locked)"),
            ),
            Err(error) => checked(&pause, now, &format!("{detail} (failed to lift: {error})")),
        }
    }
}

/// 解除に至らなかった確認の結果。
fn checked(pause: &PauseState, now: i64, detail: &str) -> Update {
    Update::Checked(Check {
        at: now,
        since: pause.since,
        detail: tidy(detail),
    })
}

/// 表示用に 1 行へ畳み、エスケープシーケンスと制御文字を除き、長ければ切る。
/// ai-usage のエラー文（`error`）をそのままペインへ流さない。
fn tidy(text: &str) -> String {
    let line = text
        .lines()
        .map(super::ansi::strip)
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if line.chars().count() <= MAX_DETAIL {
        return line;
    }
    let mut cut: String = line.chars().take(MAX_DETAIL - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::rate_control::{PauseCause, read_pause, write_pause};

    fn quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', r"'\''"))
    }

    fn quote_path(path: &Path) -> String {
        quote(&path.to_string_lossy())
    }

    /// 実行中のアカウント（Work / claude）の `ai-usage --json` の出力。
    fn usage_json(five_hour: f64) -> String {
        format!(
            r#"{{"accounts":[{{"profile":"Work","provider":"claude","ok":true,"five_hour":{{"kind":"five_hour","used_percent":{five_hour},"resets_at":null}},"weekly":{{"kind":"weekly","used_percent":12.0,"resets_at":"2026-10-27T00:00:00+00:00"}},"error":null}}]}}"#
        )
    }

    /// 取得のたびに `marker` へ 1 行足してから `body` を実行するコマンド。
    fn counted(marker: &Path, body: &str) -> String {
        format!("echo fetch >> {}; {body}", quote_path(marker))
    }

    fn prints(json: &str) -> String {
        format!("printf '%s' {}", quote(json))
    }

    fn fetches(marker: &Path) -> usize {
        std::fs::read_to_string(marker)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn timing(interval: Duration) -> Timing {
        Timing {
            poll: Duration::from_millis(10),
            interval,
            timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        }
    }

    /// `live` で回復を確かめる Checker。表示用のコマンドは使わないので何もしない。
    fn checker(stop: &Path, live: String, timing: Timing) -> Checker {
        let commands = AiUsageCommands {
            refresh: "false".to_string(),
            statusline: String::new(),
            disk: String::new(),
            live,
            profile: "Work".to_string(),
            provider: "claude".to_string(),
            cache_file: stop.with_file_name("cache.json"),
        };
        Checker::new(commands, stop.to_path_buf(), 95, timing)
    }

    /// API の拒否で始まった一時停止（実際に起きた事故と同じ 5 時間枠）。根拠の観測は
    /// 1 分前で、取り直したデータ（取得開始の 35 秒前以降）より古い。
    fn api_pause(used: f64) -> PauseState {
        let now = rate_control::now_epoch();
        PauseState::new(
            now + 3600,
            "five_hour",
            "request rejected (five_hour); five_hour at 100%",
        )
        .observed(PauseSource::Api, PauseCause::Rejected, now - 60)
        .with_basis(used, Some(now + 3570))
    }

    fn paused(pause_file: &Path, state: &PauseState) -> PauseState {
        write_pause(pause_file, state).unwrap();
        read_pause(pause_file).unwrap().unwrap()
    }

    /// 取得の最中に pause file へ書かれる内容を、別のファイル（`staged`）に前もって作る。
    /// 取得のコマンドがこれを `cp` で pause file へ置き換える。
    ///
    /// 本番では新しい観測は同じ pause file へ書かれ、世代は「直前の世代 + 1」以上に進む。
    /// それと同じになるよう、いまの pause file を写してから書く。空のファイルへ書くと、
    /// 世代は現在時刻（ミリ秒）だけで決まる。fsync の速い環境（Linux の CI）では 2 回の
    /// 書き込みが同じミリ秒に収まり、元の世代と同じ値になっていた。
    fn staged_rewrite(pause_file: &Path, staged: &Path, state: &PauseState) -> PauseState {
        std::fs::copy(pause_file, staged).unwrap();
        paused(staged, state)
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let pause_file = rate_control::pause_path_for(&stop);
        let marker = tmp.path().join("fetches");
        (tmp, stop, pause_file, marker)
    }

    #[tokio::test]
    async fn a_manual_reset_lifts_the_pause() {
        // 実際に起きた事故の再現: 5 時間枠 100% で止まった後、利用者が手動リセットした。
        let (_tmp, stop, pause_file, marker) = setup();
        paused(&pause_file, &api_pause(100.0));
        let mut checker = checker(
            &stop,
            counted(&marker, &prints(&usage_json(0.0))),
            timing(Duration::ZERO),
        );

        let update = checker.poll().await;
        let Some(Update::Lifted { window, detail }) = update else {
            panic!("expected a lift: {update:?}");
        };
        assert_eq!(window, "five_hour");
        assert_eq!(detail, "five_hour dropped from 100% to 0%");
        let state = read_pause(&pause_file).unwrap().unwrap();
        assert!(state.is_lifted());
        assert!(!state.blocks(rate_control::now_epoch()));
        assert_eq!(state.lift_reason.as_deref(), Some(detail.as_str()));
        assert_eq!(fetches(&marker), 1);

        // 解除済みの一時停止では、もう取り直さない。
        assert_eq!(checker.poll().await, None);
        assert_eq!(fetches(&marker), 1);
    }

    #[tokio::test]
    async fn the_display_cache_is_left_to_the_usage_task() {
        // 表示用キャッシュ（全アカウント分）は使用量の表示が 10 秒ごとに更新する。回復の
        // 確認は 1 アカウント分の取得なので、それで置き換えない（ほかのアカウントの表示が
        // 消える）。再開前の再検証（usage-gate）は `--refresh` で取り直すので、解除の前に
        // 表示用キャッシュを新しくしておく必要も無い。
        let (_tmp, stop, pause_file, marker) = setup();
        paused(&pause_file, &api_pause(100.0));
        let cache = stop.with_file_name("cache.json");
        std::fs::write(&cache, r#"{"accounts":[]}"#).unwrap();
        let mut checker = checker(
            &stop,
            counted(&marker, &prints(&usage_json(0.0))),
            timing(Duration::ZERO),
        );

        assert!(matches!(checker.poll().await, Some(Update::Lifted { .. })));
        assert_eq!(
            std::fs::read_to_string(&cache).unwrap(),
            r#"{"accounts":[]}"#
        );
        assert!(read_pause(&pause_file).unwrap().unwrap().is_lifted());
    }

    #[tokio::test]
    async fn nothing_is_fetched_without_a_pause_that_could_be_lifted() {
        let (_tmp, stop, pause_file, marker) = setup();
        let mut checker = checker(
            &stop,
            counted(&marker, &prints(&usage_json(0.0))),
            timing(Duration::ZERO),
        );
        let now = rate_control::now_epoch();

        // 一時停止が無い。
        assert_eq!(checker.poll().await, None);

        // 見積もりの時刻を過ぎた一時停止（ワーカーはもう待っていない）。
        let mut expired = api_pause(100.0);
        expired.resume_at = now - 10;
        paused(&pause_file, &expired);
        assert_eq!(checker.poll().await, None);
        std::fs::remove_file(&pause_file).unwrap();

        // 根拠の使用率が無い一時停止（比べる元が無いので、取り直しても判定できない）。
        paused(
            &pause_file,
            &PauseState::new(now + 3600, "five_hour", "held from a previous run"),
        );
        assert_eq!(checker.poll().await, None);
        std::fs::remove_file(&pause_file).unwrap();

        // 早期解除の直後に API がまた拒否した一時停止（再発の印は write_pause が付ける）。
        let lifted = paused(&pause_file, &api_pause(100.0));
        assert!(
            rate_control::lift_pause(
                &pause_file,
                lifted.revision,
                "reset",
                Duration::from_secs(1)
            )
            .unwrap()
        );
        // 解除より後の観測でないと、write_pause は古いデータとして書かない。
        let rejected_at = rate_control::now_epoch() + 1;
        let state = paused(
            &pause_file,
            &PauseState::new(now + 3600, "five_hour", "request rejected (five_hour)")
                .observed(PauseSource::Api, PauseCause::Rejected, rejected_at)
                .with_basis(100.0, Some(now + 3570)),
        );
        assert!(state.relapse && state.blocks(now));
        assert_eq!(checker.poll().await, None);
        std::fs::remove_file(&pause_file).unwrap();

        // 恒久停止の後（一時停止を解いてもワーカーは止まる）。
        paused(&pause_file, &api_pause(100.0));
        std::fs::write(&stop, "interrupted").unwrap();
        assert_eq!(checker.poll().await, None);

        assert_eq!(fetches(&marker), 0);
    }

    #[tokio::test]
    async fn the_pause_stays_until_the_usage_really_recovers() {
        let (_tmp, stop, pause_file, marker) = setup();
        paused(&pause_file, &api_pause(100.0));
        let cases = [
            // まだ閾値を超えている。
            (prints(&usage_json(99.0)), "five_hour at 99%"),
            // 取得の失敗。
            (
                "exit 1".to_string(),
                if cfg!(windows) {
                    "ai-usage failed (exit code: 1)"
                } else {
                    "ai-usage failed (exit status: 1)"
                },
            ),
            (
                "printf 'not json'".to_string(),
                "cannot parse the ai-usage output",
            ),
            (
                prints(r#"{"accounts":[]}"#),
                "no ai-usage entry for (Work, claude)",
            ),
            (
                prints(
                    r#"{"accounts":[{"profile":"Work","provider":"claude","ok":false,"error":"session expired\n\u001b[31mlog in again\u001b[0m"}]}"#,
                ),
                "session expired log in again",
            ),
        ];
        for (command, expected) in cases {
            let mut checker = checker(&stop, counted(&marker, &command), timing(Duration::ZERO));
            let update = checker.poll().await;
            let Some(Update::Checked(check)) = &update else {
                panic!("{command}: expected a check: {update:?}");
            };
            assert!(
                check.detail.contains(expected),
                "{command}: {:?}",
                check.detail
            );
            let state = read_pause(&pause_file).unwrap().unwrap();
            assert!(state.blocks(rate_control::now_epoch()), "{command}");
            assert_eq!(check.since, state.since);
        }
        assert_eq!(fetches(&marker), 5);
    }

    #[tokio::test]
    async fn a_hanging_fetch_is_abandoned_without_lifting() {
        let (_tmp, stop, pause_file, _marker) = setup();
        paused(&pause_file, &api_pause(100.0));
        let mut checker = checker(
            &stop,
            "sleep 30".to_string(),
            Timing {
                timeout: Duration::from_millis(200),
                ..timing(Duration::ZERO)
            },
        );
        let update = checker.poll().await;
        let Some(Update::Checked(check)) = &update else {
            panic!("expected a check: {update:?}");
        };
        assert_eq!(check.detail, "ai-usage timed out");
        assert!(
            read_pause(&pause_file)
                .unwrap()
                .unwrap()
                .blocks(rate_control::now_epoch())
        );
    }

    #[tokio::test]
    async fn a_rejection_recorded_during_the_fetch_prevents_the_lift() {
        // 取得の最中に新しい拒否が届いた（世代と根拠の観測時刻が進んだ）。取得したデータは
        // その拒否より前のものかもしれないので、解除しない。
        let (tmp, stop, pause_file, marker) = setup();
        let before = paused(&pause_file, &api_pause(100.0));
        let newer = tmp.path().join("newer-pause.json");
        let now = rate_control::now_epoch();
        staged_rewrite(
            &pause_file,
            &newer,
            &PauseState::new(now + 3600, "five_hour", "rejected again")
                .observed(PauseSource::Api, PauseCause::Rejected, now)
                .with_basis(100.0, Some(now + 3570)),
        );
        let command = format!(
            "cp {} {}; {}",
            quote_path(&newer),
            quote_path(&pause_file),
            prints(&usage_json(0.0))
        );
        let mut checker = checker(&stop, counted(&marker, &command), timing(Duration::ZERO));

        let update = checker.poll().await;
        let Some(Update::Checked(check)) = &update else {
            panic!("expected a check: {update:?}");
        };
        assert_eq!(check.detail, "the usage may predate the pause");
        let state = read_pause(&pause_file).unwrap().unwrap();
        assert!(state.revision > before.revision);
        assert!(state.observed_at > before.observed_at);
        assert!(!state.is_lifted());
        assert!(state.blocks(rate_control::now_epoch()));
    }

    #[tokio::test]
    async fn the_lift_uses_the_state_read_after_the_fetch() {
        // 取得の間に書き直されたが、根拠は同じまま（観測時刻が進んでいない）。読み直した
        // 世代で解除する（取得前の世代のままだと、書き直しのたびに解除できない）。
        let (tmp, stop, pause_file, marker) = setup();
        let state = api_pause(100.0);
        let before = paused(&pause_file, &state);
        let rewritten = tmp.path().join("rewritten-pause.json");
        let after = staged_rewrite(&pause_file, &rewritten, &state);
        assert!(after.revision > before.revision);
        assert_eq!(after.observed_at, before.observed_at);
        let command = format!(
            "cp {} {}; {}",
            quote_path(&rewritten),
            quote_path(&pause_file),
            prints(&usage_json(0.0))
        );
        let mut checker = checker(&stop, counted(&marker, &command), timing(Duration::ZERO));

        let update = checker.poll().await;
        assert!(
            matches!(update, Some(Update::Lifted { .. })),
            "expected a lift: {update:?}"
        );
        assert!(read_pause(&pause_file).unwrap().unwrap().is_lifted());
    }

    #[tokio::test]
    async fn checks_again_only_after_the_interval_from_the_last_fetch() {
        let (_tmp, stop, pause_file, marker) = setup();
        paused(&pause_file, &api_pause(100.0));
        let mut checker = checker(
            &stop,
            counted(&marker, &prints(&usage_json(99.0))),
            timing(Duration::from_millis(300)),
        );
        // 一時停止を見つけたらすぐ確かめる。
        assert!(matches!(checker.poll().await, Some(Update::Checked(_))));
        assert_eq!(fetches(&marker), 1);
        // 取得の完了から間隔が空くまでは取り直さない。
        assert_eq!(checker.poll().await, None);
        assert_eq!(fetches(&marker), 1);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(matches!(checker.poll().await, Some(Update::Checked(_))));
        assert_eq!(fetches(&marker), 2);
    }

    #[tokio::test]
    async fn an_old_ai_usage_stops_the_checks_for_the_rest_of_the_run() {
        // `--refresh` を知らない ai-usage は clap の使い方の誤りで終了コード 2 を返す。
        let (_tmp, stop, pause_file, marker) = setup();
        paused(&pause_file, &api_pause(100.0));
        let command = counted(
            &marker,
            "echo \"error: unexpected argument '--refresh' found\" >&2; exit 2",
        );
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(run_with(
            checker(&stop, command, timing(Duration::ZERO)),
            tx,
        ));
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, Event::Recovery(Update::Unsupported)));
        // 一度伝えたら確認をやめる（タスクが終わり、それ以上取り直さない）。
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the checks must stop")
            .unwrap();
        assert_eq!(fetches(&marker), 1);
        assert!(
            read_pause(&pause_file)
                .unwrap()
                .unwrap()
                .blocks(rate_control::now_epoch())
        );
    }

    #[tokio::test]
    async fn the_loop_reports_the_lift_to_the_monitor() {
        let (_tmp, stop, pause_file, marker) = setup();
        paused(&pause_file, &api_pause(100.0));
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(run_with(
            checker(
                &stop,
                counted(&marker, &prints(&usage_json(0.0))),
                timing(Duration::from_secs(30)),
            ),
            tx,
        ));
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        let Event::Recovery(Update::Lifted { window, .. }) = event else {
            panic!("unexpected event");
        };
        assert_eq!(window, "five_hour");
        assert!(read_pause(&pause_file).unwrap().unwrap().is_lifted());
    }

    #[test]
    fn the_status_keeps_the_last_check_and_warns_once_about_an_old_ai_usage() {
        let mut status = Status::new(true);
        let check = Check {
            at: 100,
            since: 50,
            detail: "five_hour at 99% (was 100% when paused)".to_string(),
        };
        assert_eq!(status.apply(Update::Checked(check.clone())), None);
        assert_eq!(status.last, Some(check));
        assert_eq!(
            status.apply(Update::Lifted {
                window: "five_hour".into(),
                detail: "five_hour dropped from 100% to 0%".into(),
            }),
            None
        );
        assert_eq!(status.last, None);
        assert_eq!(
            status.apply(Update::Unsupported).as_deref(),
            Some(UNSUPPORTED_ALERT)
        );
        assert!(!status.active, "確かめないなら早めの再開を案内しない");
        assert!(UNSUPPORTED_ALERT.contains("--refresh"));
    }

    #[test]
    fn details_are_one_short_line_without_escape_sequences() {
        assert_eq!(tidy("a\n\x1b[31mb\x1b[0m\t c"), "a b c");
        let long = "x".repeat(MAX_DETAIL * 2);
        let cut = tidy(&long);
        assert_eq!(cut.chars().count(), MAX_DETAIL);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn pauses_without_a_comparable_basis_are_not_liftable() {
        let now = rate_control::now_epoch();
        assert_eq!(not_liftable(&api_pause(100.0)), None);
        assert!(not_liftable(&PauseState::new(now + 60, "five_hour", "legacy")).is_some());
        let mut relapse = api_pause(100.0);
        relapse.relapse = true;
        assert!(not_liftable(&relapse).is_some());
        // ai-usage 自身が書いた再発なら、同じ取得元どうしの比較なので確かめる。
        relapse.source = PauseSource::AiUsage;
        assert_eq!(not_liftable(&relapse), None);
    }

    /// 根拠の観測時刻だけを変えた、API の拒否で始まった一時停止。
    fn rejected_at(observed_at: i64) -> PauseState {
        let now = rate_control::now_epoch();
        PauseState::new(
            now + 3600,
            "five_hour",
            "request rejected (five_hour); five_hour at 100%",
        )
        .observed(PauseSource::Api, PauseCause::Rejected, observed_at)
        .with_basis(100.0, Some(now + 3570))
    }

    #[tokio::test]
    async fn a_fresh_pause_is_checked_once_fetched_data_can_postdate_it() {
        // 取り直したデータの取得時刻の下限は「取得開始 − LIVE_FRESHNESS_MARGIN_SECS」。
        // 書かれた直後の一時停止では、取っても check_recovery が必ず判定できない
        // （the usage may predate the pause）ので、claude.ai へ問い合わせない。
        let (_tmp, stop, pause_file, marker) = setup();
        let now = rate_control::now_epoch();
        paused(&pause_file, &rejected_at(now));
        // 取得の間隔は長くしておく。見送った確認が間隔の数え始めになっていれば、
        // 次の取得は 1 時間後まで行われない。
        let mut checker = checker(
            &stop,
            counted(&marker, &prints(&usage_json(0.0))),
            timing(Duration::from_secs(3600)),
        );
        assert_eq!(checker.poll().await, None);
        assert_eq!(fetches(&marker), 0);

        // 根拠の観測から余裕の秒数が過ぎた（取り直すデータは根拠より後のものと言える）。
        // 次の確認ですぐ取得し、判定できる。
        std::fs::remove_file(&pause_file).unwrap();
        paused(
            &pause_file,
            &rejected_at(now - rate_control::LIVE_FRESHNESS_MARGIN_SECS),
        );
        let update = checker.poll().await;
        assert!(matches!(update, Some(Update::Lifted { .. })), "{update:?}");
        assert_eq!(fetches(&marker), 1);
    }

    #[tokio::test]
    async fn the_loop_fetches_on_the_next_poll_once_the_basis_is_old_enough() {
        // 見送った後の確認は、取得の間隔（30 秒）ではなく毎回の poll で判定し直す。
        let (_tmp, stop, pause_file, marker) = setup();
        let now = rate_control::now_epoch();
        paused(&pause_file, &rejected_at(now));
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(run_with(
            checker(
                &stop,
                counted(&marker, &prints(&usage_json(0.0))),
                timing(Duration::from_secs(3600)),
            ),
            tx,
        ));
        // poll は 10ms ごと。何度確かめても、根拠の観測から余裕の秒数が過ぎるまでは取らない。
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fetches(&marker), 0);
        assert!(rx.try_recv().is_err());

        std::fs::remove_file(&pause_file).unwrap();
        paused(
            &pause_file,
            &rejected_at(now - rate_control::LIVE_FRESHNESS_MARGIN_SECS),
        );
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(
            matches!(event, Event::Recovery(Update::Lifted { .. })),
            "unexpected event"
        );
        assert_eq!(fetches(&marker), 1);
    }

    #[tokio::test]
    async fn a_pause_held_from_a_previous_run_is_checked_at_once() {
        // 前回の実行で中断したセッションの再開時刻まで保留した一時停止（source=saved）。
        // 根拠の観測は前回の中断の時刻なので、見つけたらすぐ確かめる。
        let (_tmp, stop, pause_file, marker) = setup();
        let now = rate_control::now_epoch();
        paused(
            &pause_file,
            &PauseState::new(
                now + 3600,
                "five_hour",
                "a resumed session was rate limited (five_hour)",
            )
            .observed(PauseSource::Saved, PauseCause::Rejected, now - 3600)
            .with_basis(100.0, Some(now + 3570)),
        );
        let mut checker = checker(
            &stop,
            counted(&marker, &prints(&usage_json(0.0))),
            timing(Duration::from_secs(3600)),
        );
        let update = checker.poll().await;
        assert!(matches!(update, Some(Update::Lifted { .. })), "{update:?}");
        assert_eq!(fetches(&marker), 1);
    }

    #[test]
    fn not_liftable_agrees_with_check_recovery_on_the_recorded_usage() {
        // 根拠の使用率が RECOVERY_DROP_POINTS 未満だと、0%（取り直して得られる最良の値）まで
        // 下がっても回復とは判定されない。そうした一時停止を「解除できない」と先に判定する。
        let now = rate_control::now_epoch();
        let reset = rate_control::LiveObservation {
            windows: vec![rate_control::WindowObservation::new(
                rate_control::WindowKind::FIVE_HOUR,
                "five_hour",
                0.0,
            )],
            observed_from: now,
            fresh: true,
        };
        for basis in [0.0, 5.0, 8.0, 9.9, 10.0, 10.5, 42.0, 100.0] {
            let pause = api_pause(basis);
            let recovered = matches!(
                rate_control::check_recovery(&pause, &reset, 95, now),
                Recovery::Recovered(_)
            );
            assert_eq!(
                not_liftable(&pause).is_none(),
                recovered,
                "basis {basis}%: {:?}",
                not_liftable(&pause)
            );
        }
        assert!(not_liftable(&api_pause(8.0)).is_some());
        assert_eq!(not_liftable(&api_pause(10.0)), None);
    }

    #[tokio::test]
    async fn nothing_is_fetched_for_a_pause_recorded_at_a_low_usage() {
        // 8% で拒否された一時停止。取り直しても解除されることが無いので問い合わせない
        // （一時停止のあいだ 30 秒ごとに claude.ai へ問い合わせ続けない）。
        let (_tmp, stop, pause_file, marker) = setup();
        paused(&pause_file, &api_pause(8.0));
        let mut checker = checker(
            &stop,
            counted(&marker, &prints(&usage_json(0.0))),
            timing(Duration::ZERO),
        );
        assert_eq!(checker.poll().await, None);
        assert_eq!(fetches(&marker), 0);
    }
}
