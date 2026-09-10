//! `rate_limit_event` の表示と、使用率が閾値を超えた際の stop file 作成を担う
//! モジュール。

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use crate::format_stream::util::truncate_str;
use crate::rate_control::{self, Basis, Decision, WindowKind, WindowObservation};

/// 自動停止の判定に使う枠（実際にリクエストを止める枠）と、併記用の短縮名。
///
/// `overage`（月次の追加課金枠）と `seven_day_overage_included`（overage 込みで
/// 分母が変わる派生指標）は含めない。前者は課金枠であって実行を止める枠ではなく、
/// 後者は overage を消費する前提の指標で、実データでは常に `seven_day` より小さい
/// （分母が増えるぶん率が下がる）ため、判定に入れる意味が無い。
const GATING_WINDOWS: [(&str, &str); 2] = [("five_hour", "5h"), ("seven_day", "7d")];

/// 追加課金枠を指す `rateLimitType` の値。
const OVERAGE_TYPE: &str = "overage";

/// 停止判定に使える枠の実測値。
struct GatingWindow {
    /// `unifiedWindows` のキー（`five_hour` 等）。停止理由の表示に使う。
    name: &'static str,
    /// 併記用の短縮名（`5h` 等）。
    short: &'static str,
    utilization: f64,
    /// その枠自身のリセット時刻。top-level の `resetsAt` は `rateLimitType` が
    /// 指す枠のもので、停止した枠のリセットとは限らない。
    resets_at: Option<i64>,
}

impl GatingWindow {
    /// 周期で分類した観測値へ変換する。5 時間枠はリセットで回復するので一時停止、
    /// 7 日枠はデッドラインと同周期なので恒久停止の根拠になる。
    fn observation(&self) -> WindowObservation {
        let kind = if self.name == "five_hour" {
            WindowKind::FIVE_HOUR
        } else {
            WindowKind::Deadline
        };
        WindowObservation::new(kind, self.name, self.utilization * 100.0)
            .with_reset_at(self.resets_at)
    }
}

/// レート制限イベントを表示する。
/// `allowed_warning` は使用率の警告、`rejected` はリクエスト拒否を示す。
/// 使用率が閾値を超えた場合、stop_file を作成して後続タスクを停止する。
pub(crate) fn handle_rate_limit_event(
    v: &serde_json::Value,
    out: &mut impl Write,
    stop_file: Option<&Path>,
    threshold: u8,
    signal_failed: &mut bool,
) -> Result<()> {
    let info = &v["rate_limit_info"];
    let status = info["status"].as_str().unwrap_or("");
    match status {
        // `allowed` と `allowed_warning` はどちらもリクエストが通っている状態であり、
        // 違いはサーバ側が警告閾値を跨いだかどうかだけ。停止判定は同じ基準（実際に
        // 実行を止める枠の使用率）で行い、表示の詳しさだけを分ける。`allowed` を
        // 判定から外すと、`rate_limit_threshold` を警告閾値より低く設定した場合に
        // 停止が漏れる。
        "allowed" | "allowed_warning" => {
            let windows = gating_windows(info);
            match decide(info, &windows, threshold) {
                Decision::Stop { basis, detail } => {
                    write_auto_stop(out, info, &basis, detail.as_deref(), &windows, threshold)?;
                    apply_stop(out, stop_file, &basis.reason(threshold), signal_failed)?;
                }
                Decision::Pause { resume_at, basis } => {
                    write_auto_pause(out, info, &basis, resume_at, &windows, threshold)?;
                    let reason = basis.reason(threshold);
                    apply_pause(out, stop_file, resume_at, &basis, &reason, signal_failed)?;
                }
                Decision::Proceed => {
                    if status == "allowed_warning" {
                        write_warning(out, info, &windows)?;
                    } else {
                        write_allowed(out, info)?;
                    }
                }
            }
        }
        "rejected" => {
            let limit_type = info["rateLimitType"].as_str().unwrap_or("");
            let windows = gating_windows(info);
            // 実データの rejected は overageStatus / overageResetsAt / isUsingOverage を
            // 伴う。これらを落とすと「resets <5時間枠の時刻>」だけが残り、実際には超過枠
            // まで使い切って復旧が数週間先（overageResetsAt）でも、その時刻まで待てば
            // 再開できるように読めてしまう。allowed と同じ補足を付けて誤読を防ぐ。
            writeln!(
                out,
                "\x1b[31m  \u{1f6ab} Rate limited: request rejected ({}){}{}\x1b[0m",
                limit_type,
                wrap_overage_details(info),
                format_resets_at(info)
            )?;
            // 拒否の原因が 5 時間枠だけだと確認できたときは、その枠のリセットまでの
            // 一時停止で足りる。確認できなければ従来どおり恒久停止（fail-closed）。
            match rejected_decision(info, &windows, threshold) {
                Some((resume_at, basis)) => {
                    write_rejected_pause(out, &basis, resume_at)?;
                    // 拒否は閾値超過とは別の理由で起きる（実データでは 5 時間枠が
                    // 42% でも拒否される）。閾値の不等式を書くと、後から pause file を
                    // 読んだときに成立していない条件が待機の根拠に見える。
                    let reason = format!(
                        "request rejected ({limit_type}); {} at {:.0}%",
                        basis.window, basis.used_percent
                    );
                    apply_pause(out, stop_file, resume_at, &basis, &reason, signal_failed)?;
                }
                None => apply_stop(
                    out,
                    stop_file,
                    &format!("request rejected ({limit_type})"),
                    signal_failed,
                )?,
            }
        }
        _ => {}
    }
    Ok(())
}

/// `allowed` / `allowed_warning` の閾値判定。
///
/// `unifiedWindows` があるときは枠ごとの実測値で判定する。無い古い形式では枠の種類が
/// 分からないため、`rateLimitType` から周期を推定し、判別できなければ週次（＝恒久停止）
/// として扱う。待てば回復すると誤って判断するより、止まる側へ倒す。
fn decide(info: &serde_json::Value, windows: &[GatingWindow], threshold: u8) -> Decision {
    let now = rate_control::now_epoch();
    if !windows.is_empty() {
        let observations: Vec<WindowObservation> =
            windows.iter().map(GatingWindow::observation).collect();
        return rate_control::evaluate(&observations, threshold, now);
    }

    // `unifiedWindows` を持たない形式へのフォールバック。overage は実行を止める枠では
    // ないため、その使用率では停止しない（判定基準が無い曖昧な警告では fail-open に
    // 倒す。本当に枯れていれば `rejected` か、実行を止める枠側の警告として届く）。
    let limit_type = info["rateLimitType"].as_str().unwrap_or("");
    if limit_type == OVERAGE_TYPE {
        return Decision::Proceed;
    }
    let Some(utilization) = info["utilization"]
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)
    else {
        return Decision::Proceed;
    };
    let kind = if limit_type == "five_hour" {
        WindowKind::FIVE_HOUR
    } else {
        WindowKind::Deadline
    };
    let label = if limit_type.is_empty() {
        "unknown"
    } else {
        limit_type
    };
    let observation = WindowObservation::new(kind, label, utilization * 100.0)
        .with_reset_at(info["resetsAt"].as_i64());
    rate_control::evaluate(&[observation], threshold, now)
}

/// `rejected` を 5 時間枠のリセットまでの一時停止に落とせるか判定する。
///
/// 落とせるのは「拒否の主語が 5 時間枠」かつ「週次枠に余裕がある」ことを実測値で
/// 確認できたときだけ。overage が絡む拒否は、月次の追加課金枠を使い切った状態であり、
/// 5 時間枠のリセットを待っても再開できない（実データの復旧は 28 日先）ため対象外。
fn rejected_decision(
    info: &serde_json::Value,
    windows: &[GatingWindow],
    threshold: u8,
) -> Option<(i64, Basis)> {
    if info["rateLimitType"].as_str()? != "five_hour" {
        return None;
    }
    // overage を使っていないことを**明示的に**確認できたときだけ先へ進む。フィールドが
    // 欠けているだけで一時停止を許すと、「確認できた」ではなく「確認できなかった」状態で
    // 待機に倒れる。実データの `rejected` は必ずこれらを伴うため、欠損は想定外の形式で
    // あり、そこで安全側を選ぶ。
    let is_using = info["isUsingOverage"].as_bool();
    let in_use = info["overageInUse"].as_bool();
    let uses_overage = is_using == Some(true) || in_use == Some(true);
    let confirmed_not_using = is_using == Some(false) || in_use == Some(false);
    if uses_overage || !confirmed_not_using {
        return None;
    }
    // 週次枠の実測値が読めない、または閾値に触れているなら一時停止にはできない。
    let weekly = windows.iter().find(|w| w.name == "seven_day")?;
    if weekly.utilization * 100.0 >= f64::from(threshold) {
        return None;
    }
    let five_hour = windows.iter().find(|w| w.name == "five_hour")?;
    // 使用率に関わらず「拒否された」事実で止めるため、閾値ではなく枠のリセット時刻の
    // 妥当性だけを evaluate に確かめさせる（100% として渡す）。
    let observation = WindowObservation::new(WindowKind::FIVE_HOUR, five_hour.name, 100.0)
        .with_reset_at(five_hour.resets_at);
    match rate_control::evaluate(&[observation], threshold, rate_control::now_epoch()) {
        Decision::Pause {
            resume_at,
            mut basis,
        } => {
            basis.used_percent = five_hour.utilization * 100.0;
            Some((resume_at, basis))
        }
        _ => None,
    }
}

/// `unifiedWindows` から停止判定に使える枠を取り出す。
///
/// 使用率が数値として読めない枠（欠損 / NaN / 負値）は判定の基準にできないため除外する。
fn gating_windows(info: &serde_json::Value) -> Vec<GatingWindow> {
    GATING_WINDOWS
        .iter()
        .filter_map(|(name, short)| {
            let window = &info["unifiedWindows"][*name];
            let utilization = window["utilization"]
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)?;
            Some(GatingWindow {
                name,
                short,
                utilization,
                resets_at: window["resetsAt"].as_i64(),
            })
        })
        .collect()
}

/// 閾値超過で停止した行を書く。
fn write_auto_stop(
    out: &mut impl Write,
    info: &serde_json::Value,
    basis: &Basis,
    detail: Option<&str>,
    windows: &[GatingWindow],
    threshold: u8,
) -> Result<()> {
    // `surpassedThreshold` はサーバーが top-level の `rateLimitType` について通過を
    // 報告した閾値。停止理由が別の枠になったとき（overage の警告イベントに含まれる
    // 5 時間枠が閾値を超えた場合など）に併記すると、その枠の警告閾値だと誤読される。
    let surpassed = if basis.window == info["rateLimitType"].as_str().unwrap_or("") {
        format_surpassed_threshold(info)
    } else {
        String::new()
    };
    // 恒久停止を選んだ理由（リセット時刻が読めない等）は、なぜ一時停止で済まなかったかの
    // 唯一の手掛かりなので落とさない。
    let detail = detail.map(|d| format!(" ({d})")).unwrap_or_default();
    writeln!(
        out,
        "\x1b[31m  \u{26d4} Rate limit auto-stop: {:.0}% used ({}){}{}{}{} >= threshold {}%{}\x1b[0m",
        basis.used_percent,
        basis.window,
        surpassed,
        detail,
        wrap_overage_details(info),
        // 停止した枠しか実測値が無いときは本文と同じ値の繰り返しになるので併記しない。
        breakdown_suffix(windows, windows.len() > 1),
        threshold,
        format_reset_suffix(basis.reset_at),
    )?;
    Ok(())
}

/// 閾値超過だが、その枠のリセットで回復する（＝一時停止で済む）行を書く。
fn write_auto_pause(
    out: &mut impl Write,
    info: &serde_json::Value,
    basis: &Basis,
    resume_at: i64,
    windows: &[GatingWindow],
    threshold: u8,
) -> Result<()> {
    let surpassed = if basis.window == info["rateLimitType"].as_str().unwrap_or("") {
        format_surpassed_threshold(info)
    } else {
        String::new()
    };
    writeln!(
        out,
        "\x1b[33m  \u{23f8} Rate limit pause: {:.0}% used ({}){}{}{} >= threshold {}% — resuming{}\x1b[0m",
        basis.used_percent,
        basis.window,
        surpassed,
        wrap_overage_details(info),
        breakdown_suffix(windows, windows.len() > 1),
        threshold,
        format_resume_suffix(resume_at),
    )?;
    Ok(())
}

/// 拒否されたが 5 時間枠のリセットで回復する場合の補足行を書く。
fn write_rejected_pause(out: &mut impl Write, basis: &Basis, resume_at: i64) -> Result<()> {
    writeln!(
        out,
        "\x1b[33m  \u{23f8} Pausing until the {} window resets{}\x1b[0m",
        basis.window,
        format_resume_suffix(resume_at),
    )?;
    Ok(())
}

/// ` at <時刻>` の接尾辞。時刻へ整形できなければ空文字列。
fn format_resume_suffix(resume_at: i64) -> String {
    format_clock(resume_at)
        .map(|t| format!(" at {t}"))
        .unwrap_or_default()
}

/// 閾値未満の警告行を書く。
fn write_warning(
    out: &mut impl Write,
    info: &serde_json::Value,
    windows: &[GatingWindow],
) -> Result<()> {
    let pct = info["utilization"].as_f64().unwrap_or(0.0) * 100.0;
    let limit_type = info["rateLimitType"].as_str().unwrap_or("");
    // 停止判定に使わない枠（overage）の警告は、その % で止まると誤読されやすいので
    // `no auto-stop` を明示する。判定に使う枠の実測値は、主語が何であれ併記する。
    // 主語が 5 時間枠でも、警告に出ない 7 日枠の残量や、壊れて判定から外れた枠が
    // あることは、なぜ止まった（止まらなかった）のかを読むのに必要な情報になる。
    let label = if limit_type.is_empty() || is_gating_type(limit_type) {
        limit_type.to_string()
    } else {
        format!("{limit_type}, no auto-stop")
    };
    writeln!(
        out,
        "\x1b[33m  \u{26a0} Rate limit warning: {:.0}% used ({}){}{}{}{}\x1b[0m",
        pct,
        label,
        format_surpassed_threshold(info),
        wrap_overage_details(info),
        breakdown_suffix(windows, true),
        format_resets_at(info)
    )?;
    Ok(())
}

/// `allowed`（警告閾値未満）の補足情報を書く。補足が何も無ければ何も書かない。
fn write_allowed(out: &mut impl Write, info: &serde_json::Value) -> Result<()> {
    let limit_type = info["rateLimitType"].as_str().unwrap_or("");
    let resets_at = format_resets_at(info);
    let extras = format_overage_details(info);
    if extras.is_empty() && resets_at.is_empty() {
        return Ok(());
    }
    let details = if limit_type.is_empty() {
        extras
    } else if extras.is_empty() {
        limit_type.to_string()
    } else {
        format!("{limit_type} {extras}")
    };
    let details = truncate_str(&details, 80);
    if details.is_empty() {
        writeln!(
            out,
            "\x1b[2m  \u{2139} Rate limit status: allowed{}\x1b[0m",
            resets_at
        )?;
    } else {
        writeln!(
            out,
            "\x1b[2m  \u{2139} Rate limit status: allowed ({}){}\x1b[0m",
            details, resets_at
        )?;
    }
    Ok(())
}

/// 停止判定に使う枠かどうか。
fn is_gating_type(limit_type: &str) -> bool {
    GATING_WINDOWS.iter().any(|(name, _)| *name == limit_type)
}

/// 停止判定に使う枠の実測値を ` [5h 13% / 7d 54%]` の形で返す。
/// `include` が false、または実測値が無ければ空文字列。
fn breakdown_suffix(windows: &[GatingWindow], include: bool) -> String {
    if !include || windows.is_empty() {
        return String::new();
    }
    let summary = windows
        .iter()
        .map(|w| format!("{} {:.0}%", w.short, w.utilization * 100.0))
        .collect::<Vec<_>>()
        .join(" / ");
    format!(" [{summary}]")
}

/// サーバー側が利用率を判定した閾値（例: 0.9 → 通過済み警告閾値 90%）を返す。
fn format_surpassed_threshold(info: &serde_json::Value) -> String {
    info["surpassedThreshold"]
        .as_f64()
        .filter(|v| *v > 0.0)
        .map(|v| format!(" (warning at {:.0}%)", v * 100.0))
        .unwrap_or_default()
}

/// overage（超過枠）の補足情報を括弧付きで返す。空なら空文字列。
/// `allowed` 以外のステータスは補足用の括弧を持たないため、この形で本文へ差し込む。
fn wrap_overage_details(info: &serde_json::Value) -> String {
    let details = format_overage_details(info);
    if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", truncate_str(&details, 80))
    }
}

/// overage（超過枠）の補足情報を 1 行表示用に連結する。
fn format_overage_details(info: &serde_json::Value) -> String {
    let mut parts = Vec::new();

    if let Some(overage_status) = info["overageStatus"].as_str()
        && !overage_status.is_empty()
    {
        parts.push(format!("overage:{overage_status}"));
    }
    // 実データでは同義の `isUsingOverage` と `overageInUse` の両方が現れる
    // （rejected イベントでは両方が同時に立つ）。どちらか一方でも true なら表示する。
    if info["isUsingOverage"].as_bool() == Some(true)
        || info["overageInUse"].as_bool() == Some(true)
    {
        parts.push("using_overage".to_string());
    }
    if let Some(reason) = info["overageDisabledReason"].as_str()
        && !reason.is_empty()
    {
        parts.push(format!("reason:{reason}"));
    }
    if let Some(overage_resets) = format_timestamp_clock(info, "overageResetsAt") {
        parts.push(format!("overage_resets:{overage_resets}"));
    }

    parts.join(" ")
}

/// `resetsAt` Unix タイムスタンプをローカル時刻の文字列に整形する。
/// フィールドが存在しない場合は空文字列を返す。
pub(crate) fn format_resets_at(info: &serde_json::Value) -> String {
    format_reset_suffix(info["resetsAt"].as_i64())
}

/// Unix タイムスタンプを ` resets <時刻>` の接尾辞へ整形する。`None` なら空文字列。
fn format_reset_suffix(ts: Option<i64>) -> String {
    ts.and_then(format_clock)
        .map(|time| format!(" resets {time}"))
        .unwrap_or_default()
}

/// レート制限のリセット時刻をローカル時刻へ整形する。
fn format_timestamp_clock(info: &serde_json::Value, key: &str) -> Option<String> {
    format_clock(info[key].as_i64()?)
}

/// Unix タイムスタンプをローカル時刻へ整形する。
///
/// 当日中なら `HH:MM`、翌日以降なら `MM/DD HH:MM` を返す。時刻だけを出すと、
/// `seven_day` 枠（最大 7 日先）や overage 枠（実データで 28 日先）のリセットが
/// 「今日のその時刻」に見えてしまい、待てば再開できると誤読される。実ログでは
/// 復旧が 1 か月先でも `resets 09:00` としか出ていなかった。
fn format_clock(ts: i64) -> Option<String> {
    let at = chrono::DateTime::from_timestamp(ts, 0)?.with_timezone(&chrono::Local);
    Some(format_reset_datetime(at, chrono::Local::now()))
}

/// リセット日時を「当日は時刻のみ / 別日は日付付き」で整形する。
fn format_reset_datetime(
    at: chrono::DateTime<chrono::Local>,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    if at.date_naive() == now.date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%m/%d %H:%M").to_string()
    }
}

/// stop_file が指定されていれば冪等に作成する（全ワーカーの後続タスクを停止するシグナル）。
/// 既存ファイルは上書きせず、`create_new` で並列ワーカー間の race を回避する。
///
/// 既存ファイル（`AlreadyExists`）は別ワーカーが既に作成済みの冪等な正常系として無視する。
/// それ以外の作成失敗（ENOSPC・権限不足等）は、後続タスクを止める停止シグナルが
/// 生成されないことを意味する。ログに明示するだけでなく `signal_failed` を立て、
/// `format-stream` 自体を停止シグナル書き込み不能の終了コードで終わらせる。ここを
/// ログだけで済ませると、止めるべき状況で全ワーカーが走り続ける（fail-open）。
fn apply_stop(
    out: &mut impl Write,
    stop_file: Option<&Path>,
    reason: &str,
    signal_failed: &mut bool,
) -> Result<()> {
    let Some(path) = stop_file else {
        return Ok(());
    };
    if let Err(e) = rate_control::write_stop(path, reason) {
        *signal_failed = true;
        writeln!(
            out,
            "\x1b[31m  \u{26d4} stop file ({}) の作成に失敗しました: {}（後続タスクの自動停止が効きません）\x1b[0m",
            path.display(),
            e
        )?;
    }
    Ok(())
}

/// 一時停止を pause file へ記録する（`resume_at` 以降は後続タスクを再開してよい）。
///
/// 恒久停止と違い、こちらは「いつまで止めるか」を持つ。書き込みに失敗した場合は
/// 恒久停止へフォールバックして止める側に倒し、そちらも書けなければ `signal_failed` が
/// 立つ（`apply_stop` 参照）。
/// `reason` は pause file に残り、待機がデッドラインを越えたときの停止理由にもなる。
/// 閾値超過で待つ場合と `rejected` で待つ場合では成立している条件が違うため、
/// 呼び出し側が事実に合う文言を渡す（`Basis::reason` を無条件に使うと、`rejected`
/// では `five_hour 42% >= threshold 95%` のような成立していない不等式が残る）。
fn apply_pause(
    out: &mut impl Write,
    stop_file: Option<&Path>,
    resume_at: i64,
    basis: &Basis,
    reason: &str,
    signal_failed: &mut bool,
) -> Result<()> {
    let Some(path) = stop_file else {
        return Ok(());
    };
    let state = rate_control::PauseState {
        resume_at,
        window: basis.window.clone(),
        reason: reason.to_string(),
    };
    if let Err(e) = rate_control::write_pause(&rate_control::pause_path_for(path), &state) {
        writeln!(
            out,
            "\x1b[31m  \u{26d4} pause file の書き込みに失敗しました: {e}（恒久停止へ切り替えます）\x1b[0m",
        )?;
        apply_stop(out, stop_file, reason, signal_failed)?;
    }
    Ok(())
}
