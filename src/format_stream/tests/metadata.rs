//! `tool_result_metadata` / `extract_tool_result_summary` とツール結果メタデータ表示のテスト。

use super::*;

#[test]
fn process_tool_result_error() {
    // is_error=true の tool_result
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t_err","name":"Bash","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"exit 1\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_err","type":"tool_result","is_error":true,"content":"Command failed"}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    // エラー時は ✓ ではなく ✗ を表示し、エラー内容のサマリーを併記する
    assert!(
        clean.contains("\u{2717} Bash"),
        "expected error mark in: {}",
        clean
    );
    assert!(
        clean.contains("Command failed"),
        "expected error summary in: {}",
        clean
    );
    assert!(
        !clean.contains("\u{2713}"),
        "should not have checkmark on error: {}",
        clean
    );
}

#[test]
fn extract_tool_result_summary_unwraps_tool_use_error_tag() {
    // <tool_use_error>...</tool_use_error> ラッパーは除去される
    let v = serde_json::json!("<tool_use_error>Skill disabled</tool_use_error>");
    assert_eq!(extract_tool_result_summary(&v), "Skill disabled");
}

#[test]
fn extract_tool_result_summary_unwraps_multiline_tool_use_error_tag() {
    // 実データでは tool_use_error が複数行の診断全体を包むことがある
    let v = serde_json::json!(
        "<tool_use_error>String to replace not found in file.\nString: old\n</tool_use_error>"
    );
    assert_eq!(
        extract_tool_result_summary(&v),
        "String to replace not found in file."
    );
}

#[test]
fn extract_tool_result_summary_takes_first_non_empty_line() {
    // 複数行のエラーは先頭の有意な行のみ
    let v = serde_json::json!("\n\nExit code 1\nFrom https://github.com/example\nMore details");
    assert_eq!(extract_tool_result_summary(&v), "Exit code 1");
}

#[test]
fn extract_tool_result_summary_handles_array_content() {
    // 配列形式の content にも対応
    let v = serde_json::json!([
        {"type": "text", "text": "Error occurred"},
        {"type": "text", "text": "additional info"}
    ]);
    let result = extract_tool_result_summary(&v);
    assert_eq!(result, "Error occurred");
}

#[test]
fn extract_tool_result_summary_truncates_long_text() {
    // 長文は省略される
    let long = "a".repeat(200);
    let v = serde_json::Value::String(long);
    let result = extract_tool_result_summary(&v);
    assert!(result.ends_with("..."), "expected truncation: {}", result);
    assert!(
        result.chars().count() <= 120,
        "result length should be <= 120: {}",
        result
    );
}

#[test]
fn extract_tool_result_summary_empty_input() {
    assert_eq!(
        extract_tool_result_summary(&serde_json::Value::String(String::new())),
        ""
    );
    assert_eq!(
        extract_tool_result_summary(&serde_json::Value::String("   \n   ".to_string())),
        ""
    );
    assert_eq!(extract_tool_result_summary(&serde_json::Value::Null), "");
}

#[test]
fn process_tool_result_shows_top_level_result_metadata() {
    // 実 jsonl の top-level tool_use_result には、content だけでは分からない補足情報が入る
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t_read","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"src/main.rs\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","tool_use_result":{"truncated":true,"appliedLimit":15,"staleReadFileStateHint":"This command modified 1 file you've previously read: src/main.rs. Call Read before editing."},"message":{"role":"user","content":[{"tool_use_id":"t_read","type":"tool_result","is_error":false,"content":"file content"}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("\u{2713} Read"),
        "expected success mark in: {}",
        clean
    );
    assert!(
        clean.contains("truncated"),
        "expected truncated metadata in: {}",
        clean
    );
    assert!(
        clean.contains("limit:15"),
        "expected applied limit metadata in: {}",
        clean
    );
    assert!(
        clean.contains("stale-read:"),
        "expected stale-read metadata in: {}",
        clean
    );
}

#[test]
fn tool_result_metadata_shows_actual_jsonl_result_counts() {
    // 実 jsonl で確認した Grep / ToolSearch の result メタデータを完了行に出す
    let value = serde_json::json!({
        "numFiles": 2,
        "numLines": 15,
        "matches": ["TaskCreate", "TaskUpdate"],
        "total_deferred_tools": 34,
        "commandName": "codex"
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("files:2"), "{metadata}");
    assert!(metadata.contains("lines:15"), "{metadata}");
    assert!(metadata.contains("matches:2"), "{metadata}");
    assert!(metadata.contains("deferred:34"), "{metadata}");
    assert!(metadata.contains("command:codex"), "{metadata}");
}

#[test]
fn tool_result_metadata_shows_git_commit_operation() {
    // Bash 経由の git commit は gitOperation.commit に sha/kind が入る。
    // 進捗上の重要なマイルストーンなので完了行に出す。
    let value = serde_json::json!({
        "gitOperation": {"commit": {"sha": "800f03f", "kind": "committed"}}
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("commit:800f03f committed"), "{metadata}");
}

#[test]
fn tool_result_metadata_shows_git_commit_without_kind() {
    // kind が無い場合は sha のみ表示する
    let value = serde_json::json!({
        "gitOperation": {"commit": {"sha": "abc1234"}}
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("commit:abc1234"), "{metadata}");
    assert!(!metadata.contains("commit:abc1234 "), "{metadata}");
}

#[test]
fn tool_result_metadata_shows_grep_count_matches() {
    // Grep の count モードは matches 配列ではなく numMatches 整数で件数を返す。
    // matches 配列(ToolSearch 用)の経路では拾えないため numMatches を別途表示する。
    let value = serde_json::json!({
        "mode": "count",
        "numMatches": 483,
        "numFiles": 1
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("matches:483"), "{metadata}");
    assert!(metadata.contains("mode:count"), "{metadata}");
    assert!(metadata.contains("files:1"), "{metadata}");
}

#[test]
fn tool_result_metadata_prefers_matches_array_over_num_matches() {
    // ToolSearch の matches 配列が優先され、件数は配列長になる
    let value = serde_json::json!({
        "matches": ["a", "b", "c"]
    });

    assert!(tool_result_metadata(&value).contains("matches:3"));
}

#[test]
fn tool_result_metadata_shows_actual_jsonl_agent_usage() {
    // Agent tool の完了結果には duration / token / tool count が入る
    let value = serde_json::json!({
        "totalDurationMs": 60730,
        "totalTokens": 90631,
        "totalToolUseCount": 3,
        "statusChange": {"from": "pending", "to": "completed"}
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("duration:60.7s"), "{metadata}");
    assert!(metadata.contains("tokens:90,631"), "{metadata}");
    assert!(metadata.contains("tools:3"), "{metadata}");
    assert!(metadata.contains("status:pending->completed"), "{metadata}");
}

#[test]
fn tool_result_metadata_shows_actual_jsonl_task_and_monitor_fields() {
    // 実 jsonl で確認した TaskList / TaskCreate / TaskOutput / Monitor 系の結果補足。
    let value = serde_json::json!({
        "durationMs": 176,
        "tasks": [{"id": "1"}, {"id": "2"}],
        "task": {"id": "3", "subject": "レビューと改善"},
        "retrieval_status": "success",
        "outputFile": "/tmp/agent-output.log",
        "canReadOutputFile": true,
        "timeoutMs": 240000,
        "persistent": true
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("duration:0.2s"), "{metadata}");
    assert!(metadata.contains("tasks:2"), "{metadata}");
    assert!(metadata.contains("task:3 レビューと改善"), "{metadata}");
    assert!(metadata.contains("retrieval:success"), "{metadata}");
    assert!(metadata.contains("output-file:readable"), "{metadata}");
    assert!(metadata.contains("timeout:240s"), "{metadata}");
    assert!(metadata.contains("persistent"), "{metadata}");
}

#[test]
fn tool_result_metadata_shows_persisted_output_size_and_schedule() {
    // Bash の永続化出力サイズと ScheduleWakeup の scheduledFor は実データに出る
    let value = serde_json::json!({
        "persistedOutputPath": "/tmp/tool-result.txt",
        "persistedOutputSize": 41119,
        "scheduledFor": 1779898800000_i64
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("persisted-output:40.2KB"), "{metadata}");
    assert!(metadata.contains("scheduled:"), "{metadata}");
}

#[test]
fn process_tool_result_error_flag() {
    // is_error: true の tool_result は異なるマーカーで表示
    let input = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"command failed"}]}}"#,
        ]
        .join("\n");
    let output = run_process(&input);
    let clean = strip_ansi(&output);
    // エラーマーカー "✗" が表示される
    assert!(clean.contains("\u{2717}"), "エラーマーカーが表示されるべき");
}

#[test]
fn tool_result_metadata_non_object_returns_empty() {
    // result が object でない場合は空文字（配列・文字列・null・数値）
    assert_eq!(tool_result_metadata(&serde_json::json!([1, 2, 3])), "");
    assert_eq!(tool_result_metadata(&serde_json::json!("text")), "");
    assert_eq!(tool_result_metadata(&serde_json::Value::Null), "");
    assert_eq!(tool_result_metadata(&serde_json::json!(42)), "");
}

#[test]
fn tool_result_metadata_empty_object_returns_empty() {
    // 既知フィールドが何も無ければ空文字
    assert_eq!(tool_result_metadata(&serde_json::json!({})), "");
}

#[test]
fn tool_result_metadata_interrupted_and_failed_flags() {
    // interrupted:true と success:false はどちらもフラグとして表示される
    let value = serde_json::json!({
        "interrupted": true,
        "success": false
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("interrupted"), "{metadata}");
    assert!(metadata.contains("failed"), "{metadata}");
}

#[test]
fn tool_result_metadata_success_true_is_not_shown() {
    // success:true は正常系なので "failed" を出さない
    let value = serde_json::json!({"success": true});
    assert!(!tool_result_metadata(&value).contains("failed"));
}

#[test]
fn tool_result_metadata_false_bool_flags_are_omitted() {
    // truncated:false / interrupted:false は表示しない
    let value = serde_json::json!({
        "truncated": false,
        "interrupted": false,
        "userModified": false,
        "assistantAutoBackgrounded": false,
        "wasClamped": false
    });
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_user_modified_flag() {
    let value = serde_json::json!({"userModified": true});
    assert!(tool_result_metadata(&value).contains("user-modified"));
}

#[test]
fn tool_result_metadata_auto_backgrounded_and_background_task_id() {
    // 自動バックグラウンド化フラグと backgroundTaskId の併記
    let value = serde_json::json!({
        "assistantAutoBackgrounded": true,
        "backgroundTaskId": "bg-task-123"
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("auto-backgrounded"), "{metadata}");
    assert!(metadata.contains("background:bg-task-123"), "{metadata}");
}

#[test]
fn tool_result_metadata_empty_background_task_id_is_omitted() {
    // backgroundTaskId が空文字なら表示しない
    let value = serde_json::json!({"backgroundTaskId": ""});
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_clamped_with_delay_seconds() {
    // wasClamped + clampedDelaySeconds → "clamped:<n>s"
    let value = serde_json::json!({
        "wasClamped": true,
        "clampedDelaySeconds": 30
    });
    assert!(tool_result_metadata(&value).contains("clamped:30s"));
}

#[test]
fn tool_result_metadata_clamped_without_delay_seconds() {
    // wasClamped のみ（delay 無し）なら "clamped" 単独
    let value = serde_json::json!({"wasClamped": true});
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("clamped"), "{metadata}");
    assert!(!metadata.contains("clamped:"), "{metadata}");
}

#[test]
fn tool_result_metadata_return_code_interpretation() {
    let value = serde_json::json!({
        "returnCodeInterpretation": "command exited with status 127 (command not found)"
    });
    assert!(tool_result_metadata(&value).contains("return:"));
}

#[test]
fn tool_result_metadata_empty_return_code_interpretation_is_omitted() {
    let value = serde_json::json!({"returnCodeInterpretation": ""});
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_output_size_without_persisted_path() {
    // persistedOutputPath が無く persistedOutputSize だけある場合は "output:<size>"
    let value = serde_json::json!({"persistedOutputSize": 2048});
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("output:2.0KB"), "{metadata}");
    assert!(!metadata.contains("persisted-output"), "{metadata}");
}

#[test]
fn tool_result_metadata_persisted_path_without_size() {
    // path はあるが size が無い場合は "persisted-output"（サイズ無し）
    let value = serde_json::json!({"persistedOutputPath": "/tmp/out.txt"});
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("persisted-output"), "{metadata}");
    assert!(!metadata.contains("persisted-output:"), "{metadata}");
}

#[test]
fn tool_result_metadata_empty_persisted_path_with_size_uses_output_branch() {
    // persistedOutputPath が空文字なら path 無し扱いで "output:" ブランチに落ちる
    let value = serde_json::json!({
        "persistedOutputPath": "",
        "persistedOutputSize": 512
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("output:512B"), "{metadata}");
    assert!(!metadata.contains("persisted-output"), "{metadata}");
}

#[test]
fn tool_result_metadata_empty_command_name_is_omitted() {
    // commandName が空文字なら表示しない
    let value = serde_json::json!({"commandName": ""});
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_status_change_missing_endpoint_is_omitted() {
    // statusChange の from か to が欠けていれば表示しない
    let value = serde_json::json!({"statusChange": {"from": "pending"}});
    assert!(!tool_result_metadata(&value).contains("status:"));
}

#[test]
fn tool_result_metadata_zero_counts_are_shown() {
    // numFiles:0 / numLines:0 も as_u64 が Some を返すため件数として表示される
    let value = serde_json::json!({"numFiles": 0, "numLines": 0});
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("files:0"), "{metadata}");
    assert!(metadata.contains("lines:0"), "{metadata}");
}

#[test]
fn tool_result_metadata_commit_without_sha_is_omitted() {
    // sha が無い gitOperation.commit は表示しない
    let value = serde_json::json!({"gitOperation": {"commit": {"kind": "committed"}}});
    assert!(!tool_result_metadata(&value).contains("commit"));
}

#[test]
fn tool_result_metadata_preserves_attr_order() {
    // 複数フィールドが ", " で連結され、実装の評価順（truncated → limit → files ...）を保つ
    let value = serde_json::json!({
        "truncated": true,
        "appliedLimit": 10,
        "numFiles": 3
    });
    assert_eq!(tool_result_metadata(&value), "truncated, limit:10, files:3");
}

#[test]
fn tool_result_metadata_webfetch_http_status_and_bytes() {
    // 実 jsonl の WebFetch 結果: HTTP ステータスコード + codeText と応答サイズを表示する
    let value = serde_json::json!({
        "bytes": 123088,
        "code": 200,
        "codeText": "OK",
        "durationMs": 5122
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("http:200 OK"), "{metadata}");
    assert!(metadata.contains("bytes:120.2KB"), "{metadata}");
    assert!(metadata.contains("duration:5.1s"), "{metadata}");
}

#[test]
fn tool_result_metadata_webfetch_http_without_code_text() {
    // codeText が無い場合は HTTP ステータスコードのみ表示する
    let value = serde_json::json!({"code": 404});
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("http:404"), "{metadata}");
    assert!(!metadata.contains("http:404 "), "{metadata}");
}

#[test]
fn tool_result_metadata_websearch_results_and_duration() {
    // 実 jsonl の WebSearch 結果: 検索結果件数と所要時間（秒の float）を表示する
    let value = serde_json::json!({
        "query": "example query",
        "results": [{"title": "a"}, {"title": "b"}, {"title": "c"}],
        "durationSeconds": 6.919656833999994,
        "searchCount": 1
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("results:3"), "{metadata}");
    assert!(metadata.contains("duration:6.9s"), "{metadata}");
    // searchCount が 1 の通常ケースはノイズを避けるため表示しない
    assert!(!metadata.contains("searches:"), "{metadata}");
}

#[test]
fn tool_result_metadata_websearch_multiple_searches_shown() {
    // searchCount が 2 以上の場合のみ検索回数を表示する
    let value = serde_json::json!({
        "results": [],
        "searchCount": 3
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("searches:3"), "{metadata}");
    assert!(metadata.contains("results:0"), "{metadata}");
}

#[test]
fn tool_result_metadata_read_partial_shows_line_ratio() {
    // Read の file.numLines < totalLines（部分読み取り）は "lines:N/M" で切り詰めを示す
    let value = serde_json::json!({
        "type": "text",
        "file": {"filePath": "/src/big.rs", "numLines": 50, "startLine": 1, "totalLines": 2000}
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("lines:50/2000"), "{metadata}");
}

#[test]
fn tool_result_metadata_read_full_omits_line_ratio() {
    // 全行読み取り（numLines == totalLines）は比率表示しない（ノイズ回避）
    let value = serde_json::json!({
        "type": "text",
        "file": {"filePath": "/src/small.rs", "numLines": 23, "startLine": 1, "totalLines": 23}
    });
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_read_missing_total_lines_omits_ratio() {
    // totalLines が欠けている場合は比率を出さない（部分読み取りの判定不能）
    let value = serde_json::json!({
        "file": {"numLines": 10}
    });
    assert_eq!(tool_result_metadata(&value), "");
}
