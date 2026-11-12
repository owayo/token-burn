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
            Self::Short { period_secs } => period_secs.saturating_add(RESET_SKEW_MARGIN_SECS),
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
    //
    // `reset_at` は rate_limit_event の JSON を `as_i64()` で読んだだけの未検証の値で、
    // i64 の端の値も来得る。時刻の加減算は飽和させる（overflow の panic は format-stream
    // の中ではパイプを閉じ、上流の claude を SIGPIPE で道連れにする）。
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
        if reset_at > now.saturating_add(o.kind.max_reset_ahead()) {
            // 枠 1 周期ぶんより先を指している。枠の取り違えか壊れた値。
            return Decision::Stop {
                basis: o.basis(),
                detail: Some(format!(
                    "implausible reset {}s ahead",
                    reset_at.saturating_sub(now)
                )),
            };
        }
        if reset_at < now.saturating_sub(RESET_SKEW_MARGIN_SECS) {
            // 使用率は閾値を超えているのに、リセットはとうに過ぎたことになっている。
            // 観測が古すぎる（実データはリセットと同時に次の窓の時刻へ更新される）ので、
            // これを根拠に「猶予ぶん待てば再開できる」と判断してはいけない。
            return Decision::Stop {
                basis: o.basis(),
                detail: Some(format!(
                    "stale reset {}s in the past",
                    now.saturating_sub(reset_at)
                )),
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
        resume_at: reset_at
            .max(now)
            .saturating_add(RESET_PROPAGATION_GRACE_SECS),
        basis: worst.basis(),
    }
}

/// 早期再開の証拠にする使用率の下がり幅（ポイント）。
///
/// 1 つの枠の中で使用率は下がらない。下がったならリセット（手動リセットを含む）が
/// あった証拠になる。ただし取得元の違う値（API ヘッダと claude.ai の `/usage`）の比較や、
/// 反映の遅れを「下がった」と読まないよう、半分以下かつこの幅以上の低下だけを認める。
pub const RECOVERY_DROP_POINTS: f64 = 10.0;

/// 根拠の使用率 `basis` から `used` への低下を、回復（リセット）の証拠と認めるか。
///
/// 記録時の半分以下、かつ [`RECOVERY_DROP_POINTS`] 以上の低下だけを認める（理由は
/// [`check_recovery`]）。取り直す前に解除できない一時停止を見分けるモニター
/// （`monitor::recovery::not_liftable`）も、判定を食い違わせないようにこれを使う。
pub fn is_recovery_drop(basis: f64, used: f64) -> bool {
    used <= basis / 2.0 && basis - used >= RECOVERY_DROP_POINTS
}

/// 早期再開の直後にまた止まったとき、その一時停止を「再発」とみなす期間（秒）。
///
/// 早期再開した直後に API が同じ枠でまた拒否したなら、再開の根拠にした観測が実際の
/// 制限と食い違っている（ai-usage が別アカウントを見ている等）。その状態で同じ種類の
/// 証拠を信じ続けると、解除と拒否を繰り返してキューを失敗で使い切る。
pub const RELAPSE_SECS: i64 = 15 * 60;

/// ai-usage を `--refresh` で取り直したデータが、取得を始めた時刻より古い可能性の上限（秒）。
///
/// `--refresh` は、ほかのプロセスが先に始めた取得が終わるのを待ってその結果を共有する
/// ことがある（ai-usage 側の仕様）。その取得の開始は、こちらの開始より最大で ai-usage の
/// ロック待ちの上限ぶん前になり得る。
pub const LIVE_FRESHNESS_MARGIN_SECS: i64 = 35;

/// 一時停止の根拠になった観測の取得元。
///
/// 早期再開の判定で、取得元の違う使用率を比べてよいかを決めるのに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseSource {
    /// claude の stream-json の `rate_limit_event`（API の応答）。
    Api,
    /// `ai-usage --json`（claude.ai / chatgpt.com の使用量ページと同じ値）。
    AiUsage,
    /// 以前の実行で保存した中断記録（`resume.json` の `retry_after`）。
    Saved,
    /// 取得元を記録していない（旧形式の pause file）。
    Unknown,
}

impl PauseSource {
    fn as_str(self) -> &'static str {
        match self {
            PauseSource::Api => "api",
            PauseSource::AiUsage => "ai-usage",
            PauseSource::Saved => "saved",
            PauseSource::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "api" => PauseSource::Api,
            "ai-usage" => PauseSource::AiUsage,
            "saved" => PauseSource::Saved,
            _ => PauseSource::Unknown,
        }
    }
}

/// 一時停止した理由の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseCause {
    /// 使用率が閾値に触れた。
    Threshold,
    /// リクエストが拒否された（使用率とは別の理由でも起きる）。
    Rejected,
    /// 理由を記録していない（旧形式の pause file）。
    Unknown,
}

impl PauseCause {
    fn as_str(self) -> &'static str {
        match self {
            PauseCause::Threshold => "threshold",
            PauseCause::Rejected => "rejected",
            PauseCause::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "threshold" => PauseCause::Threshold,
            "rejected" => PauseCause::Rejected,
            _ => PauseCause::Unknown,
        }
    }
}

/// 一時停止の状態。pause file に `key=value` の行で保存する。
///
/// 平文にしたのは、以前は tmux モニターのシェルスクリプトもこれを読んでいたため
/// （1 行 1 キーなら `sed -n 's/^resume_at=//p'` で確実に取り出せる）。今はワーカー・
/// usage-gate・モニターのすべてが [`read_pause`] で読むが、書式は変えていない。
/// 値に改行を含めない正規化は [`PauseState::to_text`] が行う。
///
/// `resume_at` は「その時刻まで待てば再開できる」という確定値ではなく、根拠の観測から
/// 見積もった回復時刻にすぎない。利用者は手動リセットで枠を早く戻せるため、ai-usage
/// 連携がある構成では、モニターが待機中に実データを取り直して回復を確かめ、
/// [`lift_pause`] で早期に解除する。解除した状態は削除せずに残す（`lifted_at`）。
/// 削除すると、まだ解除を読んでいないワーカーが「消えても元の時刻まで待つ」で
/// 取り残され、直後の再発も判定できなくなる。
#[derive(Debug, Clone, PartialEq)]
pub struct PauseState {
    /// 再開してよいと見積もった時刻（Unix epoch 秒）。
    pub resume_at: i64,
    /// 停止理由になった枠名。
    pub window: String,
    /// 表示用の理由。
    pub reason: String,
    /// 書き込みのたびに増える世代（Unix epoch ミリ秒以上の単調増加）。
    ///
    /// 待機中のワーカーはこれが変わったら状態全体を読み直す。モニターの早期解除は、
    /// 判定に使った世代のままのときだけ書く（判定の最中に届いた新しい拒否を消さない）。
    /// 旧形式の pause file では 0。
    pub revision: u64,
    /// この一時停止（途切れずに続いている保留）が始まった時刻。旧形式では 0。
    pub since: i64,
    /// 根拠の観測の取得元。
    pub source: PauseSource,
    /// 一時停止した理由の種類。
    pub cause: PauseCause,
    /// 根拠の観測が行われた時刻の上限（Unix epoch 秒）。旧形式では 0。
    ///
    /// 早期解除の証拠は、これより後に取得したデータに限る（一時停止より前の古いデータが
    /// 「下がった」ように見えるのを防ぐ）。同じ一時停止に新しい観測が届いたら進める。
    pub observed_at: i64,
    /// 根拠にした枠の使用率（%）。早期解除の判定で比べる。
    pub used_percent: Option<f64>,
    /// 根拠にした枠自身のリセット時刻（猶予を含まない。`resume_at` とは別）。
    pub window_reset_at: Option<i64>,
    /// 早期解除の直後に始まった一時停止か（[`RELAPSE_SECS`] 参照）。
    pub relapse: bool,
    /// 早期解除した時刻。`Some` なら新しいタスクを止めていない。
    pub lifted_at: Option<i64>,
    /// 早期解除の理由（表示用）。
    pub lift_reason: Option<String>,
}

impl PauseState {
    /// 1 つの観測に基づく一時停止。取得元と根拠は [`Self::observed`] /
    /// [`Self::with_basis`] で足す。
    pub fn new(resume_at: i64, window: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            resume_at,
            window: window.into(),
            reason: reason.into(),
            revision: 0,
            since: 0,
            source: PauseSource::Unknown,
            cause: PauseCause::Unknown,
            observed_at: 0,
            used_percent: None,
            window_reset_at: None,
            relapse: false,
            lifted_at: None,
            lift_reason: None,
        }
    }

    /// 根拠の観測の取得元・理由・観測時刻（の上限）を記録する。
    pub fn observed(mut self, source: PauseSource, cause: PauseCause, observed_at: i64) -> Self {
        self.source = source;
        self.cause = cause;
        self.observed_at = observed_at;
        self
    }

    /// 根拠にした枠の使用率とリセット時刻を記録する。
    pub fn with_basis(mut self, used_percent: f64, window_reset_at: Option<i64>) -> Self {
        self.used_percent = Some(used_percent).filter(|v| v.is_finite() && *v >= 0.0);
        self.window_reset_at = window_reset_at;
        self
    }

    /// 早期解除済みか。
    pub fn is_lifted(&self) -> bool {
        self.lifted_at.is_some()
    }

    /// `now` の時点で新しいタスクの開始を止めているか。
    pub fn blocks(&self, now: i64) -> bool {
        !self.is_lifted() && self.resume_at > now
    }

    fn to_text(&self) -> String {
        let mut text = format!(
            "resume_at={}\nwindow={}\nreason={}\nrevision={}\nsince={}\nsource={}\ncause={}\nobserved_at={}\n",
            self.resume_at,
            single_line(&self.window),
            single_line(&self.reason),
            self.revision,
            self.since,
            self.source.as_str(),
            self.cause.as_str(),
            self.observed_at,
        );
        if let Some(used) = self.used_percent {
            text.push_str(&format!("used_percent={used}\n"));
        }
        if let Some(reset) = self.window_reset_at {
            text.push_str(&format!("window_reset_at={reset}\n"));
        }
        if self.relapse {
            text.push_str("relapse=1\n");
        }
        if let Some(lifted) = self.lifted_at {
            text.push_str(&format!("lifted_at={lifted}\n"));
        }
        if let Some(reason) = &self.lift_reason {
            text.push_str(&format!("lift_reason={}\n", single_line(reason)));
        }
        text
    }

    /// `key=value` 行から復元する。`resume_at` が読めなければ壊れているとみなす。
    ///
    /// 負値やゼロは epoch として成立しない。壊れた状態を「期限切れの一時停止」として
    /// 受け入れると即座に続行してしまうため、構文が通っても値を検証する。
    /// 追加のキーが無い旧形式も読める（世代 0・取得元不明として扱う）。
    fn parse(text: &str) -> Option<Self> {
        let mut resume_at = None;
        let mut state = Self::new(0, String::new(), String::new());
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "resume_at" => resume_at = value.trim().parse::<i64>().ok().filter(|v| *v > 0),
                "window" => state.window = value.to_string(),
                "reason" => state.reason = value.to_string(),
                "revision" => state.revision = value.trim().parse().unwrap_or(0),
                "since" => state.since = value.trim().parse().unwrap_or(0),
                "source" => state.source = PauseSource::parse(value.trim()),
                "cause" => state.cause = PauseCause::parse(value.trim()),
                "observed_at" => state.observed_at = value.trim().parse().unwrap_or(0),
                "used_percent" => {
                    state.used_percent = value
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite() && *v >= 0.0);
                }
                "window_reset_at" => state.window_reset_at = value.trim().parse().ok(),
                "relapse" => state.relapse = value.trim() == "1",
                // 解除時刻が読めない解除済みの状態を「解除されていない」へ倒すと、待機中の
                // ワーカーが元の時刻まで待つだけで済む（解除を誤って作るよりは安全）。
                "lifted_at" => {
                    state.lifted_at = value.trim().parse::<i64>().ok().filter(|v| *v > 0)
                }
                "lift_reason" => state.lift_reason = Some(value.to_string()),
                _ => {}
            }
        }
        state.resume_at = resume_at?;
        Some(state)
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

    /// `timeout` まで待ってもロックを取れなければ None。
    pub fn acquire_within(lock_path: &Path, timeout: std::time::Duration) -> Option<Self> {
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .ok()?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if lock_file.try_lock_exclusive().is_ok() {
                return Some(Self(lock_file));
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // lock_exclusive と対になる fs2 の実装で解除する。Rust 1.89 以降は std の File::unlock が
        // 同名で先に解決されるため、完全修飾で呼ぶ。
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

/// [`write_pause`] が pause file に何をしたか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseWrite {
    /// 新しい一時停止を始めた（一時停止が無い・期限切れ・早期解除済みだった）。
    /// `relapse` は早期解除の直後の再発か。
    Started { relapse: bool },
    /// 同じ一時停止の再開時刻を遅らせた。
    Extended,
    /// 再開時刻は変えず、より新しい観測があったことだけを記録した（世代は進む）。
    Noted,
    /// 早期解除より前の観測だったので書かなかった（解除を古いデータで取り消さない）。
    Ignored,
}

/// pause file を書く。
///
/// 並列ワーカーが同時に書いても壊れないよう、control ロックの下で read-modify-write を
/// 行い、本体は同一ディレクトリの一時ファイル → `rename` で置き換える。
///
/// - 同じ一時停止が続いているあいだは、再開時刻の遅い方を残す（早い時刻での上書きは
///   待つべき時間を縮めてしまう）。再開時刻が変わらなくても、より新しい観測時刻を記録して
///   世代を進める。こうすると、その観測より前のデータで判定していた早期解除は
///   [`lift_pause`] の世代の照合で失敗する。
/// - 同じ一時停止に重なった観測は、早期解除の条件をどの観測の条件より緩くしないように
///   まとめる（[`merge_basis`] / [`merge_source`]）。再開時刻の遅い方の観測で根拠を置き換えると、
///   例えば ai-usage の 100% で止まった後に API が 42% で拒否した場合、比較の元が 100% のまま
///   残り、42% のままでも「100% から下がった」と誤って解除する。
/// - 早期解除済みの状態に届いた観測は、解除と同じ秒かそれより前のもの
///   （`observed_at <= lifted_at`）なら書かない。解除より後なら新しい一時停止として書き、
///   [`RELAPSE_SECS`] 以内なら再発の印を付ける。再開時刻の大小は問わない（解除済みの状態は
///   何も止めていない）。
pub fn write_pause(pause_file: &Path, state: &PauseState) -> Result<PauseWrite> {
    with_control_lock(&pause_lock_path(pause_file), || {
        let now = now_epoch();
        // 壊れた既存ファイルは上書きしてよい（一時停止は再取得できる揮発状態であり、
        // state.json のような失うと困る履歴ではない）。
        let existing = read_pause(pause_file).ok().flatten();
        let mut next = state.clone();
        next.lifted_at = None;
        next.lift_reason = None;
        let outcome = match &existing {
            Some(current) if current.is_lifted() => {
                let lifted_at = current.lifted_at.unwrap_or(0);
                // 時刻は秒単位なので、解除と同じ秒の観測は前後を決められない。解除より前に
                // 作った観測が解除の後に届いたものなら、それを再発として書くと（API 由来なら）
                // 以後の早期解除ができず、手動リセットしても元の時刻まで待つ。逆に解除の直後の
                // 本物の拒否を 1 回読み落としても、続くリクエストの拒否で次の秒に書かれる
                // （解除の後に始まるタスクは、再開前の再検証を経るので同じ秒には届かない）。
                // 取り返しのつかない前者を避け、同じ秒は解除より前の観測として扱う。
                if state.observed_at <= lifted_at {
                    return Ok(PauseWrite::Ignored);
                }
                // 元の枠がまだリセットされていない（見積もった再開時刻の前）うちに止まり
                // 直したなら、早期解除の根拠が実際の制限と食い違っていた。
                next.relapse = current.resume_at > now && now - lifted_at < RELAPSE_SECS;
                next.since = now;
                PauseWrite::Started {
                    relapse: next.relapse,
                }
            }
            Some(current) if current.resume_at > now => {
                next = merge_pauses(current, state);
                next.lifted_at = None;
                next.lift_reason = None;
                next.since = current.since;
                if state.resume_at > current.resume_at {
                    PauseWrite::Extended
                } else {
                    PauseWrite::Noted
                }
            }
            _ => {
                next.since = now;
                next.relapse = false;
                PauseWrite::Started { relapse: false }
            }
        };
        next.revision = next_revision(existing.as_ref());
        write_pause_file(pause_file, &next)?;
        Ok(outcome)
    })
}

/// 同じ枠を止めている 2 つの一時停止（観測）を 1 つにまとめる。
///
/// 再開時刻・枠名・理由・リセット時刻は再開時刻の遅い方（同じなら `a`）に合わせる。観測時刻は
/// 新しい方、早期解除の条件はどちらの条件より緩くならないように（[`merge_basis`] /
/// [`merge_source`]）、再発の印はどちらかに付いていれば残す。[`write_pause`] が同じ一時停止に
/// 届いた観測を重ねるときと、実行開始時に実データの閾値超過と保存した拒否の記録を重ねる
/// ときに使う。世代・開始時刻・解除の記録は呼び出し側が決める。
pub fn merge_pauses(a: &PauseState, b: &PauseState) -> PauseState {
    let mut merged = if b.resume_at > a.resume_at {
        b.clone()
    } else {
        a.clone()
    };
    merged.observed_at = a.observed_at.max(b.observed_at);
    merged.used_percent = merge_basis(a.used_percent, b.used_percent);
    merged.source = merge_source(a.source, b.source);
    merged.relapse = a.relapse || b.relapse;
    merged
}

/// 同じ一時停止に重なった観測の、根拠の使用率をまとめる。
///
/// 回復とみなす条件（[`check_recovery`]）は根拠が低いほど厳しい（大きな下がり幅が要る）ので、
/// 低い方を採る。拒否は使用率と別の理由でも起きる（42% で拒否された例がある）ため、拒否した
/// ときの低い使用率を、先に記録した高い使用率で上書きしてはいけない。どちらかが不明なら
/// 不明にする（比べる元が無い一時停止は早期解除しない）。
pub fn merge_basis(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    Some(a?.min(b?))
}

/// 同じ一時停止に重なった観測の取得元をまとめる。
///
/// どれも ai-usage のときだけ ai-usage とする。API や保存した記録の観測が混じった一時停止を
/// ai-usage 由来として扱うと、API が拒否した再発（`relapse`）の後に ai-usage の観測が重なった
/// だけで、取得元の違う証拠で解除しない再発ガード（[`check_recovery`]）が外れる。
pub fn merge_source(a: PauseSource, b: PauseSource) -> PauseSource {
    match (a, b) {
        (PauseSource::AiUsage, PauseSource::AiUsage) => PauseSource::AiUsage,
        (PauseSource::Api, _) | (_, PauseSource::Api) => PauseSource::Api,
        (PauseSource::AiUsage, other) | (other, PauseSource::AiUsage) => other,
        (first, _) => first,
    }
}

/// 一時停止を早期解除する（手動リセット等で枠が回復したことを確かめたとき）。
///
/// 判定に使った世代（`expected_revision`）のままで、解除済みでも期限切れでもないときだけ
/// 書く。判定の最中に新しい拒否が記録されていたら世代が進んでいるので、解除しない。
/// 解除した状態は削除せずに残す（[`PauseState`] 参照）。待機中のワーカーは毎秒これを読み、
/// 再開前の再検証を経て次のタスクへ進む。
///
/// ロックは `lock_timeout` までしか待たない（モニターの描画と Ctrl-C の受け付けを
/// 止めないため）。取れなければ解除せず `false` を返す（待ち続けるだけで安全側）。
pub fn lift_pause(
    pause_file: &Path,
    expected_revision: u64,
    reason: &str,
    lock_timeout: std::time::Duration,
) -> Result<bool> {
    let Some(_guard) = FileLock::acquire_within(&pause_lock_path(pause_file), lock_timeout) else {
        return Ok(false);
    };
    let now = now_epoch();
    let Some(current) = read_pause(pause_file)? else {
        return Ok(false);
    };
    if current.revision != expected_revision || !current.blocks(now) {
        return Ok(false);
    }
    let mut next = current.clone();
    next.lifted_at = Some(now);
    next.lift_reason = Some(reason.to_string());
    next.revision = next_revision(Some(&current));
    write_pause_file(pause_file, &next)?;
    Ok(true)
}

/// 次の世代。直前の世代より大きく、かつ現在時刻（ミリ秒）以上にする。
///
/// ファイルを消してから作り直した場合でも、以前の世代と同じ値にならないようにする
/// （待機中のワーカーは世代が変わったかどうかで読み直しを決めるため）。
fn next_revision(previous: Option<&PauseState>) -> u64 {
    let now_millis = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
    previous
        .map(|p| p.revision.saturating_add(1))
        .unwrap_or(0)
        .max(now_millis)
}

/// pause file 本体を同一ディレクトリの一時ファイル → `rename` で置き換える。
/// 呼び出し側で control ロックを取っていること。
fn write_pause_file(pause_file: &Path, state: &PauseState) -> Result<()> {
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
    Ok(())
}

/// 待機中に取り直した、1 アカウントの実データ。
#[derive(Debug, Clone)]
pub struct LiveObservation {
    /// 使用率が数値として読めた枠（読めない枠は含めない）。
    pub windows: Vec<WindowObservation>,
    /// データが取得された時刻の下限（Unix epoch 秒）。取得開始から
    /// [`LIVE_FRESHNESS_MARGIN_SECS`] を引いた値。
    pub observed_from: i64,
    /// ai-usage の共有キャッシュを通さずに取り直したか（`--refresh`）。古い ai-usage で
    /// `--refresh` が使えずに通常の取得へ落ちた場合は false で、早期解除の証拠にしない。
    pub fresh: bool,
}

/// 早期解除の判定結果。
#[derive(Debug, Clone, PartialEq)]
pub enum Recovery {
    /// 一時停止の根拠になった枠が回復した（値は表示用の説明）。
    Recovered(String),
    /// まだ回復していない、または回復と言い切れるほど下がっていない。
    NotYet(String),
    /// このデータでは判定できない（古い・取得元を比べられない・枠が無い等）。
    Undecidable(String),
}

/// 待機中に取り直した実データで、一時停止の根拠が解消したかを判定する。
///
/// 回復とみなすのは、次をすべて満たすときだけ。
///
/// - 一時停止がまだ効いている（解除済み・期限切れではない）
/// - `--refresh` で取り直したデータで、一時停止の観測より後に取得された
///   （一時停止より前の古いデータが「下がった」ように見えるのを防ぐ）
/// - 一時停止に根拠の使用率が記録されていて、同じ枠の値が取れている
/// - 取ったデータのどの枠も閾値未満（[`evaluate`] が `Proceed`）
/// - 根拠の枠の使用率が、記録時の半分以下、かつ [`RECOVERY_DROP_POINTS`] 以上下がった
///
/// 下がり幅で判定するのは、閾値を下回っただけでは回復と言えないため。拒否は使用率と
/// 別の理由でも起きる（実データでは 5 時間枠が 42% でも拒否された）し、取得元の違う
/// 値（API ヘッダと claude.ai の `/usage`）は丸めや反映の遅れでずれる。リセットされた枠は
/// 0% 近くまで下がるので、半分以下という大きな改善は反映の遅れでは説明できない。
///
/// 早期解除の直後に API が再び拒否した一時停止（`relapse`）では、取得元の違う証拠を
/// 使わない。ai-usage が実際に動かしているのと別のアカウントを見ている構成では、
/// 解除と拒否を繰り返してキューを失敗で使い切るため。ai-usage 自身が書いた一時停止なら
/// 同じ取得元どうしの比較なので、再発でも判定する。
pub fn check_recovery(
    pause: &PauseState,
    live: &LiveObservation,
    threshold: u8,
    now: i64,
) -> Recovery {
    if !pause.blocks(now) {
        return Recovery::Undecidable("no pause in effect".to_string());
    }
    if !live.fresh {
        return Recovery::Undecidable(
            "the usage may come from the ai-usage cache (--refresh unavailable)".to_string(),
        );
    }
    if live.observed_from < pause.observed_at {
        return Recovery::Undecidable("the usage may predate the pause".to_string());
    }
    let Some(basis) = pause.used_percent else {
        return Recovery::Undecidable("the pause has no recorded usage".to_string());
    };
    if pause.relapse && pause.source != PauseSource::AiUsage {
        return Recovery::Undecidable(
            "the API rejected requests again right after an early resume".to_string(),
        );
    }
    let Some(window) = live
        .windows
        .iter()
        .find(|w| same_window(&w.label, &pause.window))
    else {
        return Recovery::Undecidable(format!("no usage for the {} window", pause.window));
    };
    if !window.used_percent.is_finite() || window.used_percent < 0.0 {
        return Recovery::Undecidable(format!("no usage for the {} window", pause.window));
    }
    if evaluate(&live.windows, threshold, now) != Decision::Proceed {
        return Recovery::NotYet(format!(
            "{} at {:.0}% (a window is still at or over {}%)",
            pause.window, window.used_percent, threshold
        ));
    }
    if is_recovery_drop(basis, window.used_percent) {
        Recovery::Recovered(format!(
            "{} dropped from {:.0}% to {:.0}%",
            pause.window, basis, window.used_percent
        ))
    } else {
        Recovery::NotYet(format!(
            "{} at {:.0}% (was {:.0}% when paused)",
            pause.window, window.used_percent, basis
        ))
    }
}

/// 枠名が同じ枠を指すか。7 日枠は API では `seven_day`、ai-usage のスロット名では
/// `weekly` と呼ばれる。
fn same_window(a: &str, b: &str) -> bool {
    fn normalize(label: &str) -> &str {
        match label {
            "seven_day" => "weekly",
            other => other,
        }
    }
    normalize(a) == normalize(b)
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

/// ロックを取らずに stop file を作成する。呼び出し側で control ロックを取得して使う。
/// 同一プロセスでも別の open で取り直すと待機し得るため、ここでは再取得しない。
/// [`write_stop`] と同じだが、ロックを待つ時間に上限を付ける。
///
/// 画面の描画や 2 回目の Ctrl-C の受け付けを止められないモニターが使う。ロックを握る処理は
/// 本来短いが、握ったプロセスが止まると無期限に待つことになる。期限を過ぎたらロック無しで
/// 書く（ロックを取れなかった場合と同じ扱い。停止を伝えられない方が危険なため）。
pub fn write_stop_within(
    stop_file: &Path,
    reason: &str,
    lock_timeout: std::time::Duration,
) -> std::io::Result<bool> {
    let _guard =
        FileLock::acquire_within(&pause_lock_path(&pause_path_for(stop_file)), lock_timeout);
    write_stop_unlocked(stop_file, reason)
}

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
    /// 一時停止を抜けた直後。値は抜けたときの pause file の (世代, 見積もった再開時刻) で、
    /// 延長・再停止の後にもう一度再検証するかを決めるのに使う。再開時刻も含めるのは、
    /// 世代を持たない旧形式（世代 0）の書き込みが続いた場合も別の一時停止と見分けるため。
    Resumed((u64, i64)),
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
    let mut revalidated = None;
    // 再検証のコマンドがある（ai-usage 連携が有効な）構成では、モニターが待機中に実データを
    // 取り直し、手動リセット等で枠が戻れば一時停止を早期解除する。
    let recovery_checked = revalidate.is_some();

    loop {
        match wait_for_gate(stop_file, &pause_file, deadline_epoch, recovery_checked).await? {
            GateOutcome::Stop => return Ok(ClaimOutcome::Stopped),
            GateOutcome::Proceed => {}
            GateOutcome::Resumed(pause) => {
                // 一時停止を抜けた直後だけ、実データで裏を取ってから開始する。
                // 再検証が一時停止を延長・再開した場合は、次に抜けたときにも確認する。
                if let Some(cmd) = revalidate.filter(|_| revalidated != Some(pause)) {
                    revalidated = Some(pause);
                    if !run_revalidation(cmd, stop_file) {
                        return Ok(ClaimOutcome::Stopped);
                    }
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
                // 一時停止が再び効いているなら claim せず待ち直す（早期解除済みは止めない）。
                Ok(Some(state)) if state.blocks(now_epoch()) => return Ok(None),
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
                // 外部から pending を消された場合だけ待ち直す。権限異常などまで
                // 競合として扱うと、同じ失敗をデッドラインまで繰り返してしまう。
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("failed to claim {}", pending.display())),
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

/// キューの先頭（実行番号が最小）の `pending-*` を返す。
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
    let mut best: Option<(usize, PathBuf)> = None;
    for entry in entries {
        // 列挙途中の I/O エラーを空キューと取り違えると、pending が残ったまま
        // ワーカーが正常終了してしまう。
        let entry =
            entry.with_context(|| format!("failed to enumerate {}", queue_dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(index) = name.strip_prefix("pending-") else {
            continue;
        };
        // ゼロ埋めは最小 4 桁なので、辞書順だと 10000 が 9999 より先になる。
        // 壊れた番号は空キュー扱いにせず、claim 前に失敗として伝える。
        anyhow::ensure!(
            !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()),
            "invalid pending task index: {name}"
        );
        let number = index
            .parse::<usize>()
            .with_context(|| format!("pending task index out of range: {name}"))?;
        anyhow::ensure!(number > 0, "invalid pending task index: {name}");
        let path = entry.path();
        match &best {
            Some((current, _)) if *current <= number => {}
            _ => best = Some((number, path)),
        }
    }
    Ok(best.map(|(_, path)| path))
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
/// 成功したときだけ `true` を返す。失敗時は stop file の作成も試みるが、作成に
/// 失敗しても呼び出し元のワーカーは止める。
///
/// 子の stdout は捨てる。`gate-claim` の stdout は claim 番号専用で、ワーカーの
/// `CLAIMED=$(... gate-claim ...)` へ直結している。子が 1 行でも stdout へ書くと
/// それが claim 番号に連結され、タスクスクリプト名が壊れて claim 済みのタスクが
/// マーカーも残さず失われる。診断は stderr に出るのでワーカーペインからは見える。
fn run_revalidation(cmd: &str, stop_file: &Path) -> bool {
    eprintln!("\x1b[33m  \u{21bb} 再開前に使用率を再確認します\x1b[0m");
    let status = (|| -> std::io::Result<std::process::ExitStatus> {
        let mut command = std::process::Command::new(crate::platform::bash()?);
        #[cfg(windows)]
        command.envs(crate::platform::shell_environment()?);
        command
            .args(["-c", cmd])
            .stdout(std::process::Stdio::null())
            .status()
    })();
    match status {
        Ok(status) if status.success() => true,
        Ok(status) => {
            // usage-gate は fail-closed で非ゼロ終了する。停止シグナルは既に書かれて
            // いるはずだが、書けていない場合に備えてこちらでも恒久停止へ倒す。
            eprintln!("gate-claim: revalidation exited with {status}");
            let _ = write_stop(stop_file, "revalidation failed before resuming");
            false
        }
        Err(e) => {
            eprintln!("gate-claim: failed to run revalidation ({e})");
            let _ = write_stop(stop_file, "revalidation could not be started");
            false
        }
    }
}

/// 停止・一時停止の判定と待機。claim はしない。
///
/// `recovery_checked` は、待機中にモニターが実データを取り直して早期解除する構成か
/// （ai-usage 連携あり）。その場合は見積もった再開時刻がデッドラインを越えても、
/// 手動リセットで枠が戻る可能性があるので、デッドラインまで待つ。
async fn wait_for_gate(
    stop_file: &Path,
    pause_file: &Path,
    deadline_epoch: Option<i64>,
    recovery_checked: bool,
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
    let mut state = match read_pause_for_gate(pause_file, stop_file) {
        // 早期解除済みの一時停止は何も止めていない。待っていたワーカーではないので、
        // 再開前の再検証もしない（解除はモニターが実データで確かめた結果）。元の見積もり
        // 時刻を過ぎていれば片付ける（解除後のワーカーは Resumed の経路を通らないので、
        // ここで消さないと期限切れの記録が残り続ける）。
        Ok(Some(state)) if state.is_lifted() => {
            if state.resume_at <= now_epoch() {
                clear_expired_pause(pause_file);
            }
            return Ok(GateOutcome::Proceed);
        }
        Ok(Some(state)) => state,
        Ok(None) => return Ok(GateOutcome::Proceed),
        Err(outcome) => return Ok(outcome),
    };

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
        // 書き換わっていたら状態全体を読み直す。延長だけでなく、早期解除・解除後の
        // 再停止・より早い再開時刻への変化も拾うため（遅い再開時刻だけを採ると、解除しても
        // 元の時刻まで待ち続ける）。世代だけで比べないのは、世代を持たない旧形式（世代 0）の
        // 書き込みが続いた場合も取りこぼさないため。期限越えもすぐ検出し、残り時間と
        // 停止理由も最新の観測に揃える。削除されても元の時刻までは待つ。
        match read_pause_for_gate(pause_file, stop_file) {
            Ok(Some(latest)) if latest != state => {
                state = latest;
            }
            Ok(_) => {}
            Err(outcome) => return Ok(outcome),
        }
        if state.is_lifted() {
            eprintln!(
                "\x1b[32m  \u{25b6} {} の枠の回復を確認したため再開します（{}）\x1b[0m",
                state.window,
                state.lift_reason.as_deref().unwrap_or("usage recovered")
            );
            return Ok(GateOutcome::Resumed((state.revision, state.resume_at)));
        }
        if now >= state.resume_at {
            return Ok(GateOutcome::Resumed((state.revision, state.resume_at)));
        }
        // 待ってもデッドラインを越えるなら、待つ意味が無いので恒久停止にする。ただし
        // 実データで回復を確かめる構成では、手動リセットで戻ればまだ進められるので、
        // デッドラインまで待つ（到達したら上の検査で止まる）。
        if let Some(deadline) = deadline_epoch
            && state.resume_at >= deadline
            && !recovery_checked
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
            let early = if recovery_checked {
                "。リセットされれば早めに再開"
            } else {
                ""
            };
            eprintln!(
                "\x1b[33m  \u{23f8} {} の枠がリセットされるまで待機中（残り {}{early}）\x1b[0m",
                state.window,
                format_remaining(state.resume_at - now)
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

    #[tokio::test]
    async fn gate_claim_preserves_numeric_order_across_the_four_digit_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("queue");
        std::fs::create_dir(&queue).unwrap();
        for index in [9998, 9999, 10000, 10001] {
            std::fs::write(queue.join(format!("pending-{index:04}")), "").unwrap();
        }
        let stop = tmp.path().join("stop");
        for index in ["9998", "9999", "10000", "10001"] {
            assert_eq!(
                run_gate_claim(&stop, &queue, None, None).await.unwrap(),
                ClaimOutcome::Claimed(index.into())
            );
            assert!(queue.join(format!("claimed-{index}")).exists());
        }
        assert_eq!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Empty
        );
    }

    #[tokio::test]
    async fn gate_claim_rejects_invalid_pending_numbers_without_losing_tasks() {
        for index in ["", "bad", "+1", "-1", "0", &format!("{}0", usize::MAX)] {
            let tmp = tempfile::tempdir().unwrap();
            let queue = tmp.path().join("queue");
            std::fs::create_dir(&queue).unwrap();
            let pending = queue.join(format!("pending-{index}"));
            std::fs::write(&pending, "").unwrap();
            let error = run_gate_claim(&tmp.path().join("stop"), &queue, None, None)
                .await
                .expect_err("不正な番号を完了や claim 成功として扱わない");
            assert!(error.to_string().contains("pending task index"), "{error}");
            assert!(pending.exists(), "エラー時は pending を移動しない");
        }
    }

    #[tokio::test]
    async fn pause_extension_past_deadline_stops_before_the_original_resume_time() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        let pause = pause_path_for(&stop);
        let now = now_epoch();
        write_pause(
            &pause,
            &PauseState::new(now + 300, "five_hour", "five_hour 90% >= threshold 90%"),
        )
        .unwrap();
        let gate = wait_for_gate(&stop, &pause, Some(now + 600), false);
        tokio::pin!(gate);
        // 最初の待機に入るまでポーリングし、経過時間に頼らず別ワーカーの延長を再現する。
        tokio::select! {
            biased;
            result = &mut gate => panic!("待機せず終了した: {result:?}"),
            _ = std::future::ready(()) => {}
        }

        let reason = "request rejected (daily); daily at 42%";
        write_pause(&pause, &PauseState::new(now + 900, "daily", reason)).unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), gate)
            .await
            .expect("元の再開時刻まで待たず、延長を検出して停止する")
            .unwrap();
        assert_eq!(outcome, GateOutcome::Stop);
        let recorded = std::fs::read_to_string(&stop).unwrap();
        assert!(recorded.contains(reason), "{recorded}");
        assert!(!recorded.contains("90%"), "{recorded}");
    }

    #[tokio::test]
    async fn pause_corrupted_while_waiting_stops_before_the_resume_time() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        let pause = pause_path_for(&stop);
        write_pause(
            &pause,
            &PauseState::new(now_epoch() + 300, "five_hour", "waiting for reset"),
        )
        .unwrap();
        let gate = wait_for_gate(&stop, &pause, None, false);
        tokio::pin!(gate);
        tokio::select! {
            biased;
            result = &mut gate => panic!("待機せず終了した: {result:?}"),
            _ = std::future::ready(()) => {}
        }

        std::fs::write(&pause, "resume_at=invalid\n").unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), gate)
            .await
            .expect("待機中の破損を検出して停止する")
            .unwrap();
        assert_eq!(outcome, GateOutcome::Stop);
        assert!(
            std::fs::read_to_string(&stop)
                .unwrap()
                .contains("unreadable pause state")
        );
    }

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
    fn extreme_reset_times_stop_without_overflowing() {
        // `resetsAt` は rate_limit_event の JSON を `as_i64()` で読んだだけの未検証の値。
        // 表示用の差（何秒先・何秒前か）を素の減算で求めると、i64::MIN 付近で debug build
        // が overflow の panic を起こす。format-stream はパイプの中段なので、panic は
        // パイプを閉じて上流の claude を SIGPIPE で道連れにする。
        match evaluate(&[five_hour(95.0, Some(i64::MIN))], 90, NOW) {
            Decision::Stop { detail, .. } => {
                let detail = detail.expect("恒久停止にした理由を添えるべき");
                assert!(detail.starts_with("stale reset"), "{detail}");
            }
            other => panic!("遠い過去のリセットは恒久停止になるべき: {other:?}"),
        }
        match evaluate(&[five_hour(95.0, Some(i64::MAX))], 90, NOW) {
            Decision::Stop { detail, .. } => {
                let detail = detail.expect("恒久停止にした理由を添えるべき");
                assert!(detail.starts_with("implausible reset"), "{detail}");
            }
            other => panic!("遠い未来のリセットは恒久停止になるべき: {other:?}"),
        }
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
        let now = now_epoch();
        let state = PauseState::new(now + 600, "five_hour", "five_hour 90% >= threshold 90%")
            .observed(PauseSource::Api, PauseCause::Threshold, now)
            .with_basis(90.0, Some(now + 570));
        assert_eq!(
            write_pause(&path, &state).unwrap(),
            PauseWrite::Started { relapse: false }
        );
        let read = read_pause(&path).unwrap().unwrap();
        // 世代と開始時刻は書き手（write_pause）が決める。それ以外は書いたとおりに読める。
        assert!(read.revision > 0);
        assert!(read.since >= now);
        assert_eq!(
            read,
            PauseState {
                revision: read.revision,
                since: read.since,
                ..state
            }
        );
    }

    #[test]
    fn pause_text_keeps_lift_and_relapse_marks() {
        let mut state = PauseState::new(NOW + 600, "five_hour", "request rejected (five_hour)")
            .observed(PauseSource::AiUsage, PauseCause::Rejected, NOW)
            .with_basis(100.0, None);
        state.revision = 7;
        state.since = NOW - 5;
        state.relapse = true;
        state.lifted_at = Some(NOW + 10);
        state.lift_reason = Some("five_hour dropped\nfrom 100% to 0%".to_string());
        let read = PauseState::parse(&state.to_text()).unwrap();
        // 値の改行は 1 行に畳む（1 行 1 キーの書式を保つ）。
        assert_eq!(
            read.lift_reason.as_deref(),
            Some("five_hour dropped from 100% to 0%")
        );
        assert_eq!(
            read,
            PauseState {
                lift_reason: read.lift_reason.clone(),
                ..state
            }
        );
    }

    #[test]
    fn legacy_pause_files_are_still_readable() {
        // 以前の版が書いた 3 キーだけの pause file。世代 0・取得元不明として読む。
        let read =
            PauseState::parse("resume_at=1788480600\nwindow=five_hour\nreason=old\n").unwrap();
        assert_eq!(read.resume_at, 1_788_480_600);
        assert_eq!(read.revision, 0);
        assert_eq!(read.source, PauseSource::Unknown);
        assert_eq!(read.used_percent, None);
        assert!(!read.is_lifted());
    }

    #[test]
    fn write_pause_keeps_the_later_resume_time() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        let late = PauseState::new(now + 1200, "five_hour", "late").observed(
            PauseSource::Api,
            PauseCause::Threshold,
            now,
        );
        let early = PauseState::new(now + 600, "five_hour", "early").observed(
            PauseSource::Api,
            PauseCause::Rejected,
            now + 5,
        );
        write_pause(&path, &late).unwrap();
        let first = read_pause(&path).unwrap().unwrap();
        // 早い再開時刻での上書きは、待つべき時間を短縮してしまうので拒否する。
        assert_eq!(write_pause(&path, &early).unwrap(), PauseWrite::Noted);
        let second = read_pause(&path).unwrap().unwrap();
        assert_eq!(second.resume_at, now + 1200);
        assert_eq!(second.reason, "late");
        // ただし新しい観測があったことは記録し、世代を進める（その観測より前のデータで
        // 判定した早期解除を通さないため）。
        assert_eq!(second.observed_at, now + 5);
        assert!(second.revision > first.revision);
        assert_eq!(second.since, first.since);
    }

    #[test]
    fn write_pause_extends_within_the_same_pause() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        write_pause(&path, &PauseState::new(now + 600, "five_hour", "first")).unwrap();
        let first = read_pause(&path).unwrap().unwrap();
        assert_eq!(
            write_pause(&path, &PauseState::new(now + 900, "five_hour", "later")).unwrap(),
            PauseWrite::Extended
        );
        let second = read_pause(&path).unwrap().unwrap();
        assert_eq!(second.resume_at, now + 900);
        assert_eq!(second.reason, "later");
        assert_eq!(second.since, first.since, "延長は同じ一時停止の続き");
    }

    /// 早期解除済みの pause file を作る。
    fn lifted_pause(path: &Path, resume_at: i64, observed_at: i64) -> PauseState {
        write_pause(
            path,
            &PauseState::new(resume_at, "five_hour", "five_hour 100% >= threshold 95%")
                .observed(PauseSource::Api, PauseCause::Threshold, observed_at)
                .with_basis(100.0, Some(resume_at - RESET_PROPAGATION_GRACE_SECS)),
        )
        .unwrap();
        let state = read_pause(path).unwrap().unwrap();
        assert!(
            lift_pause(
                path,
                state.revision,
                "reset",
                std::time::Duration::from_secs(1)
            )
            .unwrap()
        );
        read_pause(path).unwrap().unwrap()
    }

    #[test]
    fn lift_pause_marks_the_pause_lifted_without_deleting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        let lifted = lifted_pause(&path, now + 3600, now - 10);
        assert!(lifted.is_lifted());
        assert!(!lifted.blocks(now), "解除済みは新しいタスクを止めない");
        assert_eq!(lifted.resume_at, now + 3600, "元の見積もりは残す");
        assert_eq!(lifted.lift_reason.as_deref(), Some("reset"));
        // 二重の解除はしない。
        assert!(
            !lift_pause(
                &path,
                lifted.revision,
                "again",
                std::time::Duration::from_secs(1)
            )
            .unwrap()
        );
    }

    #[test]
    fn lift_pause_refuses_when_the_pause_changed_since_the_check() {
        // 判定に使った世代のまま、という条件が無いと、取得中に届いた新しい拒否を
        // 古いデータの判定で消してしまう。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        write_pause(&path, &PauseState::new(now + 600, "five_hour", "first")).unwrap();
        let checked = read_pause(&path).unwrap().unwrap();
        write_pause(
            &path,
            &PauseState::new(now + 600, "five_hour", "rejected again").observed(
                PauseSource::Api,
                PauseCause::Rejected,
                now + 1,
            ),
        )
        .unwrap();
        assert!(
            !lift_pause(
                &path,
                checked.revision,
                "reset",
                std::time::Duration::from_secs(1)
            )
            .unwrap()
        );
        assert!(read_pause(&path).unwrap().unwrap().blocks(now));
        // 期限切れの一時停止も解除しない（もう何も止めていない）。
        let mut expired = read_pause(&path).unwrap().unwrap();
        expired.resume_at = now - 1;
        write_pause_file(&path, &expired).unwrap();
        assert!(
            !lift_pause(
                &path,
                expired.revision,
                "reset",
                std::time::Duration::from_secs(1)
            )
            .unwrap()
        );
    }

    #[test]
    fn write_pause_ignores_observations_older_than_the_lift() {
        // 解除の前に取得した古い使用率で、解除を取り消さない。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        let lifted = lifted_pause(&path, now + 3600, now - 100);
        let stale = PauseState::new(now + 3600, "five_hour", "stale").observed(
            PauseSource::AiUsage,
            PauseCause::Threshold,
            lifted.lifted_at.unwrap() - 1,
        );
        assert_eq!(write_pause(&path, &stale).unwrap(), PauseWrite::Ignored);
        assert_eq!(read_pause(&path).unwrap().unwrap(), lifted);
    }

    #[test]
    fn write_pause_after_an_early_lift_starts_a_relapse() {
        // 解除の直後に API がまた拒否したら、新しい一時停止として書き、再発の印を付ける。
        // 再開時刻が解除前と同じでも書く（解除済みの状態は何も止めていない）。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        let lifted = lifted_pause(&path, now + 3600, now - 100);
        let again = PauseState::new(now + 3600, "five_hour", "request rejected (five_hour)")
            .observed(PauseSource::Api, PauseCause::Rejected, now + 1)
            .with_basis(100.0, Some(now + 3570));
        assert_eq!(
            write_pause(&path, &again).unwrap(),
            PauseWrite::Started { relapse: true }
        );
        let read = read_pause(&path).unwrap().unwrap();
        assert!(read.blocks(now));
        assert!(read.relapse);
        assert!(read.revision > lifted.revision);
    }

    #[test]
    fn an_observation_in_the_same_second_as_the_lift_is_ignored() {
        // 秒単位では解除と同じ秒の観測の前後を決められない。解除より前に作った観測が後から
        // 届いたものを再発として書くと、早期解除できないまま元の時刻まで待つことになる。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        let lifted = lifted_pause(&path, now + 3600, now - 100);
        let same_second = PauseState::new(now + 3600, "five_hour", "late arrival")
            .observed(
                PauseSource::Api,
                PauseCause::Rejected,
                lifted.lifted_at.unwrap(),
            )
            .with_basis(100.0, None);
        assert_eq!(
            write_pause(&path, &same_second).unwrap(),
            PauseWrite::Ignored
        );
        assert_eq!(read_pause(&path).unwrap().unwrap(), lifted);
    }

    #[test]
    fn overlapping_observations_keep_the_strictest_basis() {
        // ai-usage の 100% で止まった後に、API が 42% で拒否した。比較の元を 100% のまま残すと、
        // 42% のままでも「100% から下がった」と誤って解除する。低い方（厳しい方）を残す。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        write_pause(
            &path,
            &PauseState::new(now + 3600, "five_hour", "usage-gate: five_hour 100%")
                .observed(PauseSource::AiUsage, PauseCause::Threshold, now - 60)
                .with_basis(100.0, Some(now + 3570)),
        )
        .unwrap();
        assert_eq!(
            write_pause(
                &path,
                &PauseState::new(now + 3000, "five_hour", "request rejected (five_hour)")
                    .observed(PauseSource::Api, PauseCause::Rejected, now - 30)
                    .with_basis(42.0, Some(now + 2970)),
            )
            .unwrap(),
            PauseWrite::Noted
        );
        let merged = read_pause(&path).unwrap().unwrap();
        assert_eq!(merged.resume_at, now + 3600, "待つ時刻は遅い方");
        assert_eq!(merged.used_percent, Some(42.0), "比較の元は低い方");
        assert_eq!(
            merged.source,
            PauseSource::Api,
            "API の観測が混じれば API 扱い"
        );

        let still_42 = live(vec![five_hour(42.0, Some(now + 2970))], now);
        assert!(matches!(
            check_recovery(&merged, &still_42, 95, now),
            Recovery::NotYet(_)
        ));
        let reset = live(vec![five_hour(0.0, None)], now);
        assert!(matches!(
            check_recovery(&merged, &reset, 95, now),
            Recovery::Recovered(_)
        ));
        // どちらかの根拠が不明なら、比べる元が無いので早期解除しない。
        assert_eq!(merge_basis(Some(90.0), None), None);
    }

    #[test]
    fn an_ai_usage_extension_keeps_the_relapse_guard() {
        // 早期解除の直後に API がまた拒否した（API 由来の再発）。その後に ai-usage の観測が
        // より遅い再開時刻で重なっても、取得元の違う証拠で解除しないガードは外さない。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        lifted_pause(&path, now + 3600, now - 100);
        write_pause(
            &path,
            &PauseState::new(now + 3600, "five_hour", "request rejected (five_hour)")
                .observed(PauseSource::Api, PauseCause::Rejected, now + 1)
                .with_basis(100.0, Some(now + 3570)),
        )
        .unwrap();
        assert_eq!(
            write_pause(
                &path,
                &PauseState::new(now + 4000, "five_hour", "usage-gate: five_hour 99%")
                    .observed(PauseSource::AiUsage, PauseCause::Threshold, now + 2)
                    .with_basis(99.0, Some(now + 3970)),
            )
            .unwrap(),
            PauseWrite::Extended
        );
        let merged = read_pause(&path).unwrap().unwrap();
        assert!(merged.relapse);
        assert_eq!(merged.source, PauseSource::Api);
        let observation = live(vec![five_hour(0.0, None)], now + 10);
        assert!(matches!(
            check_recovery(&merged, &observation, 95, now),
            Recovery::Undecidable(_)
        ));
    }

    #[test]
    fn a_pause_after_the_original_reset_is_not_a_relapse() {
        // 元の枠がリセットされた後に止まり直したのは、新しい枠を使い切っただけ。
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stop-pause.json");
        let now = now_epoch();
        let lifted = lifted_pause(&path, now + 3600, now - 100);
        // 元の見積もり時刻を過ぎた状態を作る（解除済みの記録は時刻だけ書き換える）。
        let mut past = lifted.clone();
        past.resume_at = now - 1;
        write_pause_file(&path, &past).unwrap();
        let next = PauseState::new(now + 7200, "five_hour", "new window exhausted").observed(
            PauseSource::Api,
            PauseCause::Threshold,
            now + 1,
        );
        assert_eq!(
            write_pause(&path, &next).unwrap(),
            PauseWrite::Started { relapse: false }
        );
    }

    fn live(windows: Vec<WindowObservation>, observed_from: i64) -> LiveObservation {
        LiveObservation {
            windows,
            observed_from,
            fresh: true,
        }
    }

    fn api_pause(now: i64, used: f64) -> PauseState {
        let mut pause = PauseState::new(now + 3600, "five_hour", "five_hour >= threshold")
            .observed(PauseSource::Api, PauseCause::Threshold, now - 60)
            .with_basis(used, Some(now + 3570));
        pause.revision = 1;
        pause
    }

    #[test]
    fn a_manual_reset_is_recognized_as_recovery() {
        // 実際に起きた事故の再現: 5 時間枠が 99% で止まった後、利用者が手動リセットした。
        // ai-usage では 5 時間枠も週次枠も 0% に戻っている。
        let now = now_epoch();
        let pause = api_pause(now, 99.0);
        let observation = live(
            vec![five_hour(0.0, None), weekly(0.0, Some(now + 50_000))],
            now,
        );
        assert!(matches!(
            check_recovery(&pause, &observation, 95, now),
            Recovery::Recovered(_)
        ));
    }

    #[test]
    fn a_lagging_usage_page_is_not_mistaken_for_a_reset() {
        // API ヘッダの 99% と、反映の遅れた /usage の 85% の差はリセットの証拠にならない。
        let now = now_epoch();
        let pause = api_pause(now, 99.0);
        let observation = live(vec![five_hour(85.0, Some(now + 3570))], now);
        assert!(matches!(
            check_recovery(&pause, &observation, 95, now),
            Recovery::NotYet(_)
        ));
    }

    #[test]
    fn a_rejection_below_the_threshold_needs_a_real_drop() {
        // 拒否は使用率と別の理由でも起きる（実データでは 42% で拒否された）。閾値未満と
        // いうだけでは回復と言えない。0% まで戻れば回復。
        let now = now_epoch();
        let mut pause = api_pause(now, 42.0);
        pause.cause = PauseCause::Rejected;
        let unchanged = live(vec![five_hour(43.0, Some(now + 3570))], now);
        assert!(matches!(
            check_recovery(&pause, &unchanged, 95, now),
            Recovery::NotYet(_)
        ));
        let reset = live(vec![five_hour(0.0, None)], now);
        assert!(matches!(
            check_recovery(&pause, &reset, 95, now),
            Recovery::Recovered(_)
        ));
    }

    #[test]
    fn recovery_needs_every_window_under_the_threshold() {
        // 5 時間枠だけ戻しても、週次枠が閾値に触れていれば再開しない。
        let now = now_epoch();
        let pause = api_pause(now, 99.0);
        let observation = live(
            vec![five_hour(0.0, None), weekly(96.0, Some(now + 50_000))],
            now,
        );
        assert!(matches!(
            check_recovery(&pause, &observation, 95, now),
            Recovery::NotYet(_)
        ));
    }

    #[test]
    fn recovery_is_undecidable_without_trustworthy_data() {
        let now = now_epoch();
        let pause = api_pause(now, 99.0);
        let reset = vec![five_hour(0.0, None)];
        // 一時停止より前に取得された可能性があるデータ。
        let stale = live(reset.clone(), pause.observed_at - 1);
        assert!(matches!(
            check_recovery(&pause, &stale, 95, now),
            Recovery::Undecidable(_)
        ));
        // --refresh を使えず、ai-usage のキャッシュを通った可能性があるデータ。
        let cached = LiveObservation {
            fresh: false,
            ..live(reset.clone(), now)
        };
        assert!(matches!(
            check_recovery(&pause, &cached, 95, now),
            Recovery::Undecidable(_)
        ));
        // 根拠の枠のデータが無い（欠損を「余裕あり」とは扱わない）。
        let missing = live(vec![weekly(0.0, None)], now);
        assert!(matches!(
            check_recovery(&pause, &missing, 95, now),
            Recovery::Undecidable(_)
        ));
        // 根拠の使用率を記録していない旧形式の一時停止。
        let mut legacy = pause.clone();
        legacy.used_percent = None;
        assert!(matches!(
            check_recovery(&legacy, &live(reset.clone(), now), 95, now),
            Recovery::Undecidable(_)
        ));
        // 解除済み・期限切れの一時停止は判定の対象外。
        let mut lifted = pause.clone();
        lifted.lifted_at = Some(now);
        assert!(matches!(
            check_recovery(&lifted, &live(reset.clone(), now), 95, now),
            Recovery::Undecidable(_)
        ));
    }

    #[test]
    fn a_relapse_is_not_lifted_by_evidence_from_another_source() {
        // 早期再開の直後に API がまた拒否した。ai-usage が別アカウントを見ている構成では
        // ai-usage は 0% のままなので、取得元の違う証拠で解除すると拒否と解除を繰り返す。
        let now = now_epoch();
        let mut pause = api_pause(now, 100.0);
        pause.relapse = true;
        let observation = live(vec![five_hour(0.0, None)], now);
        assert!(matches!(
            check_recovery(&pause, &observation, 95, now),
            Recovery::Undecidable(_)
        ));
        // ai-usage 自身が書いた一時停止なら同じ取得元どうしの比較なので判定する。
        pause.source = PauseSource::AiUsage;
        assert!(matches!(
            check_recovery(&pause, &observation, 95, now),
            Recovery::Recovered(_)
        ));
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
            &PauseState::new(resume_at, "five_hour", "five_hour 90% >= threshold 90%"),
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
    async fn gate_reports_rename_error_instead_of_retrying_forever() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        // ファイルを同名のディレクトリへ rename できない状態を作る。
        std::fs::create_dir(queue.join("claimed-0001")).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_gate_claim(&stop, &queue, None, None),
        )
        .await
        .expect("claim エラーを待ち直し続けてはいけない");
        assert!(result.unwrap_err().to_string().contains("failed to claim"));
        assert!(queue.join("pending-0001").exists());
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
        // 再検証がデッドラインより先の再開時刻を書く。実データで回復を確かめる構成なので
        // 手動リセットに備えてデッドラインまで待ち、到達したら claim せずに止まる。
        let cmd = format!(
            "printf 'resume_at=%s\\nwindow=five_hour\\nreason=still over\\n' {} > {}",
            now_epoch() + 3600,
            pause_file.display()
        );
        assert_eq!(
            run_gate_claim(&stop, &queue, Some(now_epoch() + 2), Some(&cmd))
                .await
                .unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(queue.join("pending-0001").exists(), "claim してはいけない");
    }

    #[tokio::test]
    async fn gate_resumes_as_soon_as_the_pause_is_lifted() {
        // 手動リセットで枠が戻り、モニターが一時停止を早期解除したら、見積もった再開時刻
        // （ここでは 1 時間後）を待たずに claim する。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        let pause_file = pause_path_for(&stop);
        pause_at(&pause_file, now_epoch() + 3600);
        let revision = read_pause(&pause_file).unwrap().unwrap().revision;
        let probe = tmp.path().join("revalidated");
        let cmd = format!("touch {}", probe.display());
        let gate = run_gate_claim(&stop, &queue, None, Some(&cmd));
        tokio::pin!(gate);
        tokio::select! {
            biased;
            result = &mut gate => panic!("待機せず終了した: {result:?}"),
            _ = std::future::ready(()) => {}
        }

        assert!(
            lift_pause(
                &pause_file,
                revision,
                "five_hour dropped from 99% to 0%",
                std::time::Duration::from_secs(1)
            )
            .unwrap()
        );
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), gate)
            .await
            .expect("解除を検出したら待たずに進む")
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::Claimed("0001".to_string()));
        assert!(probe.exists(), "再開前に使用率を確かめ直すべき");
        // 解除済みの記録は元の見積もり時刻まで残す（再発の判定に使う）。
        assert!(read_pause(&pause_file).unwrap().unwrap().is_lifted());
    }

    #[tokio::test]
    async fn gate_claims_without_waiting_under_a_lifted_pause() {
        // 解除の後に来たワーカーは待たない。再検証も繰り返さない。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        let pause_file = pause_path_for(&stop);
        lifted_pause(&pause_file, now_epoch() + 3600, now_epoch() - 10);
        let probe = tmp.path().join("revalidated");
        let cmd = format!("touch {}", probe.display());
        let started = std::time::Instant::now();
        assert_eq!(
            run_gate_claim(&stop, &queue, None, Some(&cmd))
                .await
                .unwrap(),
            ClaimOutcome::Claimed("0001".to_string())
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!probe.exists());
    }

    #[tokio::test]
    async fn gate_clears_a_lifted_pause_once_its_original_time_passes() {
        // 解除済みの記録は元の見積もり時刻まで残し（再発の判定に使う）、過ぎたら片付ける。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 2);
        let stop = tmp.path().join("stop");
        let pause_file = pause_path_for(&stop);
        let mut lifted = lifted_pause(&pause_file, now_epoch() + 3600, now_epoch() - 10);
        assert!(matches!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        assert!(pause_file.exists(), "見積もり時刻の前は残す");

        lifted.resume_at = now_epoch() - 1;
        write_pause_file(&pause_file, &lifted).unwrap();
        assert!(matches!(
            run_gate_claim(&stop, &queue, None, None).await.unwrap(),
            ClaimOutcome::Claimed(_)
        ));
        assert!(
            !pause_file.exists(),
            "見積もり時刻を過ぎた解除済みの記録は片付ける"
        );
    }

    #[tokio::test]
    async fn gate_waits_for_a_relapse_after_an_early_lift() {
        // 解除の後に再び止まったら（新しい世代）、それに従って待ち直す。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        let pause_file = pause_path_for(&stop);
        let now = now_epoch();
        lifted_pause(&pause_file, now + 3600, now - 10);
        // 解除より後（次の秒）に届いた拒否。
        write_pause(
            &pause_file,
            &PauseState::new(now + 3600, "five_hour", "request rejected (five_hour)").observed(
                PauseSource::Api,
                PauseCause::Rejected,
                now + 1,
            ),
        )
        .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            run_gate_claim(&stop, &queue, None, None),
        )
        .await;
        assert!(result.is_err(), "再停止した一時停止の間は claim しない");
        assert!(queue.join("pending-0001").exists());
    }

    #[tokio::test]
    async fn gate_waits_until_the_deadline_when_recovery_is_checked() {
        // 見積もった再開時刻がデッドラインを越えても、実データで回復を確かめる構成なら
        // 手動リセットに備えてデッドラインまで待つ（すぐには止めない）。
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("stop");
        let pause = pause_path_for(&stop);
        let now = now_epoch();
        pause_at(&pause, now + 3600);
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait_for_gate(&stop, &pause, Some(now + 2), true),
        )
        .await
        .expect("デッドラインで止まる")
        .unwrap();
        assert_eq!(outcome, GateOutcome::Stop);
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(900),
            "デッドラインまで待つべき（{:?} で止まった）",
            started.elapsed()
        );
        assert!(stop.exists());
    }

    #[tokio::test]
    async fn gate_revalidates_again_after_a_pause_is_extended() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        let pause_file = pause_path_for(&stop);
        let probe = tmp.path().join("revalidated-once");
        pause_at(&pause_file, now_epoch() - 60);
        // 1 回目は pause を延長し、2 回目は再検証失敗として停止する。
        // 2 回目を省くと、枠がまだ使えないのに pending が claim される。
        let cmd = format!(
            "if [ -e '{probe}' ]; then exit 3; fi; touch '{probe}'; printf 'resume_at=%s\\nwindow=five_hour\\nreason=still over\\n' {} > '{pause}'",
            now_epoch() + 2,
            probe = probe.display(),
            pause = pause_file.display()
        );

        assert_eq!(
            run_gate_claim(&stop, &queue, None, Some(&cmd))
                .await
                .unwrap(),
            ClaimOutcome::Stopped
        );
        assert!(probe.exists());
        assert!(stop.exists());
        assert!(queue.join("pending-0001").exists());
    }

    #[tokio::test]
    async fn gate_revalidates_each_legacy_pause_written_in_a_row() {
        // 世代を持たない旧形式（世代 0）の pause file が続けて書かれても、別の一時停止と
        // して読み直し、抜けるたびに再検証する。世代だけで見分けると、2 回目の延長を
        // 読み落とすか、抜けた後の再検証を飛ばして claim してしまう。
        let tmp = tempfile::tempdir().unwrap();
        let queue = queue_with(tmp.path(), 1);
        let stop = tmp.path().join("stop");
        let pause_file = pause_path_for(&stop);
        let calls = tmp.path().join("calls");
        pause_at(&pause_file, now_epoch() - 60);
        // 1 回目と 2 回目は旧形式で延長し、3 回目は何も書かずに通す。
        let cmd = format!(
            "echo x >> '{calls}'; n=$(wc -l < '{calls}'); if [ \"$n\" -le 2 ]; then printf 'resume_at=%s\\nwindow=five_hour\\nreason=still over\\n' $(( $(date +%s) + 1 )) > '{pause}'; fi",
            calls = calls.display(),
            pause = pause_file.display()
        );
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            run_gate_claim(&stop, &queue, None, Some(&cmd)),
        )
        .await
        .expect("延長のたびに抜けて再検証し、最後は claim する")
        .unwrap();
        assert_eq!(outcome, ClaimOutcome::Claimed("0001".to_string()));
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap().lines().count(),
            3,
            "延長された一時停止を抜けるたびに再検証する"
        );
    }

    #[test]
    fn write_stop_within_writes_without_the_lock_once_the_wait_expires() {
        // ロックを握ったまま止まったプロセスがいても、モニターは期限で待つのをやめて書く。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let _held = FileLock::acquire(&pause_lock_path(&pause_path_for(&stop))).unwrap();
        let start = std::time::Instant::now();
        assert!(
            write_stop_within(
                &stop,
                "deadline reached",
                std::time::Duration::from_millis(100)
            )
            .unwrap()
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(
            std::fs::read_to_string(&stop).unwrap(),
            "deadline reached\n"
        );
        // 既にあれば上書きしない（write_stop と同じ）。
        assert!(!write_stop_within(&stop, "again", std::time::Duration::from_millis(10)).unwrap());
    }

    #[test]
    fn failed_revalidation_stops_the_worker_even_if_stop_file_cannot_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = tmp.path().join("missing-parent/stop");
        assert!(!run_revalidation("exit 3", &stop));
        assert!(!stop.exists());
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
