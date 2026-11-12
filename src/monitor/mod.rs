//! バックグラウンド実行プロセス内のモニター。画面を閉じても判定と計測を続ける。
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

pub(crate) mod ansi;
mod controller;
mod disk;
mod history;
pub(crate) mod inference;
mod load;
pub mod manifest;
pub(crate) mod process;
mod recovery;
mod system;

// 実行計画の表示（executor）も、モニターが一時停止を早期解除できるかを同じ条件で判定する。
pub(crate) use recovery::not_liftable;
mod ui;
mod usage;

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::text::Line;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::rate_control;
pub use controller::read_errors;
use controller::{Action, Controller, Finish};
pub use manifest::{AiUsageCommands, Manifest};
pub use ui::summary_text;

/// 取得や入力の結果。メインのループが受け取り、状態を更新して描き直す。
pub enum Event {
    Usage(Vec<Line<'static>>),
    Disk(disk::Update),
    /// stop file の書き込みが終わった（失敗なら理由）。
    StopWritten(Result<(), String>),
    /// 一時停止中の回復の確認の結果（早期解除を含む）。
    Recovery(recovery::Update),
    Inference(inference::Settings),
}

/// stop file のロックを待つ上限。ロックを握る処理（pause の読み書きと claim）は短いので
/// 通常は待たない。握ったプロセスが止まっていても、これを過ぎたらロック無しで書く。
const STOP_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// Monitor control stays in the detached runner even when no UI is attached.
pub async fn run(
    manifest_path: &Path,
    mut interrupts: mpsc::UnboundedReceiver<()>,
) -> Result<bool> {
    let mut manifest = Manifest::read(manifest_path)?;
    let manifest = &mut manifest;
    let pause_file = rate_control::pause_path_for(&manifest.stop_file);
    let directory = manifest_path.parent().unwrap_or(Path::new("."));
    let mut controller = Controller::new(
        manifest.total,
        manifest.worker_count,
        manifest.deadline_epoch,
        rate_control::now_epoch(),
    );

    let (events_tx, mut events) = mpsc::channel::<Event>(64);
    if manifest.inference.provider.is_some() {
        tokio::spawn(inference::run(
            manifest.inference.clone(),
            manifest.report_dir.clone(),
            manifest.marker_dir.clone(),
            events_tx.clone(),
        ));
    }
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
    let mut termination = crate::platform::Signals::new().context("failed to watch termination")?;

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
                Event::Inference(settings) => manifest.inference.settings = settings,
                Event::Disk(update) => disk.apply(update, Instant::now()),
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
            _ = termination.recv() => actions.push(controller.interrupt()),
            Some(()) = interrupts.recv() => actions.push(controller.interrupt()),
        }

        refresh_workload(&mut controller, manifest);
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

        let view = ui::View {
            manifest,
            controller: &controller,
            usage: &usage,
            load: &load,
            disk: &disk,
            alerts: &alerts,
            recovery: &recovery_status,
            now: Instant::now(),
        };
        let (rows, cols) =
            crate::runtime::read_json::<(u16, u16)>(&directory.join("monitor-size.json"))
                .unwrap_or((40, 40));
        let lines = ui::layout(
            &view,
            usize::from(cols.clamp(1, 500)),
            usize::from(rows.clamp(1, 300)),
        );
        if let Err(error) = crate::runtime::atomic_json(
            &directory.join("monitor-view.json"),
            &crate::runtime::ansi_lines(&lines),
        ) {
            eprintln!("Cannot publish monitor view: {error:#}");
        }
        if actions.contains(&Action::ForceKill) {
            return Ok(true);
        }
        let adding = controller.finished() == Some(Finish::Completed)
            && crate::runtime::read_json::<i64>(&directory.join("add-lease.json"))
                .is_ok_and(|until| until > rate_control::now_epoch())
            && !manifest.stop_file.try_exists().unwrap_or(true)
            && rate_control::now_epoch() < manifest.deadline_epoch
            && !actions
                .iter()
                .any(|action| matches!(action, Action::WriteStop(_)));
        if controller.finished().is_some() && !adding {
            return Ok(false);
        }
    }
}

/// マーカーと停止・一時停止のファイルを読んで判定へ反映し、行うべきことを返す。
fn observe(controller: &mut Controller, manifest: &mut Manifest, pause_file: &Path) -> Vec<Action> {
    refresh_workload(controller, manifest);
    let observation = controller::read_observation(
        &manifest.marker_dir,
        &manifest.stop_file,
        pause_file,
        rate_control::now_epoch(),
    );
    controller.observe(observation)
}

fn refresh_workload(controller: &mut Controller, manifest: &mut Manifest) {
    if let Some(directory) = manifest.marker_dir.parent()
        && let Ok(session) =
            crate::runtime::read_json::<crate::runtime::Session>(&directory.join("session.json"))
        && session.monitor.total > manifest.total
    {
        manifest.total = session.monitor.total;
        controller.extend(manifest.total);
    }
}
