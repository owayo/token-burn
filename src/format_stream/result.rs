//! `result` イベント受信時に表示する集計フッター（コスト・所要時間・トークン・
//! キャッシュ・モデル別使用量・終了情報）を生成するモジュール。
//! `handle_result` は各行の生成を担う `write_*` ヘルパーを順に呼ぶ薄い入口。

use anyhow::Result;
use std::io::Write;

use crate::format_stream::state::StreamSummary;
use crate::format_stream::util::{format_millis_as_seconds, format_number, format_token_size};

pub(crate) fn handle_result(
    v: &serde_json::Value,
    summary: &StreamSummary,
    out: &mut impl Write,
) -> Result<()> {
    write_cost(v, out)?;
    write_duration(v, out)?;
    write_token_summary(summary, out)?;
    write_cache_summary(summary, out)?;
    write_model_stop(summary, out)?;
    write_web_summary(summary, out)?;
    write_usage_metadata(v, out)?;
    write_model_usage(v, out)?;
    write_terminal_info(v, out)?;
    Ok(())
}

/// 総コスト（USD）を表示する。
fn write_cost(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    if let Some(cost) = v["total_cost_usd"].as_f64() {
        writeln!(out, "\n\x1b[33m\u{1f4b0} ${:.4}\x1b[0m", cost)?;
    }
    Ok(())
}

/// 所要時間と turns / api / ttft 属性を表示する。
fn write_duration(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    if let Some(ms) = v["duration_ms"].as_u64() {
        let secs = ms / 1000;
        let m = secs / 60;
        let s = secs % 60;
        let mut attrs = Vec::new();
        if let Some(turns) = v["num_turns"].as_u64() {
            attrs.push(format!("{turns} turns"));
        }
        if let Some(api_ms) = v["duration_api_ms"].as_u64() {
            let api_secs = api_ms / 1000;
            attrs.push(format!("api:{}m {}s", api_secs / 60, api_secs % 60));
        }
        if let Some(ttft_ms) = v["ttft_ms"].as_u64() {
            attrs.push(format!("ttft:{}", format_millis_as_seconds(ttft_ms)));
        }
        // ttft_stream_ms: 初回ストリームトークン到達時間。キュー/リトライ待ちを含む ttft_ms より
        // 小さい純粋なストリーム遅延で、両者の差が待ち時間の目安になる。
        if let Some(stream_ms) = v["ttft_stream_ms"].as_u64() {
            attrs.push(format!("stream:{}", format_millis_as_seconds(stream_ms)));
        }
        // time_to_request_ms: リクエスト送信までの所要時間。通常は数十〜数百 ms のためミリ秒で表示。
        if let Some(req_ms) = v["time_to_request_ms"].as_u64() {
            attrs.push(format!("req:{req_ms}ms"));
        }
        if attrs.is_empty() {
            writeln!(out, "\x1b[33m\u{23f1}  {}m {}s\x1b[0m", m, s)?;
        } else {
            writeln!(
                out,
                "\x1b[33m\u{23f1}  {}m {}s ({})\x1b[0m",
                m,
                s,
                attrs.join(" ")
            )?;
        }
    }
    Ok(())
}

/// 入力/出力トークンの合計を表示する。
fn write_token_summary(summary: &StreamSummary, out: &mut impl Write) -> Result<()> {
    let input = summary.usage.total_input_tokens();
    let output = summary.usage.output_tokens;
    if output > 0 {
        writeln!(
            out,
            "\x1b[33m\u{1f4ca} in:{} out:{}\x1b[0m",
            format_number(input),
            format_number(output)
        )?;
    }
    Ok(())
}

/// キャッシュ内訳（read / write5m / write1h）を表示する。
fn write_cache_summary(summary: &StreamSummary, out: &mut impl Write) -> Result<()> {
    if summary.usage.has_cache_details() {
        let mut details = Vec::new();
        if summary.usage.cache_read_input_tokens > 0 {
            details.push(format!(
                "read:{}",
                format_number(summary.usage.cache_read_input_tokens)
            ));
        }
        if summary.usage.cache_write_5m_tokens() > 0 {
            details.push(format!(
                "write5m:{}",
                format_number(summary.usage.cache_write_5m_tokens())
            ));
        }
        if summary.usage.cache_creation_1h_input_tokens > 0 {
            details.push(format!(
                "write1h:{}",
                format_number(summary.usage.cache_creation_1h_input_tokens)
            ));
        }
        writeln!(out, "\x1b[2m   cache {}\x1b[0m", details.join(" "))?;
    }
    Ok(())
}

/// モデル名と stop_reason（end_turn 以外）を表示する。
fn write_model_stop(summary: &StreamSummary, out: &mut impl Write) -> Result<()> {
    if let Some(model) = &summary.model
        && !model.is_empty()
    {
        writeln!(out, "\x1b[2m   model {}\x1b[0m", model)?;
    }
    if let Some(stop_reason) = &summary.stop_reason
        && !stop_reason.is_empty()
        && stop_reason != "end_turn"
    {
        writeln!(out, "\x1b[2m   stop {}\x1b[0m", stop_reason)?;
    }
    Ok(())
}

/// Web 検索/フェッチ回数を表示する。
fn write_web_summary(summary: &StreamSummary, out: &mut impl Write) -> Result<()> {
    if summary.usage.web_search_requests > 0 || summary.usage.web_fetch_requests > 0 {
        let mut parts = Vec::new();
        if summary.usage.web_search_requests > 0 {
            parts.push(format!("search:{}", summary.usage.web_search_requests));
        }
        if summary.usage.web_fetch_requests > 0 {
            parts.push(format!("fetch:{}", summary.usage.web_fetch_requests));
        }
        writeln!(out, "\x1b[2m   web {}\x1b[0m", parts.join(" "))?;
    }
    Ok(())
}

/// `usage` オブジェクトの tier / speed / geo / iterations を表示する。
fn write_usage_metadata(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    if let Some(usage) = v["usage"].as_object() {
        let mut parts = Vec::new();
        if let Some(service_tier) = usage.get("service_tier").and_then(|v| v.as_str())
            && !service_tier.is_empty()
        {
            parts.push(format!("tier:{service_tier}"));
        }
        if let Some(speed) = usage.get("speed").and_then(|v| v.as_str())
            && !speed.is_empty()
        {
            parts.push(format!("speed:{speed}"));
        }
        if let Some(inference_geo) = usage.get("inference_geo").and_then(|v| v.as_str())
            && !inference_geo.is_empty()
        {
            parts.push(format!("geo:{inference_geo}"));
        }
        if let Some(iterations) = usage.get("iterations").and_then(|v| v.as_array())
            && !iterations.is_empty()
        {
            parts.push(format!("iterations:{}", iterations.len()));
        }
        if !parts.is_empty() {
            writeln!(out, "\x1b[2m   usage {}\x1b[0m", parts.join(" "))?;
        }
    }
    Ok(())
}

/// モデル別使用量（modelUsage）の内訳を表示する。
fn write_model_usage(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    // モデル別使用量（modelUsage）の表示
    if let Some(model_usage) = v["modelUsage"].as_object() {
        for (model_id, usage) in model_usage {
            let cost = usage["costUSD"].as_f64().unwrap_or(0.0);
            let input_tokens = usage["inputTokens"].as_u64().unwrap_or(0);
            let output_tokens = usage["outputTokens"].as_u64().unwrap_or(0);
            let cache_read = usage["cacheReadInputTokens"].as_u64().unwrap_or(0);
            let cache_creation = usage["cacheCreationInputTokens"].as_u64().unwrap_or(0);
            let web_search = usage["webSearchRequests"].as_u64().unwrap_or(0);
            if cost > 0.0 || output_tokens > 0 {
                let mut extras = Vec::new();
                if cache_read > 0 {
                    extras.push(format!("cache_read:{}", format_number(cache_read)));
                }
                if cache_creation > 0 {
                    extras.push(format!("cache_write:{}", format_number(cache_creation)));
                }
                if web_search > 0 {
                    extras.push(format!("web:{}", web_search));
                }
                // モデル別の使用枠（contextWindow / maxOutputTokens）が含まれていれば併記する
                if let Some(window) = usage["contextWindow"].as_u64()
                    && window > 0
                {
                    extras.push(format!("ctx:{}", format_token_size(window)));
                }
                if let Some(max_output) = usage["maxOutputTokens"].as_u64()
                    && max_output > 0
                {
                    extras.push(format!("max_out:{}", format_token_size(max_output)));
                }
                let extra_str = if extras.is_empty() {
                    String::new()
                } else {
                    format!(" {}", extras.join(" "))
                };
                writeln!(
                    out,
                    "\x1b[2m   {} ${:.4} (in:{} out:{}{})\x1b[0m",
                    model_id,
                    cost,
                    format_number(input_tokens),
                    format_number(output_tokens),
                    extra_str,
                )?;
            }
        }
    }
    Ok(())
}

/// fast_mode / origin / 終了理由 / 権限拒否件数を表示する。
fn write_terminal_info(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    // fast_mode の表示（off 以外の場合）
    if let Some(fast_mode) = v["fast_mode_state"].as_str()
        && fast_mode != "off"
    {
        writeln!(out, "\x1b[2m   fast_mode {}\x1b[0m", fast_mode)?;
    }
    if let Some(origin_kind) = v["origin"]["kind"].as_str()
        && !origin_kind.is_empty()
    {
        writeln!(out, "\x1b[2m   origin {}\x1b[0m", origin_kind)?;
    }
    // 異常終了時の終了理由（completed 以外）を表示
    if let Some(reason) = v["terminal_reason"].as_str()
        && !reason.is_empty()
        && reason != "completed"
    {
        writeln!(out, "\x1b[33m   terminal {}\x1b[0m", reason)?;
    }
    // 権限拒否されたツール呼び出しの件数を表示
    if let Some(denials) = v["permission_denials"].as_array()
        && !denials.is_empty()
    {
        let tool_names: Vec<_> = denials
            .iter()
            .filter_map(|denial| denial["tool_name"].as_str())
            .filter(|name| !name.is_empty())
            .take(3)
            .collect();
        let detail = if tool_names.is_empty() {
            String::new()
        } else {
            format!(" ({})", tool_names.join(", "))
        };
        writeln!(
            out,
            "\x1b[33m   permission_denials {}{}\x1b[0m",
            denials.len(),
            detail
        )?;
    }
    Ok(())
}
