//! `result` イベント受信時に表示する集計フッター（コスト・所要時間・トークン・
//! キャッシュ・モデル別使用量・終了情報）を生成するモジュール。
//! `handle_result` は各行の生成を担う `write_*` ヘルパーを順に呼ぶ薄い入口。

use anyhow::Result;
use std::collections::BTreeMap;
use std::io::Write;

use crate::format_stream::state::{ModelTotals, StreamSummary};
use crate::format_stream::util::{
    format_millis_as_seconds, format_number, format_token_size, normalize_model_name,
    truncate_inline,
};

pub(crate) fn handle_result(
    v: &serde_json::Value,
    summary: &StreamSummary,
    out: &mut impl Write,
) -> Result<()> {
    let usage = FooterUsage::new(v, summary);
    write_cost(v, &usage, out)?;
    write_duration(v, &usage, out)?;
    write_subagent_summary(v, out)?;
    write_token_summary(summary, out)?;
    write_session_total(&usage, summary, out)?;
    write_cache_summary(summary, out)?;
    write_model_stop(summary, out)?;
    write_web_summary(summary, out)?;
    write_usage_metadata(v, out)?;
    write_model_usage(&usage, out)?;
    write_terminal_info(v, out)?;
    Ok(())
}

/// フッターに出すモデル別使用量と、その集計の範囲。
///
/// `result.total_cost_usd` / `modelUsage` / `duration_api_ms` はセッションの累計で、
/// `--resume` で再開したセッションでは中断前のプロセスの消費まで含む。そのまま出すと、
/// 再開した実行（実データ: 14 ターン・$6.61、サブエージェント無し）のフッターが
/// `💰 $40.2602` / `api:74m 38s` になり、中断前の出力 448,091 トークンが
/// `📊 total ... (incl. subagents)` としてサブエージェントの消費に見えていた。
/// `model_usage_since_last_result` を持つ形式では、それを足した「この実行」の値を主に出し、
/// セッションの累計は補足に回す。持たない旧形式は従来どおり `modelUsage` を出す。
struct FooterUsage {
    /// モデル別の行と総計に使う値。新形式ならこの実行の分、旧形式なら `modelUsage`。
    models: BTreeMap<String, ModelTotals>,
    /// この実行のコストの合計（新形式のみ）。
    run_cost: Option<f64>,
    /// セッションの報告値（`modelUsage`）が、この実行の集計を上回るか。
    ///
    /// 再開したセッションでは中断前の消費の分だけ上回る。判定はトークン数で行う（コストは
    /// 浮動小数の足し算で末尾の桁がずれるため、同じ値どうしでも大小が付き得る）。
    session_exceeds_run: bool,
    /// モデル別の報告が欠けた `result` があったか、そのターン数（[`RunModelUsage`] 参照）。
    ///
    /// [`RunModelUsage`]: crate::format_stream::state::RunModelUsage
    incomplete: bool,
    unreported_turns: u64,
}

impl FooterUsage {
    fn new(v: &serde_json::Value, summary: &StreamSummary) -> Self {
        let session: BTreeMap<String, ModelTotals> = v["modelUsage"]
            .as_object()
            .map(|models| {
                models
                    .iter()
                    .map(|(model, usage)| (model.clone(), ModelTotals::from_value(usage)))
                    .collect()
            })
            .unwrap_or_default();
        let Some(run) = &summary.run_models else {
            return Self {
                models: session,
                run_cost: None,
                session_exceeds_run: false,
                incomplete: false,
                unreported_turns: 0,
            };
        };
        let none = ModelTotals::default();
        let session_exceeds_run = session
            .iter()
            .any(|(model, totals)| totals.exceeds_tokens(run.models.get(model).unwrap_or(&none)));
        let mut models = run.models.clone();
        // 枠の大きさ（contextWindow / maxOutputTokens）は消費量ではないので、モデル別の
        // 報告に無ければセッション側の値で補う。
        for (model, totals) in &mut models {
            if let Some(reported) = session.get(model) {
                totals.context_window = totals.context_window.or(reported.context_window);
                totals.max_output_tokens = totals.max_output_tokens.or(reported.max_output_tokens);
            }
        }
        Self {
            // 空のときの `Iterator::sum` は -0.0 を返し、`$-0.0000` と表示されるので 0.0 から足す。
            run_cost: Some(
                models
                    .values()
                    .fold(0.0, |total, totals| total + totals.cost_usd),
            ),
            models,
            session_exceeds_run,
            incomplete: run.incomplete,
            unreported_turns: run.unreported_turns,
        }
    }
}

/// サブエージェントの完了内訳を表示する。
///
/// top-level result が成功でも、配下のサブエージェントだけが全件失敗する実データがある。
/// この内訳を隠すとセッション全体が正常完了したように見えるため、失敗・強制終了・
/// 起動拒否が 1 件でもあれば警告色で表示する。
fn write_subagent_summary(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let Some(stats) = v["subagent_stats"].as_object() else {
        return Ok(());
    };

    let spawned = stats
        .get("spawned")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let completed = stats
        .get("completed")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let failed = stats
        .get("failed")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let killed = sum_counter_object(stats.get("killed"));
    let refused = sum_counter_object(stats.get("refused"));

    if spawned == 0 && completed == 0 && failed == 0 && killed == 0 && refused == 0 {
        return Ok(());
    }

    let mut parts = vec![
        format!("spawned:{}", format_number(spawned)),
        format!("completed:{}", format_number(completed)),
        format!("failed:{}", format_number(failed)),
    ];
    if killed > 0 {
        parts.push(format!("killed:{}", format_number(killed)));
    }
    if refused > 0 {
        parts.push(format!("refused:{}", format_number(refused)));
    }
    if let Some(background) = stats
        .get("started_in_background")
        .and_then(|value| value.as_u64())
        .filter(|count| *count > 0)
    {
        parts.push(format!("bg:{}", format_number(background)));
    }
    if let Some(nested) = stats
        .get("spawned_by_subagents")
        .and_then(|value| value.as_u64())
        .filter(|count| *count > 0)
    {
        parts.push(format!("nested:{}", format_number(nested)));
    }
    if let Some(depth) = stats
        .get("max_depth")
        .and_then(|value| value.as_u64())
        .filter(|depth| *depth > 0)
    {
        parts.push(format!("max-depth:{depth}"));
    }

    // どの種類のサブエージェントを何体使ったか。実データでは
    // `{"general-purpose":4,"Explore":5,"security-engineer":1,"codex":2}` のように
    // 混在し、`codex` が 2 体なのか `Explore` が 5 体なのかでコストの意味が全く違う。
    // 件数だけの `spawned:12` では読み取れない。
    if let Some(by_type) = stats.get("by_type").and_then(|value| value.as_object())
        && !by_type.is_empty()
    {
        let mut kinds: Vec<(&str, u64)> = by_type
            .iter()
            .filter_map(|(kind, count)| count.as_u64().map(|count| (kind.as_str(), count)))
            .filter(|(_, count)| *count > 0)
            .collect();
        // 多い順（同数は名前順）で安定させる。
        kinds.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        if !kinds.is_empty() {
            let detail = kinds
                .iter()
                .map(|(kind, count)| format!("{kind}:{count}"))
                .collect::<Vec<_>>()
                .join(" ");
            parts.push(format!("[{detail}]"));
        }
    }

    let detail = parts.join(" ");
    if failed > 0 || killed > 0 || refused > 0 {
        writeln!(out, "\x1b[33m\u{26a0}  subagents {detail}\x1b[0m")?;
    } else {
        writeln!(out, "\x1b[2m   subagents {detail}\x1b[0m")?;
    }
    Ok(())
}

/// killed / refused の理由別カウンタを飽和加算し、壊れた巨大値でも panic させない。
fn sum_counter_object(value: Option<&serde_json::Value>) -> u64 {
    value
        .and_then(|value| value.as_object())
        .map(|counts| {
            counts
                .values()
                .filter_map(|value| value.as_u64())
                .fold(0, u64::saturating_add)
        })
        .unwrap_or(0)
}

/// 総コスト（USD）を表示する。
///
/// セッションの累計がこの実行の集計を上回る（再開したセッション）ときは、この実行の
/// コストを主に出し、累計を `(session $X)` として添える。モデル別の報告が欠けた
/// `result` があれば、コストとモデル別の値が実際の消費より少ないことを次の行で伝える。
fn write_cost(v: &serde_json::Value, usage: &FooterUsage, out: &mut impl Write) -> Result<()> {
    let Some(session_cost) = v["total_cost_usd"].as_f64() else {
        return Ok(());
    };
    match usage.run_cost {
        Some(run_cost) if usage.session_exceeds_run => writeln!(
            out,
            "\n\x1b[33m\u{1f4b0} ${run_cost:.4} (session ${session_cost:.4})\x1b[0m"
        )?,
        _ => writeln!(out, "\n\x1b[33m\u{1f4b0} ${session_cost:.4}\x1b[0m")?,
    }
    if usage.incomplete {
        let turns = if usage.unreported_turns > 0 {
            format!("{} turns", format_number(usage.unreported_turns))
        } else {
            "some turns".to_string()
        };
        writeln!(
            out,
            "\x1b[2m   model usage not reported for {turns} (cost and model totals undercount)\x1b[0m"
        )?;
    }
    Ok(())
}

/// 所要時間と turns / api / ttft 属性を表示する。
///
/// `duration_ms` と `num_turns` はこの `result` の分、`duration_api_ms` はセッションの累計。
/// 再開したセッションでは API 時間が中断前の分を含み、壁時計より何十倍も長く見えるため、
/// セッションの累計がこの実行を上回るときは `session-api:` と書いて範囲を明示する
/// （この実行の API 時間は報告に無く、求められない）。
fn write_duration(v: &serde_json::Value, usage: &FooterUsage, out: &mut impl Write) -> Result<()> {
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
            let label = if usage.session_exceeds_run {
                "session-api"
            } else {
                "api"
            };
            attrs.push(format!("{label}:{}m {}s", api_secs / 60, api_secs % 60));
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
        // 思考トークンは出力トークンの内訳（実ログで 10〜52%）なので、出力へ加算せず
        // 括弧で内訳として添える。0 のときは省略してノイズを増やさない。
        let thinking = if summary.usage.thinking_tokens > 0 {
            format!(
                " (thinking:{})",
                format_number(summary.usage.thinking_tokens)
            )
        } else {
            String::new()
        };
        writeln!(
            out,
            "\x1b[33m\u{1f4ca} in:{} out:{}{}\x1b[0m",
            format_number(input),
            format_number(output),
            thinking
        )?;
    }
    Ok(())
}

/// サブエージェント込みのセッション総消費量を表示する。
///
/// `result.usage` はメインループの消費しか含まない。実ログで検証したところ、
/// `subagent_stats.spawned` が 0 のセッションでは `usage` と `modelUsage` の合計が
/// 完全に一致し、サブエージェントを起動したセッションでは `modelUsage` 側が
/// 最大 100 倍大きくなる（cache_read で 2,110,690 → 220,321,325 の実例）。
/// トークン消費量そのものを目的とするツールで、見出しの `in/out` が実消費の
/// 1/100 になり得るのは致命的なため、乖離があるときだけ総計を併記する。
///
/// 比べるモデル別の値は [`FooterUsage`] の範囲（新形式ならこの実行の分）。セッションの
/// 累計と比べると、再開したセッションでは中断前の消費がサブエージェントの分に見える。
fn write_session_total(
    usage: &FooterUsage,
    summary: &StreamSummary,
    out: &mut impl Write,
) -> Result<()> {
    if usage.models.is_empty() {
        return Ok(());
    }

    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_thinking = 0u64;
    for totals in usage.models.values() {
        total_input = total_input.saturating_add(totals.total_input_tokens());
        total_output = total_output.saturating_add(totals.output_tokens);
        total_thinking = total_thinking.saturating_add(totals.thinking_tokens);
    }

    // モデル別の値は複数 result を通じた累計、result.usage は今回のターン分。
    // 今回分だけと比べると、サブエージェントが 0 件でも 2 件目以降を
    // 「incl. subagents」と誤表示するため、メインループ側も累積して比べる。
    if total_output <= summary.cumulative_main_output_tokens
        && total_input <= summary.cumulative_main_input_tokens
    {
        return Ok(());
    }

    // `modelUsage[].thinkingTokens` はサブエージェント込みの思考トークンで、実ログでは
    // 総出力の 66%（1,219,128 / 1,843,731）を占める。`usage.output_tokens_details`
    // 側はメインループ分（89,291）しか含まないため、これを落とすとサブエージェントに
    // 消えたトークンの最大の内訳が丸ごと見えなくなる。
    let thinking = if total_thinking > 0 {
        format!("thinking:{}, ", format_number(total_thinking))
    } else {
        String::new()
    };
    writeln!(
        out,
        "\x1b[33m\u{1f4ca} total in:{} out:{} ({}incl. subagents)\x1b[0m",
        format_number(total_input),
        format_number(total_output),
        thinking
    )?;
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
///
/// モデル名は `normalize_model_name` を通す。`summary.model` は `message_start.model`
/// と `result.model` から来るが、Claude Code は同じセッションのモデル ID を
/// `init.model` や `modelUsage` のキーへ `claude-opus-5[1m]` の形（ESC を欠いた
/// 壊れた SGR 断片付き）で載せる。セッション情報行とモデル別内訳では既に除去して
/// いるので、ここだけ素通しすると同じセッションの中で表記が食い違う。
fn write_model_stop(summary: &StreamSummary, out: &mut impl Write) -> Result<()> {
    if let Some(model) = summary
        .model
        .as_deref()
        .map(normalize_model_name)
        .filter(|model| !model.is_empty())
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

/// モデル別使用量の内訳を表示する（範囲は [`FooterUsage`]。新形式ならこの実行の分）。
fn write_model_usage(usage: &FooterUsage, out: &mut impl Write) -> Result<()> {
    for (model_id, totals) in &usage.models {
        if totals.cost_usd <= 0.0 && totals.output_tokens == 0 {
            continue;
        }
        let mut extras = Vec::new();
        if totals.cache_read_input_tokens > 0 {
            extras.push(format!(
                "cache_read:{}",
                format_number(totals.cache_read_input_tokens)
            ));
        }
        if totals.cache_creation_input_tokens > 0 {
            extras.push(format!(
                "cache_write:{}",
                format_number(totals.cache_creation_input_tokens)
            ));
        }
        if totals.web_search_requests > 0 {
            extras.push(format!("web:{}", totals.web_search_requests));
        }
        // モデル別の使用枠（contextWindow / maxOutputTokens）が含まれていれば併記する
        if let Some(window) = totals.context_window.filter(|window| *window > 0) {
            extras.push(format!("ctx:{}", format_token_size(window)));
        }
        if let Some(max_output) = totals.max_output_tokens.filter(|max| *max > 0) {
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
            normalize_model_name(model_id),
            totals.cost_usd,
            format_number(totals.input_tokens),
            format_number(totals.output_tokens),
            extra_str,
        )?;
    }
    Ok(())
}

/// `is_error:true` のセッションで、失敗の事実と原因（HTTP ステータス・本文）を表示する。
///
/// 実データでは 47 ドル・30 分を消費したセッションが
/// `is_error:true` / `api_error_status:429` /
/// `result:"You've hit your individual spend limit …"` で終わっていたのに、フッターへ
/// 出ていたのは `terminal api_error` の 1 行だけだった。`subtype` は `success` のまま
/// なので、その 1 行を読み落とすと**正常完了したように見える**。
///
/// `rate_limit_event` 由来の `🚫 Rate limited` 行は「five_hour が rejected」としか
/// 言わず、真因（個人の支出上限）は伝えない。両者は別情報なので重複にはならない。
/// `api_error_status` は `classify-result` が 408/429/5xx で再試行を判断する値そのもの
/// なので、ログと分類結果を突き合わせられるようになる。
///
/// 成功時の `result` は最終テキストとして既にストリーム済みなので出さない。
fn write_error_result(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    if v["is_error"].as_bool() != Some(true) {
        return Ok(());
    }
    let status = v["api_error_status"]
        .as_u64()
        .map(|status| format!(" (HTTP {status})"))
        .unwrap_or_default();
    // 起動時の失敗は result 本文を持たず errors 配列だけを返す。分類と同じ抽出規則を
    // 使い、セッション不存在などの原因が整形済みログから消えないようにする。
    let message = crate::classify::result_message(v)
        .map(|message| truncate_inline(&message, 160))
        .filter(|message| !message.is_empty());
    match message {
        Some(message) => writeln!(out, "\x1b[31m   error{}: {}\x1b[0m", status, message)?,
        None => writeln!(out, "\x1b[31m   error{}\x1b[0m", status)?,
    }
    Ok(())
}

/// fast_mode / 無効化理由 / origin / 終了理由 / 権限拒否件数を表示する。
fn write_terminal_info(v: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    // fast_mode の表示（off 以外の場合）
    if let Some(fast_mode) = v["fast_mode_state"].as_str()
        && fast_mode != "off"
    {
        writeln!(out, "\x1b[2m   fast_mode {}\x1b[0m", fast_mode)?;
    }
    if let Some(reason) = v["fast_mode_disabled_reason"].as_str()
        && !reason.is_empty()
    {
        writeln!(out, "\x1b[2m   fast_mode disabled:{}\x1b[0m", reason)?;
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
    write_error_result(v, out)?;
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
