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
            let extras = format_rate_limit_allowed_details(info);
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
            if pct >= threshold as f64 {
                touch_stop_file(stop_file);
                writeln!(
                    out,
                    "\x1b[31m  \u{26d4} Rate limit auto-stop: {:.0}% used ({}){} >= threshold {}%{}\x1b[0m",
                    pct, limit_type, surpassed, threshold, resets_at
                )?;
            } else {
                writeln!(
                    out,
                    "\x1b[33m  \u{26a0} Rate limit warning: {:.0}% used ({}){}{}\x1b[0m",
                    pct, limit_type, surpassed, resets_at
                )?;
            }
        }
        "rejected" => {
            let limit_type = info["rateLimitType"].as_str().unwrap_or("");
            touch_stop_file(stop_file);
            writeln!(
                out,
                "\x1b[31m  \u{1f6ab} Rate limited: request rejected ({}){}\x1b[0m",
                limit_type, resets_at
            )?;
        }
        _ => {} // "allowed" は表示不要
    }
    Ok(())
}

/// `allowed` の補足情報を 1 行表示用に連結する。
fn format_rate_limit_allowed_details(info: &serde_json::Value) -> String {
    let mut parts = Vec::new();

    if let Some(overage_status) = info["overageStatus"].as_str()
        && !overage_status.is_empty()
    {
        parts.push(format!("overage:{overage_status}"));
    }
    if info["isUsingOverage"].as_bool() == Some(true) {
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

fn format_timestamp_clock(info: &serde_json::Value, key: &str) -> Option<String> {
    info[key]
        .as_i64()
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

/// stop_file が指定されていれば冪等に作成する（全ワーカーの後続タスクを停止するシグナル）。
/// 既存ファイルは上書きせず、`create_new` で並列ワーカー間の race を回避する。
fn touch_stop_file(stop_file: Option<&Path>) {
    if let Some(path) = stop_file {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path);
    }
}
