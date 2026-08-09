use std::path::Path;

use crate::config::RuntimeAgent;
use crate::scanner::ResolvedTarget;

use super::AI_USAGE_MONITOR_TIMEOUT_SECS;
use super::util::task_log_base;

#[allow(clippy::too_many_arguments)]
pub(super) fn generate_monitor_script(
    agent_name: &str,
    command_str: &str,
    reset_info: &str,
    total: usize,
    deadline_secs: u64,
    marker_dir: &std::path::Path,
    session: &str,
    worker_count: usize,
    stop_file: &std::path::Path,
    report_dir: &std::path::Path,
    ai_usage_statusline_cmd: Option<&str>,
    ai_usage_refresh_cmd: Option<&str>,
    ai_usage_cache_file: Option<&std::path::Path>,
) -> String {
    let agent_escaped = shell_escape(agent_name);
    let command_escaped = shell_escape(command_str);
    let reset_escaped = shell_escape(reset_info);
    let marker_dir_escaped = shell_escape(&marker_dir.to_string_lossy());
    let session_escaped = shell_escape(session);
    let stop_file_escaped = shell_escape(&stop_file.to_string_lossy());
    let report_dir_escaped = shell_escape(&report_dir.to_string_lossy());
    // ai-usage 連携時のみ statusline コマンド（shell_escape 済みを再度 escape し、
    // monitor 側の eval で 1 段戻して実行する）。未設定は空文字列で monitor が無効判定する。
    let ai_usage_cmd_escaped = ai_usage_statusline_cmd
        .map(shell_escape)
        .unwrap_or_else(|| "''".to_string());
    // ai-usage --json を bash で実行してキャッシュを atomic 更新するための文字列。
    // statusline は `--input <cache>` でキャッシュを読むだけなので、monitor 側で
    // キャッシュを更新しないと長時間タスク中に表示が固定される。usage-gate と同じ
    // キャッシュファイルを共有するため、並列ワーカー側も新しい値を読める。
    let ai_usage_refresh_escaped = ai_usage_refresh_cmd
        .map(shell_escape)
        .unwrap_or_else(|| "''".to_string());
    let ai_usage_cache_escaped = ai_usage_cache_file
        .map(|p| shell_escape(&p.to_string_lossy()))
        .unwrap_or_else(|| "''".to_string());

    format!(
        r#"#!/bin/bash
AGENT={agent}
COMMAND={command}
RESET={reset}
TOTAL={total}
DEADLINE={deadline}
MARKER_DIR={marker_dir}
SESSION={session}
WORKER_COUNT={worker_count}
STOP_FILE={stop_file}
REPORT_DIR={report_dir}
AI_USAGE_CMD={ai_usage_cmd}
AI_USAGE_REFRESH_CMD={ai_usage_refresh_cmd}
AI_USAGE_CACHE_FILE={ai_usage_cache_file}
AI_USAGE_TIMEOUT={ai_usage_timeout}
STOPPED=0
USAGE_DISPLAY=""
LAST_USAGE=0
LAST_ERR_COUNT=-1
STATUS_MSG=""
RESIZED=0

restore_terminal() {{ tput cnorm 2>/dev/null; printf '\033[?7h'; }}

# 完了・停止時はセッションごと閉じて token-burn を終了する。Ctrl-C 待ちはしない。
# 画面に出した集計とログパスは Rust 側（execute_plan_tmux）が attach から戻った直後に
# stdout へ出し直すので、読み返すために tmux を開いたままにする必要はない。
# モニターが自分で exit してペインを閉じる（＝残りペインが消えてセッションが自然終了する）
# 方式は採らない。それだと「モニターが最後のペインである」ことが前提になり、ワーカーが
# worker-done を作れずに死んだ場合（touch 失敗、シェルのクラッシュ、ユーザーがペインを
# 手動で kill）にセッションが残ってデッドラインまで token-burn が終わらない。全タスク処理済み
# 判定（PROCESSED >= TOTAL）で入ったときはワーカーが最後の usage-gate を実行中の可能性が
# あるが、後続タスクが無いのでその結果は使われず、待たずに閉じてよい。
# kill-session はモニター自身のペインも道連れに殺すため、後続行には基本到達しない。
# セッション名の解決失敗などで kill-session が空振りした場合に取り残されないよう、
# 明示的に exit する（モニターが最後のペインならこの exit でもセッションは閉じる）。
finish_session() {{
    restore_terminal
    tmux kill-session -t "$SESSION" 2>/dev/null
    exit 0
}}

# ai-usage サブプロセスがハングしても monitor ループを止めないための timeout ラッパー。
# `$1` 秒で子プロセスを SIGTERM → 1 秒後 SIGKILL する。コマンド自身の stdout はそのまま
# 親に伝搬するため `$(run_with_timeout N bash -c "$CMD")` の形で捕捉できる。
#
# 監視サブシェルの stdout は必ず捨てる。継承したままだと、コマンドが即座に終わっても
# サブシェルの子 `sleep $secs` がコマンド置換のパイプ書き込み端を握ったまま孤児化し
# （`kill -TERM $wpid` はサブシェル本体しか殺せない）、`$( )` が EOF を待って
# timeout 秒まるごとブロックする。結果として monitor の再描画が 10 秒ごとに
# AI_USAGE_TIMEOUT 秒固まっていた（ハング対策のはずが逆に固まる）。
run_with_timeout() {{
    local secs=$1
    shift
    "$@" &
    local cpid=$!
    ( sleep "$secs"; kill -TERM $cpid 2>/dev/null; sleep 1; kill -KILL $cpid 2>/dev/null ) >/dev/null 2>&1 &
    local wpid=$!
    wait $cpid 2>/dev/null
    local rc=$?
    kill -TERM $wpid 2>/dev/null
    wait $wpid 2>/dev/null
    return $rc
}}

fetch_usage() {{
    [ -z "$AI_USAGE_CMD" ] && return
    # まず ai-usage --json を実行してキャッシュを atomic 更新する。statusline は
    # `--input <cache>` でキャッシュを読むだけなので、ここで更新しないと表示が
    # 固定される。usage-gate と同じキャッシュファイルを共有するため、並列
    # ワーカーの使用率判定も最新の値を読める。tmp ファイル経由で mv するのは
    # 読み手が壊れた JSON を読まないようにするため。
    # `run_with_timeout` で包み、ai-usage がハングしても monitor ループが固まらないようにする。
    if [ -n "$AI_USAGE_REFRESH_CMD" ] && [ -n "$AI_USAGE_CACHE_FILE" ]; then
        if run_with_timeout "$AI_USAGE_TIMEOUT" bash -c "$AI_USAGE_REFRESH_CMD" > "$AI_USAGE_CACHE_FILE.tmp" 2>/dev/null; then
            mv "$AI_USAGE_CACHE_FILE.tmp" "$AI_USAGE_CACHE_FILE" 2>/dev/null
        else
            rm -f "$AI_USAGE_CACHE_FILE.tmp" 2>/dev/null
        fi
    fi
    local new
    new=$(run_with_timeout "$AI_USAGE_TIMEOUT" bash -c "$AI_USAGE_CMD" 2>/dev/null)
    [ -n "$new" ] && USAGE_DISPLAY="$new"
}}

render() {{
    printf '\033[H\033[J'
    echo "━━━━━━━━━━━━━━━━━━━━━━━━"
    echo " 🔥 token-burn 🔥"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    echo " Agent:   $AGENT"
    echo " Command: $COMMAND"
    echo " Reset:   $RESET"
    echo " Tasks:   $TOTAL"
    echo " Workers: $WORKER_COUNT"
    echo " Logs:    $REPORT_DIR"
    if [ -n "$USAGE_DISPLAY" ]; then
        echo ""
        echo " AI Usage:"
        printf '%s\n' "$USAGE_DISPLAY"
    fi
    echo ""
    while IFS= read -r f; do
        printf ' ❌ %s\n' "$(cat "$f")"
    done < <(find "$MARKER_DIR" -name 'error-*' 2>/dev/null)
    [ -n "$STATUS_MSG" ] && echo "$STATUS_MSG"
    echo ""
}}

handle_signal() {{
    if [ $STOPPED -eq 0 ]; then
        STOPPED=1
        touch "$STOP_FILE"
        STATUS_MSG=" ⏳ Waiting for current tasks to finish... (Ctrl-C again to force kill)"
        render
    else
        restore_terminal
        echo ""
        echo " 📁 Logs: $REPORT_DIR"
        echo " Force killing session..."
        tmux kill-session -t "$SESSION" 2>/dev/null
        exit
    fi
}}
trap handle_signal INT TERM
trap restore_terminal EXIT
# ワーカーはタスクを消化し切るたびに自分のペインを閉じ、そのぶん tmux がモニターペインを
# 広げるため SIGWINCH が届く。通常の全体再描画は ai-usage の 10 秒間隔取得・エラー件数の
# 変化・状態遷移でしか走らないので、描き直さないと旧い幅で折り返した行が残る。
# ハンドラから直接 render を呼ぶと、進行中の render の途中に次の SIGWINCH が入って出力が
# 混ざるため、フラグだけ立ててメインループの再描画判定に合流させる。
trap 'RESIZED=1' WINCH

printf '\033]2;token-burn\033\\'
printf '\033[?7l'
tput civis 2>/dev/null

END=$(($(date +%s) + DEADLINE))

fetch_usage
LAST_USAGE=$(date +%s)
render

while true; do
    NOW=$(date +%s)
    REMAINING=$((END - NOW))
    # worker-done は各ワーカーが自分の done-*/failed-*/retry-* を書き切った後に作る。
    # 先に worker-done を読んでからタスクマーカーを読めば「worker-done が見えている
    # のにそのワーカーのタスクマーカーが見えていない」状態は起こらない。逆順（タスク
    # マーカーを読む → fetch_usage で最大 AI_USAGE_TIMEOUT 秒ブロック → worker-done を
    # 読む）だと、その待ち時間に最後のワーカーが完走した場合に古い PROCESSED と新しい
    # WORKERS_DONE が組み合わさり、全件成功でも「⏹ Stopped: 9/10 processed」と誤報告する。
    WORKERS_DONE=$(find "$MARKER_DIR" -name 'worker-done-*' 2>/dev/null | wc -l | tr -d ' ')
    DONE=$(find "$MARKER_DIR" -name 'done-*' 2>/dev/null | wc -l | tr -d ' ')
    FAILED=$(find "$MARKER_DIR" -name 'failed-*' 2>/dev/null | wc -l | tr -d ' ')
    RETRY=$(find "$MARKER_DIR" -name 'retry-*' 2>/dev/null | wc -l | tr -d ' ')
    PROCESSED=$((DONE + FAILED + RETRY))
    ERR_COUNT=$(find "$MARKER_DIR" -name 'error-*' 2>/dev/null | wc -l | tr -d ' ')

    NEED_RENDER=0
    # ワーカーペインが閉じてモニターペインの幅が変わったら全体を描き直す
    if [ $RESIZED -eq 1 ]; then
        RESIZED=0
        NEED_RENDER=1
    fi
    # ai-usage statusline は 10 秒ごとに再取得・再描画する
    if [ -n "$AI_USAGE_CMD" ] && [ $((NOW - LAST_USAGE)) -ge 10 ]; then
        fetch_usage
        # fetch 開始時刻（$NOW）ではなく完了時刻を記録する。fetch_usage は
        # run_with_timeout を 2 回呼ぶため最長 2*AI_USAGE_TIMEOUT 秒かかり、開始時刻を
        # 基準にすると次の周回で即座に条件が成立して間隔を空けずに再取得し続ける。
        LAST_USAGE=$(date +%s)
        NEED_RENDER=1
    fi
    # 新規エラー検出時も全体を再描画する
    if [ "$ERR_COUNT" != "$LAST_ERR_COUNT" ]; then
        LAST_ERR_COUNT=$ERR_COUNT
        NEED_RENDER=1
    fi

    # デッドライン到達確認
    if [ $REMAINING -le 0 ] && [ $STOPPED -eq 0 ]; then
        STOPPED=1
        touch "$STOP_FILE"
        STATUS_MSG=" ⚠ DEADLINE REACHED — waiting for current tasks (Ctrl-C to force kill)"
        NEED_RENDER=1
    fi

    # usage-gate / rate_limit_event がワーカー側で stop file を作った場合も STOPPED に遷移する。
    # これが無いと、シグナルもデッドラインも来ていないのに STOPPED=0 のままとなり、全ワーカー
    # 完了判定（STOPPED=1 の内側）が発火せず、モニターがデッドラインまで張り付いてしまう。
    if [ $STOPPED -eq 0 ] && [ -f "$STOP_FILE" ]; then
        STOPPED=1
        STATUS_MSG=" ⛔ Usage/rate limit reached — waiting for current tasks to finish"
        NEED_RENDER=1
    fi

    if [ $NEED_RENDER -eq 1 ]; then render; fi

    # 失敗・リトライを含め、全タスクが処理済みか確認
    if [ "$PROCESSED" -ge "$TOTAL" ]; then
        render
        if [ "$FAILED" -gt 0 ] || [ "$RETRY" -gt 0 ]; then
            printf " ⚠  Completed: %d succeeded / %d failed / %d retry\n" "$DONE" "$FAILED" "$RETRY"
        else
            printf " ✅ All %d/%d tasks completed!\n" "$DONE" "$TOTAL"
        fi
        echo ""
        echo " 📁 Logs: $REPORT_DIR"
        echo ""
        finish_session
    fi

    # 全ワーカーが終了していれば、STOPPED かどうかに関わらず停止と判定する。
    # 上の PROCESSED>=TOTAL で正常完了は既に抜けているため、ここに来るのは「全ワーカーが
    # 終了したのにタスクが処理し切れていない」ケース＝早期停止（stop file 検出、usage-gate
    # の fail-closed による break、タスクスクリプト欠落など）。STOPPED の内側に閉じ込めると、
    # usage-gate が stop file を作れずに fail-closed した場合にモニターがデッドラインまで
    # ハングするため、ワーカー全滅は独立した終了条件として扱う。
    if [ "$WORKERS_DONE" -ge "$WORKER_COUNT" ]; then
        render
        printf " ⏹ Stopped: %d/%d processed (fail:%d retry:%d)\n" "$PROCESSED" "$TOTAL" "$FAILED" "$RETRY"
        echo ""
        echo " 📁 Logs: $REPORT_DIR"
        echo ""
        finish_session
    fi

    # 進捗バー（画面最下部、毎秒 \r 上書き）
    if [ $TOTAL -gt 0 ]; then
        PCT=$((PROCESSED * 100 / TOTAL))
        BAR_W=20
        FILLED=$((PCT * BAR_W / 100))
        EMPTY=$((BAR_W - FILLED))
        BAR=""
        # BSD seq(macOS) は `seq 1 0` が降順で "1 0" を出すため、算術ループで描画する
        # （seq 依存だと FILLED/EMPTY が 0 の 0%/100% でバーが2文字ずれる）
        i=0; while [ $i -lt $FILLED ]; do BAR="${{BAR}}█"; i=$((i+1)); done
        i=0; while [ $i -lt $EMPTY ]; do BAR="${{BAR}}░"; i=$((i+1)); done
    else
        BAR="░░░░░░░░░░░░░░░░░░░░"
        PCT=0
    fi

    if [ $STOPPED -eq 0 ]; then
        D=$((REMAINING / 86400))
        H=$(((REMAINING % 86400) / 3600))
        M=$(((REMAINING % 3600) / 60))
        S=$((REMAINING % 60))
        printf "\r\033[2K ⏱ %dd %02dh %02dm %02ds  [%s] %d/%d (%d%%, fail:%d retry:%d)" \
            "$D" "$H" "$M" "$S" "$BAR" "$PROCESSED" "$TOTAL" "$PCT" "$FAILED" "$RETRY"
    else
        printf "\r\033[2K ⏳ Stopping...  [%s] %d/%d (%d%%, fail:%d retry:%d)" \
            "$BAR" "$PROCESSED" "$TOTAL" "$PCT" "$FAILED" "$RETRY"
    fi

    sleep 1
done
"#,
        session = session_escaped,
        agent = agent_escaped,
        command = command_escaped,
        reset = reset_escaped,
        total = total,
        deadline = deadline_secs,
        marker_dir = marker_dir_escaped,
        worker_count = worker_count,
        stop_file = stop_file_escaped,
        report_dir = report_dir_escaped,
        ai_usage_cmd = ai_usage_cmd_escaped,
        ai_usage_refresh_cmd = ai_usage_refresh_escaped,
        ai_usage_cache_file = ai_usage_cache_escaped,
        ai_usage_timeout = AI_USAGE_MONITOR_TIMEOUT_SECS,
    )
}

pub(super) struct TaskCtx<'a> {
    pub(super) idx: usize,
    pub(super) total: usize,
    pub(super) task: &'a ResolvedTarget,
    pub(super) agent: &'a RuntimeAgent,
    pub(super) prompt_file: &'a Path,
    pub(super) run_dir: &'a Path,
    pub(super) marker_dir: &'a Path,
    pub(super) exe_path: &'a Path,
    pub(super) state_file: &'a Path,
    pub(super) stop_file: &'a Path,
    pub(super) rate_limit_threshold: u8,
    pub(super) is_claude: bool,
}

pub(super) struct WorkerCtx<'a> {
    pub(super) worker_id: usize,
    pub(super) queue_dir: &'a Path,
    pub(super) task_dir: &'a Path,
    pub(super) marker_dir: &'a Path,
    pub(super) stop_file: &'a Path,
    /// 各タスク完了後に実行する usage-gate コマンド（ai-usage 連携時のみ）。
    pub(super) usage_gate_cmd: Option<&'a str>,
}

/// キューから claim したワーカーが source して実行する、タスク単位のシェルスクリプトを生成する。
pub(super) fn build_task_script(ctx: &TaskCtx<'_>) -> String {
    let log_base = task_log_base(ctx.idx, &ctx.task.display_name);
    let log_file = shell_escape(
        &ctx.run_dir
            .join(format!("{log_base}.log"))
            .to_string_lossy(),
    );
    let jsonl_file = shell_escape(
        &ctx.run_dir
            .join(format!("{log_base}.jsonl"))
            .to_string_lossy(),
    );
    let done_marker = shell_escape(
        &ctx.marker_dir
            .join(format!("done-{}", ctx.idx))
            .to_string_lossy(),
    );
    let failed_marker = shell_escape(
        &ctx.marker_dir
            .join(format!("failed-{}", ctx.idx))
            .to_string_lossy(),
    );
    let retry_marker = shell_escape(
        &ctx.marker_dir
            .join(format!("retry-{}", ctx.idx))
            .to_string_lossy(),
    );
    let error_file = shell_escape(
        &ctx.marker_dir
            .join(format!("error-{}", ctx.idx))
            .to_string_lossy(),
    );
    let error_prefix = shell_escape(&format!("[{}] ", ctx.task.display_name));
    let stop_file_escaped = shell_escape(&ctx.stop_file.to_string_lossy());
    let cmd_str = build_shell_command(&ctx.agent.command, &ctx.agent.env, ctx.prompt_file);
    let mark_cmd = format!(
        "{} mark {} {} {}",
        shell_escape(&ctx.exe_path.to_string_lossy()),
        shell_escape(&ctx.agent.name),
        shell_escape(&ctx.task.directory.to_string_lossy()),
        shell_escape(&ctx.state_file.to_string_lossy()),
    );

    let mut script = String::new();
    // 現在処理中のタスクをシグナルハンドラから参照できるようにする
    script += &format!("CURRENT_FAILED_MARKER={failed_marker}\n");
    script += &build_task_header_script(ctx.idx, ctx.total, &ctx.task.display_name);
    // 対象ディレクトリへの移動はパイプラインと分けて明示的に扱う。
    // `cd X && cmd | fmt | tee` と書くと bash は `cd X && (3 要素パイプライン)` と解釈し、
    // cd 失敗時はパイプラインが実行されず PIPESTATUS が cd の 1 要素だけになる。
    // すると FORMAT_EXIT / TEE_EXIT が空文字に展開されて `[ "" -ne 0 ]` が
    // "integer expression expected" を吐き（ワーカーペインに漏れる）、記録される
    // エラーも「logging pipeline failed」という真因と無関係な文言になっていた。
    // スキャンから実行までの間に対象リポジトリが削除・リネームされると発生する。
    script += &format!(
        concat!(
            "cd {dir} || {{\n",
            "  printf '%starget directory is unavailable\\n' {prefix} > {error}\n",
            "  touch {failed}\n",
            "  echo '━━━ Error - target directory is unavailable ━━━'\n",
            "  echo ''\n",
            "  return 0\n",
            "}}\n",
        ),
        dir = shell_escape(&ctx.task.directory.to_string_lossy()),
        prefix = error_prefix,
        error = error_file,
        failed = failed_marker,
    );

    if ctx.is_claude {
        let tb_cmd = shell_escape(&ctx.exe_path.to_string_lossy());
        script += &format!(
            "{cmd_str} 2>&1 | {tb_cmd} format-stream --raw-output {jsonl_file} --stop-file {stop_file_escaped} --threshold {rate_limit_threshold} 2>&1 | tee {log_file}\n",
            rate_limit_threshold = ctx.rate_limit_threshold,
        );
        script += "PIPE_STATUS=(\"${PIPESTATUS[@]}\")\n";
        script += "CMD_EXIT=${PIPE_STATUS[0]}\n";
        script += "FORMAT_EXIT=${PIPE_STATUS[1]}\n";
        script += "TEE_EXIT=${PIPE_STATUS[2]}\n";
        script += "CURRENT_FAILED_MARKER=\"\"\n";
        script += &format!(
            concat!(
                "if [ \"$FORMAT_EXIT\" -ne 0 ] || [ \"$TEE_EXIT\" -ne 0 ] || [ ! -s {jsonl} ]; then\n",
                "  printf '%slogging/classification pipeline failed (format=%s tee=%s)\\n' {prefix} \"$FORMAT_EXIT\" \"$TEE_EXIT\" > {error}\n",
                "  touch {failed}\n",
                "  echo '━━━ Error - logging pipeline failed ━━━'\n",
                "  echo ''\n",
                "  return 0\n",
                "fi\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
            jsonl = jsonl_file,
        );
    } else {
        script += &format!("{cmd_str} 2>&1 | tee {log_file}\n");
        script += "PIPE_STATUS=(\"${PIPESTATUS[@]}\")\n";
        script += "CMD_EXIT=${PIPE_STATUS[0]}\n";
        script += "TEE_EXIT=${PIPE_STATUS[1]}\n";
        script += "CURRENT_FAILED_MARKER=\"\"\n";
        script += &format!(
            concat!(
                "if [ \"$TEE_EXIT\" -ne 0 ]; then\n",
                "  printf '%slogging pipeline failed (tee=%s)\\n' {prefix} \"$TEE_EXIT\" > {error}\n",
                "  touch {failed}\n",
                "  echo '━━━ Error - logging pipeline failed ━━━'\n",
                "  echo ''\n",
                "  return 0\n",
                "fi\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
        );
    }

    if ctx.is_claude {
        let tb_cmd = shell_escape(&ctx.exe_path.to_string_lossy());
        script += &format!(
            concat!(
                "CLASSIFIED=$({tb} classify-result {jsonl} 2>/dev/null)\n",
                "CLASS_CODE=$?\n",
                "case $CLASS_CODE in\n",
                "  2)\n",
                // 後続タスクが誤って「Cancelled」と判定されないよう、ここでフラグを必ずリセットする
                "    CANCELLED=0\n",
                "    touch {failed}\n",
                "    echo '━━━ Rate limited - not marking as completed ━━━'\n",
                "    ;;\n",
                "  3)\n",
                // 後続タスクが誤って「Cancelled」と判定されないよう、ここでフラグを必ずリセットする
                "    CANCELLED=0\n",
                "    if [ -n \"$CLASSIFIED\" ]; then\n",
                "      printf '%s%s\\n' {prefix} \"$CLASSIFIED\" > {error}\n",
                "    fi\n",
                "    touch {retry}\n",
                "    echo \"━━━ Retryable error (will retry next run): $CLASSIFIED ━━━\"\n",
                "    ;;\n",
                "  1)\n",
                "    if [ $CANCELLED -eq 1 ]; then\n",
                "      CANCELLED=0\n",
                "      touch {failed}\n",
                "      echo '━━━ Cancelled ━━━'\n",
                "    else\n",
                "      printf '%s%s\\n' {prefix} \"$CLASSIFIED\" > {error}\n",
                "      touch {failed}\n",
                "      echo '━━━ Error - continuing ━━━'\n",
                "    fi\n",
                "    ;;\n",
                "  *)\n",
                "    if [ \"$CMD_EXIT\" -ne 0 ]; then\n",
                "      if [ $CANCELLED -eq 1 ]; then\n",
                "        CANCELLED=0\n",
                "        touch {failed}\n",
                "        echo '━━━ Cancelled ━━━'\n",
                "      else\n",
                "        ERROR_MSG=$(tmux capture-pane -t \"$TMUX_PANE\" -p -J -S -10 | grep -v '^$' | tail -1)\n",
                "        printf '%s%s\\n' {prefix} \"$ERROR_MSG\" > {error}\n",
                "        touch {failed}\n",
                "        echo '━━━ Error - continuing ━━━'\n",
                "      fi\n",
                "    elif {mark}; then\n",
                "      touch {done}\n",
                "    else\n",
                // state 書き込み失敗を成功扱いしない（次回再処理されるよう done を作らない）
                "      printf '%sstate update failed\\n' {prefix} > {error}\n",
                "      touch {failed}\n",
                "      echo '━━━ Error - state update failed ━━━'\n",
                "    fi\n",
                "    ;;\n",
                "esac\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
            retry = retry_marker,
            done = done_marker,
            jsonl = jsonl_file,
            tb = tb_cmd,
            mark = mark_cmd,
        );
    } else {
        script += &format!(
            concat!(
                "if [ \"$CMD_EXIT\" -ne 0 ]; then\n",
                "  if [ $CANCELLED -eq 1 ]; then\n",
                "    CANCELLED=0\n",
                "    touch {failed}\n",
                "    echo '━━━ Cancelled ━━━'\n",
                "  else\n",
                "    ERROR_MSG=$(tmux capture-pane -t \"$TMUX_PANE\" -p -J -S -10 | grep -v '^$' | tail -1)\n",
                "    printf '%s%s\\n' {prefix} \"$ERROR_MSG\" > {error}\n",
                "    touch {failed}\n",
                "    echo '━━━ Error - continuing ━━━'\n",
                "  fi\n",
                "elif {mark}; then\n",
                "  touch {done}\n",
                "else\n",
                // state 書き込み失敗を成功扱いしない（次回再処理されるよう done を作らない）
                "  printf '%sstate update failed\\n' {prefix} > {error}\n",
                "  touch {failed}\n",
                "  echo '━━━ Error - state update failed ━━━'\n",
                "fi\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
            done = done_marker,
            mark = mark_cmd,
        );
    }
    script += "echo ''\n";
    script
}

/// 共通ワーカースクリプト: queue_dir/pending-* をアトミックに claim しつつタスクを逐次実行する。
pub(super) fn build_worker_script(ctx: &WorkerCtx<'_>) -> String {
    let w = ctx.worker_id + 1;
    let queue_dir = shell_escape(&ctx.queue_dir.to_string_lossy());
    let task_dir = shell_escape(&ctx.task_dir.to_string_lossy());
    let stop_file = shell_escape(&ctx.stop_file.to_string_lossy());
    let worker_done = shell_escape(
        &ctx.marker_dir
            .join(format!("worker-done-{}", ctx.worker_id))
            .to_string_lossy(),
    );
    // ai-usage 連携時は各タスク完了後（次の pending を claim する前）に usage-gate を実行する。
    // 未設定なら空文字列で、ワーカースクリプトに行は追加されない。
    // usage-gate が非ゼロ終了した場合（使用率確認不能や stop file 作成失敗による
    // fail-closed）はループを break して止める。終了コードを無視すると、stop file を
    // 作れなかった fail-closed 時にワーカーが後続タスクを処理し続けてしまう。break 後は
    // ループ末尾で worker-done マーカーを作るため、モニターの停止判定も正しく発火する。
    let gate_line = match ctx.usage_gate_cmd {
        Some(cmd) => format!(
            "  if ! {cmd}; then\n    echo '━━━ usage-gate failed closed — stopping ━━━'\n    break\n  fi\n"
        ),
        None => String::new(),
    };

    format!(
        concat!(
            "#!/bin/bash\n",
            "CURRENT_FAILED_MARKER=\"\"\n",
            "CANCELLED=0\n",
            "handle_cancel() {{\n",
            "  CANCELLED=1\n",
            "  if [ -n \"$CURRENT_FAILED_MARKER\" ]; then touch \"$CURRENT_FAILED_MARKER\"; fi\n",
            "}}\n",
            "trap handle_cancel INT TERM\n",
            "\n",
            "QUEUE_DIR={queue_dir}\n",
            "TASK_DIR={task_dir}\n",
            "\n",
            "while true; do\n",
            "  if [ -f {stop_file} ]; then\n",
            "    printf '\\033]2;Worker {w} stopped\\033\\\\'\n",
            "    echo '━━━ Stopped ━━━'\n",
            "    break\n",
            "  fi\n",
            // 各タスク開始前に必ずリセットする。直前タスクの実行中に SIGINT/SIGTERM を
            // 受けて CANCELLED=1 が立ったまま成功・早期 return した場合でも、後続タスクの
            // 通常エラーを誤って「Cancelled」と判定しエラー記録を欠落させるのを防ぐ。
            "  CANCELLED=0\n",
            "  CLAIMED=\"\"\n",
            "  for pending in \"$QUEUE_DIR\"/pending-*; do\n",
            "    [ -e \"$pending\" ] || continue\n",
            "    base=$(basename \"$pending\")\n",
            "    idx=${{base#pending-}}\n",
            "    if mv \"$pending\" \"$QUEUE_DIR/claimed-$idx\" 2>/dev/null; then\n",
            "      CLAIMED=\"$idx\"\n",
            "      break\n",
            "    fi\n",
            "  done\n",
            "  if [ -z \"$CLAIMED\" ]; then\n",
            "    break\n",
            "  fi\n",
            "  TASK_SCRIPT=\"$TASK_DIR/task-$CLAIMED.sh\"\n",
            "  if [ ! -f \"$TASK_SCRIPT\" ]; then\n",
            "    echo \"━━━ Missing task script: $TASK_SCRIPT ━━━\"\n",
            "    continue\n",
            "  fi\n",
            "  # shellcheck disable=SC1090\n",
            "  source \"$TASK_SCRIPT\"\n",
            "{gate_line}",
            "done\n",
            "\n",
            "printf '\\033]2;Worker {w} done\\033\\\\'\n",
            "echo '━━━ All tasks completed ━━━'\n",
            "touch {worker_done}\n",
            // 全タスク完了後はキャンセル trap を外してから抜ける。残したまま exit すると、
            // 終了間際に届いた INT/TERM で handle_cancel だけが走り、処理するタスクが無いのに
            // 直前タスクの failed マーカーを立て直してしまう。trap 解除の直前に届いた分でも
            // 立て直さないよう、参照先の変数を先に空にする。
            "CURRENT_FAILED_MARKER=\"\"\n",
            "trap - INT TERM\n",
            // 消化し切ったワーカーはそのまま終了し、tmux ペインを閉じる（remain-on-exit は
            // Rust 側でセッション単位に off を明示している）。以前は
            // `while true; do sleep 3600; done` でペインを開いたままログを読み返せるように
            // していたが、同じ出力は run_dir 配下の `.log` / `.jsonl` に残るため、画面に
            // 貼り付けておく必要はない。空のペインが残らない分、走っているワーカーだけが
            // 画面に出る。
            "exit 0\n",
        ),
        queue_dir = queue_dir,
        task_dir = task_dir,
        stop_file = stop_file,
        w = w,
        worker_done = worker_done,
        gate_line = gate_line,
    )
}

fn build_task_header_script(idx: usize, total: usize, display_name: &str) -> String {
    let pane_title = format!("[{}/{}] {}", idx, total, display_name);
    let section_title = format!("━━━ [{}/{}] {} ━━━", idx, total, display_name);
    format!(
        "printf '\\033]2;%s\\033\\\\' {}\necho {}\n",
        shell_escape(&pane_title),
        shell_escape(&section_title),
    )
}

/// 対象ディレクトリでエージェントを起動するコマンド文字列を組み立てる。
///
/// `cd` は含めない。呼び出し側（`build_task_script`）がパイプラインの手前で
/// 明示的に `cd ... || { ... }` を発行する。`cd X && cmd | fmt | tee` の形にすると
/// cd 失敗時に PIPESTATUS の要素数が変わってしまうため。
fn build_shell_command(
    cmd_parts: &[String],
    env: &std::collections::BTreeMap<String, String>,
    prompt_file: &std::path::Path,
) -> String {
    // 環境変数を `KEY='val' cmd ...` の形で前置する。key は設定読み込み時に
    // [A-Za-z_][A-Za-z0-9_]* へ制限済みなのでエスケープ不要、値のみ shell_escape する。
    let env_prefix = env_prefix_parts(env).join(" ");
    let cmd_joined = cmd_parts
        .iter()
        .map(|s| shell_escape(s))
        .collect::<Vec<_>>()
        .join(" ");
    let run = if env_prefix.is_empty() {
        cmd_joined
    } else {
        format!("{env_prefix} {cmd_joined}")
    };
    // プロンプトをコマンド置換 $(cat file) で引数として渡す
    // stdin パイプは claude -p で確実に動作しないため
    format!(
        "{} \"$(cat {})\"",
        run,
        shell_escape(&prompt_file.to_string_lossy())
    )
}

pub(super) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// env マップをシェル前置きトークン列にする。key は設定読み込み時に
/// [A-Za-z_][A-Za-z0-9_]* へ制限済みなのでクオート不要、値のみ shell_escape する。
///
/// 値が空文字のキーは `env -u KEY` で子プロセスから完全に除去する。bash の
/// `KEY= cmd` は KEY を「空文字に設定」するため、CLAUDE_CONFIG_DIR を空文字で
/// 渡すと claude code 側が CWD 相対の設定ディレクトリと解釈し、対象プロジェクト
/// 直下に projects/ sessions/ backups/ をぶちまける。空文字 → unset 変換で
/// 「親環境継承を封じる」設計意図（環境を上書きする）を保ったまま、claude 側に
/// 空文字パスを渡さないようにする。
///
/// 戻り値の例:
/// - 全て非空: `["K1='v1'", "K2='v2'"]`
/// - 一部空: `["env", "-u", "K1", "K2='v2'"]`
/// - 全て空: `["env", "-u", "K1", "-u", "K2"]`
/// - env マップ自体が空: `[]`
pub(super) fn env_prefix_parts(env: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut unset_keys: Vec<&str> = Vec::new();
    let mut set_pairs: Vec<String> = Vec::new();
    for (k, v) in env.iter() {
        if v.is_empty() {
            unset_keys.push(k.as_str());
        } else {
            set_pairs.push(format!("{k}={}", shell_escape(v)));
        }
    }
    if unset_keys.is_empty() {
        return set_pairs;
    }
    let mut parts: Vec<String> = Vec::with_capacity(1 + unset_keys.len() * 2 + set_pairs.len());
    parts.push("env".to_string());
    for k in unset_keys {
        parts.push("-u".to_string());
        parts.push(k.to_string());
    }
    parts.extend(set_pairs);
    parts
}

/// ai-usage の `--json` 取得コマンドから monitor 表示用の statusline コマンドを組み立てる。
/// 出力モードの `--json` を `--statusline --logos --input <cache>` に差し替え、
/// `env FOO=1 ai-usage --json` のようなラッパー前置きを保持する。`--json` が無ければ末尾に追加する。
/// さらに実行中 agent の profile/provider を `--active-profile`/`--active-provider` で渡し、選択
/// agent の env を前置きして monitor が実行中アカウント行を強調するようにする。ai-usage の active
/// 既定（CLAUDE_CONFIG_DIR/.claude.json の email）は email を持たないアカウントで解決できない
/// ため、email 非依存の profile 指定を使う。空コマンドのときは None。
pub(super) fn build_statusline_cmd(
    command: &[String],
    cache_file: &std::path::Path,
    env: &std::collections::BTreeMap<String, String>,
    profile: &str,
    provider: &str,
) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    let cache = cache_file.to_string_lossy();
    // monitor ペインは横幅が狭いため、ゲージ幅を半分にする --compact を常に付ける。
    let statusline_args = [
        "--statusline",
        "--logos",
        "--compact",
        "--input",
        cache.as_ref(),
    ];
    let mut parts: Vec<String> = Vec::new();
    let mut replaced = false;
    for arg in command {
        if arg == "--json" && !replaced {
            parts.extend(statusline_args.iter().map(|s| s.to_string()));
            replaced = true;
        } else {
            parts.push(arg.clone());
        }
    }
    if !replaced {
        parts.extend(statusline_args.iter().map(|s| s.to_string()));
    }
    // 実行中アカウント行を強調する active 指定（email 非依存の profile/provider）。
    parts.push("--active-profile".to_string());
    parts.push(profile.to_string());
    parts.push("--active-provider".to_string());
    parts.push(provider.to_string());
    // 選択 agent の env を前置きし、statusline の実行文脈を揃える。
    let mut all = env_prefix_parts(env);
    all.extend(parts.iter().map(|s| shell_escape(s)));
    Some(all.join(" "))
}

/// ai-usage の `--json` 取得コマンドに env prefix を付けて bash で実行できる
/// 文字列を組み立てる。monitor 側で `eval $CMD > $cache.tmp && mv $cache.tmp $cache`
/// の atomic 更新に使い、`--input <cache>` で読む statusline と同じファイルを
/// 共有する。これにより monitor のリフレッシュが usage-gate にも反映され、
/// 長時間タスク中に表示が古いまま固定される問題を解消する。空コマンドなら None。
pub(super) fn build_refresh_cmd(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    let mut all = env_prefix_parts(env);
    all.extend(command.iter().map(|s| shell_escape(s)));
    Some(all.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::Visibility;

    #[test]
    fn build_task_header_script_escapes_display_name() {
        let script = build_task_header_script(1, 3, "repo'; touch /tmp/pwn #");
        assert!(
            script.contains("printf '\\033]2;%s\\033\\\\' '[1/3] repo'\\''; touch /tmp/pwn #'")
        );
        assert!(script.contains("echo '━━━ [1/3] repo'\\''; touch /tmp/pwn # ━━━'"));
    }

    fn task_ctx_for_test<'a>(
        idx: usize,
        agent: &'a RuntimeAgent,
        task: &'a ResolvedTarget,
        tmp: &'a std::path::Path,
        is_claude: bool,
    ) -> TaskCtx<'a> {
        TaskCtx {
            idx,
            total: 3,
            task,
            agent,
            prompt_file: std::path::Path::new("/tmp/prompt.txt"),
            run_dir: tmp,
            marker_dir: tmp,
            exe_path: std::path::Path::new("/usr/local/bin/token-burn"),
            state_file: std::path::Path::new("/tmp/state.json"),
            stop_file: std::path::Path::new("/tmp/stop"),
            rate_limit_threshold: 95,
            is_claude,
        }
    }

    #[test]
    fn build_task_script_for_claude_uses_classify_result_and_no_sleep_infinity() {
        let agent = RuntimeAgent {
            name: "claude".to_string(),
            command: vec!["claude".to_string(), "-p".to_string()],
            ..Default::default()
        };
        let task = ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        };
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(7, &agent, &task, &tmp, true);
        let script = build_task_script(&ctx);

        // キュー方式ではエラー時にワーカーを止めない
        assert!(
            !script.contains("exec sleep infinity"),
            "タスクスクリプトは sleep infinity せず次タスクに進むべき: {script}"
        );
        assert!(!script.contains("touch {wdone}"));
        assert!(!script.contains("touch ") || !script.contains("worker-done"));
        // jsonl 分類呼び出し
        assert!(script.contains("classify-result"));
        assert!(script.contains("Error - continuing"));
        assert!(script.contains("PIPE_STATUS=(\"${PIPESTATUS[@]}\")"));
        assert!(script.contains("FORMAT_EXIT=${PIPE_STATUS[1]}"));
        assert!(script.contains("TEE_EXIT=${PIPE_STATUS[2]}"));
        assert!(script.contains("logging/classification pipeline failed"));
        assert!(script.contains("[ ! -s '/tmp/0007_repo.jsonl' ]"));
        // error マーカーは task idx 単位
        assert!(script.contains("/error-7"));
        assert!(script.contains("/failed-7"));
        assert!(script.contains("/retry-7"));
        assert!(script.contains("/done-7"));
        // state 書き込み(mark)失敗時は done を作らず failed 扱いにする（成功扱いを防ぐ）
        assert!(script.contains("state update failed"));
        assert!(script.contains("━━━ Error - state update failed ━━━"));
        // 成功パスは mark の終了コードで分岐する（elif <mark>; then touch done）
        assert!(script.contains("elif "));
    }

    #[test]
    fn build_task_script_resets_cancelled_in_rate_limited_and_retry_branches() {
        // SIGINT で CANCELLED=1 になった後、レート制限 (CLASS_CODE=2) や
        // リトライ可能エラー (CLASS_CODE=3) で終了したタスクは CANCELLED を
        // リセットしないと、後続タスクで誤って Cancelled と判定されてしまう。
        let agent = RuntimeAgent {
            name: "claude".to_string(),
            command: vec!["claude".to_string(), "-p".to_string()],
            ..Default::default()
        };
        let task = ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        };
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(1, &agent, &task, &tmp, true);
        let script = build_task_script(&ctx);

        // CLASS_CODE=2 (レート制限) ブランチで CANCELLED をリセットすること
        let rate_limited_idx = script
            .find("Rate limited - not marking as completed")
            .expect("rate limited branch missing");
        let preceding = &script[..rate_limited_idx];
        let last_case2 = preceding.rfind("  2)\n").expect("case 2 branch missing");
        assert!(
            script[last_case2..rate_limited_idx].contains("CANCELLED=0"),
            "CLASS_CODE=2 branch must reset CANCELLED:\n{}",
            &script[last_case2..rate_limited_idx]
        );

        // CLASS_CODE=3 (リトライ可能) ブランチで CANCELLED をリセットすること
        let retry_idx = script
            .find("Retryable error (will retry next run)")
            .expect("retry branch missing");
        let preceding = &script[..retry_idx];
        let last_case3 = preceding.rfind("  3)\n").expect("case 3 branch missing");
        assert!(
            script[last_case3..retry_idx].contains("CANCELLED=0"),
            "CLASS_CODE=3 branch must reset CANCELLED:\n{}",
            &script[last_case3..retry_idx]
        );
    }

    #[test]
    fn build_task_script_for_non_claude_skips_classify() {
        let agent = RuntimeAgent {
            name: "codex".to_string(),
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        let task = ResolvedTarget {
            directory: std::path::PathBuf::from("/tmp/repo"),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        };
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(2, &agent, &task, &tmp, false);
        let script = build_task_script(&ctx);

        assert!(!script.contains("classify-result"));
        assert!(!script.contains("exec sleep infinity"));
        assert!(script.contains("Error - continuing"));
        assert!(script.contains("TEE_EXIT=${PIPE_STATUS[1]}"));
        assert!(script.contains("logging pipeline failed"));
    }

    #[test]
    fn build_worker_script_consumes_queue_atomically() {
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            stop_file: &tmp.join("stop"),
            usage_gate_cmd: None,
        });

        assert!(script.contains("#!/bin/bash"));
        // mv でアトミック claim
        assert!(script.contains("mv \"$pending\" \"$QUEUE_DIR/claimed-$idx\""));
        // source で個別タスクを取り込む
        assert!(script.contains("source \"$TASK_SCRIPT\""));
        // ワーカー完了マーカー
        assert!(script.contains("worker-done-0"));
        // 停止シグナル対応
        assert!(script.contains("trap handle_cancel INT TERM"));
    }

    #[test]
    fn build_worker_script_runs_usage_gate_after_source() {
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            stop_file: &tmp.join("stop"),
            usage_gate_cmd: Some("tb usage-gate --profile P --provider claude"),
        });
        let source_idx = script
            .find("source \"$TASK_SCRIPT\"")
            .expect("source missing");
        let gate_idx = script
            .find("usage-gate --profile P")
            .expect("usage-gate line missing");
        let done_idx = script.find("\ndone\n").expect("loop end missing");
        // gate はタスク source の後、ループ末尾(done)の前に実行される
        assert!(source_idx < gate_idx && gate_idx < done_idx);
    }

    #[test]
    fn build_worker_script_breaks_loop_when_usage_gate_fails_closed() {
        // usage-gate が非ゼロ終了（fail-closed）したら、ワーカーはループを break して停止する。
        // 終了コードを握り潰すと、stop file を作れなかった fail-closed 時に後続タスクを
        // 処理し続けてしまうため。break 後はループ末尾で worker-done マーカーが作られる。
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            stop_file: &tmp.join("stop"),
            usage_gate_cmd: Some("tb usage-gate --profile P --provider claude"),
        });
        // 非ゼロ終了で break する分岐を持つこと
        assert!(
            script.contains("if ! tb usage-gate --profile P --provider claude; then"),
            "gate must guard on non-zero exit: {script}"
        );
        // gate の break はループ末尾の worker-done 生成より前にある（break 後に到達する）
        let gate_break = script
            .find("failed closed — stopping")
            .expect("gate break branch missing");
        let worker_done = script.find("worker-done-0").expect("worker-done missing");
        assert!(gate_break < worker_done);
    }

    #[test]
    fn build_worker_script_omits_usage_gate_when_unset() {
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            stop_file: &tmp.join("stop"),
            usage_gate_cmd: None,
        });
        assert!(!script.contains("usage-gate"));
    }

    #[test]
    fn build_worker_script_resets_cancelled_before_each_task() {
        // 各タスクを source する前に CANCELLED=0 をリセットすることで、直前タスクの
        // 実行中に SIGINT/SIGTERM を受けて立った Cancelled フラグが後続タスクへ漏れ、
        // 通常のエラーを誤って「Cancelled」と判定するのを防ぐ。
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            stop_file: &tmp.join("stop"),
            usage_gate_cmd: None,
        });

        // while ループ本体（task を source する前）で CANCELLED をリセットすること
        let loop_start = script.find("while true; do").expect("worker loop missing");
        let source_idx = script
            .find("source \"$TASK_SCRIPT\"")
            .expect("source missing");
        let loop_body = &script[loop_start..source_idx];
        assert!(
            loop_body.contains("CANCELLED=0"),
            "worker loop must reset CANCELLED before sourcing each task:\n{}",
            loop_body
        );
    }

    #[test]
    fn build_worker_script_escapes_paths_with_spaces() {
        let script = build_worker_script(&WorkerCtx {
            worker_id: 1,
            queue_dir: std::path::Path::new("/tmp/my queue"),
            task_dir: std::path::Path::new("/tmp/my tasks"),
            marker_dir: std::path::Path::new("/tmp/my markers"),
            stop_file: std::path::Path::new("/tmp/my stop"),
            usage_gate_cmd: None,
        });
        assert!(script.contains("QUEUE_DIR='/tmp/my queue'"));
        assert!(script.contains("TASK_DIR='/tmp/my tasks'"));
        assert!(script.contains("'/tmp/my stop'"));
        assert!(script.contains("'/tmp/my markers/worker-done-1'"));
    }

    #[test]
    fn generate_monitor_script_handles_failed_markers_and_escapes_values() {
        let script = generate_monitor_script(
            "ag\"$(touch /tmp/pwn)\"",
            "claude -p",
            "2026/02/24 09:00",
            2,
            60,
            std::path::Path::new("/tmp/marker dir"),
            "token-burn",
            1,
            std::path::Path::new("/tmp/stop file"),
            std::path::Path::new("/tmp/report dir"),
            None,
            None,
            None,
        );

        assert!(script.contains("AGENT='ag\"$(touch /tmp/pwn)\"'"));
        assert!(script.contains("FAILED=$(find \"$MARKER_DIR\" -name 'failed-*'"));
        assert!(script.contains("RETRY=$(find \"$MARKER_DIR\" -name 'retry-*'"));
        assert!(script.contains("PROCESSED=$((DONE + FAILED + RETRY))"));
        assert!(script.contains("Completed: %d succeeded / %d failed / %d retry"));
        assert!(script.contains("fail:%d retry:%d"));
        // 全体再描画方式: render 関数とマーカー由来のエラー表示を持つ
        assert!(script.contains("render() {"));
        assert!(script.contains(r" ❌ %s\n"));
        // ai-usage 連携なしのときは AI_USAGE_CMD / REFRESH_CMD / CACHE_FILE は空文字列
        assert!(script.contains("AI_USAGE_CMD=''"));
        assert!(script.contains("AI_USAGE_REFRESH_CMD=''"));
        assert!(script.contains("AI_USAGE_CACHE_FILE=''"));
        // 進捗バーは BSD seq(macOS) の "seq 1 0 → 1 0" 問題を避けるため算術ループで描画する
        assert!(script.contains("while [ $i -lt $FILLED ]"));
        assert!(script.contains("while [ $i -lt $EMPTY ]"));
        assert!(!script.contains("seq 1 $FILLED"));
        // 完了/停止後も端末状態（カーソル/autowrap）を復元する。
        assert!(script.contains("restore_terminal() {"));
        assert!(script.contains("trap restore_terminal EXIT"));
        // 完了時ブロック・停止時ブロックのいずれも Ctrl-C 待ちせず、finish_session で
        // セッションごと閉じて token-burn を終了する（どちらも 8 スペースインデント）。
        assert_eq!(
            script.matches("\n        finish_session\n").count(),
            2,
            "both completion and stopped blocks must close the session: {script}"
        );
        assert!(script.contains("finish_session() {"));
        assert!(
            script.contains("    restore_terminal\n    tmux kill-session -t \"$SESSION\" 2>/dev/null\n    exit 0"),
            "finish_session must restore the terminal, kill the session, then exit: {script}"
        );
        // Ctrl-C 待ちの案内と無限待機は残さない。残っていると自動終了しなくなる。
        assert!(
            !script.contains("Press Ctrl-C to close session"),
            "monitor must not wait for Ctrl-C after finishing: {script}"
        );
        assert!(
            !script.contains("sleep 3600"),
            "monitor must not park in a sleep loop after finishing: {script}"
        );
        // macOS の BSD sleep は `infinity` を受け付けず usage エラーで即終了するため、
        // 待機に使うと成立しない（過去の回帰を検知するための残置）。
        assert!(
            !script.contains("sleep infinity"),
            "monitor must not rely on `sleep infinity` (unsupported by BSD sleep): {script}"
        );
        // ワーカーがタスクを消化し切るたびペインが閉じ、モニターペインが広がる。
        // SIGWINCH で描き直さないと旧い幅で折り返した行が残る。ハンドラから直接 render を
        // 呼ぶと進行中の render に割り込んで出力が混ざるため、フラグ経由で合流させる。
        assert!(
            script.contains("trap 'RESIZED=1' WINCH"),
            "monitor must flag pane resizes instead of rendering inside the handler: {script}"
        );
        assert!(
            script
                .contains("if [ $RESIZED -eq 1 ]; then\n        RESIZED=0\n        NEED_RENDER=1"),
            "monitor must convert the resize flag into a full redraw: {script}"
        );
    }

    #[test]
    fn build_worker_script_exits_after_consuming_queue() {
        // タスクを消化し切ったワーカーはそのまま終了してペインを閉じる。待機ループを
        // 残すと空のペインが画面を占め続ける（ログは run_dir 側に残るので読み返せる）。
        let script = build_worker_script(&WorkerCtx {
            worker_id: 1,
            queue_dir: std::path::Path::new("/tmp/queue"),
            task_dir: std::path::Path::new("/tmp/tasks"),
            marker_dir: std::path::Path::new("/tmp/markers"),
            stop_file: std::path::Path::new("/tmp/stop"),
            usage_gate_cmd: None,
        });
        assert!(
            !script.contains("sleep infinity"),
            "worker must not rely on `sleep infinity`: {script}"
        );
        assert!(
            !script.contains("sleep 3600"),
            "worker must not park in a sleep loop after finishing: {script}"
        );
        // worker-done マーカーは終了前に必ず作る（モニターの停止判定がこれを見る）。
        assert!(
            script.contains("touch '/tmp/markers/worker-done-1'\n"),
            "worker must create its done marker before exiting: {script}"
        );
        // 終了直前にキャンセル trap を外し、その直前に届いた INT/TERM でも直前タスクの
        // failed マーカーを立て直さないよう参照先を空にしてから抜ける。
        assert!(
            script.contains("CURRENT_FAILED_MARKER=\"\"\ntrap - INT TERM\nexit 0\n"),
            "worker must clear the pending marker and cancel trap before exiting: {script}"
        );
        assert_valid_bash(&script, "worker script exit path");
    }

    #[test]
    fn generate_monitor_script_reads_worker_done_before_task_markers() {
        // worker-done はワーカーが自分のタスクマーカーを書き切った後に作られる。
        // 先に worker-done を読むことで「worker 全滅は見えているがタスクマーカーが
        // 古い」状態を防ぎ、全件成功を「⏹ Stopped: 9/10」と誤報告しないようにする。
        let script = monitor_script_for_test();
        let workers_done = script
            .find("WORKERS_DONE=$(find")
            .expect("WORKERS_DONE の取得が必要");
        let done = script
            .find("DONE=$(find \"$MARKER_DIR\" -name 'done-*'")
            .expect("DONE の取得が必要");
        assert!(
            workers_done < done,
            "WORKERS_DONE はタスクマーカーより先に読むべき: {script}"
        );
        // 再取得は残さない（古い値と新しい値の混在を避ける）。
        assert_eq!(
            script.matches("WORKERS_DONE=$(find").count(),
            1,
            "WORKERS_DONE の取得は 1 箇所のみ: {script}"
        );
    }

    #[test]
    fn generate_monitor_script_records_fetch_completion_time() {
        // スロットルは fetch 開始時刻ではなく完了時刻を基準にする。開始時刻だと
        // fetch が 10 秒以上かかったとき次の周回で即座に再取得してしまう。
        let script = monitor_script_for_test();
        assert!(
            !script.contains("LAST_USAGE=$NOW"),
            "fetch 開始時刻を記録してはいけない: {script}"
        );
        assert_eq!(
            script.matches("LAST_USAGE=$(date +%s)").count(),
            2,
            "初回 fetch 直後とループ内 fetch 直後の 2 箇所で完了時刻を記録する: {script}"
        );
    }

    #[test]
    fn generate_monitor_script_transitions_to_stopped_when_stop_file_appears() {
        // usage-gate / rate_limit_event がワーカー側で stop file を作った場合も、モニターが
        // STOPPED に遷移して全ワーカー完了判定を発火させること。これが無いとデッドラインまで
        // ハングする。
        let script = generate_monitor_script(
            "claude",
            "claude -p",
            "2026/02/24 09:00",
            2,
            60,
            std::path::Path::new("/tmp/markers"),
            "token-burn",
            2,
            std::path::Path::new("/tmp/stop"),
            std::path::Path::new("/tmp/report"),
            None,
            None,
            None,
        );
        // stop file を検出して STOPPED=1 に遷移する分岐を持つこと（STATUS_MSG 表示用）
        assert!(
            script.contains("[ $STOPPED -eq 0 ] && [ -f \"$STOP_FILE\" ]"),
            "monitor must observe stop file created by workers: {script}"
        );
        // 全ワーカー終了判定は STOPPED に依存しない独立した終了条件であること。
        // これが STOPPED の内側に閉じ込められていると、usage-gate が stop file を作れずに
        // fail-closed した場合にモニターがデッドラインまでハングする。
        assert!(
            !script.contains("if [ $STOPPED -eq 1 ]; then"),
            "workers-done check must not be gated on STOPPED: {script}"
        );
        assert!(script.contains("WORKERS_DONE=$(find \"$MARKER_DIR\" -name 'worker-done-*'"));
        assert!(script.contains("if [ \"$WORKERS_DONE\" -ge \"$WORKER_COUNT\" ]; then"));
    }

    #[test]
    fn generate_monitor_script_includes_statusline_when_enabled() {
        let cache = std::path::Path::new("/tmp/cache.json");
        let script = generate_monitor_script(
            "claude",
            "claude",
            "2026/02/24 09:00",
            1,
            60,
            std::path::Path::new("/tmp/markers"),
            "token-burn",
            1,
            std::path::Path::new("/tmp/stop"),
            std::path::Path::new("/tmp/report"),
            Some("'ai-usage' --statusline --logos --input '/tmp/cache.json'"),
            Some("'ai-usage' '--json'"),
            Some(cache),
        );
        // statusline コマンドが AI_USAGE_CMD に埋め込まれ、10秒ごとに再取得・再描画する
        assert!(script.contains("AI_USAGE_CMD="));
        assert!(script.contains("--statusline"));
        assert!(script.contains("fetch_usage"));
        assert!(script.contains("$((NOW - LAST_USAGE)) -ge 10"));
        assert!(script.contains("AI Usage:"));
        // refresh コマンドが atomic にキャッシュを更新するロジックが入っていること。
        assert!(script.contains("AI_USAGE_REFRESH_CMD="));
        assert!(script.contains("AI_USAGE_CACHE_FILE="));
        assert!(script.contains("> \"$AI_USAGE_CACHE_FILE.tmp\""));
        assert!(
            script.contains("mv \"$AI_USAGE_CACHE_FILE.tmp\" \"$AI_USAGE_CACHE_FILE\""),
            "refresh must atomically swap cache via mv"
        );
        // fetch_usage は必ずタイムアウトラッパー経由で ai-usage を起動する
        // （ai-usage がハングしても monitor ループが固まらないため）。
        assert!(
            script.contains("run_with_timeout() {"),
            "monitor script must define run_with_timeout"
        );
        assert!(
            script.contains("AI_USAGE_TIMEOUT="),
            "monitor script must set AI_USAGE_TIMEOUT"
        );
        assert!(
            script.contains(
                "run_with_timeout \"$AI_USAGE_TIMEOUT\" bash -c \"$AI_USAGE_REFRESH_CMD\""
            ),
            "refresh must be wrapped in run_with_timeout"
        );
        assert!(
            script.contains("run_with_timeout \"$AI_USAGE_TIMEOUT\" bash -c \"$AI_USAGE_CMD\""),
            "statusline fetch must be wrapped in run_with_timeout"
        );
    }

    #[test]
    fn generate_monitor_script_is_valid_bash() {
        // statusline あり/なしの両方で、生成されるスクリプトが bash 構文として妥当なこと。
        let cache = std::path::Path::new("/tmp/c.json");
        let cases: [(Option<&str>, Option<&str>, Option<&std::path::Path>); 2] = [
            (None, None, None),
            (
                Some("'ai-usage' --statusline --logos --input '/tmp/c.json'"),
                Some("'ai-usage' '--json'"),
                Some(cache),
            ),
        ];
        for (statusline, refresh, cache_path) in cases {
            let script = generate_monitor_script(
                "claude",
                "claude --model opus",
                "2026/02/24 09:00",
                3,
                3600,
                std::path::Path::new("/tmp/markers"),
                "token-burn",
                2,
                std::path::Path::new("/tmp/stop"),
                std::path::Path::new("/tmp/report"),
                statusline,
                refresh,
                cache_path,
            );
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("monitor.sh");
            std::fs::write(&path, &script).unwrap();
            let status = std::process::Command::new("bash")
                .arg("-n")
                .arg(&path)
                .status()
                .expect("bash should be available");
            assert!(
                status.success(),
                "monitor script must be valid bash (statusline={})",
                statusline.is_some()
            );
        }
    }

    #[test]
    fn build_refresh_cmd_includes_env_prefix_and_command() {
        // env を前置きしつつ、ai-usage --json をそのまま実行できる文字列になる。
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/home/u/.claude".to_string(),
        );
        let cmd = build_refresh_cmd(&["ai-usage".to_string(), "--json".to_string()], &env)
            .expect("non-empty command");
        assert_eq!(
            cmd,
            "CLAUDE_CONFIG_DIR='/home/u/.claude' 'ai-usage' '--json'"
        );
    }

    #[test]
    fn build_refresh_cmd_none_for_empty_command() {
        assert!(build_refresh_cmd(&[], &std::collections::BTreeMap::new()).is_none());
    }

    #[test]
    fn progress_bar_loop_produces_exact_width_at_boundaries() {
        // 進捗バーの算術ループが FILLED/EMPTY=0（0%/100%）でも正しい幅になること。
        // BSD seq は `seq 1 0` が "1 0" を出すため、旧実装では 0%/100% で2文字ずれていた。
        let script = r#"
            for case in "0 20" "20 0" "10 10"; do
                set -- $case
                FILLED=$1; EMPTY=$2
                BAR=""
                i=0; while [ $i -lt $FILLED ]; do BAR="${BAR}X"; i=$((i+1)); done
                i=0; while [ $i -lt $EMPTY ]; do BAR="${BAR}Y"; i=$((i+1)); done
                echo "${#BAR}"
            done
        "#;
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(script)
            .output()
            .expect("bash should be available");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let widths: Vec<&str> = stdout.lines().collect();
        // 0%、100%、50% いずれもバー幅はちょうど 20。
        assert_eq!(widths, vec!["20", "20", "20"]);
    }

    #[test]
    fn build_statusline_cmd_replaces_json_in_default_command() {
        let cmd = build_statusline_cmd(
            &["ai-usage".to_string(), "--json".to_string()],
            std::path::Path::new("/tmp/cache.json"),
            &std::collections::BTreeMap::new(),
            "Work",
            "claude",
        )
        .expect("non-empty command");
        assert_eq!(
            cmd,
            "'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/cache.json' '--active-profile' 'Work' '--active-provider' 'claude'"
        );
    }

    #[test]
    fn build_statusline_cmd_preserves_wrapper_prefix() {
        // `env FOO=1 ai-usage --json` のようなラッパー構成でも先頭要素を捨てず保持する
        // （以前は command.first() のみ使い env が実行ファイル扱いされて壊れていた）。
        let cmd = build_statusline_cmd(
            &[
                "env".to_string(),
                "FOO=1".to_string(),
                "ai-usage".to_string(),
                "--json".to_string(),
            ],
            std::path::Path::new("/tmp/c.json"),
            &std::collections::BTreeMap::new(),
            "Home",
            "codex",
        )
        .expect("non-empty command");
        assert_eq!(
            cmd,
            "'env' 'FOO=1' 'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/c.json' '--active-profile' 'Home' '--active-provider' 'codex'"
        );
    }

    #[test]
    fn build_statusline_cmd_appends_when_no_json_flag() {
        let cmd = build_statusline_cmd(
            &["ai-usage".to_string()],
            std::path::Path::new("/tmp/c.json"),
            &std::collections::BTreeMap::new(),
            "Work",
            "claude",
        )
        .expect("non-empty command");
        assert_eq!(
            cmd,
            "'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/c.json' '--active-profile' 'Work' '--active-provider' 'claude'"
        );
    }

    #[test]
    fn build_statusline_cmd_none_for_empty_command() {
        assert!(
            build_statusline_cmd(
                &[],
                std::path::Path::new("/tmp/c.json"),
                &std::collections::BTreeMap::new(),
                "Work",
                "claude",
            )
            .is_none()
        );
    }

    #[test]
    fn build_statusline_cmd_prepends_agent_env() {
        // 選択 agent の env（CLAUDE_CONFIG_DIR 等）が `KEY='val'` 形式で前置きされ、
        // monitor の statusline が実行中アカウントの実行文脈で動くこと。
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/home/u/.claude".to_string(),
        );
        let cmd = build_statusline_cmd(
            &["ai-usage".to_string(), "--json".to_string()],
            std::path::Path::new("/tmp/c.json"),
            &env,
            "Work",
            "claude",
        )
        .expect("non-empty command");
        assert_eq!(
            cmd,
            "CLAUDE_CONFIG_DIR='/home/u/.claude' 'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/c.json' '--active-profile' 'Work' '--active-provider' 'claude'"
        );
    }

    #[test]
    fn build_statusline_cmd_unsets_empty_env_value() {
        // 空文字値は `env -u KEY` で除去し、statusline 側にも CLAUDE_CONFIG_DIR="" を
        // 渡さない。空文字を渡すと ai-usage / claude が CWD 相対の設定ディレクトリと
        // 解釈する余地が残るため。
        let mut env = std::collections::BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), String::new());
        env.insert("CODEX_HOME".to_string(), "/home/u/.codex".to_string());
        let cmd = build_statusline_cmd(
            &["ai-usage".to_string(), "--json".to_string()],
            std::path::Path::new("/tmp/c.json"),
            &env,
            "Home",
            "codex",
        )
        .expect("non-empty command");
        assert_eq!(
            cmd,
            "env -u CLAUDE_CONFIG_DIR CODEX_HOME='/home/u/.codex' 'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/c.json' '--active-profile' 'Home' '--active-provider' 'codex'"
        );
    }

    #[test]
    fn env_prefix_parts_empty_value_becomes_unset() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("FOO".to_string(), String::new());
        env.insert("BAR".to_string(), "v".to_string());
        let parts = env_prefix_parts(&env);
        // BTreeMap の決定的な順序 (BAR, FOO) に従って、unset は先頭の env コマンドに集約される
        assert_eq!(parts, vec!["env", "-u", "FOO", "BAR='v'"]);
    }

    #[test]
    fn env_prefix_parts_all_set_keeps_legacy_form() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("A".to_string(), "1".to_string());
        env.insert("B".to_string(), "2".to_string());
        let parts = env_prefix_parts(&env);
        // 全て非空なら従来通り `K='v'` のみで env コマンドは付かない（後方互換）
        assert_eq!(parts, vec!["A='1'", "B='2'"]);
    }

    #[test]
    fn env_prefix_parts_empty_map_is_empty() {
        let env = std::collections::BTreeMap::new();
        let parts = env_prefix_parts(&env);
        assert!(parts.is_empty());
    }

    #[test]
    fn shell_escape_escapes_single_quotes() {
        assert_eq!(shell_escape("a'b"), "'a'\\''b'");
    }

    #[test]
    fn build_shell_command_escapes_paths() {
        let cmd = vec!["claude".to_string(), "-p".to_string()];
        let env = std::collections::BTreeMap::new();
        let prompt = std::path::Path::new("/tmp/prompt.txt");
        let result = build_shell_command(&cmd, &env, prompt);
        assert!(result.contains("'claude' '-p'"));
        assert!(result.contains("$(cat '/tmp/prompt.txt')"));
    }

    #[test]
    fn build_shell_command_prepends_env_vars() {
        let cmd = vec!["claude".to_string(), "-p".to_string()];
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/home/user/.config/work".to_string(),
        );
        let prompt = std::path::Path::new("/tmp/prompt.txt");
        let result = build_shell_command(&cmd, &env, prompt);
        // env は cmd の直前に KEY='val' 形式で前置される
        assert!(
            result.contains("CLAUDE_CONFIG_DIR='/home/user/.config/work' 'claude' '-p'"),
            "got: {result}"
        );
    }

    #[test]
    fn build_shell_command_without_env_has_no_prefix() {
        let cmd = vec!["codex".to_string()];
        let env = std::collections::BTreeMap::new();
        let prompt = std::path::Path::new("/tmp/p.txt");
        let result = build_shell_command(&cmd, &env, prompt);
        assert!(result.starts_with("'codex' \"$(cat"));
    }

    #[test]
    fn build_shell_command_unsets_empty_env_value() {
        // 値が空文字の env キーは `env -u KEY` 形式で子プロセスから完全に除去する。
        // bash の `KEY= cmd` だと KEY を空文字に設定するため、claude code が
        // CLAUDE_CONFIG_DIR="" を CWD 相対の設定ディレクトリと解釈し、対象プロジェクト
        // 直下に projects/ sessions/ backups/ をぶちまける問題があった（owa profile）。
        let cmd = vec!["claude".to_string(), "-p".to_string()];
        let mut env = std::collections::BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), String::new());
        env.insert("CODEX_HOME".to_string(), "/home/u/.codex".to_string());
        let prompt = std::path::Path::new("/tmp/prompt.txt");
        let result = build_shell_command(&cmd, &env, prompt);
        assert!(
            result.contains("env -u CLAUDE_CONFIG_DIR CODEX_HOME='/home/u/.codex' 'claude' '-p'"),
            "got: {result}"
        );
    }

    #[test]
    fn build_shell_command_unsets_all_empty_env_values() {
        // 全ての env 値が空文字でも、`env -u K1 -u K2 cmd ...` で正しく unset される。
        let cmd = vec!["claude".to_string(), "-p".to_string()];
        let mut env = std::collections::BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), String::new());
        env.insert("CODEX_HOME".to_string(), String::new());
        let prompt = std::path::Path::new("/tmp/prompt.txt");
        let result = build_shell_command(&cmd, &env, prompt);
        assert!(
            result.contains("env -u CLAUDE_CONFIG_DIR -u CODEX_HOME 'claude' '-p'"),
            "got: {result}"
        );
    }

    #[test]
    fn build_shell_command_has_no_cd_prefix() {
        // cd は build_task_script 側で `cd ... || {{ ... }}` として別行に出す。
        // パイプラインと && で繋ぐと cd 失敗時に PIPESTATUS の要素数が変わるため。
        let cmd = build_shell_command(
            &["claude".to_string(), "-p".to_string()],
            &std::collections::BTreeMap::new(),
            std::path::Path::new("/tmp/prompt.txt"),
        );
        assert!(!cmd.contains("cd "), "got: {cmd}");
        assert!(cmd.contains("$(cat '/tmp/prompt.txt')"));
        assert!(cmd.contains("'claude' '-p'"));
    }

    // ───────────────────────── 回帰テスト用ヘルパー ─────────────────────────

    /// bash が無い環境ではスクリプト実行系テストをスキップするための判定。
    fn bash_available() -> bool {
        std::process::Command::new("bash")
            .args(["-c", "exit 0"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// テスト用の monitor スクリプト（ai-usage 連携あり）を生成する。
    fn monitor_script_for_test() -> String {
        generate_monitor_script(
            "claude",
            "claude -p",
            "2026/02/24 09:00",
            1,
            60,
            std::path::Path::new("/tmp/markers"),
            "token-burn",
            1,
            std::path::Path::new("/tmp/stop"),
            std::path::Path::new("/tmp/report"),
            Some("'ai-usage' --statusline --logos --input '/tmp/cache.json'"),
            Some("'ai-usage' '--json'"),
            Some(std::path::Path::new("/tmp/cache.json")),
        )
    }

    /// 生成済み monitor スクリプトから `run_with_timeout` の関数定義だけを切り出す。
    /// monitor 本体は無限ループなのでそのままは source できないため、実際に生成された
    /// 定義を実行して挙動を実測できるようにする。
    fn extract_run_with_timeout(script: &str) -> String {
        let start = script
            .find("run_with_timeout() {")
            .expect("run_with_timeout definition missing");
        let rest = &script[start..];
        let end = rest
            .find("\n}\n")
            .expect("run_with_timeout definition must be closed");
        rest[..end + 3].to_string()
    }

    fn target_for_test(directory: &str) -> ResolvedTarget {
        ResolvedTarget {
            directory: std::path::PathBuf::from(directory),
            display_name: "repo".to_string(),
            prompt: "review".to_string(),
            visibility: Visibility::Public,
            defer: false,
        }
    }

    fn agent_for_test(name: &str, command: &[&str]) -> RuntimeAgent {
        RuntimeAgent {
            name: name.to_string(),
            command: command.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// bash -n で構文チェックする（`bash` が無い環境では何もしない）。
    fn assert_valid_bash(script: &str, label: &str) {
        if !bash_available() {
            return;
        }
        let dir = tempfile::TempDir::new().expect("temp dir should be created");
        let path = dir.path().join("script.sh");
        std::fs::write(&path, script).expect("script should be written");
        let status = std::process::Command::new("bash")
            .arg("-n")
            .arg(&path)
            .status()
            .expect("bash -n should run");
        assert!(status.success(), "{label} must be valid bash:\n{script}");
    }

    // ─────────── バグ1: watchdog サブシェルが stdout を継承して固まる ───────────

    #[test]
    fn generate_monitor_script_redirects_watchdog_subshell_output() {
        // watchdog サブシェルが呼び出し側の stdout を継承していると、コマンドが即座に
        // 終わってもサブシェルの子 `sleep $secs` がコマンド置換 `$( )` のパイプ書き込み端を
        // 握ったまま孤児化し（`kill -TERM $wpid` はサブシェル本体しか殺せない）、
        // EOF 待ちで timeout 秒まるごとブロックする。monitor の再描画が 10 秒ごとに
        // AI_USAGE_TIMEOUT 秒固まる回帰を止める。
        let script = monitor_script_for_test();
        assert!(
            script.contains("kill -KILL $cpid 2>/dev/null ) >/dev/null 2>&1 &"),
            "watchdog subshell must discard inherited stdout/stderr: {script}"
        );
        // リダイレクト無しの旧形（`) &` で終わる）が残っていないこと
        assert!(
            !script.contains("kill -KILL $cpid 2>/dev/null ) &"),
            "watchdog subshell must not inherit caller stdout: {script}"
        );
    }

    #[test]
    fn run_with_timeout_returns_immediately_for_fast_command() {
        // 生成された `run_with_timeout` を実際に source し、実運用と同じ
        // `new=$(run_with_timeout N bash -c "$CMD")` の形で即座に終わるコマンドを
        // 包んでも待たされないことを実測する。watchdog が stdout を継承していた頃は
        // ここで指定秒（下記なら 6 秒）まるごとブロックしていた。
        if !bash_available() {
            return;
        }
        let fragment = extract_run_with_timeout(&monitor_script_for_test());
        let harness = format!(
            "{fragment}\nout=$(run_with_timeout 6 bash -c 'printf ready')\nprintf '%s' \"$out\"\n"
        );

        let start = std::time::Instant::now();
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&harness)
            .output()
            .expect("bash should run");
        let elapsed = start.elapsed();

        assert!(
            output.status.success(),
            "harness must succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "ready");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "fast command must not wait for the timeout (took {elapsed:?})"
        );
    }

    #[test]
    fn run_with_timeout_kills_hanging_command_at_deadline() {
        // ハング側の契約も維持されていること（stdout を捨てても timeout は効く）。
        // 2 秒指定でハングするコマンドを打ち切り、出力は空で返る。
        if !bash_available() {
            return;
        }
        let fragment = extract_run_with_timeout(&monitor_script_for_test());
        let harness =
            format!("{fragment}\nout=$(run_with_timeout 2 sleep 30)\nprintf '%s' \"$out\"\n");

        let start = std::time::Instant::now();
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&harness)
            .output()
            .expect("bash should run");
        let elapsed = start.elapsed();

        assert!(
            output.stdout.is_empty(),
            "killed command must produce no output: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            elapsed >= std::time::Duration::from_secs(1),
            "timeout must actually wait for the deadline (took {elapsed:?})"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(15),
            "hanging command must be killed at the deadline (took {elapsed:?})"
        );
    }

    // ─────────── バグ2: cd 失敗で PIPESTATUS の要素数が変わる ───────────

    #[test]
    fn build_task_script_guards_cd_failure_for_claude() {
        // `cd X && cmd | fmt | tee` は bash が `cd && (3要素パイプライン)` と解釈するため、
        // cd 失敗時に PIPESTATUS が cd の 1 要素だけになり、FORMAT_EXIT / TEE_EXIT が
        // 空文字へ展開されて `[ "" -ne 0 ]` が "integer expression expected" を吐き、
        // 記録されるエラーも真因と無関係な「logging pipeline failed」になっていた。
        // cd は必ずパイプラインと分離し、失敗時は専用メッセージで failed 扱いにする。
        let agent = agent_for_test("claude", &["claude", "-p"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(3, &agent, &task, &tmp, true);
        let script = build_task_script(&ctx);

        assert!(
            script.contains("cd '/tmp/repo' || {"),
            "cd はガード付きの独立文であるべき: {script}"
        );
        assert!(
            !script.contains("cd '/tmp/repo' &&"),
            "cd をパイプラインへ && で連結してはいけない: {script}"
        );
        assert!(script.contains("target directory is unavailable"));
        // ガードは失敗マーカーを作り、ワーカーは次タスクへ進む
        assert!(script.contains("touch '/tmp/failed-3'"));
        assert!(
            script.contains(
                "printf '%starget directory is unavailable\\n' '[repo] ' > '/tmp/error-3'"
            )
        );
        // ガードはパイプラインより前に出る
        let cd_idx = script
            .find("cd '/tmp/repo' || {")
            .expect("cd guard missing");
        let pipe_idx = script.find("| tee ").expect("pipeline missing");
        assert!(cd_idx < pipe_idx, "cd ガードはパイプラインの手前に出すべき");
    }

    #[test]
    fn build_task_script_guards_cd_failure_for_non_claude() {
        // 非 claude（codex 等）の 2 要素パイプラインでも同じガードが必要。
        let agent = agent_for_test("codex", &["codex", "exec"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(4, &agent, &task, &tmp, false);
        let script = build_task_script(&ctx);

        assert!(
            script.contains("cd '/tmp/repo' || {"),
            "cd はガード付きの独立文であるべき: {script}"
        );
        assert!(
            !script.contains("cd '/tmp/repo' &&"),
            "cd をパイプラインへ && で連結してはいけない: {script}"
        );
        assert!(script.contains("target directory is unavailable"));
        assert!(script.contains("touch '/tmp/failed-4'"));
        let cd_idx = script
            .find("cd '/tmp/repo' || {")
            .expect("cd guard missing");
        let pipe_idx = script.find("| tee ").expect("pipeline missing");
        assert!(cd_idx < pipe_idx, "cd ガードはパイプラインの手前に出すべき");
    }

    #[test]
    fn build_task_script_cd_guard_escapes_special_characters_in_directory() {
        // ディレクトリ名に空白やシングルクォートが混ざっても cd ガードが壊れないこと。
        // エスケープが崩れると cd 行が構文エラーになり、ガード自体が機能しなくなる。
        let agent = agent_for_test("claude", &["claude", "-p"]);
        let task = target_for_test("/tmp/re po's dir");
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(5, &agent, &task, &tmp, true);
        let script = build_task_script(&ctx);

        assert!(
            script.contains("cd '/tmp/re po'\\''s dir' || {"),
            "cd の引数はシェルエスケープされるべき: {script}"
        );
        assert_valid_bash(&script, "task script with quoted directory");
    }

    #[test]
    fn build_task_script_is_valid_bash() {
        // cd ガードを差し込んだ後も、claude / 非 claude 双方のタスクスクリプトが
        // bash 構文として妥当であること（ワーカーは source して実行する）。
        let claude = agent_for_test("claude", &["claude", "-p"]);
        let codex = agent_for_test("codex", &["codex", "exec"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");

        for (agent, is_claude, label) in [(&claude, true, "claude"), (&codex, false, "non-claude")]
        {
            let ctx = task_ctx_for_test(1, agent, &task, &tmp, is_claude);
            let script = build_task_script(&ctx);
            assert_valid_bash(&script, label);
        }
    }

    #[test]
    fn build_worker_script_is_valid_bash() {
        // usage-gate の有無どちらでもワーカースクリプトが bash 構文として妥当であること。
        let tmp = std::path::PathBuf::from("/tmp/burn");
        for gate in [None, Some("tb usage-gate --profile P --provider claude")] {
            let script = build_worker_script(&WorkerCtx {
                worker_id: 0,
                queue_dir: &tmp.join("queue"),
                task_dir: &tmp.join("tasks"),
                marker_dir: &tmp.join("markers"),
                stop_file: &tmp.join("stop"),
                usage_gate_cmd: gate,
            });
            assert_valid_bash(&script, "worker script");
        }
    }

    #[test]
    fn build_shell_command_never_prefixes_cd_regardless_of_env() {
        // env の有無・空文字 unset のいずれの経路でも cd が混ざらないこと。
        // cd が戻ると PIPESTATUS の要素数が変わる回帰が再発する。
        let cmd = vec!["claude".to_string(), "-p".to_string()];
        let prompt = std::path::Path::new("/tmp/prompt.txt");

        let mut set_env = std::collections::BTreeMap::new();
        set_env.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/home/u/.claude".to_string(),
        );
        let mut unset_env = std::collections::BTreeMap::new();
        unset_env.insert("CLAUDE_CONFIG_DIR".to_string(), String::new());

        for env in [
            std::collections::BTreeMap::new(),
            set_env.clone(),
            unset_env.clone(),
        ] {
            let result = build_shell_command(&cmd, &env, prompt);
            assert!(!result.contains("cd "), "got: {result}");
            assert!(!result.contains("&&"), "got: {result}");
        }

        // env 前置きの形自体は従来どおり保たれる
        assert_eq!(
            build_shell_command(&cmd, &std::collections::BTreeMap::new(), prompt),
            "'claude' '-p' \"$(cat '/tmp/prompt.txt')\""
        );
        assert_eq!(
            build_shell_command(&cmd, &set_env, prompt),
            "CLAUDE_CONFIG_DIR='/home/u/.claude' 'claude' '-p' \"$(cat '/tmp/prompt.txt')\""
        );
        assert_eq!(
            build_shell_command(&cmd, &unset_env, prompt),
            "env -u CLAUDE_CONFIG_DIR 'claude' '-p' \"$(cat '/tmp/prompt.txt')\""
        );
    }
}
