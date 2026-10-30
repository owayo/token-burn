//! ストリーミング処理中に集約する状態（セッション要約・トークン使用量）を保持する
//! モジュール。`StreamState` は 1 行の処理に必要な可変参照をまとめた束。

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::format_stream::blocks::ContentBlockState;

pub(crate) struct StreamState<'a> {
    pub(crate) blocks: &'a mut HashMap<usize, ContentBlockState>,
    pub(crate) tool_id_map: &'a mut HashMap<String, String>,
    pub(crate) streamed_message_ids: &'a mut HashSet<String>,
    pub(crate) summary: &'a mut StreamSummary,
}

#[derive(Default)]
pub(crate) struct StreamSummary {
    pub(crate) session_id: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) message_id: Option<String>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) usage: UsageSummary,
    /// 各 `result.usage` はその結果のターン分だけなので、比較用に累積する。
    pub(crate) cumulative_main_input_tokens: u64,
    pub(crate) cumulative_main_output_tokens: u64,
    /// この実行（このプロセス）のモデル別使用量。`model_usage_since_last_result` を持たない
    /// 旧形式では `None` のまま。
    pub(crate) run_models: Option<RunModelUsage>,
}

/// `result.model_usage_since_last_result` を足し合わせた、この実行のモデル別使用量。
///
/// `result.total_cost_usd` / `modelUsage` / `duration_api_ms` はセッションの累計で、
/// `--resume` で再開したセッションでは中断前のプロセスの消費まで含む（実データ: 中断前
/// $33.65・出力 448,091 → 再開後の 14 ターンで $40.26・459,323）。一方
/// `model_usage_since_last_result` は前の `result` 以降（最初の `result` ではプロセスの
/// 開始以降）の分で、サブエージェントの消費も含む。これを足せば、この実行の消費が求まる。
#[derive(Default)]
pub(crate) struct RunModelUsage {
    /// モデル ID ごとの合計。キー順は `modelUsage`（serde_json の `Map`）と同じ昇順。
    pub(crate) models: BTreeMap<String, ModelTotals>,
    /// モデル別の報告が欠けたターン数（`result.usage` は消費を報告しているのに
    /// `model_usage_since_last_result` が空か欠けていた `result` の `num_turns` の合計）。
    ///
    /// 実データでは、バックグラウンドのサブエージェントの完了通知で走る 2 件目以降の
    /// `result` が、`usage` に 14 ターン・出力 11,048 トークンを報告しながら
    /// `model_usage_since_last_result` を `{}` とし、`modelUsage` と `total_cost_usd` も
    /// 1 件目から変えなかった。これらのターンの消費はどの集計にも入っていない。
    pub(crate) unreported_turns: u64,
    /// モデル別の報告が欠けた `result` があったか（ターン数が 0 と報告された場合も含む）。
    pub(crate) incomplete: bool,
}

/// 1 モデル分の使用量（`modelUsage` / `model_usage_since_last_result` の 1 エントリ）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ModelTotals {
    pub(crate) cost_usd: f64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) thinking_tokens: u64,
    pub(crate) web_search_requests: u64,
    pub(crate) context_window: Option<u64>,
    pub(crate) max_output_tokens: Option<u64>,
}

impl ModelTotals {
    pub(crate) fn from_value(value: &serde_json::Value) -> Self {
        let mut totals = Self::default();
        totals.add(value);
        totals
    }

    /// 1 エントリ分を足す。壊れた巨大値でも panic させないよう飽和加算し、負値・NaN の
    /// コストは足さない。`contextWindow` / `maxOutputTokens` は枠の大きさなので足さずに
    /// 最後に見た値を持つ。
    pub(crate) fn add(&mut self, value: &serde_json::Value) {
        let field = |key: &str| value[key].as_u64().unwrap_or(0);
        if let Some(cost) = value["costUSD"]
            .as_f64()
            .filter(|c| c.is_finite() && *c >= 0.0)
        {
            self.cost_usd += cost;
        }
        self.input_tokens = self.input_tokens.saturating_add(field("inputTokens"));
        self.output_tokens = self.output_tokens.saturating_add(field("outputTokens"));
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(field("cacheReadInputTokens"));
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(field("cacheCreationInputTokens"));
        self.thinking_tokens = self.thinking_tokens.saturating_add(field("thinkingTokens"));
        self.web_search_requests = self
            .web_search_requests
            .saturating_add(field("webSearchRequests"));
        if let Some(window) = value["contextWindow"].as_u64() {
            self.context_window = Some(window);
        }
        if let Some(max_output) = value["maxOutputTokens"].as_u64() {
            self.max_output_tokens = Some(max_output);
        }
    }

    /// 入力側トークンの合計（キャッシュの読み書きを含む）。
    pub(crate) fn total_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    /// トークン数のどれかが `other` を上回るか。
    pub(crate) fn exceeds_tokens(&self, other: &Self) -> bool {
        self.input_tokens > other.input_tokens
            || self.output_tokens > other.output_tokens
            || self.cache_read_input_tokens > other.cache_read_input_tokens
            || self.cache_creation_input_tokens > other.cache_creation_input_tokens
    }
}

impl StreamSummary {
    pub(crate) fn update_from_system(&mut self, value: &serde_json::Value) {
        if self.session_id.is_none() {
            self.session_id = value["session_id"].as_str().map(ToOwned::to_owned);
        }
        if self.cwd.is_none() {
            self.cwd = value["cwd"].as_str().map(ToOwned::to_owned);
        }
    }

    pub(crate) fn update_from_message(&mut self, value: &serde_json::Value) {
        if let Some(model) = value["model"].as_str() {
            self.model = Some(model.to_string());
        }
        if let Some(id) = value["id"].as_str() {
            self.message_id = Some(id.to_string());
        }
        self.usage.merge_from_value(value.get("usage"));
    }

    pub(crate) fn update_from_message_delta(&mut self, event: &serde_json::Value) {
        if let Some(stop_reason) = event["delta"]["stop_reason"].as_str() {
            self.stop_reason = Some(stop_reason.to_string());
        }
        self.usage.merge_from_value(event.get("usage"));
    }

    pub(crate) fn update_from_result(
        &mut self,
        value: Option<&serde_json::Map<String, serde_json::Value>>,
    ) {
        let Some(value) = value else {
            return;
        };
        if let Some(model) = value.get("model").and_then(|v| v.as_str()) {
            self.model = Some(model.to_string());
        }
        if let Some(stop_reason) = value.get("stop_reason").and_then(|v| v.as_str()) {
            self.stop_reason = Some(stop_reason.to_string());
        }
        let mut result_usage = UsageSummary::default();
        result_usage.merge_from_value(value.get("usage"));
        self.cumulative_main_input_tokens = self
            .cumulative_main_input_tokens
            .saturating_add(result_usage.total_input_tokens());
        self.cumulative_main_output_tokens = self
            .cumulative_main_output_tokens
            .saturating_add(result_usage.output_tokens);
        self.usage.merge_from_value(value.get("usage"));
        self.update_run_models(value, &result_usage);
    }

    /// この実行のモデル別使用量へ `model_usage_since_last_result` を足す。
    fn update_run_models(
        &mut self,
        value: &serde_json::Map<String, serde_json::Value>,
        result_usage: &UsageSummary,
    ) {
        let reports_usage = result_usage.total_input_tokens() > 0 || result_usage.output_tokens > 0;
        let turns = value
            .get("num_turns")
            .and_then(|turns| turns.as_u64())
            .unwrap_or(0);
        let since = value
            .get("model_usage_since_last_result")
            .and_then(|since| since.as_object());
        let run = match since {
            Some(_) => self.run_models.get_or_insert_with(RunModelUsage::default),
            // 旧形式（フィールドが無い）は従来どおり `modelUsage` を表示に使う。新形式を
            // 見た後で欠けた `result` は、モデル別の報告が欠けたものとして数える。
            None => match self.run_models.as_mut() {
                Some(run) => run,
                None => return,
            },
        };
        let mut reported = false;
        for (model, usage) in since.into_iter().flatten() {
            if usage.is_object() {
                run.models.entry(model.clone()).or_default().add(usage);
                reported = true;
            }
        }
        if reports_usage && !reported {
            run.incomplete = true;
            run.unreported_turns = run.unreported_turns.saturating_add(turns);
        }
    }
}

#[derive(Default)]
pub(crate) struct UsageSummary {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    /// `output_tokens` の内訳のうち思考（extended thinking）に使われた分。
    /// 実ログでは出力トークンの 10〜52% を占めるため、これを表示しないと
    /// 「何にトークンを使ったのか」の最大の内訳が丸ごと失われる。
    pub(crate) thinking_tokens: u64,
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) cache_creation_5m_input_tokens: u64,
    pub(crate) cache_creation_1h_input_tokens: u64,
    pub(crate) web_search_requests: u64,
    pub(crate) web_fetch_requests: u64,
}

impl UsageSummary {
    /// `usage` ペイロードからフィールドを取り込む。
    /// Claude Code の stream-json は各 message_start / message_delta が
    /// その API 呼び出し単独の usage を返し、`result` イベントに最終累計が入る。
    /// そのため最後に `update_from_result` が呼ばれた時点で正しい合計値となる。
    /// 各フィールドは累積ではなく上書き代入することで `result` の値を最終値として優先する。
    pub(crate) fn merge_from_value(&mut self, value: Option<&serde_json::Value>) {
        let Some(value) = value else {
            return;
        };

        if let Some(v) = value["input_tokens"].as_u64() {
            self.input_tokens = v;
        }
        if let Some(v) = value["output_tokens"].as_u64() {
            self.output_tokens = v;
        }
        if let Some(v) = value["output_tokens_details"]["thinking_tokens"].as_u64() {
            self.thinking_tokens = v;
        }
        if let Some(v) = value["cache_read_input_tokens"].as_u64() {
            self.cache_read_input_tokens = v;
        }
        if let Some(v) = value["cache_creation_input_tokens"].as_u64() {
            self.cache_creation_input_tokens = v;
        }
        if let Some(v) = value["cache_creation"]["ephemeral_5m_input_tokens"].as_u64() {
            self.cache_creation_5m_input_tokens = v;
        }
        if let Some(v) = value["cache_creation"]["ephemeral_1h_input_tokens"].as_u64() {
            self.cache_creation_1h_input_tokens = v;
        }
        if let Some(v) = value["server_tool_use"]["web_search_requests"].as_u64() {
            self.web_search_requests = v;
        }
        if let Some(v) = value["server_tool_use"]["web_fetch_requests"].as_u64() {
            self.web_fetch_requests = v;
        }
    }

    /// 入力側トークンの合計。壊れた巨大値でも panic させないため飽和加算する。
    ///
    /// `format-stream` はパイプの中段に置かれ、`claude ... 2>&1 | format-stream | tee` で
    /// stderr が同じパイプへ合流するため、行が破損した jsonl を読む可能性がある。
    /// debug build ではオーバーフローが panic になり、パイプが閉じて `claude` 本体が
    /// SIGPIPE で落ちるため、表示の都合で数時間の実行を巻き添えにしてしまう。
    pub(crate) fn total_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    pub(crate) fn cache_write_5m_tokens(&self) -> u64 {
        if self.cache_creation_5m_input_tokens > 0 {
            return self.cache_creation_5m_input_tokens;
        }
        // 1hの内訳が存在する場合、5mは本当に0
        if self.cache_creation_1h_input_tokens > 0 {
            return 0;
        }
        // 内訳が存在しない場合は合計値をフォールバック
        self.cache_creation_input_tokens
    }

    pub(crate) fn has_cache_details(&self) -> bool {
        self.cache_read_input_tokens > 0
            || self.cache_creation_input_tokens > 0
            || self.cache_creation_5m_input_tokens > 0
            || self.cache_creation_1h_input_tokens > 0
    }
}
