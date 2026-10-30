//! tmux のモニターペイン（隠しサブコマンド `token-burn monitor`）。
//!
//! 実行全体の進捗・停止・一時停止を見張り、ai-usage の使用量、マシン負荷のグラフ、
//! ディスクの空き容量を描く。以前は Rust が生成する bash スクリプトだったが、負荷のグラフには
//! 計測の履歴を持ち続ける常駐プロセスが要るため、Rust（ratatui）へ移した。
//!
//! 構成:
//! - [`controller`] はマーカーと停止・一時停止のファイルから、停止の発行と終了を決める
//! - [`system`] / [`load`] / [`history`] は 1 秒ごとの負荷の計測と、5 分ぶんの履歴
//! - [`usage`] / [`disk`] は ai-usage の取得（別タスク。取得を待つ間も判定と描画を止めない）
//! - [`recovery`] は一時停止中に実データを取り直し、手動リセット等で枠が戻ったら一時停止を
//!   早期解除する（別タスク。ai-usage 連携があるときだけ）
//! - [`ui`] は描画（描画に失敗しても判定と終了処理は続ける）

mod ansi;
mod controller;
mod disk;
mod history;
mod load;
pub mod manifest;
mod process;
mod recovery;
mod system;
mod ui;
mod usage;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::rate_control;
pub use controller::read_errors;
use controller::{Action, Controller};
pub use manifest::{AiUsageCommands, Manifest};
pub use ui::summary_text;

/// 取得や入力の結果。メインのループが受け取り、状態を更新して描き直す。
pub enum Event {
    Usage(Vec<Line<'static>>),
    Disk(disk::Update),
    /// Ctrl-C（raw mode ではキー入力として届く）。
    Interrupt,
    /// ペインの大きさが変わった。
    Redraw,
    /// 端末からの入力を読めなくなった。raw mode のままだと Ctrl-C が届かないので解除する。
    InputClosed,
    /// stop file の書き込みが終わった（失敗なら理由）。
    StopWritten(Result<(), String>),
    /// 一時停止中の回復の確認の結果（早期解除を含む）。
    Recovery(recovery::Update),
}

/// stop file のロックを待つ上限。ロックを握る処理（pause の読み書きと claim）は短いので
/// 通常は待たない。握ったプロセスが止まっていても、これを過ぎたらロック無しで書く。
const STOP_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn run(manifest_path: &Path) -> Result<()> {
    let manifest = Manifest::read(manifest_path)?;
    // ペインの枠のタイトル（pane-border-format が #{pane_title} を出す）。
    print!("\x1b]2;token-burn\x1b\\");
    let _ = std::io::stdout().flush();
    let terminal = match ratatui::try_init() {
        Ok(terminal) => Some(terminal),
        Err(error) => {
            // raw mode に入った後の段階（代替画面・端末サイズの取得）で失敗すると、raw mode の
            // まま残り、Ctrl-C が SIGINT にならない。描かずに続けるので、端末を元に戻す。
            ratatui::restore();
            // 描けなくても、デッドラインでの停止と全タスク完了時の終了は続ける。
            eprintln!("token-burn monitor: cannot draw the monitor: {error}");
            None
        }
    };
    let result = event_loop(terminal, &manifest).await;
    ratatui::restore();
    result
}

struct Screen {
    terminal: Option<DefaultTerminal>,
}

impl Screen {
    fn draw(&mut self, view: &ui::View) {
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        if terminal.draw(|frame| ui::draw(frame, view)).is_err() {
            // 端末に書けなくなったら描画だけをやめる（停止判定と終了処理は続ける）。
            self.terminal = None;
        }
    }
}

async fn event_loop(terminal: Option<DefaultTerminal>, manifest: &Manifest) -> Result<()> {
    let pause_file = rate_control::pause_path_for(&manifest.stop_file);
    let mut screen = Screen { terminal };
    let mut controller = Controller::new(
        manifest.total,
        manifest.worker_count,
        manifest.deadline_epoch,
        rate_control::now_epoch(),
    );

    let (events_tx, mut events) = mpsc::channel::<Event>(64);
    if let Some(commands) = manifest.ai_usage.clone() {
        tokio::spawn(usage::run(commands, events_tx.clone()));
    }
    // 回復の確認は ai-usage 連携があるときだけ（連携が無ければ ai-usage を一切呼ばない）。
    let recovery_commands = manifest
        .ai_usage
        .clone()
        .filter(|commands| !commands.live.is_empty());
    let mut recovery_status = recovery::Status::new(recovery_commands.is_some());
    if let Some(commands) = recovery_commands {
        tokio::spawn(recovery::run(
            commands,
            manifest.stop_file.clone(),
            manifest.threshold,
            events_tx.clone(),
        ));
    }
    tokio::spawn(disk::run(
        manifest
            .ai_usage
            .as_ref()
            .map(|commands| commands.disk.clone()),
        events_tx.clone(),
    ));
    if screen.terminal.is_some() && !spawn_input_reader(events_tx.clone()) {
        // 入力を読めないまま raw mode にいると Ctrl-C を受け取れない。描くのをやめて端末を戻す
        // （戻せば Ctrl-C は SIGINT として届く）。
        screen.terminal = None;
        ratatui::restore();
    }
    let mut sigint = signal(SignalKind::interrupt()).context("failed to watch SIGINT")?;
    let mut sigterm = signal(SignalKind::terminate()).context("failed to watch SIGTERM")?;
    // tmux がセッションを閉じたとき（外から kill-session された場合を含む）はペインに SIGHUP が
    // 届く。取得中の ai-usage は別のプロセスグループなので、止めてから終わる。
    let mut sighup = signal(SignalKind::hangup()).context("failed to watch SIGHUP")?;

    let mut sampler = system::Sampler::new();
    let mut load = load::LoadHistory::default();
    let mut disk = disk::DiskState::default();
    let mut usage: Vec<Line<'static>> = Vec::new();
    let mut alerts: Vec<String> = Vec::new();
    let mut tick = tokio::time::interval(load::INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let mut actions = Vec::new();
        tokio::select! {
            _ = tick.tick() => {
                load.record(sampler.sample(Instant::now()));
                actions.extend(observe(&mut controller, manifest, &pause_file));
            }
            Some(event) = events.recv() => match event {
                Event::Usage(lines) => usage = lines,
                Event::Disk(update) => disk.apply(update, Instant::now()),
                Event::Interrupt => actions.push(controller.interrupt()),
                Event::Redraw => {}
                Event::InputClosed => {
                    let _ = ratatui::crossterm::terminal::disable_raw_mode();
                }
                Event::StopWritten(Err(error)) => {
                    // 書けないとワーカーは次のタスクへ進んでしまう。黙らず画面に出す。
                    alerts.push(format!(
                        "failed to write the stop file {}: {error}",
                        manifest.stop_file.display()
                    ));
                }
                Event::StopWritten(Ok(())) => {}
                Event::Recovery(update) => {
                    let lifted = matches!(update, recovery::Update::Lifted { .. });
                    if let Some(alert) = recovery_status.apply(update) {
                        alerts.push(alert);
                    }
                    if lifted {
                        // 解除を通知と最下行へすぐ反映する（次の 1 秒の観測を待たない）。
                        actions.extend(observe(&mut controller, manifest, &pause_file));
                    }
                }
            },
            _ = sigint.recv() => actions.push(controller.interrupt()),
            _ = sigterm.recv() => actions.push(controller.interrupt()),
            _ = sighup.recv() => {
                process::terminate_all();
                std::process::exit(0);
            }
        }

        for action in &actions {
            if let Action::WriteStop(reason) = action {
                // ロック待ちでループを止めない（描画と 2 回目の Ctrl-C を受け付け続ける）。
                let stop_file = manifest.stop_file.clone();
                let reason = *reason;
                let events = events_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result =
                        rate_control::write_stop_within(&stop_file, reason, STOP_LOCK_TIMEOUT)
                            .map(|_| ())
                            .map_err(|error| error.to_string());
                    let _ = events.blocking_send(Event::StopWritten(result));
                });
            }
        }

        screen.draw(&ui::View {
            manifest,
            controller: &controller,
            usage: &usage,
            load: &load,
            disk: &disk,
            alerts: &alerts,
            recovery: &recovery_status,
            now: Instant::now(),
        });

        if actions.contains(&Action::ForceKill) {
            force_kill(&mut screen, manifest);
        }
        if actions
            .iter()
            .any(|action| matches!(action, Action::Finish(_)))
        {
            close_session(&mut screen, &manifest.session);
        }
    }
}

/// マーカーと停止・一時停止のファイルを読んで判定へ反映し、行うべきことを返す。
fn observe(controller: &mut Controller, manifest: &Manifest, pause_file: &Path) -> Vec<Action> {
    let observation = controller::read_observation(
        &manifest.marker_dir,
        &manifest.stop_file,
        pause_file,
        rate_control::now_epoch(),
    );
    controller.observe(observation)
}

/// Ctrl-C とペインの大きさの変化を受け取る。raw mode では Ctrl-C は SIGINT ではなく
/// キー入力として届くので、ここで中断として扱う（SIGINT / SIGTERM は別に受ける）。
/// スレッドを起動できなければ false。
fn spawn_input_reader(events: mpsc::Sender<Event>) -> bool {
    std::thread::Builder::new()
        .name("monitor-input".into())
        .spawn(move || {
            loop {
                let event = match event::read() {
                    Ok(event::Event::Key(key))
                        if key.kind == KeyEventKind::Press
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        Event::Interrupt
                    }
                    Ok(event::Event::Resize(..)) => Event::Redraw,
                    Ok(_) => continue,
                    Err(_) => {
                        let _ = events.blocking_send(Event::InputClosed);
                        return;
                    }
                };
                if events.blocking_send(event).is_err() {
                    return;
                }
            }
        })
        .is_ok()
}

/// 完了・停止時はセッションごと閉じて token-burn を終了する。Ctrl-C 待ちはしない。
///
/// 画面に出した集計とログパスは Rust 側（execute_plan_tmux）が attach から戻った直後に
/// 出し直すので、読み返すために tmux を開いたままにする必要はない。モニターが自分だけ
/// 終了する方式は採らない。ワーカーが worker-done を作れずに死んだ場合（ユーザーがペインを
/// 手動で閉じた等）にセッションが残り、デッドラインまで token-burn が終わらなくなる。
/// kill-session はモニター自身のペインも道連れに終了させるため、後続には基本到達しない。
/// セッション名の解決に失敗して空振りした場合に取り残されないよう、明示的に終了する。
fn close_session(screen: &mut Screen, session: &str) -> ! {
    screen.terminal = None;
    ratatui::restore();
    // 取得中の ai-usage は別のプロセスグループなので、セッションを閉じても止まらない。
    // exit は Drop を動かさないので、ここで止める。
    process::terminate_all();
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &format!("={session}")])
        .status();
    std::process::exit(0);
}

/// 停止済みの状態での中断。走行中のタスクを待たずにセッションを閉じる。
fn force_kill(screen: &mut Screen, manifest: &Manifest) -> ! {
    screen.terminal = None;
    ratatui::restore();
    println!();
    println!(" 📁 Logs: {}", manifest.report_dir.display());
    println!(" Force killing session...");
    process::terminate_all();
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &format!("={}", manifest.session)])
        .status();
    std::process::exit(0);
}
