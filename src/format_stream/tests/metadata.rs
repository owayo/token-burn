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
fn process_tool_result_shows_structured_content_metadata() {
    // mcp__codex__codex の実 jsonl と同じく、top-level tool_use_result の
    // structuredContent.content を完了行の補足に出す。
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t_codex","name":"mcp__codex__codex","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"prompt\":\"レビューして\",\"cwd\":\"/repo\",\"sandbox\":\"read-only\",\"approval-policy\":\"never\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","tool_use_result":{"structuredContent":{"threadId":"t","content":"**指摘**\n重要な指摘"}},"message":{"role":"user","content":[{"tool_use_id":"t_codex","type":"tool_result","is_error":false,"content":"{}"}]}}"#,
        ]
        .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("structured:**指摘** 重要な指摘"),
        "structured content metadata should be shown: {clean}"
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
fn tool_result_metadata_shows_file_unchanged_type() {
    // 実ログの Read 結果には type:"file_unchanged" が現れる。前回読み取りから内容が
    // 変わらず本文が返らなかったケースで、表示しないと通常の Read 成功と区別できず、
    // ポーリング中の Read が実は何も取得していないという判断材料を失う。
    let value = serde_json::json!({
        "type": "file_unchanged",
        "filePath": "/workspace/token-burn/src/main.rs"
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("file-unchanged"), "{metadata}");
}

#[test]
fn tool_result_metadata_omits_unchanged_marker_for_other_types() {
    // "text"（通常の読み取り）/"update"（書き込み）では属性を出さない。
    // ここが緩むと全ての Read 結果に file-unchanged が付いて意味を失う。
    for kind in ["text", "update"] {
        let value = serde_json::json!({"type": kind, "filePath": "/src/main.rs"});
        let metadata = tool_result_metadata(&value);
        assert!(
            !metadata.contains("file-unchanged"),
            "type={kind} で file-unchanged が出てはいけない: {metadata}"
        );
    }
}

#[test]
fn tool_result_metadata_omits_unchanged_marker_when_type_absent() {
    // type フィールドを持たない結果でも属性は出ない。
    let value = serde_json::json!({"filePath": "/src/main.rs"});
    assert!(!tool_result_metadata(&value).contains("file-unchanged"));
}

#[test]
fn process_tool_result_shows_file_unchanged_marker() {
    // process パイプライン経由でも完了行の [...] に補足されること。
    let input = [
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t_unchanged","name":"Read","input":{}}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"/src/main.rs\"}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"user","tool_use_result":{"type":"file_unchanged","filePath":"/src/main.rs"},"message":{"role":"user","content":[{"tool_use_id":"t_unchanged","type":"tool_result","is_error":false,"content":"<file unchanged>"}]}}"#,
        ]
        .join("\n");

    let clean = strip_ansi(&run_process(&input));

    assert!(clean.contains("\u{2713} Read"), "{clean}");
    assert!(clean.contains("file-unchanged"), "{clean}");
}

#[test]
fn tool_result_metadata_shows_edit_file_and_patch_summary() {
    // 実 jsonl の Edit 結果は top-level に filePath / structuredPatch / originalFile を持つ。
    // originalFile は巨大なので出さず、ファイル名と patch 規模だけを短く表示する。
    let value = serde_json::json!({
        "filePath": "/workspace/gui-git-editor/src/components/rebase/RebaseEditor.tsx",
        "originalFile": "x".repeat(10_000),
        "structuredPatch": [
            {
                "oldStart": 81,
                "oldLines": 6,
                "newStart": 81,
                "newLines": 13,
                "lines": [
                    "   const handleKeyDown = useCallback(",
                    "+      if (document.querySelector(\"[aria-modal='true']\")) {",
                    "+        return;",
                    "+      }",
                    "-      if (event.defaultPrevented) {",
                    "       // 入力中はショートカットを無効化する"
                ]
            }
        ],
        "replaceAll": true
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("file:"), "{metadata}");
    assert!(metadata.contains("RebaseEditor.tsx"), "{metadata}");
    assert!(metadata.contains("patch:1 hunk +3/-1"), "{metadata}");
    assert!(metadata.contains("replace_all"), "{metadata}");
    assert!(!metadata.contains("originalFile"), "{metadata}");
    assert!(!metadata.contains(&"x".repeat(120)), "{metadata}");
}

#[test]
fn tool_result_metadata_counts_content_resembling_diff_headers() {
    let value = serde_json::json!({
        "structuredPatch": [
            {
                "lines": [
                    "+++追加内容",
                    "---削除内容",
                    " 変更なし"
                ]
            }
        ]
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("patch:1 hunk +1/-1"), "{metadata}");
}

#[test]
fn tool_result_metadata_shows_stdout_and_stderr_summary() {
    // Bash の実 jsonl では tool_result.content が完了文だけになり、
    // 実際のコマンド出力は top-level の stdout/stderr に入る。
    let value = serde_json::json!({
        "stdout": "main\nfeature/docs",
        "stderr": "warning: slow command\nsecond line"
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("stdout:main feature/docs"), "{metadata}");
    assert!(
        metadata.contains("stderr:warning: slow command second line"),
        "{metadata}"
    );
}

#[test]
fn process_tool_result_shows_stdout_metadata_from_top_level_result() {
    let input = [
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t_branch","name":"Bash"}]}}"#,
        r#"{"type":"user","tool_use_result":{"stdout":"main\nrelease","stderr":""},"message":{"role":"user","content":[{"tool_use_id":"t_branch","type":"tool_result","is_error":false,"content":"main"}]}}"#,
    ]
    .join("\n");

    let output = run_process(&input);
    let clean = strip_ansi(&output);

    assert!(
        clean.contains("Bash [stdout:main release]"),
        "stdout metadata should be shown: {clean}"
    );
}

#[test]
fn tool_result_metadata_shows_structured_content_summary() {
    // mcp__codex__codex の実 jsonl では structuredContent.content に回答本文が入る。
    let value = serde_json::json!({
        "structuredContent": {
            "threadId": "019ed11e-c8f2",
            "content": "**指摘**\n[src/lib/services/team.ts:25] の条件不足"
        }
    });

    let metadata = tool_result_metadata(&value);

    assert!(
        metadata.contains("structured:**指摘** [src/lib/services/team.ts:25] の条件不足"),
        "{metadata}"
    );
}

#[test]
fn tool_result_metadata_shows_error_message_and_task_identity() {
    // 実 jsonl の TaskStop / TaskUpdate 系では snake_case の task 情報と error/message が返る。
    let value = serde_json::json!({
        "success": false,
        "error": "Task not found",
        "message": "Successfully stopped task: byc5hl3kr",
        "task_id": "byc5hl3kr",
        "task_type": "local_bash"
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("failed"), "{metadata}");
    assert!(metadata.contains("error:Task not found"), "{metadata}");
    assert!(
        metadata.contains("message:Successfully stopped task: byc5hl3kr"),
        "{metadata}"
    );
    assert!(metadata.contains("task:byc5hl3kr"), "{metadata}");
    assert!(metadata.contains("task-type:local_bash"), "{metadata}");
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
fn tool_result_metadata_shows_agent_type_and_tool_stats() {
    // 実 jsonl で確認した Agent 結果の agentType / resolvedModel / toolStats(編集行数)。
    let value = serde_json::json!({
        "agentType": "general-purpose",
        "resolvedModel": "claude-opus-4-8",
        "totalToolUseCount": 9,
        "toolStats": {
            "readCount": 6, "searchCount": 0, "bashCount": 2, "editFileCount": 1,
            "linesAdded": 120, "linesRemoved": 45, "otherToolCount": 0
        }
    });

    let metadata = tool_result_metadata(&value);

    assert!(metadata.contains("agent:general-purpose"), "{metadata}");
    assert!(metadata.contains("model:claude-opus-4-8"), "{metadata}");
    assert!(metadata.contains("tools:9"), "{metadata}");
    assert!(metadata.contains("edits:+120/-45"), "{metadata}");
}

#[test]
fn tool_result_metadata_omits_edits_when_no_lines_changed() {
    // 読み取り専用サブエージェント（linesAdded/Removed=0）では edits: を出さない。
    let value = serde_json::json!({
        "agentType": "general-purpose",
        "toolStats": {"readCount": 6, "bashCount": 3, "linesAdded": 0, "linesRemoved": 0}
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("agent:general-purpose"), "{metadata}");
    assert!(!metadata.contains("edits:"), "{metadata}");
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
fn tool_result_metadata_marks_image_results() {
    let value = serde_json::json!({
        "isImage": true
    });

    assert_eq!(tool_result_metadata(&value), "image");
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
        "isImage": false,
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
fn tool_result_metadata_shows_actual_stale_recovery_flag() {
    // 実データの Edit 成功結果に含まれる staleRecovered を回帰テストする。
    let value = serde_json::json!({
        "filePath": "/tmp/example.rs",
        "userModified": false,
        "staleRecovered": true
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("stale-recovered"), "{metadata}");
    assert!(!metadata.contains("user-modified"), "{metadata}");
}

#[test]
fn tool_result_metadata_normalizes_actual_resolved_model_suffix() {
    let metadata = tool_result_metadata(&serde_json::json!({
        "resolvedModel": "claude-opus-5[1m]"
    }));

    assert!(metadata.contains("model:claude-opus-5"), "{metadata}");
    assert!(!metadata.contains("[1m]"), "{metadata}");
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
fn tool_result_metadata_shows_actual_agent_identifiers() {
    // 実ログの非同期 Agent 起動結果と SendMessage 再開結果に現れる識別子。
    // 両者を表示すると、起動した Agent と後続の送信・再開を対応付けられる。
    let value = serde_json::json!({
        "agentId": "a0ba96398b3dbd878",
        "resumedAgentId": "ac33a76d38596530e"
    });
    let metadata = tool_result_metadata(&value);
    assert!(
        metadata.contains("agent-id:a0ba96398b3dbd878"),
        "{metadata}"
    );
    assert!(
        metadata.contains("resumed-agent:ac33a76d38596530e"),
        "{metadata}"
    );
}

#[test]
fn tool_result_metadata_omits_empty_agent_identifiers() {
    let value = serde_json::json!({"agentId": "", "resumedAgentId": ""});
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_shows_actual_background_timeout_fields() {
    let result = serde_json::json!({
        "backgroundTaskId": "task-123",
        "timedOutAfterMs": 600_000,
        "backgroundCwdHint": "Session cwd remains /tmp/project; directory changes do not persist."
    });
    let metadata = tool_result_metadata(&result);
    assert!(metadata.contains("background:task-123"));
    assert!(metadata.contains("wait-timeout:600s"));
    assert!(metadata.contains("cwd-hint:Session cwd remains /tmp/project"));
}

#[test]
fn process_tool_result_shows_non_execution_kind() {
    let input = [
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool_1","name":"Bash","input":{"command":"rm file"}}]}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool_1","is_error":true,"content":"blocked"}]},"tool_use_result":"Error: blocked","tool_result_meta":[{"id":"tool_1","non_execution_kind":"permission-rule"}]}"#,
    ]
    .join("\n");
    let clean = strip_ansi(&run_process(&input));
    assert!(clean.contains("not-executed:permission-rule"));
}

#[test]
fn tool_result_meta_metadata_matches_tool_use_id() {
    let meta = serde_json::json!([
        {"id": "tool_1", "non_execution_kind": "permission-rule"},
        {"id": "tool_2", "non_execution_kind": "policy-rule"}
    ]);
    assert_eq!(
        tool_result_meta_metadata(&meta, "tool_2"),
        "not-executed:policy-rule"
    );
    assert!(tool_result_meta_metadata(&meta, "missing").is_empty());
}

#[test]
fn tool_result_metadata_shows_allowed_tools_count() {
    // Skill が許可されたツール一覧（allowedTools）を返したときに件数を表示する。
    // 実データでは Skill 起動時に "allowedTools": ["Bash(astro-sight:*)"] のような
    // 配列が返ることがあるため、件数だけ補足する。
    let value = serde_json::json!({
        "commandName": "astro-sight",
        "allowedTools": ["Bash(astro-sight:*)", "Read"],
        "success": true
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("command:astro-sight"), "{metadata}");
    assert!(metadata.contains("allowed-tools:2"), "{metadata}");
}

#[test]
fn tool_result_metadata_empty_allowed_tools_is_omitted() {
    // 空配列のときは表示しない（ノイズ防止）
    let value = serde_json::json!({"allowedTools": []});
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_empty_background_task_id_is_omitted() {
    // backgroundTaskId が空文字なら表示しない
    let value = serde_json::json!({"backgroundTaskId": ""});
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_clamped_with_delay_seconds() {
    // wasClamped と clampedDelaySeconds が揃うと "clamped:<n>s" と表示する
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
fn tool_result_metadata_read_partial_from_offset_shows_line_range() {
    // 実ログの offset 付き Read は startLine から読み取った範囲を表示する。
    let value = serde_json::json!({
        "type": "text",
        "file": {"filePath": "/src/big.rs", "numLines": 60, "startLine": 995, "totalLines": 1221}
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("lines:995-1054/1221"), "{metadata}");
}

#[test]
fn tool_result_metadata_read_invalid_range_falls_back_to_line_ratio() {
    // 壊れた範囲や加算オーバーフローでも panic せず、従来の件数表示へ戻す。
    for start_line in [200, u64::MAX] {
        let value = serde_json::json!({
            "file": {"numLines": 50, "startLine": start_line, "totalLines": 100}
        });
        assert_eq!(tool_result_metadata(&value), "lines:50/100");
    }
}

#[test]
fn tool_result_metadata_read_token_cap_truncation_is_shown() {
    // 実データの Read 結果では token cap による切り詰めが file 配下に入る。
    let value = serde_json::json!({
        "type": "text",
        "file": {
            "filePath": "/src/huge.rs",
            "numLines": 1200,
            "startLine": 1,
            "totalLines": 1200,
            "truncatedByTokenCap": true
        }
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("truncated:token-cap"), "{metadata}");
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

#[test]
fn tool_result_metadata_async_agent_is_marked_with_id() {
    // Agent を run_in_background=true で起動した際の応答は async と識別子を表示する。
    let value = serde_json::json!({
        "isAsync": true,
        "status": "async_launched",
        "agentId": "a32bc162897eb706d",
        "outputFile": "/tmp/agent-output",
        "canReadOutputFile": true
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("async"), "{metadata}");
    assert!(
        metadata.contains("agent-id:a32bc162897eb706d"),
        "{metadata}"
    );
    // 既存の output-file:readable と共存する
    assert!(metadata.contains("output-file:readable"), "{metadata}");
}

#[test]
fn tool_result_metadata_sync_agent_does_not_mark_async() {
    // 同期 Agent 結果（isAsync が無い）には async マークを付けない
    let value = serde_json::json!({
        "agentId": "a32bc162897eb706d",
        "status": "completed",
        "totalDurationMs": 5000
    });
    let metadata = tool_result_metadata(&value);
    assert!(!metadata.contains("async"), "{metadata}");
}

#[test]
fn tool_result_metadata_updated_fields_status_only_is_omitted() {
    // updatedFields=["status"] は statusChange と重複するため非表示
    let value = serde_json::json!({
        "updatedFields": ["status"],
        "statusChange": {"from": "pending", "to": "completed"}
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("status:pending->completed"), "{metadata}");
    assert!(!metadata.contains("updated:"), "{metadata}");
}

#[test]
fn tool_result_metadata_updated_fields_non_status_is_shown() {
    // status 以外のフィールド変更があるときは updated:... を表示する
    let value = serde_json::json!({
        "updatedFields": ["description", "status", "subject"]
    });
    let metadata = tool_result_metadata(&value);
    // sort + dedup によりアルファベット順、status を除外
    assert!(
        metadata.contains("updated:description,subject"),
        "{metadata}"
    );
}

#[test]
fn tool_result_metadata_updated_fields_empty_is_omitted() {
    // 空配列・空文字列フィールドは表示しない
    let value = serde_json::json!({"updatedFields": []});
    assert_eq!(tool_result_metadata(&value), "");

    let value = serde_json::json!({"updatedFields": ["", "status"]});
    assert_eq!(tool_result_metadata(&value), "");
}

#[test]
fn tool_result_metadata_shows_workflow_name() {
    // Workflow の async 起動結果は workflowName を表示する（どのワークフローが走ったか）。
    // 実 jsonl の形: {status, taskId, taskType:local_workflow, workflowName, runId, ...}
    let value = serde_json::json!({
        "status": "async_launched",
        "taskId": "wl9rh41wo",
        "taskType": "local_workflow",
        "workflowName": "claw-hooks-audit",
        "runId": "wf_a821ec7d-5bb"
    });
    let metadata = tool_result_metadata(&value);
    assert!(metadata.contains("workflow:claw-hooks-audit"), "{metadata}");
    // taskType も併記される（既存挙動）。
    assert!(metadata.contains("task-type:local_workflow"), "{metadata}");
    // runId は内部識別子のため表示しない。
    assert!(!metadata.contains("wf_a821ec7d"), "{metadata}");
}

#[test]
fn tool_result_metadata_shows_memdir_stamped_flag() {
    // 実 jsonl ではメモリ用ディレクトリへの Edit/Write 成功時に true で現れる。
    assert_eq!(
        tool_result_metadata(&serde_json::json!({"memdirStamped": true})),
        "memdir-stamped"
    );
    assert_eq!(
        tool_result_metadata(&serde_json::json!({"memdirStamped": false})),
        ""
    );
}

#[test]
fn tool_result_string_summary_returns_first_meaningful_line() {
    // 実データ例: MCP ツール応答が文字列で入るケース
    let value = serde_json::json!("initialize OK, protocol: 2025-06-18");
    assert_eq!(
        tool_result_string_summary(&value).as_deref(),
        Some("result:initialize OK, protocol: 2025-06-18")
    );
    // 先頭の空行はスキップして有意な行を採用する
    let value = serde_json::json!("\n  \n  second line here\nrest");
    assert_eq!(
        tool_result_string_summary(&value).as_deref(),
        Some("result:second line here")
    );
    // 空文字列・object は対象外
    assert_eq!(tool_result_string_summary(&serde_json::json!("")), None);
    assert_eq!(
        tool_result_string_summary(&serde_json::json!({"stdout":"x"})),
        None
    );
}

#[test]
fn tool_result_string_summary_supports_text_block_array() {
    // 実 jsonl の Context7 応答は tool_use_result 自体が text ブロック配列になる。
    let value = serde_json::json!([
        {"type": "text", "text": "\n  first Context7 result\nrest"}
    ]);
    assert_eq!(
        tool_result_string_summary(&value).as_deref(),
        Some("result:first Context7 result")
    );

    // text 以外のブロックや空配列を結果本文として誤表示しない。
    assert_eq!(
        tool_result_string_summary(&serde_json::json!([
            {"type": "image", "text": "not a text result"}
        ])),
        None
    );
    assert_eq!(tool_result_string_summary(&serde_json::json!([])), None);
}

#[test]
fn process_string_tool_use_result_shown_on_success_only() {
    // 成功時: 文字列 tool_use_result を result: として補足表示する
    let input = [
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t_mcp","name":"mcp__probe__init","input":{}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_mcp","type":"tool_result","content":"initialize OK, protocol: 2025-06-18"}]},"tool_use_result":"initialize OK, protocol: 2025-06-18"}"#,
    ]
    .join("\n");
    let clean = strip_ansi(&run_process(&input));
    assert!(
        clean.contains("[result:initialize OK, protocol: 2025-06-18]"),
        "{clean}"
    );

    // エラー時: content 側サマリーと同文になるため補足しない（重複防止）
    let input = [
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t_e","name":"Write","input":{}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_e","type":"tool_result","is_error":true,"content":"Error: File has not been read yet. Read it first before writing to it."}]},"tool_use_result":"Error: File has not been read yet. Read it first before writing to it."}"#,
    ]
    .join("\n");
    let clean = strip_ansi(&run_process(&input));
    assert!(
        clean.contains("Error: File has not been read yet"),
        "{clean}"
    );
    assert!(!clean.contains("[result:"), "{clean}");
}

#[test]
fn process_text_block_array_tool_use_result_is_shown() {
    let input = [
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t_context7","name":"mcp__context7__query-docs","input":{}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t_context7","type":"tool_result","content":[{"type":"text","text":"Context7 result body"}]}]},"tool_use_result":[{"type":"text","text":"Context7 result body"}]}"#,
    ]
    .join("\n");
    let clean = strip_ansi(&run_process(&input));
    assert!(clean.contains("[result:Context7 result body]"), "{clean}");
}
