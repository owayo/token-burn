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
pub(crate) const RESET_SKEW_MARGIN_SECS: i64 = 3600;

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
        if reset_at < now - RESET_SKEW_MARGIN_SECS {
            // 使用率は閾値を超えているのに、リセットはとうに過ぎたことになっている。
            // 観測が古すぎる（実データはリセットと同時に次の窓の時刻へ更新される）ので、
            // これを根拠に「猶予ぶん待てば再開できる」と判断してはいけない。
            return Decision::Stop {
                basis: o.basis(),
                detail: Some(format!("stale reset {}s in the past", now - reset_at)),
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
    ///
    /// 負値やゼロは epoch として成立しない。壊れた状態を「期限切れの一時停止」として
    /// 受け入れると即座に続行してしまうため、構文が通っても値を検証する。
    fn parse(text: &str) -> Option<Self> {
        let mut resume_at = None;
        let mut window = String::new();
        let mut reason = String::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "resume_at" => resume_at = value.trim().parse::<i64>().ok().filter(|v| *v > 0),
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
    sidecar_lock_path(pause_file)
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

/// 停止シグナルの発行と、ワーカーのタスク claim を直列化する排他ロック。
///
/// 「停止状態を確かめてから claim する」と「停止を発行する」が同じロックの下で行われ
/// なければ、確認と claim の隙間に停止が発行されてタスクが 1 件余計に始まる。
/// 一時停止の read-modify-write もこのロックで守る（別ロックに分けると、pause の更新中に
/// claim 側がその途中の状態を見る）。
fn with_control_lock<T>(lock_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = FileLock::acquire(lock_path)?;
    f()
}

/// ファイルロックのガード。`Drop` で解放する。
///
/// クロージャで囲えない場面（間に `await` を挟む処理）でも同じ排他を使えるようにする。
pub struct FileLock(std::fs::File);

impl FileLock {
    pub fn acquire(lock_path: &Path) -> Result<Self> {
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
        lock_file
            .lock_exclusive()
            .with_context(|| format!("failed to lock {}", lock_path.display()))?;
        Ok(Self(lock_file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Rust 1.89 の File::unlock ではなく、MSRV 1.88 で使える fs2 の実装を明示する。
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// `path` の隣に置く sidecar ロックのパス。
///
/// 本体をロックしないのは、atomic rename で本体を置き換えると、ロック対象の inode が
/// 古くなって別プロセスとの排他が破れるため（`state.rs` と同じ理由）。
pub fn sidecar_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "lock".to_string());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{file_name}.lock"))
}

/// pause file を書く。既存より再開時刻が遅い場合だけ更新する。
///
/// 並列ワーカーが同時に書いても壊れないよう、control ロックの下で read-modify-write を
/// 行い、本体は同一ディレクトリの一時ファイル → `rename` で置き換える。既存の方が遅い
/// （＝より長く待つ必要がある）場合は上書きしない。戻り値は実際に更新したかどうか。
pub fn write_pause(pause_file: &Path, state: &PauseState) -> Result<bool> {
    with_control_lock(&pause_lock_path(pause_file), || {
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
    })
}

/// stop file を冪等に作成する（恒久停止シグナル）。
///
/// 既に存在する場合は上書きせず `false` を返す。並列ワーカーから同時に呼ばれても
/// 最初の理由が残る。
///
/// 作成は claim / pause 更新と同じ sidecar ロックの下で行う。これが無いと、
/// `run_gate_claim` がロック内で「停止が無い」ことを確認してから `rename` で claim する
/// までの隙間に停止が発行され、停止後にタスクが 1 件開始され得る。
/// ロックを取れなかった場合でも停止シグナルは書く。直列化の取りこぼしより、
/// 停止を伝えられないまま走り続ける方が危険なため（fail-closed）。
pub fn write_stop(stop_file: &Path, reason: &str) -> std::io::Result<bool> {
    let _guard = FileLock::acquire(&pause_lock_path(&pause_path_for(stop_file))).ok();
    write_stop_unlocked(stop_file, reason)
}

/// ロックを取らずに stop file を作成する。呼び出し側が既に control ロックを保持して
/// いる場合（同一プロセスからの再取得は `flock` の意味論上ブロックしないが、意図を
/// 明示するため経路を分ける）に使う。
fn write_stop_unlocked(stop_file: &Path, reason: &str) -> std::io::Result<bool> {
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

/// ゲート判定の結果（claim はまだしていない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateOutcome {
    /// 停止シグナルが無く、そのまま進んでよい。
    Proceed,
    /// 一時停止を抜けた直後。再開前の再検証はこの場合だけ行う。
    Resumed,
    /// 恒久停止。ワーカーはループを抜ける。
    Stop,
}

/// ワーカーの claim 要求の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// キューから claim したタスクの番号（`pending-<index>` の `<index>`）。
    Claimed(String),
    /// 処理できる pending が無い。ワーカーは正常終了する。
    Empty,
    /// 恒久停止。
    Stopped,
}

impl ClaimOutcome {
    /// ワーカースクリプトが分岐に使う終了コード。
    pub fn exit_code(&self) -> i32 {
        match self {
            ClaimOutcome::Claimed(_) => 0,
            ClaimOutcome::Stopped => 10,
            ClaimOutcome::Empty => 20,
        }
    }
}

/// 待機中に停止シグナルと pause の延長を確認する間隔。
const GATE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 待機中に残り時間を表示する間隔（秒）。
const GATE_PROGRESS_INTERVAL_SECS: i64 = 60;

/// ワーカーが次のタスクを claim する前のゲートと、キューからの claim。
///
/// **判定と claim を 1 つのロックの下で行う**のが要点。以前はシェル側で「stop file を
/// 見る → `mv` で claim」と 2 段に分かれており、その隙間に別ワーカーが停止を発行すると
/// 停止後にタスクが 1 件（最悪、並列数ぶん）開始されていた。停止の発行（[`write_stop`] /
/// [`write_pause`]）も同じロックを取るため、「停止を発行してから claim される」か
/// 「claim してから停止が発行される」のどちらかに必ず並ぶ。
///
/// 恒久停止（stop file）があれば止まる。一時停止（pause file）があり、再開時刻を過ぎて
/// いれば**そのまま再開する**（ここが、5 時間枠の枯渇で実行全体を捨てていた挙動の修正
/// 点）。再開時刻がまだ先なら、その時刻まで待ってから再開する。ただし待つと実行全体の
/// デッドラインを越えてしまう場合は、待たずに恒久停止へ倒す。
///
/// 待機中も stop file と pause の延長を毎秒確認するので、他のワーカーが恒久停止を書けば
/// すぐ止まり、より遅い再開時刻が書かれればそちらまで待つ。
///
/// `revalidate` は一時停止から再開するときだけ実行する外部コマンド（`usage-gate`）。
/// 再開の根拠は停止時点の観測なので、待っている間に状況が変わっていないかを実データで
/// 確かめてから 1 件目を始める。ai-usage 連携が無ければ `None`。
pub async fn run_gate_claim(
    stop_file: &Path,
    queue_dir: &Path,
    deadline_epoch: Option<i64>,
    revalidate: Option<&str>,
) -> Result<ClaimOutcome> {
    // 取るものが無いワーカーを待たせない（待たせると空のペインがリセットまで居座る）。
    if next_pending(queue_dir)?.is_none() {
        return Ok(ClaimOutcome::Empty);
    }
    let pause_file = pause_path_for(stop_file);
    let mut revalidated = false;

    loop {
        match wait_for_gate(stop_file, &pause_file, deadline_epoch).await? {
            GateOutcome::Stop => return Ok(ClaimOutcome::Stopped),
            GateOutcome::Proceed => {}
            GateOutcome::Resumed => {
                // 一時停止を抜けた直後だけ、実データで裏を取ってから開始する。
                // 1 回で足りる（再検証が新しい pause を書けば次の周回で待ち直す）。
                if let Some(cmd) = revalidate.filter(|_| !revalidated) {
                    revalidated = true;
                    run_revalidation(cmd, stop_file);
                    continue;
                }
                // 再開が確定したので、期限切れの一時停止状態を片付ける。残したままだと
                // pause file は「存在するが resume_at を過ぎている」状態で居座り、
                // 以後の claim（タスクごとに別プロセス）が毎回 Resumed 判定になって
                // 再検証（ai-usage 起動）が走り続ける。再検証は一時停止を抜けた
                // 1 回だけで足りる。
                clear_expired_pause(&pause_file);
            }
        }

        // 判定と claim を同じロックの下で行う。ロックを取ってから、停止していないことを
        // もう一度確かめる（待機を抜けてからロックを取るまでの間に発行され得る）。
        let claimed = with_control_lock(&pause_lock_path(&pause_file), || {
            if signal_present(stop_file) {
                return Ok(None);
            }
            if deadline_reached(deadline_epoch, now_epoch()) {
                return Ok(None);
            }
            match read_pause(&pause_file) {
                // 一時停止が再び未来を指しているなら claim せず待ち直す。
                Ok(Some(state)) if state.resume_at > now_epoch() => return Ok(None),
                Ok(_) => {}
                Err(_) => return Ok(None),
            }
            let Some(pending) = next_pending(queue_dir)? else {
                return Ok(Some(ClaimOutcome::Empty));
            };
            let index = pending
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("pending-"))
                .map(str::to_string);
            let Some(index) = index else {
                return Ok(None);
            };
            let target = queue_dir.join(format!("claimed-{index}"));
            match std::fs::rename(&pending, &target) {
                Ok(()) => Ok(Some(ClaimOutcome::Claimed(index))),
                // 別ワーカーに先を越された（ロックがあるので通常は起きないが、
                // 外部から触られた場合に備えて claim 失敗は「取れなかった」に倒す）。
                Err(_) => Ok(None),
            }
        })?;

        match claimed {
            Some(outcome) => return Ok(outcome),
            None => {
                // 停止・一時停止・claim 失敗のいずれか。先頭へ戻って判定し直す。
                if signal_present(stop_file) {
                    return Ok(ClaimOutcome::Stopped);
                }
                if deadline_reached(deadline_epoch, now_epoch()) {
                    return Ok(stop_for_deadline_outcome(stop_file, "deadline reached"));
                }
                if next_pending(queue_dir)?.is_none() {
                    return Ok(ClaimOutcome::Empty);
                }
                tokio::time::sleep(GATE_POLL_INTERVAL).await;
            }
        }
    }
}

/// キューの先頭（辞書順で最小）の `pending-*` を返す。
///
/// ワーカーは番号順に消化するため、対象選択 TUI で確定した並びがそのまま処理順になる。
fn next_pending(queue_dir: &Path) -> Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(queue_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", queue_dir.display()));
        }
    };
    let mut best: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("pending-") {
            continue;
        }
        let path = entry.path();
        match &best {
            Some(current) if current.file_name() <= path.file_name() => {}
            _ => best = Some(path),
        }
    }
    Ok(best)
}

/// 再開時刻を過ぎた pause file を削除する。
///
/// control ロックの下で読み直してから消すため、待機中の別ワーカーによる延長
/// （read-modify-write）と競合しない。まだ未来を指す pause は消さない。
/// 待機中のワーカーは再開時刻を自分の変数に持っているので、削除しても待ち続ける。
fn clear_expired_pause(pause_file: &Path) {
    let _ = with_control_lock(&pause_lock_path(pause_file), || {
        if let Ok(Some(state)) = read_pause(pause_file)
            && state.resume_at <= now_epoch()
        {
            let _ = std::fs::remove_file(pause_file);
        }
        Ok(())
    });
}

/// 一時停止から再開する直前の再検証コマンドを実行する。
///
/// 失敗は握り潰す。`usage-gate` は自分で stop / pause を書くため、直後の判定で結果を
/// 読み取れる。ここでエラーにすると、再検証の起動に失敗しただけでワーカーが止まる。
///
/// 子の stdout は捨てる。`gate-claim` の stdout は claim 番号専用で、ワーカーの
/// `CLAIMED=$(... gate-claim ...)` へ直結している。子が 1 行でも stdout へ書くと
/// それが claim 番号に連結され、タスクスクリプト名が壊れて claim 済みのタスクが
/// マーカーも残さず失われる。診断は stderr に出るのでワーカーペインからは見える。
fn run_revalidation(cmd: &str, stop_file: &Path) {
    eprintln!("\x1b[33m  \u{21bb} 再開前に使用率を再確認します\x1b[0m");
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            // usage-gate は fail-closed で非ゼロ終了する。停止シグナルは既に書かれて
            // いるはずだが、書けていない場合に備えてこちらでも恒久停止へ倒す。
            eprintln!("gate-claim: revalidation exited with {status}");
            let _ = write_stop(stop_file, "revalidation failed before resuming");
        }
        Err(e) => {
            eprintln!("gate-claim: failed to run revalidation ({e})");
            let _ = write_stop(stop_file, "revalidation could not be started");
        }
    }
}

/// 停止・一時停止の判定と待機。claim はしない。
async fn wait_for_gate(
    stop_file: &Path,
    pause_file: &Path,
    deadline_epoch: Option<i64>,
) -> Result<GateOutcome> {
    if signal_present(stop_file) {
        return Ok(GateOutcome::Stop);
    }
    // デッドラインの検査はあらゆる「続行」より先に行う。モニターも到達時に stop file を
    // 作るが、それが失敗した場合や、モニターの書き込みと claim が競合した場合に、
    // 期限後のタスクが始まってしまう。ゲート自身が期限を持っているので自分で確かめる。
    if deadline_reached(deadline_epoch, now_epoch()) {
        return Ok(stop_for_deadline(stop_file, "deadline reached"));
    }
    let state = match read_pause_for_gate(pause_file, stop_file) {
        Ok(Some(state)) => state,
        Ok(None) => return Ok(GateOutcome::Proceed),
        Err(outcome) => return Ok(outcome),
    };

    let mut target = state.resume_at;
    let mut last_progress = 0i64;
    loop {
        if signal_present(stop_file) {
            return Ok(GateOutcome::Stop);
        }
        let now = now_epoch();
        if deadline_reached(deadline_epoch, now) {
            return Ok(stop_for_deadline(
                stop_file,
                "deadline reached while paused",
            ));
        }
        if now >= target {
            // 再開時刻に達した。待っている間に別ワーカーが延長していないか最後に確認する。
            match read_pause_for_gate(pause_file, stop_file) {
                Ok(Some(latest)) if latest.resume_at > target => {
                    target = latest.resume_at;
                }
                Ok(_) => return Ok(GateOutcome::Resumed),
                Err(outcome) => return Ok(outcome),
            }
        }
        // 待ってもデッドラインを越えるなら、待つ意味が無いので恒久停止にする。
        if let Some(deadline) = deadline_epoch
            && target >= deadline
        {
            eprintln!(
                "\x1b[31m  \u{26d4} 一時停止の再開時刻がデッドラインを越えるため停止します（{}）\x1b[0m",
                state.reason
            );
            return Ok(stop_for_deadline(
                stop_file,
                &format!("{} (resume time is past the deadline)", state.reason),
            ));
        }
        if now - last_progress >= GATE_PROGRESS_INTERVAL_SECS {
            last_progress = now;
            eprintln!(
                "\x1b[33m  \u{23f8} {} の枠がリセットされるまで待機中（残り {}）\x1b[0m",
                state.window,
                format_remaining(target - now)
            );
        }
        tokio::time::sleep(GATE_POLL_INTERVAL).await;
    }
}

/// デッドラインに達しているか。
fn deadline_reached(deadline_epoch: Option<i64>, now: i64) -> bool {
    deadline_epoch.is_some_and(|deadline| now >= deadline)
}

/// デッドライン到達を恒久停止として記録する。
///
/// stop file を書けなくても、このワーカー自身は必ず止める（書き込みの成否と、期限を
/// 過ぎたという事実は無関係）。
fn stop_for_deadline(stop_file: &Path, reason: &str) -> GateOutcome {
    record_deadline_stop(stop_file, reason);
    GateOutcome::Stop
}

/// 同上。claim 側の戻り値で使う。
fn stop_for_deadline_outcome(stop_file: &Path, reason: &str) -> ClaimOutcome {
    record_deadline_stop(stop_file, reason);
    ClaimOutcome::Stopped
}

fn record_deadline_stop(stop_file: &Path, reason: &str) {
    if write_stop(stop_file, reason).is_err() {
        eprintln!("gate-claim: failed to write stop file ({reason})");
    }
}

/// 停止シグナルが存在するか。
///
/// `Path::exists()` は metadata 取得エラーを「存在しない」に潰すため、権限異常や
/// I/O エラーで停止シグナルを見落とす。ここは安全側（存在すると見なす）へ倒す。
fn signal_present(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            eprintln!(
                "gate-wait: cannot inspect {} ({e}); treating it as a stop signal",
                path.display()
            );
            true
        }
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
            eprintln!("\x1b[31m  \u{26d4} gate-wait: {reason}（停止します）\x1b[0m");
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

    /// `pending-0001..N` を並べたキューディレクトリを作る。
    fn queue_with(root: &Path, count: usize) -> PathBuf {
        let queue = root.join("queue");
        std::fs::create_dir_all(&queue).unwrap();
        for idx in 1..=count {
            std::fs::write(queue.join(format!("pending-{idx:04}")), "").unwrap();
        }
        queue
    }

    #[tokio::test]
    async fn gate_stops_when_the_stop_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        std::fs::write(&stop, "weekly exhausted").unwrap();
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(
            queue.join("pending-0001").exists(),
            "停止時にタスクを claim してはいけない"
        );
    }

    #[tokio::test]
    async fn gate_claims_the_first_pending_task() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 2);
        let stop = tmp.path().join("stop");
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Claimed("0001".to_string())
        );
        // claim は rename なので、同じタスクが二重に取られることはない。
        assert!(!queue.join("pending-0001").exists());
        assert!(queue.join("claimed-0001").exists());
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Claimed("0002".to_string())
        );
    }

    #[tokio::test]
    async fn gate_reports_empty_when_no_task_remains() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 0);
        let stop = tmp.path().join("stop");
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Empty
        );
    }

    #[tokio::test]
    async fn gate_does_not_wait_when_no_task_remains() {
        // 一時停止中でも、取るものが無いワーカーは待たない（待つと空のペインが
        // リセット時刻まで居座る）。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 0);
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() + 3600);
        let started = std::time::Instant::now();
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Empty
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn gate_claims_when_the_pause_has_already_expired() {
        // 実際に起きた事故の核心。5 時間枠の枯渇で止まった後、その枠がリセットされて
        // からタスクが終わった場合、残りのタスクはそのまま実行できなければならない。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() - 1800);
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Claimed("0001".to_string())
        );
        assert!(!stop.exists(), "期限切れの一時停止で恒久停止してはいけない");
    }

    #[tokio::test]
    async fn gate_waits_until_the_resume_time() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() + 1);
        let started = std::time::Instant::now();
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Claimed("0001".to_string())
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
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        let now = now_epoch();
        pause_at(&pause_path_for(&stop), now + 3600);
        assert_eq!(
            run_gate_claim(&stop, &queue, Some(now + 600), None)
                .await
                .unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(
            stop.exists(),
            "デッドラインを越える待機は恒久停止として記録すべき"
        );
    }

    #[tokio::test]
    async fn gate_stops_once_the_deadline_has_passed_even_without_a_pause() {
        // 期限後にタスクを開始しないのはゲート自身の責務。モニターの stop file 作成が
        // 失敗した場合や、その書き込みと claim が競合した場合にここが最後の砦になる。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        assert_eq!(
            run_gate_claim(&stop, &queue, Some(now_epoch() - 1), None)
                .await
                .unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(queue.join("pending-0001").exists(), "claim してはいけない");
        assert!(stop.exists());
    }

    #[tokio::test]
    async fn gate_stops_after_the_deadline_even_with_an_expired_pause() {
        // 期限切れの一時停止は「再開してよい」を意味するが、デッドラインはそれより強い。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() - 1800);
        assert_eq!(
            run_gate_claim(&stop, &queue, Some(now_epoch() - 1), None)
                .await
                .unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(queue.join("pending-0001").exists(), "claim してはいけない");
    }

    #[tokio::test]
    async fn gate_stops_when_the_pause_state_is_corrupt() {
        // 一時停止の状態が読めないまま走り続けると、止めるべき場面で走ってしまう。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        std::fs::write(pause_path_for(&stop), "garbage without a resume time").unwrap();
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(stop.exists(), "読めない一時停止は恒久停止へ倒すべき");
    }

    #[tokio::test]
    async fn gate_stops_while_waiting_when_a_permanent_stop_arrives() {
        // 待機中に別ワーカーが恒久停止を書いたら、再開時刻を待たずに止まる。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() + 30);
        let stop_for_task = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let _ = write_stop(&stop_for_task, "weekly exhausted");
        });
        let started = std::time::Instant::now();
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "恒久停止を検知したら再開時刻を待たずに止まるべき"
        );
    }

    #[tokio::test]
    async fn gate_revalidates_only_when_resuming_from_a_pause() {
        // 再開の根拠は停止時点の観測なので、待っている間に状況が変わっていないかを
        // 実データで確かめてから 1 件目を始める。停止していないときは実行しない。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 2);
        let stop = tmp.path().join("stop");
        let probe = tmp.path().join("revalidated");
        let cmd = format!("touch {}", probe.display());

        // 一時停止が無ければ再検証しない（毎 claim で外部コマンドを叩かない）。
        assert!(matches!(
            run_gate_claim(&stop, &queue, None, Some(&cmd))
                .await
                .unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        assert!(
            !probe.exists(),
            "一時停止していないのに再検証してはいけない"
        );

        // 期限切れの一時停止から再開するときは再検証する。
        pause_at(&pause_path_for(&stop), now_epoch() - 60);
        assert!(matches!(
            run_gate_claim(&stop, &queue, None, Some(&cmd))
                .await
                .unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        assert!(probe.exists(), "再開前に使用率を確かめ直すべき");
    }

    #[tokio::test]
    async fn gate_clears_an_expired_pause_after_resuming() {
        // 期限切れの pause file を残したままにすると、claim（タスクごとに別プロセス）が
        // 毎回「一時停止から再開した」と判定して再検証コマンドを起動し続ける。
        // 再検証は一時停止を抜けた 1 回だけで足りる。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 2);
        let stop = tmp.path().join("stop");
        let pause_file = pause_path_for(&stop);
        pause_at(&pause_file, now_epoch() - 60);

        assert!(matches!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        assert!(!pause_file.exists(), "再開が確定した一時停止は片付けるべき");

        // 2 件目の claim では再検証コマンドを起動しない（pause が無いため）。
        let probe = tmp.path().join("revalidated");
        let cmd = format!("touch {}", probe.display());
        assert!(matches!(
            run_gate_claim(&stop, &queue, None, Some(&cmd))
                .await
                .unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        assert!(!probe.exists(), "片付けた後は再検証を繰り返してはいけない");
    }

    #[test]
    fn clear_expired_pause_keeps_a_future_pause() {
        // まだ未来を指す一時停止は消さない。待機中の別ワーカーが延長を読み直す
        // 根拠であり、消すと「待つべき時間」が共有できなくなる。
        let tmp = tempfile::tempdir().unwrap();
        let pause_file = tmp.path().join("stop-pause.json");
        pause_at(&pause_file, now_epoch() + 3600);

        clear_expired_pause(&pause_file);

        assert!(pause_file.exists(), "未来の一時停止を消してはいけない");
    }

    #[test]
    fn clear_expired_pause_removes_an_expired_pause() {
        let tmp = tempfile::tempdir().unwrap();
        let pause_file = tmp.path().join("stop-pause.json");
        pause_at(&pause_file, now_epoch() - 1);

        clear_expired_pause(&pause_file);

        assert!(!pause_file.exists(), "期限切れの一時停止は片付けるべき");
    }

    #[tokio::test]
    async fn gate_honors_a_pause_written_by_the_revalidation() {
        // 再検証がまだ枯れていると判断して新しい一時停止を書いたら、それに従って待つ。
        // ここで claim してしまうと、再検証を挟んだ意味が無い。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        let pause_file = pause_path_for(&stop);
        pause_at(&pause_file, now_epoch() - 60);
        // 再検証がデッドラインより先の再開時刻を書く = 待っても無駄なので恒久停止になる。
        let cmd = format!(
            "printf 'resume_at=%s\\nwindow=five_hour\\nreason=still over\\n' {} > {}",
            now_epoch() + 3600,
            pause_file.display()
        );
        assert_eq!(
            run_gate_claim(&stop, &queue, Some(now_epoch() + 600), Some(&cmd))
                .await
                .unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(queue.join("pending-0001").exists(), "claim してはいけない");
    }

    #[tokio::test]
    async fn gate_stops_when_the_revalidation_cannot_run() {
        // 再検証を起動できない = 使用率を確認できないので、走らせる側に倒さない。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        pause_at(&pause_path_for(&stop), now_epoch() - 60);
        assert_eq!(
            run_gate_claim(&stop, &queue, None, Some("exit 3"))
                .await
                .unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(stop.exists());
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
