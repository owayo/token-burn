//! 実行全体の進み具合と、停止・一時停止・デッドラインの判定。
//!
//! 旧来の bash モニターのループ本体を移したもの。ファイルシステムからの読み取り
//! （[`read_observation`]）と判定（[`Controller`]）を分け、判定は時刻とマーカーの数だけで
//! 決まるようにしている（時計を差し替えてテストできる）。

use std::path::Path;

/// ワーカーが書くマーカーの数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub done: usize,
    pub failed: usize,
    pub retry: usize,
    /// claim できるタスクが尽きる、または停止して終了したワーカーの数。
    pub workers_done: usize,
}

impl Counts {
    pub fn processed(&self) -> usize {
        self.done + self.failed + self.retry
    }
}

/// 読み取った一時停止（早期解除されていないもの）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pause {
    pub resume_at: i64,
    pub window: String,
    /// この一時停止が始まった時刻（`PauseState::since`）。回復の確認の結果が、どの
    /// 一時停止についてのものかを照合するのに使う。
    pub since: i64,
    /// 実データで回復を確かめても早期解除できない理由（[`super::recovery::not_liftable`]）。
    /// None なら、回復を確かめている構成ではリセットされれば早めに再開できる。
    pub not_liftable: Option<&'static str>,
}

#[cfg(test)]
impl Pause {
    /// 回復を確かめられる一時停止（テスト用）。
    pub fn new(resume_at: i64, window: &str) -> Self {
        Self {
            resume_at,
            window: window.into(),
            since: 0,
            not_liftable: None,
        }
    }
}

/// 早期解除された一時停止（モニターが実データで回復を確かめて解除したもの）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lift {
    pub window: String,
    /// 解除の理由（回復の確認の説明）。
    pub reason: String,
    /// 解除した時刻（Unix epoch 秒）。
    pub lifted_at: i64,
    /// 元の見積もりの再開時刻。これを過ぎたら解除を知らせる意味は無い。
    pub resume_at: i64,
}

/// 1 回の観測。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observation {
    /// Unix epoch 秒。
    pub now: i64,
    pub counts: Counts,
    /// error-* の中身（タスク番号順）。
    pub errors: Vec<String>,
    /// 一時停止のファイル。無い・読めない・壊れているときは None（ワーカー側の判定に任せ、
    /// モニターは「一時停止中」と表示しないだけにする）。早期解除済みのものも入れない
    /// （何も止めていない）。
    pub pause: Option<Pause>,
    /// 早期解除済みの一時停止。
    pub lift: Option<Lift>,
    pub stop_file_exists: bool,
}

/// ステータス行に出す通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Paused {
        window: String,
        resume_at: i64,
    },
    /// 実データで枠の回復を確かめ、一時停止を見積もりより早く解除した。
    ResumedEarly {
        window: String,
        reason: String,
        lifted_at: i64,
    },
    Interrupted,
    Deadline,
    UsageLimit,
}

/// セッションを閉じる理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// 全タスクを処理した（失敗・リトライを含む）。
    Completed,
    /// 全ワーカーが終了したのに処理し切れていない（停止・ゲートの失敗など）。
    Stopped,
}

/// 判定の結果、呼び出し側が行うこと。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// stop file を書く（引数は理由）。ワーカーは次の claim で止まる。
    WriteStop(&'static str),
    /// 集計を出してセッションを閉じる。
    Finish(Finish),
    /// 停止済みの状態での中断。待たずにセッションを閉じる。
    ForceKill,
}

/// 最下行の進捗の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// デッドラインまでの残り秒。
    Countdown(i64),
    /// 一時停止が明けるまでの残り秒。
    Resuming(i64),
    Stopping,
}

#[derive(Debug)]
pub struct Controller {
    total: usize,
    worker_count: usize,
    deadline_epoch: i64,
    now: i64,
    stopped: bool,
    counts: Counts,
    errors: Vec<String>,
    /// 今効いている一時停止（再開時刻が未来のもの）。
    paused: Option<Pause>,
    /// 一時停止の通知を最後に組み立てたときの状態。延長・枠の入れ替わり・早期解除を、
    /// 一時停止中のままでも通知へ反映するために比べる。
    last_pause: Option<PauseKey>,
    notice: Option<Notice>,
    finished: Option<Finish>,
}

/// 一時停止の通知を決める観測の要素。
#[derive(Debug, Clone, PartialEq, Eq)]
struct PauseKey {
    /// 一時停止中か。
    paused: bool,
    resume_at: Option<i64>,
    window: String,
    /// 知らせている早期解除（解除した時刻）。
    lifted_at: Option<i64>,
}

impl Controller {
    pub fn new(total: usize, worker_count: usize, deadline_epoch: i64, now: i64) -> Self {
        Self {
            total,
            worker_count,
            deadline_epoch,
            now,
            stopped: false,
            counts: Counts::default(),
            errors: Vec::new(),
            paused: None,
            last_pause: None,
            notice: None,
            finished: None,
        }
    }

    /// 観測を反映し、行うべきことを返す。
    pub fn observe(&mut self, observation: Observation) -> Vec<Action> {
        let mut actions = Vec::new();
        self.now = observation.now;
        self.counts = observation.counts;
        self.errors = observation.errors;

        // 一時停止（5 時間枠のように待てば回復する枠での停止）。恒久停止と違ってワーカーは
        // 生きており、再開時刻を過ぎれば自分で次のタスクへ進む。ここで stopped にすると、
        // 待機中のワーカーを終了扱いにして集計が食い違う。
        let paused = observation
            .pause
            .as_ref()
            .filter(|pause| pause.resume_at > self.now)
            .cloned();
        // 早期解除は、元の見積もりの再開時刻までのあいだ知らせる。それを過ぎれば、解除が
        // 無くても再開していた時刻なので、知らせる意味が無い。
        let lift = observation
            .lift
            .as_ref()
            .filter(|lift| lift.resume_at > self.now);
        let key = PauseKey {
            paused: paused.is_some(),
            resume_at: observation.pause.as_ref().map(|pause| pause.resume_at),
            window: observation
                .pause
                .as_ref()
                .map(|pause| pause.window.clone())
                .unwrap_or_default(),
            lifted_at: lift.map(|lift| lift.lifted_at),
        };
        if self.last_pause.as_ref() != Some(&key) {
            self.last_pause = Some(key);
            if !self.stopped {
                self.notice = match (&paused, lift) {
                    (Some(pause), _) => Some(Notice::Paused {
                        window: pause.window.clone(),
                        resume_at: pause.resume_at,
                    }),
                    (None, Some(lift)) => Some(Notice::ResumedEarly {
                        window: lift.window.clone(),
                        reason: lift.reason.clone(),
                        lifted_at: lift.lifted_at,
                    }),
                    (None, None) => None,
                };
            }
        }
        self.paused = paused;

        if self.now >= self.deadline_epoch && !self.stopped {
            self.stopped = true;
            self.notice = Some(Notice::Deadline);
            actions.push(Action::WriteStop("deadline reached"));
        }

        // usage-gate / rate_limit_event がワーカー側で stop file を作った場合も停止へ移る。
        // 表示のためで、終了の判定は下の「全ワーカー終了」が stopped に依存せず行う。
        if !self.stopped && observation.stop_file_exists {
            self.stopped = true;
            self.notice = Some(Notice::UsageLimit);
        }

        if self.finished.is_none() {
            if self.counts.processed() >= self.total {
                self.finished = Some(Finish::Completed);
                actions.push(Action::Finish(Finish::Completed));
            } else if self.counts.workers_done >= self.worker_count {
                // ここに来るのは「全ワーカーが終了したのにタスクが処理し切れていない」場合
                // （stop file の検出、usage-gate の fail-closed、タスクスクリプトの欠落など）。
                // stopped の内側に閉じ込めると、usage-gate が stop file を作れずに止まった
                // 場合にデッドラインまで終わらない。
                self.finished = Some(Finish::Stopped);
                actions.push(Action::Finish(Finish::Stopped));
            }
        }
        actions
    }

    /// Ctrl-C / SIGINT / SIGTERM。1 回目は stop file を書いて走行中のタスクを待ち、
    /// 停止済みの状態（2 回目、デッドライン後、使用量による停止後）では待たずに閉じる。
    pub fn interrupt(&mut self) -> Action {
        if self.stopped {
            Action::ForceKill
        } else {
            self.stopped = true;
            self.notice = Some(Notice::Interrupted);
            Action::WriteStop("interrupted from the monitor")
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// 今効いている一時停止。
    pub fn pause(&self) -> Option<&Pause> {
        self.paused.as_ref()
    }

    pub fn finished(&self) -> Option<Finish> {
        self.finished
    }

    pub fn now(&self) -> i64 {
        self.now
    }

    pub fn phase(&self) -> Phase {
        match (&self.paused, self.stopped) {
            (_, true) => Phase::Stopping,
            // 待機中は残りデッドラインより「あと何分で再開するか」の方が知りたい情報。
            (Some(pause), false) => Phase::Resuming((pause.resume_at - self.now).max(0)),
            (None, false) => Phase::Countdown((self.deadline_epoch - self.now).max(0)),
        }
    }

    /// 処理済みの割合（%）。タスク数 0 なら 0。
    pub fn percent(&self) -> usize {
        (self.counts.processed() * 100)
            .checked_div(self.total)
            .unwrap_or(0)
            .min(100)
    }
}

/// マーカーと停止・一時停止のファイルを読む。
///
/// worker-done は各ワーカーが自分の done-* / failed-* / retry-* を書き切った後に作る。
/// 先に worker-done を数えてからタスクマーカーを数えれば、「worker-done が見えているのに
/// そのワーカーのタスクマーカーが見えていない」状態は起こらない。1 回の走査で両方を
/// 数えると、走査中に作られたファイルが見えるかどうかは順序次第なので、全件成功でも
/// 「⏹ Stopped: 9/10 processed」と誤報告し得る。そのため 2 回に分けて読む。
pub fn read_observation(
    marker_dir: &Path,
    stop_file: &Path,
    pause_file: &Path,
    now: i64,
) -> Observation {
    let workers_done = marker_names(marker_dir)
        .iter()
        .filter(|name| name.starts_with("worker-done-"))
        .count();
    let mut counts = Counts {
        workers_done,
        ..Counts::default()
    };
    let mut error_files = Vec::new();
    for name in marker_names(marker_dir) {
        // worker-done-* は "done-" 始まりではないのでタスク件数に混ざらない。
        if name.starts_with("done-") {
            counts.done += 1;
        } else if name.starts_with("failed-") {
            counts.failed += 1;
        } else if name.starts_with("retry-") {
            counts.retry += 1;
        } else if let Some(index) = name.strip_prefix("error-") {
            error_files.push((index.parse::<u64>().unwrap_or(u64::MAX), name));
        }
    }
    error_files.sort();
    let errors = error_files
        .into_iter()
        .filter_map(|(_, name)| std::fs::read_to_string(marker_dir.join(name)).ok())
        .map(|text| single_line(&text))
        .filter(|text| !text.is_empty())
        .collect();

    let stop_file_exists = signal_present(stop_file);
    // 早期解除済みの一時停止は、元の見積もりの時刻までファイルに残る（まだ解除を読んで
    // いないワーカーを取り残さないため）。何も止めていないので「一時停止中」とは扱わない。
    let (pause, lift) = match crate::rate_control::read_pause(pause_file) {
        Ok(Some(state)) if state.is_lifted() => (
            None,
            Some(Lift {
                reason: state
                    .lift_reason
                    .as_deref()
                    .map(single_line)
                    .unwrap_or_default(),
                lifted_at: state.lifted_at.unwrap_or(now),
                resume_at: state.resume_at,
                window: state.window,
            }),
        ),
        Ok(Some(state)) => (
            Some(Pause {
                not_liftable: super::recovery::not_liftable(&state),
                resume_at: state.resume_at,
                since: state.since,
                window: state.window,
            }),
            None,
        ),
        _ => (None, None),
    };
    Observation {
        now,
        counts,
        errors,
        pause,
        lift,
        stop_file_exists,
    }
}

/// stop file があるか。権限異常などで存在を確かめられないときは「ある」側に倒す
/// （gate-claim と同じ）。
pub fn signal_present(stop_file: &Path) -> bool {
    match std::fs::symlink_metadata(stop_file) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

fn marker_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// 改行を空白へ畳んで 1 行にし、エスケープシーケンスと制御文字を除く（端末を崩す文字を
/// ペインへ流さない）。
fn single_line(text: &str) -> String {
    text.lines()
        .map(super::ansi::strip)
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1_000;
    const DEADLINE: i64 = 10_000;

    fn controller() -> Controller {
        Controller::new(10, 2, DEADLINE, START)
    }

    fn observe(now: i64, counts: Counts) -> Observation {
        Observation {
            now,
            counts,
            ..Observation::default()
        }
    }

    fn counts(done: usize, failed: usize, retry: usize, workers_done: usize) -> Counts {
        Counts {
            done,
            failed,
            retry,
            workers_done,
        }
    }

    #[test]
    fn completes_when_every_task_is_processed_including_failures() {
        let mut c = controller();
        assert!(c.observe(observe(START, counts(9, 0, 0, 0))).is_empty());
        assert_eq!(
            c.observe(observe(START + 1, counts(7, 2, 1, 1))),
            [Action::Finish(Finish::Completed)]
        );
        assert_eq!(c.finished(), Some(Finish::Completed));
        // 閉じる指示は 1 回だけ出す。
        assert!(c.observe(observe(START + 2, counts(7, 2, 1, 2))).is_empty());
    }

    #[test]
    fn stops_when_every_worker_has_exited_even_without_a_stop_signal() {
        // usage-gate が stop file を書けずに止まった場合も、デッドラインまで残らない。
        let mut c = controller();
        assert_eq!(
            c.observe(observe(START, counts(3, 1, 0, 2))),
            [Action::Finish(Finish::Stopped)]
        );
    }

    #[test]
    fn completion_wins_when_the_last_worker_exits_with_all_tasks_processed() {
        let mut c = controller();
        assert_eq!(
            c.observe(observe(START, counts(10, 0, 0, 2))),
            [Action::Finish(Finish::Completed)]
        );
    }

    #[test]
    fn deadline_writes_the_stop_file_once() {
        let mut c = controller();
        assert_eq!(
            c.observe(observe(DEADLINE, counts(1, 0, 0, 0))),
            [Action::WriteStop("deadline reached")]
        );
        assert_eq!(c.notice(), Some(&Notice::Deadline));
        assert_eq!(c.phase(), Phase::Stopping);
        assert!(
            c.observe(observe(DEADLINE + 5, counts(1, 0, 0, 0)))
                .is_empty()
        );
    }

    #[test]
    fn a_stop_file_from_a_worker_switches_to_stopping_without_writing_again() {
        let mut c = controller();
        let actions = c.observe(Observation {
            now: START,
            stop_file_exists: true,
            ..Observation::default()
        });
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(c.notice(), Some(&Notice::UsageLimit));
        assert_eq!(c.phase(), Phase::Stopping);
    }

    #[test]
    fn first_interrupt_waits_and_the_second_forces_the_session_closed() {
        let mut c = controller();
        assert_eq!(
            c.interrupt(),
            Action::WriteStop("interrupted from the monitor")
        );
        assert_eq!(c.notice(), Some(&Notice::Interrupted));
        assert_eq!(c.interrupt(), Action::ForceKill);
    }

    #[test]
    fn interrupt_after_the_deadline_closes_immediately() {
        // 停止済みの状態で待つ理由は無い（旧来のモニターと同じ）。
        let mut c = controller();
        c.observe(observe(DEADLINE, Counts::default()));
        assert_eq!(c.interrupt(), Action::ForceKill);
    }

    #[test]
    fn a_pause_shows_the_resume_countdown_without_stopping() {
        // 一時停止はワーカーが自力で再開できる状態なので、停止へ遷移させない。
        let mut c = controller();
        let actions = c.observe(Observation {
            now: START,
            pause: Some(Pause::new(START + 90, "five_hour")),
            ..Observation::default()
        });
        assert!(actions.is_empty());
        assert_eq!(
            c.notice(),
            Some(&Notice::Paused {
                window: "five_hour".into(),
                resume_at: START + 90
            })
        );
        assert_eq!(c.phase(), Phase::Resuming(90));
        // 1 回目の中断は「待つ」側（停止していないため）。
        assert!(matches!(c.interrupt(), Action::WriteStop(_)));
    }

    #[test]
    fn an_extended_pause_updates_the_notice() {
        let mut c = controller();
        let paused = |resume_at: i64, window: &str| Observation {
            now: START,
            pause: Some(Pause::new(resume_at, window)),
            ..Observation::default()
        };
        c.observe(paused(START + 100, "five_hour"));
        c.observe(paused(START + 200, "daily"));
        assert_eq!(
            c.notice(),
            Some(&Notice::Paused {
                window: "daily".into(),
                resume_at: START + 200
            })
        );
        assert_eq!(c.phase(), Phase::Resuming(200));
    }

    #[test]
    fn an_expired_pause_clears_the_notice() {
        let mut c = controller();
        let pause = Some(Pause::new(START + 10, "five_hour"));
        c.observe(Observation {
            now: START,
            pause: pause.clone(),
            ..Observation::default()
        });
        c.observe(Observation {
            now: START + 10,
            pause,
            ..Observation::default()
        });
        assert_eq!(c.notice(), None);
        assert_eq!(c.phase(), Phase::Countdown(DEADLINE - START - 10));
    }

    #[test]
    fn a_pause_does_not_replace_a_stop_notice() {
        let mut c = controller();
        c.interrupt();
        c.observe(Observation {
            now: START,
            pause: Some(Pause::new(START + 60, "five_hour")),
            ..Observation::default()
        });
        assert_eq!(c.notice(), Some(&Notice::Interrupted));
        assert_eq!(c.phase(), Phase::Stopping);
        // 停止した後の早期解除も、停止の通知を置き換えない。
        c.observe(Observation {
            now: START + 1,
            lift: Some(lift(START + 60, START)),
            ..Observation::default()
        });
        assert_eq!(c.notice(), Some(&Notice::Interrupted));
        assert_eq!(c.phase(), Phase::Stopping);
    }

    fn lift(resume_at: i64, lifted_at: i64) -> Lift {
        Lift {
            window: "five_hour".into(),
            reason: "five_hour dropped from 100% to 0%".into(),
            lifted_at,
            resume_at,
        }
    }

    #[test]
    fn an_early_lift_resumes_the_countdown_and_says_why() {
        // 手動リセットで枠が戻り、モニターが一時停止を解除した。ワーカーはもう待っていない
        // ので、最下行は再開までの残りではなくデッドラインまでの残りに戻す。
        let mut c = controller();
        c.observe(Observation {
            now: START,
            pause: Some(Pause::new(START + 3600, "five_hour")),
            ..Observation::default()
        });
        assert_eq!(c.phase(), Phase::Resuming(3600));
        let actions = c.observe(Observation {
            now: START + 5,
            lift: Some(lift(START + 3600, START + 4)),
            ..Observation::default()
        });
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(c.pause(), None);
        assert_eq!(c.phase(), Phase::Countdown(DEADLINE - START - 5));
        assert_eq!(
            c.notice(),
            Some(&Notice::ResumedEarly {
                window: "five_hour".into(),
                reason: "five_hour dropped from 100% to 0%".into(),
                lifted_at: START + 4,
            })
        );
        // 元の見積もりの時刻を過ぎたら、解除を知らせる意味は無い。
        c.observe(Observation {
            now: START + 3600,
            lift: Some(lift(START + 3600, START + 4)),
            ..Observation::default()
        });
        assert_eq!(c.notice(), None);
    }

    #[test]
    fn a_pause_after_an_early_lift_is_shown_again() {
        // 解除の直後にまた止まった（再発）。解除の通知を残さず、一時停止へ戻す。
        let mut c = controller();
        c.observe(Observation {
            now: START,
            lift: Some(lift(START + 3600, START)),
            ..Observation::default()
        });
        assert!(matches!(c.notice(), Some(Notice::ResumedEarly { .. })));
        c.observe(Observation {
            now: START + 30,
            pause: Some(Pause::new(START + 3600, "five_hour")),
            ..Observation::default()
        });
        assert_eq!(
            c.notice(),
            Some(&Notice::Paused {
                window: "five_hour".into(),
                resume_at: START + 3600
            })
        );
        assert_eq!(c.phase(), Phase::Resuming(3600 - 30));
    }

    #[test]
    fn percent_is_bounded_and_safe_for_zero_tasks() {
        let mut c = Controller::new(0, 1, DEADLINE, START);
        assert_eq!(c.percent(), 0);
        c = controller();
        c.observe(observe(START, counts(3, 0, 0, 0)));
        assert_eq!(c.percent(), 30);
    }

    #[test]
    fn reads_markers_in_two_passes_and_orders_errors_by_task() {
        let tmp = tempfile::TempDir::new().unwrap();
        let markers = tmp.path().join("markers");
        std::fs::create_dir_all(&markers).unwrap();
        for name in [
            "done-0001",
            "done-0002",
            "failed-0003",
            "retry-0004",
            "worker-done-0",
        ] {
            std::fs::write(markers.join(name), "").unwrap();
        }
        std::fs::write(markers.join("error-0010"), "later\n").unwrap();
        std::fs::write(markers.join("error-0003"), "boom\n\x1b[31mred\x1b[0m").unwrap();
        let stop = tmp.path().join("stop");
        let pause = crate::rate_control::pause_path_for(&stop);
        let observation = read_observation(&markers, &stop, &pause, START);
        assert_eq!(observation.counts, counts(2, 1, 1, 1));
        // 改行は空白に畳んでエスケープシーケンスは除き、タスク番号の順に並べる（辞書順ではない）。
        assert_eq!(observation.errors, ["boom red", "later"]);
        assert!(!observation.stop_file_exists);
        assert_eq!(observation.pause, None);
    }

    #[test]
    fn reads_the_pause_file_written_by_rate_control() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let pause = crate::rate_control::pause_path_for(&stop);
        crate::rate_control::write_pause(
            &pause,
            &crate::rate_control::PauseState::new(
                START + 600,
                "five_hour",
                "five_hour 90% >= threshold 90%",
            ),
        )
        .unwrap();
        std::fs::write(&stop, "").unwrap();
        let observation = read_observation(tmp.path(), &stop, &pause, START);
        let since = crate::rate_control::read_pause(&pause)
            .unwrap()
            .unwrap()
            .since;
        assert_eq!(
            observation.pause,
            Some(Pause {
                resume_at: START + 600,
                window: "five_hour".into(),
                since,
                // 根拠の使用率が記録されていない（比べる元が無い）。
                not_liftable: Some("the pause has no recorded usage"),
            })
        );
        assert_eq!(observation.lift, None);
        assert!(observation.stop_file_exists);
    }

    #[test]
    fn a_lifted_pause_is_not_read_as_paused() {
        // 早期解除済みの一時停止はファイルに残るが、何も止めていない。
        use crate::rate_control::{PauseCause, PauseSource, PauseState, lift_pause, read_pause};
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let pause = crate::rate_control::pause_path_for(&stop);
        let now = crate::rate_control::now_epoch();
        crate::rate_control::write_pause(
            &pause,
            &PauseState::new(now + 3600, "five_hour", "request rejected (five_hour)")
                .observed(PauseSource::Api, PauseCause::Rejected, now - 60)
                .with_basis(100.0, Some(now + 3570)),
        )
        .unwrap();
        let before = read_observation(tmp.path(), &stop, &pause, now);
        assert_eq!(before.pause.as_ref().unwrap().not_liftable, None);
        let revision = read_pause(&pause).unwrap().unwrap().revision;
        assert!(
            lift_pause(
                &pause,
                revision,
                "five_hour dropped\nfrom 100% to 0%",
                std::time::Duration::from_secs(1)
            )
            .unwrap()
        );
        let lifted_at = read_pause(&pause).unwrap().unwrap().lifted_at.unwrap();
        let observation = read_observation(tmp.path(), &stop, &pause, now);
        assert_eq!(observation.pause, None);
        assert_eq!(
            observation.lift,
            Some(Lift {
                window: "five_hour".into(),
                reason: "five_hour dropped from 100% to 0%".into(),
                lifted_at,
                resume_at: now + 3600,
            })
        );
    }

    #[test]
    fn a_corrupt_pause_file_is_not_shown_as_paused() {
        // 壊れた一時停止の扱い（恒久停止へ倒す）はワーカーが決める。モニターが「一時停止中」と
        // 出すと、存在しない再開時刻を表示してしまう。
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let pause = crate::rate_control::pause_path_for(&stop);
        std::fs::write(&pause, "garbage").unwrap();
        assert_eq!(
            read_observation(tmp.path(), &stop, &pause, START).pause,
            None
        );
    }
}
