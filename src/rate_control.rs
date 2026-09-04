//! レート制限による実行停止を、枠の周期に応じて「一時停止（pause）」と
//! 「恒久停止（stop）」に分けて表現するモジュール。
//!
//! 停止シグナルが stop file 1 個（作られたら二度と再開しない）だった頃は、5 時間枠のように
//! 短周期でリセットされる枠が閾値に触れただけで、週次デッドラインまでの実行余力を丸ごと
//! 捨てていた。実ログでは 5 時間枠が 90%（閾値ちょうど）に達して停止した数分後にその枠が
//! リセットされ、以降 30 分以上リクエストが通り続けたにもかかわらず、残りのタスクが 1 件も
//! 実行されないまま終了している（週次枠は 43%、デッドラインまで 4 時間 52 分残っていた）。
//!
//! そこで停止理由を枠の周期で分ける:
//!
//! - **週次枠**（`seven_day` / `weekly`）はデッドラインと同じ周期なので、待っても回復しない
//!   → 恒久停止（stop file）
//! - **5 時間枠**（`five_hour`）はその枠のリセットで回復する
//!   → リセット時刻まで一時停止（pause file）。時刻を過ぎていれば再開する
//!
//! 判定は `stream-json` の `rate_limit_event` 経路と `usage-gate` 経路の両方から使う。
//! 単位（前者は 0.0〜1.0 の `utilization`、後者は 0〜100 の `used_percent`）と枠名の違いは
//! 呼び出し側で [`WindowObservation`] へ正規化してから渡す。

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// リセット時刻を過ぎてから実際に枠が空くまでのラグを吸収する猶予（秒）。
///
/// サーバ側の集計が反映されるまでの数秒で再開すると、再開直後に同じ枠でまた閾値へ
/// 触れて一時停止を繰り返す。
pub const RESET_PROPAGATION_GRACE_SECS: i64 = 30;

/// リセット時刻の妥当性検査に許すサーバ時刻とのずれ（秒）。
///
/// 枠 1 周期ぶんより先を指す `resetsAt` は、枠の取り違え（月次の追加課金枠や週次枠の
/// 時刻を短周期枠のものとして読んだ）か壊れた値。一時停止の根拠にはせず恒久停止へ
/// 倒す（fail-closed）。
const RESET_SKEW_MARGIN_SECS: i64 = 3600;

/// 実行全体のデッドラインと同じ周期とみなす下限（＝週次）。
const DEADLINE_PERIOD_SECS: i64 = 7 * 86_400;

/// 停止判定に使う枠の種類。周期の長さが「待てば回復するか」を決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// デッドラインより短い周期でリセットされる枠。待てば回復する。
    /// `period_secs` はその枠 1 周期の長さで、リセット時刻の妥当性検査に使う。
    Short { period_secs: i64 },
    /// 実行全体のデッドライン（週次リセット）と同じかそれより長い周期の枠。
    /// これが枯れたら、この実行の残り時間では回復しない。
    Deadline,
}

impl WindowKind {
    /// 5 時間枠。
    pub const FIVE_HOUR: Self = Self::Short {
        period_secs: 5 * 3600,
    };

    /// 枠 1 周期の長さから分類する。
    ///
    /// スロット名では決めない。実データでは antigravity が `five_hour` スロットに
    /// 24 時間枠（`kind:"daily"`）を、pixellab が `weekly` スロットに月次枠を返すため、
    /// 名前で決め打ちすると周期を取り違える。
    pub fn from_period_secs(period_secs: i64) -> Self {
        if period_secs > 0 && period_secs < DEADLINE_PERIOD_SECS {
            Self::Short { period_secs }
        } else {
            Self::Deadline
        }
    }

    /// この枠のリセット時刻として妥当と認める、現在時刻からの最大の先行秒数。
    fn max_reset_ahead(self) -> i64 {
        match self {
            Self::Short { period_secs } => period_secs + RESET_SKEW_MARGIN_SECS,
            Self::Deadline => 0,
        }
    }
}

/// 1 つの枠の実測値。
#[derive(Debug, Clone)]
pub struct WindowObservation {
    pub kind: WindowKind,
    /// 表示に使う枠名（`five_hour` / `seven_day` / `weekly`）。判定には使わない。
    pub label: String,
    /// 使用率（%）。0〜100 に正規化してから渡す。
    pub used_percent: f64,
    /// その枠自身のリセット時刻（Unix epoch 秒）。読めないときは `None`。
    pub reset_at: Option<i64>,
}

impl WindowObservation {
    pub fn new(kind: WindowKind, label: impl Into<String>, used_percent: f64) -> Self {
        Self {
            kind,
            label: label.into(),
            used_percent,
            reset_at: None,
        }
    }

    pub fn with_reset_at(mut self, reset_at: Option<i64>) -> Self {
        self.reset_at = reset_at;
        self
    }

    fn basis(&self) -> Basis {
        Basis {
            window: self.label.clone(),
            used_percent: self.used_percent,
            reset_at: self.reset_at,
        }
    }
}

/// 判定の主語になった枠（表示と、stop / pause ファイルに書く理由の材料）。
#[derive(Debug, Clone, PartialEq)]
pub struct Basis {
    pub window: String,
    pub used_percent: f64,
    /// その枠自身のリセット時刻。top-level の `resetsAt` は `rateLimitType` が指す枠の
    /// もので、判定に使った枠のリセットとは限らないため、枠から取ったものを持ち回る。
    pub reset_at: Option<i64>,
}

impl Basis {
    /// stop / pause ファイルに書く理由の文字列。
    pub fn reason(&self, threshold: u8) -> String {
        format!(
            "{} {:.0}% >= threshold {}%",
            self.window, self.used_percent, threshold
        )
    }
}

/// 閾値判定の結果。
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// 続行してよい。
    Proceed,
    /// `resume_at`（Unix epoch 秒）まで一時停止し、その後は再開してよい。
    Pause { resume_at: i64, basis: Basis },
    /// 恒久停止。待っても回復しない。`detail` は閾値超過以外の理由（リセット時刻を
    /// 読めない等）がある場合の補足。
    Stop {
        basis: Basis,
        detail: Option<String>,
    },
}

/// 閾値超過の判定を行う。
///
/// - 閾値に触れた枠が無ければ [`Decision::Proceed`]。
/// - 週次枠が触れていれば [`Decision::Stop`]（待っても回復しない）。
/// - 5 時間枠だけが触れていて、触れた枠すべてに妥当なリセット時刻があれば
///   [`Decision::Pause`]。1 つでも時刻が読めない / 遠すぎる枠があれば
///   [`Decision::Stop`]（いつ回復するか確定できない以上、安全側に倒す）。
///
/// `now` は Unix epoch 秒。リセット時刻の妥当性検査と猶予の加算に使う。
pub fn evaluate(observations: &[WindowObservation], threshold: u8, now: i64) -> Decision {
    let over: Vec<&WindowObservation> = observations
        .iter()
        .filter(|o| o.used_percent.is_finite() && o.used_percent >= f64::from(threshold))
        .collect();
    if over.is_empty() {
        return Decision::Proceed;
    }

    // デッドラインと同じ周期の枠が枯れたら、この実行の残り時間で回復することは無いので
    // 恒久停止にする。複数枠が同時に触れている場合もこちらを優先する。
    if let Some(long) = over.iter().find(|o| matches!(o.kind, WindowKind::Deadline)) {
        return Decision::Stop {
            basis: long.basis(),
            detail: None,
        };
    }

    // ここに来るのは短周期の枠だけが触れているケース。触れた枠すべてが空くまで待てば
    // よいので、最も遅いリセット時刻を再開時刻に採る。
    let mut latest_reset: Option<i64> = None;
    for o in &over {
        let Some(reset_at) = o.reset_at else {
            // リセット時刻が読めない枠がある。いつ再開してよいか確定できないので、
            // 一時停止ではなく恒久停止に倒す。
            return Decision::Stop {
                basis: o.basis(),
                detail: Some("reset time unavailable".to_string()),
            };
        };
        if reset_at > now + o.kind.max_reset_ahead() {
            // 枠 1 周期ぶんより先を指している。枠の取り違えか壊れた値。
            return Decision::Stop {
                basis: o.basis(),
                detail: Some(format!("implausible reset {}s ahead", reset_at - now)),
            };
        }
        latest_reset = Some(latest_reset.map_or(reset_at, |cur: i64| cur.max(reset_at)));
    }

    // 判定の主語は最も使用率の高い枠にする（表示用）。
    let worst = over
        .iter()
        .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
        .expect("over is non-empty");
    let reset_at = latest_reset.expect("over is non-empty and every entry has a reset time");
    Decision::Pause {
        // 既にリセット時刻を過ぎている観測（取得と判定の間にリセットが挟まった）でも、
        // 猶予ぶんは空けてから再開する。
        resume_at: reset_at.max(now) + RESET_PROPAGATION_GRACE_SECS,
        basis: worst.basis(),
    }
}

/// 一時停止の状態。pause file に `key=value` の行で保存する。
///
/// JSON にしないのは、この状態を読むのがワーカー / usage-gate（Rust）だけでなく
/// tmux モニターのシェルスクリプトでもあるため。1 行 1 キーの平文なら
/// `sed -n 's/^resume_at=//p'` で確実に取り出せる。値に改行を含めない正規化は
/// [`PauseState::to_text`] が行う。
#[derive(Debug, Clone, PartialEq)]
pub struct PauseState {
    /// 再開してよい時刻（Unix epoch 秒）。
    pub resume_at: i64,
    /// 停止理由になった枠名。
    pub window: String,
    /// 表示用の理由。
    pub reason: String,
}

impl PauseState {
    fn to_text(&self) -> String {
        format!(
            "resume_at={}\nwindow={}\nreason={}\n",
            self.resume_at,
            single_line(&self.window),
            single_line(&self.reason)
        )
    }

    /// `key=value` 行から復元する。`resume_at` が読めなければ壊れているとみなす。
    fn parse(text: &str) -> Option<Self> {
        let mut resume_at = None;
        let mut window = String::new();
        let mut reason = String::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "resume_at" => resume_at = value.trim().parse::<i64>().ok(),
                "window" => window = value.to_string(),
                "reason" => reason = value.to_string(),
                _ => {}
            }
        }
        Some(Self {
            resume_at: resume_at?,
            window,
            reason,
        })
    }
}

/// 改行を空白へ畳んで 1 行に収める（1 行 1 キーの書式を壊さないため）。
fn single_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// stop file と同じディレクトリに置く pause file のパス。
pub fn pause_path_for(stop_file: &Path) -> PathBuf {
    let file_name = stop_file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "stop".to_string());
    let parent = stop_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{file_name}-pause.json"))
}

/// 排他ロック用の sidecar パス。
///
/// pause file 本体をロックしない理由は `state.rs` と同じで、atomic rename で本体を
/// 置き換えると、ロック対象の inode が古くなって別ワーカーとの排他が破れるため。
fn pause_lock_path(pause_file: &Path) -> PathBuf {
    let file_name = pause_file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pause".to_string());
    let parent = pause_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{file_name}.lock"))
}

/// pause file を読む。
///
/// 存在しなければ `None`。壊れている / 読めない場合はエラーを返す（呼び出し側が
/// fail-closed で扱えるように、空の状態へ潰さない）。
pub fn read_pause(pause_file: &Path) -> Result<Option<PauseState>> {
    let content = match std::fs::read_to_string(pause_file) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", pause_file.display()));
        }
    };
    let state = PauseState::parse(&content)
        .with_context(|| format!("failed to parse {}", pause_file.display()))?;
    Ok(Some(state))
}

/// pause file を書く。既存より再開時刻が遅い場合だけ更新する。
///
/// 並列ワーカーが同時に書いても壊れないよう、sidecar ロックの下で read-modify-write を
/// 行い、本体は同一ディレクトリの一時ファイル → `rename` で置き換える。既存の方が遅い
/// （＝より長く待つ必要がある）場合は上書きしない。戻り値は実際に更新したかどうか。
pub fn write_pause(pause_file: &Path, state: &PauseState) -> Result<bool> {
    let lock_path = pause_lock_path(pause_file);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    lock_file
        .lock_exclusive()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;

    let result = (|| -> Result<bool> {
        // 壊れた既存ファイルは上書きしてよい（一時停止は再取得できる揮発状態であり、
        // state.json のような失うと困る履歴ではない）。
        if let Ok(Some(existing)) = read_pause(pause_file)
            && existing.resume_at >= state.resume_at
        {
            return Ok(false);
        }
        let body = state.to_text();
        let parent = pause_file.parent().unwrap_or_else(|| Path::new("."));
        let tmp = parent.join(format!(
            ".{}.tmp.{}",
            pause_file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "pause".to_string()),
            std::process::id()
        ));
        let write_result = (|| -> Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("failed to write {}", tmp.display()));
        }
        std::fs::rename(&tmp, pause_file).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(true)
    })();

    // Rust 1.89 の File::unlock ではなく、MSRV 1.88 で使える fs2 の実装を明示する。
    let _ = fs2::FileExt::unlock(&lock_file);
    result
}

/// stop file を冪等に作成する（恒久停止シグナル）。
///
/// 既に存在する場合は上書きせず `false` を返す。並列ワーカーから同時に呼ばれても
/// 最初の理由が残る。
pub fn write_stop(stop_file: &Path, reason: &str) -> std::io::Result<bool> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stop_file)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{reason}");
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// 現在時刻（Unix epoch 秒）。
pub fn now_epoch() -> i64 {
    chrono::Utc::now().timestamp()
}

/// ワーカーが次のタスクへ進んでよいかの判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// 次のタスクを開始してよい。
    Proceed,
    /// 恒久停止。ワーカーはループを抜ける。
    Stop,
}

/// 待機中に停止シグナルと pause の延長を確認する間隔。
const GATE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 待機中に残り時間を表示する間隔（秒）。
const GATE_PROGRESS_INTERVAL_SECS: i64 = 60;

/// ワーカーが次のタスクを claim する前のゲート。
///
/// 恒久停止（stop file）があれば止まる。一時停止（pause file）があり、再開時刻を過ぎて
/// いれば**そのまま再開する**（ここが、5 時間枠の枯渇で実行全体を捨てていた挙動の修正
/// 点）。再開時刻がまだ先なら、その時刻まで待ってから再開する。ただし待つと実行全体の
/// デッドラインを越えてしまう場合は、待たずに恒久停止へ倒す。
///
/// 待機中も stop file と pause の延長を毎秒確認するので、他のワーカーが恒久停止を書けば
/// すぐ止まり、より遅い再開時刻が書かれればそちらまで待つ。
pub async fn run_gate_wait(stop_file: &Path, deadline_epoch: Option<i64>) -> Result<GateOutcome> {
    if stop_file.exists() {
        return Ok(GateOutcome::Stop);
    }
    let pause_file = pause_path_for(stop_file);
    let state = match read_pause_for_gate(&pause_file, stop_file) {
        Ok(Some(state)) => state,
        Ok(None) => return Ok(GateOutcome::Proceed),
        Err(outcome) => return Ok(outcome),
    };

    let mut target = state.resume_at;
    let mut last_progress = 0i64;
    loop {
        if stop_file.exists() {
            return Ok(GateOutcome::Stop);
        }
        let now = now_epoch();
        if now >= target {
            // 再開時刻に達した。待っている間に別ワーカーが延長していないか最後に確認する。
            match read_pause_for_gate(&pause_file, stop_file) {
                Ok(Some(latest)) if latest.resume_at > target => {
                    target = latest.resume_at;
                }
                Ok(_) => return Ok(GateOutcome::Proceed),
                Err(outcome) => return Ok(outcome),
            }
        }
        // 待ってもデッドラインを越えるなら、待つ意味が無いので恒久停止にする。
        if let Some(deadline) = deadline_epoch
            && target >= deadline
        {
            let reason = format!("{} (resume time is past the deadline)", state.reason);
            if write_stop(stop_file, &reason).is_err() {
                // stop file を書けなくても、このワーカーは止める。
                eprintln!("gate-wait: failed to write stop file");
            }
            println!(
                "\x1b[31m  \u{26d4} 一時停止の再開時刻がデッドラインを越えるため停止します（{}）\x1b[0m",
                state.reason
            );
            return Ok(GateOutcome::Stop);
        }
        if now - last_progress >= GATE_PROGRESS_INTERVAL_SECS {
            last_progress = now;
            println!(
                "\x1b[33m  \u{23f8} {} の枠がリセットされるまで待機中（残り {}）\x1b[0m",
                state.window,
                format_remaining(target - now)
            );
        }
        tokio::time::sleep(GATE_POLL_INTERVAL).await;
    }
}

/// 待機の残り時間を整形する。
///
/// `display::format_duration` は分未満を「0m」に丸めるため、待機の最後の 1 分が
/// 「残り 0m」になって進んでいるのか止まっているのか読めない。1 分未満は秒で出す。
fn format_remaining(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else {
        crate::display::format_duration(std::time::Duration::from_secs(secs as u64))
    }
}

/// pause file をゲート用に読む。
///
/// 壊れていて読めない場合は恒久停止シグナルを書き、`Err(GateOutcome::Stop)` を返す。
/// 一時停止の状態が読めないまま走り続けると、止めるべき場面で走ってしまう（fail-open）。
fn read_pause_for_gate(
    pause_file: &Path,
    stop_file: &Path,
) -> std::result::Result<Option<PauseState>, GateOutcome> {
    match read_pause(pause_file) {
        Ok(state) => Ok(state),
        Err(e) => {
            let reason = format!("unreadable pause state: {e}");
            let _ = write_stop(stop_file, &reason);
            println!("\x1b[31m  \u{26d4} gate-wait: {reason}（停止します）\x1b[0m");
            Err(GateOutcome::Stop)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn five_hour(pct: f64, reset_at: Option<i64>) -> WindowObservation {
        WindowObservation::new(WindowKind::FIVE_HOUR, "five_hour", pct).with_reset_at(reset_at)
    }

    fn weekly(pct: f64, reset_at: Option<i64>) -> WindowObservation {
        WindowObservation::new(WindowKind::Deadline, "seven_day", pct).with_reset_at(reset_at)
    }

    const NOW: i64 = 1_788_480_000;

    #[test]
    fn under_threshold_proceeds() {
        let obs = [five_hour(89.0, Some(NOW + 600)), weekly(43.0, None)];
        assert_eq!(evaluate(&obs, 90, NOW), Decision::Proceed);
    }

    #[test]
    fn five_hour_over_threshold_pauses_until_its_reset() {
        // 実際に起きた事故の再現: 5h 90% / 7d 43%、5h のリセットは 10 分後。
        // 週次枠には余裕があるので、恒久停止ではなくリセットまでの一時停止になる。
        let obs = [five_hour(90.0, Some(NOW + 600)), weekly(43.0, None)];
        match evaluate(&obs, 90, NOW) {
            Decision::Pause { resume_at, basis } => {
                assert_eq!(resume_at, NOW + 600 + RESET_PROPAGATION_GRACE_SECS);
                assert_eq!(basis.window, "five_hour");
                assert_eq!(basis.reset_at, Some(NOW + 600));
            }
            other => panic!("5 時間枠は一時停止になるべき: {other:?}"),
        }
    }

    #[test]
    fn weekly_over_threshold_stops_permanently() {
        // 週次枠はデッドラインと同じ周期なので、待っても回復しない。
        let obs = [five_hour(20.0, Some(NOW + 600)), weekly(95.0, None)];
        match evaluate(&obs, 90, NOW) {
            Decision::Stop { basis, .. } => assert_eq!(basis.window, "seven_day"),
            other => panic!("週次枠は恒久停止になるべき: {other:?}"),
        }
    }

    #[test]
    fn weekly_wins_when_both_windows_are_over() {
        let obs = [five_hour(99.0, Some(NOW + 600)), weekly(91.0, None)];
        assert!(matches!(evaluate(&obs, 90, NOW), Decision::Stop { .. }));
    }

    #[test]
    fn five_hour_without_reset_time_stops() {
        // いつ再開してよいか確定できないので、一時停止にはせず安全側へ倒す。
        let obs = [five_hour(95.0, None)];
        assert!(matches!(evaluate(&obs, 90, NOW), Decision::Stop { .. }));
    }

    #[test]
    fn implausible_reset_time_stops() {
        // 5 時間枠のはずが 1 日先を指している = 枠の取り違えか壊れた値。
        let obs = [five_hour(95.0, Some(NOW + 86_400))];
        assert!(matches!(evaluate(&obs, 90, NOW), Decision::Stop { .. }));
    }

    #[test]
    fn past_reset_time_still_waits_for_the_grace_period() {
        // 取得と判定の間にリセットが挟まったケース。過去の時刻をそのまま再開時刻に
        // すると猶予なしで再開し、反映前の値でまた止まる。
        let obs = [five_hour(95.0, Some(NOW - 120))];
        match evaluate(&obs, 90, NOW) {
            Decision::Pause { resume_at, .. } => {
                assert_eq!(resume_at, NOW + RESET_PROPAGATION_GRACE_SECS);
            }
            other => panic!("一時停止になるべき: {other:?}"),
        }
    }

    #[test]
    fn pause_uses_latest_reset_when_multiple_windows_are_over() {
        let obs = [
            five_hour(95.0, Some(NOW + 600)),
            WindowObservation::new(WindowKind::from_period_secs(86_400), "daily", 92.0)
                .with_reset_at(Some(NOW + 1200)),
        ];
        match evaluate(&obs, 90, NOW) {
            Decision::Pause { resume_at, basis } => {
                assert_eq!(resume_at, NOW + 1200 + RESET_PROPAGATION_GRACE_SECS);
                // 主語は使用率が最大の枠。
                assert_eq!(basis.window, "five_hour");
            }
            other => panic!("一時停止になるべき: {other:?}"),
        }
    }

    #[test]
    fn stop_detail_explains_why_pausing_was_not_possible() {
        let obs = [five_hour(95.0, None)];
        match evaluate(&obs, 90, NOW) {
            Decision::Stop { basis, detail } => {
                assert_eq!(basis.window, "five_hour");
                assert_eq!(detail.as_deref(), Some("reset time unavailable"));
                assert_eq!(basis.reason(90), "five_hour 95% >= threshold 90%");
            }
            other => panic!("恒久停止になるべき: {other:?}"),
        }
    }

    #[test]
    fn non_finite_usage_is_ignored() {
        let obs = [five_hour(f64::NAN, Some(NOW + 600))];
        assert_eq!(evaluate(&obs, 90, NOW), Decision::Proceed);
    }

    #[test]
    fn pause_path_is_a_sibling_of_the_stop_file() {
        assert_eq!(
            pause_path_for(Path::new("/tmp/token-burn/run/stop")),
            PathBuf::from("/tmp/token-burn/run/stop-pause.json")
        );
    }

    #[test]
    fn write_and_read_pause_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let state = PauseState {
            resume_at: NOW + 600,
            window: "five_hour".to_string(),
            reason: "five_hour 90% >= threshold 90%".to_string(),
        };
        assert!(write_pause(&path, &state).unwrap());
        assert_eq!(read_pause(&path).unwrap(), Some(state));
    }

    #[test]
    fn write_pause_keeps_the_later_resume_time() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let late = PauseState {
            resume_at: NOW + 1200,
            window: "five_hour".to_string(),
            reason: "late".to_string(),
        };
        let early = PauseState {
            resume_at: NOW + 600,
            window: "five_hour".to_string(),
            reason: "early".to_string(),
        };
        assert!(write_pause(&path, &late).unwrap());
        // 早い再開時刻での上書きは、待つべき時間を短縮してしまうので拒否する。
        assert!(!write_pause(&path, &early).unwrap());
        assert_eq!(read_pause(&path).unwrap().unwrap().resume_at, NOW + 1200);
    }

    #[test]
    fn read_pause_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_pause(&tmp.path().join("absent.json")).unwrap(), None);
    }

    #[test]
    fn read_pause_errors_on_corrupt_content() {
        // 壊れた内容を「一時停止なし」と解釈すると、停止すべき場面で走り続ける。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(read_pause(&path).is_err());
    }

    fn pause_at(path: &Path, resume_at: i64) {
        write_pause(
            path,
            &PauseState {
                resume_at,
                window: "five_hour".to_string(),
                reason: "five_hour 90% >= threshold 90%".to_string(),
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn gate_stops_when_the_stop_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        std::fs::write(&stop, "weekly exhausted").unwrap();
        assert_eq!(run_gate_wait(&stop, None).await.unwrap(), GateOutcome::Stop);
    }

    #[tokio::test]
    async fn gate_proceeds_without_any_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        assert_eq!(
            run_gate_wait(&stop, None).await.unwrap(),
            GateOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn gate_proceeds_when_the_pause_has_already_expired() {
        // 実際に起きた事故の核心。5 時間枠の枯渇で止まった後、その枠がリセットされて
        // からタスクが終わった場合、残りのタスクはそのまま実行できなければならない。
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() - 1800);
        assert_eq!(
            run_gate_wait(&stop, None).await.unwrap(),
            GateOutcome::Proceed
        );
        assert!(!stop.exists(), "期限切れの一時停止で恒久停止してはいけない");
    }

    #[tokio::test]
    async fn gate_waits_until_the_resume_time() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() + 1);
        let started = std::time::Instant::now();
        assert_eq!(
            run_gate_wait(&stop, None).await.unwrap(),
            GateOutcome::Proceed
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(500),
            "再開時刻まで待つべき（{:?} で戻った）",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn gate_stops_when_the_resume_time_is_past_the_deadline() {
        // 待ってもデッドラインを越えるなら、待つ意味が無い。
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        let now = now_epoch();
        pause_at(&pause_path_for(&stop), now + 3600);
        assert_eq!(
            run_gate_wait(&stop, Some(now + 600)).await.unwrap(),
            GateOutcome::Stop
        );
        assert!(
            stop.exists(),
            "デッドラインを越える待機は恒久停止として記録すべき"
        );
    }

    #[tokio::test]
    async fn gate_stops_when_the_pause_state_is_corrupt() {
        // 一時停止の状態が読めないまま走り続けると、止めるべき場面で走ってしまう。
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        std::fs::write(pause_path_for(&stop), "garbage without a resume time").unwrap();
        assert_eq!(run_gate_wait(&stop, None).await.unwrap(), GateOutcome::Stop);
        assert!(stop.exists(), "読めない一時停止は恒久停止へ倒すべき");
    }

    #[tokio::test]
    async fn gate_stops_while_waiting_when_a_permanent_stop_arrives() {
        // 待機中に別ワーカーが恒久停止を書いたら、再開時刻を待たずに止まる。
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() + 30);
        let stop_for_task = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let _ = write_stop(&stop_for_task, "weekly exhausted");
        });
        let started = std::time::Instant::now();
        assert_eq!(run_gate_wait(&stop, None).await.unwrap(), GateOutcome::Stop);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "恒久停止を検知したら再開時刻を待たずに止まるべき"
        );
    }

    #[test]
    fn write_stop_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop");
        assert!(write_stop(&path, "first").unwrap());
        assert!(!write_stop(&path, "second").unwrap());
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("first"),
            "最初の理由が残るべき"
        );
    }
}
