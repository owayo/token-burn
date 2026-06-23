//! `result` イベントのフッター表示（コスト・トークン・モデル別使用量等）のテスト。

use super::*;

#[test]
fn process_result_shows_terminal_reason_when_not_completed() {
    // terminal_reason が "completed" 以外の場合だけ表示する
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.05,"duration_ms":5000,"usage":{"input_tokens":100,"output_tokens":50},"terminal_reason":"interrupted","permission_denials":[]}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("terminal interrupted"), "got: {clean}");
}

#[test]
fn process_result_hides_terminal_reason_when_completed() {
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.05,"duration_ms":5000,"usage":{"input_tokens":100,"output_tokens":50},"terminal_reason":"completed","permission_denials":[]}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(!clean.contains("terminal"), "got: {clean}");
}

#[test]
fn process_result_shows_permission_denials_count() {
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.05,"duration_ms":5000,"usage":{"input_tokens":100,"output_tokens":50},"terminal_reason":"completed","permission_denials":[{"tool_name":"Bash"},{"tool_name":"Edit"}]}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("permission_denials 2"), "got: {clean}");
    assert!(
        clean.contains("permission_denials 2 (Bash, Edit)"),
        "got: {clean}"
    );
}

#[test]
fn process_result_hides_permission_denials_when_empty() {
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.05,"duration_ms":5000,"usage":{"input_tokens":100,"output_tokens":50},"terminal_reason":"completed","permission_denials":[]}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(!clean.contains("permission_denials"), "got: {clean}");
}

#[test]
fn format_number_with_commas() {
    assert_eq!(format_number(1234567), "1,234,567");
    assert_eq!(format_number(999), "999");
    assert_eq!(format_number(1000), "1,000");
}

#[test]
fn process_result_without_cache() {
    let input = r#"{"type":"result","total_cost_usd":0.05,"duration_ms":1234,"usage":{"input_tokens":100,"output_tokens":50}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("$0.0500"));
    assert!(clean.contains("in:100 out:50"));
}

#[test]
fn process_result_with_full_stats() {
    // 実際の claude 実行結果と同じく num_turns と cache_creation_input_tokens を含む
    let input = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":41712,"num_turns":9,"total_cost_usd":0.5565,"usage":{"input_tokens":14,"cache_creation_input_tokens":54926,"cache_read_input_tokens":372099,"output_tokens":987}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("$0.5565"), "expected cost in: {}", clean);
    assert!(clean.contains("0m 41s"), "expected duration in: {}", clean);
    assert!(clean.contains("(9 turns)"), "expected turns in: {}", clean);
    // 入力トークン合計: 14 + 54926 + 372099 = 427039
    assert!(
        clean.contains("in:427,039"),
        "expected input tokens with cache creation in: {}",
        clean
    );
    assert!(
        clean.contains("out:987"),
        "expected output tokens in: {}",
        clean
    );
    assert!(
        clean.contains("cache read:372,099 write5m:54,926"),
        "expected cache breakdown in: {}",
        clean
    );
}

#[test]
fn process_result_with_model_usage() {
    // modelUsage のモデル別内訳があっても崩れないことを確認
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.234,"duration_ms":120000,"num_turns":15,"usage":{"input_tokens":500,"cache_read_input_tokens":50000,"output_tokens":2000},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":200,"outputTokens":1500,"cacheReadInputTokens":40000,"cost":0.234},"claude-opus-4-6":{"inputTokens":300,"outputTokens":500,"cacheReadInputTokens":10000,"cost":1.0}},"stop_reason":null}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("$1.2340"), "expected cost in: {}", clean);
    assert!(clean.contains("2m 0s"), "expected duration in: {}", clean);
    assert!(clean.contains("(15 turns)"), "expected turns in: {}", clean);
    // 入力トークン合計: 500 + 50000 = 50500
    assert!(
        clean.contains("in:50,500"),
        "expected input tokens in: {}",
        clean
    );
    assert!(
        clean.contains("out:2,000"),
        "expected output tokens in: {}",
        clean
    );
}

#[test]
fn process_result_shows_model_and_web_search_usage() {
    let input = [
            r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"s1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","id":"msg_1","usage":{"input_tokens":12,"cache_creation_input_tokens":44,"cache_creation":{"ephemeral_5m_input_tokens":30,"ephemeral_1h_input_tokens":14}}}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":12,"output_tokens":7,"cache_read_input_tokens":20,"server_tool_use":{"web_search_requests":2}}}}"#,
            r#"{"type":"result","total_cost_usd":0.0101,"duration_ms":1000}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(clean.contains("model claude-opus-4-6"), "got: {}", clean);
    assert!(
        clean.contains("cache read:20 write5m:30 write1h:14"),
        "got: {}",
        clean
    );
    assert!(clean.contains("web search:2"), "got: {}", clean);
}

#[test]
fn process_result_with_stop_reason_null() {
    // team session では stop_reason が null でも処理できる
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.01,"duration_ms":3000,"usage":{"input_tokens":10,"output_tokens":5},"stop_reason":null}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("$0.0100"));
    assert!(clean.contains("0m 3s"));
}

// --- format_diff_lines の単体テスト ---

#[test]
fn process_result_duration_only() {
    // コストなし、duration_ms のみ
    let input =
        r#"{"type":"result","duration_ms":65000,"usage":{"input_tokens":0,"output_tokens":0}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("1m 5s"), "expected duration in: {}", clean);
    // output=0 の場合はトークン行を出力しない
    assert!(
        !clean.contains("in:"),
        "output=0 ではトークン行を出力しない: {}",
        clean
    );
}

#[test]
fn process_result_no_duration() {
    // duration_ms がない場合
    let input =
        r#"{"type":"result","total_cost_usd":0.01,"usage":{"input_tokens":10,"output_tokens":5}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("$0.0100"));
    assert!(!clean.contains("m "), "duration なしでは時間を表示しない");
}

#[test]
fn format_number_zero() {
    assert_eq!(format_number(0), "0");
}

#[test]
fn format_number_large() {
    assert_eq!(format_number(1_000_000), "1,000,000");
}

#[test]
fn format_number_single_digit() {
    assert_eq!(format_number(5), "5");
}

#[test]
fn format_number_three_digits() {
    assert_eq!(format_number(999), "999");
}

#[test]
fn format_number_four_digits() {
    assert_eq!(format_number(1000), "1,000");
}

#[test]
fn process_result_with_web_fetch_requests() {
    let input = [
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5,"server_tool_use":{"web_search_requests":1,"web_fetch_requests":3}}}}"#,
            r#"{"type":"result","total_cost_usd":0.01,"duration_ms":1000}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("web search:1 fetch:3"),
        "expected web search and fetch in: {}",
        clean
    );
}

#[test]
fn process_result_with_model_usage_breakdown() {
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.5,"duration_ms":60000,"modelUsage":{"claude-opus-4-6[1m]":{"inputTokens":50000,"outputTokens":10000,"costUSD":1.2},"claude-haiku-4-5-20251001":{"inputTokens":5000,"outputTokens":2000,"costUSD":0.3}}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("claude-opus-4-6[1m]"),
        "モデル名が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("$1.2000"),
        "モデル別コストが表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("claude-haiku"),
        "Haikuモデルも表示されるべき: {}",
        clean
    );
}

#[test]
fn process_result_with_model_usage_cache_and_web() {
    // modelUsage に cacheCreationInputTokens と webSearchRequests が含まれる場合
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":2.0,"duration_ms":120000,"modelUsage":{"claude-opus-4-6[1m]":{"inputTokens":80000,"outputTokens":15000,"costUSD":2.0,"cacheCreationInputTokens":50000,"webSearchRequests":3}}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("cache_write:50,000"),
        "キャッシュ書き込みトークンが表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("web:3"),
        "Web検索回数が表示されるべき: {}",
        clean
    );
}

#[test]
fn process_result_with_fast_mode_on() {
    // fast_mode_state が "on" の場合は表示される
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"duration_ms":30000,"fast_mode_state":"on"}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("fast_mode on"),
        "fast_mode が表示されるべき: {}",
        clean
    );
}

#[test]
fn process_result_with_usage_metadata() {
    let input = r#"{"type":"result","subtype":"success","duration_ms":1000,"usage":{"input_tokens":10,"output_tokens":5,"service_tier":"standard","speed":"standard","inference_geo":"us","iterations":[{"type":"message"}]}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("usage tier:standard speed:standard geo:us iterations:1"),
        "got: {clean}"
    );
}

#[test]
fn process_result_with_origin_kind() {
    let input = r#"{"type":"result","subtype":"success","duration_ms":1000,"origin":{"kind":"task-notification"}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("origin task-notification"), "got: {clean}");
}

#[test]
fn process_result_with_fast_mode_off() {
    // fast_mode_state が "off" の場合は表示されない
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"duration_ms":30000,"fast_mode_state":"off"}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.contains("fast_mode"),
        "fast_mode off は表示されるべきでない: {}",
        clean
    );
}

#[test]
fn process_result_without_fast_mode() {
    // fast_mode_state フィールドがない場合も表示されない
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"duration_ms":30000}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.contains("fast_mode"),
        "fast_mode フィールドがない場合は表示されるべきでない: {}",
        clean
    );
}

#[test]
fn process_result_model_usage_without_extras() {
    // modelUsage に cacheCreationInputTokens や webSearchRequests がない場合
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.0,"duration_ms":60000,"modelUsage":{"claude-opus-4-6":{"inputTokens":30000,"outputTokens":5000,"costUSD":1.0}}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("claude-opus-4-6"),
        "モデル名が表示されるべき: {}",
        clean
    );
    assert!(
        !clean.contains("cache_write"),
        "キャッシュ情報がない場合は表示されるべきでない: {}",
        clean
    );
    assert!(
        !clean.contains("web:"),
        "Web検索情報がない場合は表示されるべきでない: {}",
        clean
    );
}

#[test]
fn process_result_with_model_usage_cache_read() {
    // modelUsage に cacheReadInputTokens が含まれる場合の表示
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":3.0,"duration_ms":180000,"modelUsage":{"claude-opus-4-6[1m]":{"inputTokens":100,"outputTokens":20000,"costUSD":3.0,"cacheReadInputTokens":5000000,"cacheCreationInputTokens":80000}}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("cache_read:5,000,000"),
        "cacheReadInputTokens が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("cache_write:80,000"),
        "cacheCreationInputTokens も表示されるべき: {}",
        clean
    );
}

#[test]
fn process_result_with_duration_api_ms() {
    // duration_api_ms が含まれる場合 api:Xm Ys が表示される
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.0,"duration_ms":600000,"duration_api_ms":900000,"num_turns":50}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("api:15m 0s"),
        "duration_api_ms が表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("10m 0s"),
        "duration_ms も表示されるべき: {}",
        clean
    );
}

#[test]
fn process_result_with_ttft_ms() {
    let input = r#"{"type":"result","subtype":"success","duration_ms":600000,"duration_api_ms":900000,"ttft_ms":14837,"num_turns":50}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("ttft:14.8s"), "got: {clean}");
    assert!(clean.contains("api:15m 0s"), "got: {clean}");
}

#[test]
fn process_result_with_stream_latency_fields() {
    // 実 jsonl で確認した ttft_stream_ms（純粋なストリーム遅延）と
    // time_to_request_ms（リクエスト送信までの ms）を表示する。
    let input = r#"{"type":"result","subtype":"success","duration_ms":600000,"ttft_ms":19189,"ttft_stream_ms":3043,"time_to_request_ms":182,"num_turns":50}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(clean.contains("ttft:19.2s"), "got: {clean}");
    assert!(clean.contains("stream:3.0s"), "got: {clean}");
    assert!(clean.contains("req:182ms"), "got: {clean}");
}

#[test]
fn process_result_without_duration_api_ms() {
    // duration_api_ms がない場合は api: が表示されない
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":0.5,"duration_ms":120000,"num_turns":10}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.contains("api:"),
        "duration_api_ms がない場合は api: は表示されるべきでない: {}",
        clean
    );
}

#[test]
fn process_result_only_1h_cache_no_write5m() {
    // 1hキャッシュのみの場合、write5m が表示されずに write1h のみ表示される
    let lines = [
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","id":"msg_01","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":5000,"cache_creation_input_tokens":2000,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2000}}}}}"#,
        r#"{"type":"result","subtype":"success","total_cost_usd":0.1,"duration_ms":5000}"#,
    ];
    let input = lines.join("\n");
    let output = run_process(&input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.contains("write5m:"),
        "1hのみの場合 write5m は表示されるべきでない: {}",
        clean
    );
    assert!(
        clean.contains("write1h:2,000"),
        "write1h が表示されるべき: {}",
        clean
    );
}

#[test]
fn process_result_with_context_window_and_max_output() {
    // modelUsage に contextWindow / maxOutputTokens が含まれる場合に併記される
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.5,"duration_ms":60000,"modelUsage":{"claude-opus-4-7[1m]":{"inputTokens":100,"outputTokens":5000,"costUSD":1.5,"contextWindow":1000000,"maxOutputTokens":64000}}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        clean.contains("ctx:1M"),
        "contextWindow が単位付きで表示されるべき: {}",
        clean
    );
    assert!(
        clean.contains("max_out:64K"),
        "maxOutputTokens が単位付きで表示されるべき: {}",
        clean
    );
}

#[test]
fn process_result_without_context_window_or_max_output() {
    // contextWindow / maxOutputTokens がない場合は表示されない
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.0,"duration_ms":60000,"modelUsage":{"claude-opus-4-6":{"inputTokens":30000,"outputTokens":5000,"costUSD":1.0}}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.contains("ctx:"),
        "contextWindow がない場合は表示されるべきでない: {}",
        clean
    );
    assert!(
        !clean.contains("max_out:"),
        "maxOutputTokens がない場合は表示されるべきでない: {}",
        clean
    );
}

#[test]
fn process_result_context_window_zero_is_hidden() {
    // contextWindow=0 は表示されない (実データ非互換だが防御的に確認)
    let input = r#"{"type":"result","subtype":"success","total_cost_usd":1.0,"duration_ms":60000,"modelUsage":{"claude-opus-4-6":{"inputTokens":10,"outputTokens":5000,"costUSD":1.0,"contextWindow":0,"maxOutputTokens":0}}}"#;
    let output = run_process(input);
    let clean = strip_ansi(&output);
    assert!(
        !clean.contains("ctx:"),
        "contextWindow=0 は表示されるべきでない: {}",
        clean
    );
    assert!(
        !clean.contains("max_out:"),
        "maxOutputTokens=0 は表示されるべきでない: {}",
        clean
    );
}
