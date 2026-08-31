//! ストリーミング処理中に集約する状態（セッション要約・トークン使用量）を保持する
//! モジュール。`StreamState` は 1 行の処理に必要な可変参照をまとめた束。

use std::collections::HashMap;

use crate::format_stream::blocks::ContentBlockState;

pub(crate) struct StreamState<'a> {
    pub(crate) blocks: &'a mut HashMap<usize, ContentBlockState>,
    pub(crate) tool_id_map: &'a mut HashMap<String, String>,
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
        self.usage.merge_from_value(value.get("usage"));
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
