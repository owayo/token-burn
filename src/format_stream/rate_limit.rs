//! `rate_limit_event` の表示と、使用率が閾値を超えた際の stop file 作成を担う
//! モジュール。

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use crate::format_stream::util::truncate_str;

/// レート制限イベントを表示する。
/// `allowed_warning` は使用率の警告、`rejected` はリクエスト拒否を示す。
/// 使用率が閾値を超えた場合、stop_file を作成して後続タスクを停止する。
pub(crate) fn handle_rate_limit_event(
    v: &serde_json::Value,
    out: &mut impl Write,
    stop_file: Option<&Path>,
    threshold: u8,
) -> Result<()> {
    let info = &v["rate_limit_info"];
    let status = info["status"].as_str().unwrap_or("");
    let resets_at = format_resets_at(info);
    match status {
        "allowed" => {
            let limit_type = info["rateLimitType"].as_str().unwrap_or("");
            let extras = format_overage_details(info);
            if !extras.is_empty() || !resets_at.is_empty() {
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
            }
        }
        "allowed_warning" => {
            let utilization = info["utilization"].as_f64().unwrap_or(0.0);
            let pct = utilization * 100.0;
            let limit_type = info["rateLimitType"].as_str().unwrap_or("");
            // サーバー側が利用率を判定した閾値 (例: 0.9 → 通過済み警告閾値 90%)
            let surpassed = info["surpassedThreshold"]
                .as_f64()
                .filter(|v| *v > 0.0)
                .map(|v| format!(" (warning at {:.0}%)", v * 100.0))
                .unwrap_or_default();
            // overage を消費中かどうかは「この警告が通常枠の話か超過枠の話か」を分ける
            // 判断材料になる（実データの警告イベントは isUsingOverage を常に持つ）。
            let overage = wrap_overage_details(info);
            if pct >= threshold as f64 {
                writeln!(
                    out,
                    "\x1b[31m  \u{26d4} Rate limit auto-stop: {:.0}% used ({}){}{} >= threshold {}%{}\x1b[0m",
                    pct, limit_type, surpassed, overage, threshold, resets_at
                )?;
                touch_stop_file(out, stop_file)?;
            } else {
                writeln!(
                    out,
                    "\x1b[33m  \u{26a0} Rate limit warning: {:.0}% used ({}){}{}{}\x1b[0m",
                    pct, limit_type, surpassed, overage, resets_at
                )?;
            }
        }
        "rejected" => {
            let limit_type = info["rateLimitType"].as_str().unwrap_or("");
            // 実データの rejected は overageStatus / overageResetsAt / isUsingOverage を
            // 伴う。これらを落とすと「resets <5時間枠の時刻>」だけが残り、実際には超過枠
            // まで使い切って復旧が数週間先（overageResetsAt）でも、その時刻まで待てば
            // 再開できるように読めてしまう。allowed と同じ補足を付けて誤読を防ぐ。
            writeln!(
                out,
                "\x1b[31m  \u{1f6ab} Rate limited: request rejected ({}){}{}\x1b[0m",
                limit_type,
                wrap_overage_details(info),
                resets_at
            )?;
            touch_stop_file(out, stop_file)?;
        }
        _ => {} // "allowed" は表示不要
    }
    Ok(())
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
    format_timestamp_clock(info, "resetsAt")
        .map(|time| format!(" resets {time}"))
        .unwrap_or_default()
}

/// レート制限のリセット時刻をローカル時刻へ整形する。
///
/// 当日中なら `HH:MM`、翌日以降なら `MM/DD HH:MM` を返す。時刻だけを出すと、
/// `seven_day` 枠（最大 7 日先）や overage 枠（実データで 28 日先）のリセットが
/// 「今日のその時刻」に見えてしまい、待てば再開できると誤読される。実ログでは
/// 復旧が 1 か月先でも `resets 09:00` としか出ていなかった。
fn format_timestamp_clock(info: &serde_json::Value, key: &str) -> Option<String> {
    let ts = info[key].as_i64()?;
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
/// 生成されないことを意味する。`format-stream` はパイプ中段（`cmd | format-stream | tee`）で
/// 動くため exit code も観測されず、握り潰すと安全機構が無言で失効する。よって失敗は
/// `out` に明示してログ上で可視化する。
fn touch_stop_file(out: &mut impl Write, stop_file: Option<&Path>) -> Result<()> {
    if let Some(path) = stop_file {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                writeln!(
                    out,
                    "\x1b[31m  \u{26d4} stop file ({}) の作成に失敗しました: {}（後続タスクの自動停止が効きません）\x1b[0m",
                    path.display(),
                    e
                )?;
            }
        }
    }
    Ok(())
}
