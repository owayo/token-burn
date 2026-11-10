use std::path::Path;

use crate::config::RuntimeAgent;
use crate::resume::ResumeEntry;
use crate::scanner::ResolvedTarget;

use super::OutputPipeline;
use super::util::task_log_base;

/// エージェントのコマンドが「エージェントを起動する前に一時的な理由で諦めた」ことを
/// 知らせる終了コード（sysexits.h の EX_TEMPFAIL）。
///
/// `claude` を起動する前に設定を同期するラッパースクリプトのように、起動の前段で失敗し得る
/// コマンドとの約束。同期の衝突やロック待ちのタイムアウトで 75 を返したら、エージェントは
/// 1 ターンも動いていない。そこでタスクを失敗（failed-N）ではなく、次の実行で処理し直す
/// retry-N とし、再開したセッションの失敗回数（`resume-entry fail`）にも数えない。
/// 数えると、セッションと無関係な起動の失敗だけで、保存済みのセッションが捨てられる。
/// 実例では、2 つのワーカーが同じ秒に起動してラッパーの同期が衝突し、`claude` が起動する
/// 前に落ちただけで、2 件のセッションが失敗回数 2 に達した。
///
/// Claude Code 自身は 75 で終わらないため、result を残さないクラッシュ（transcript の
/// 読み込みで落ちる等）と取り違えない。それ以外の非ゼロ終了は従来どおり失敗として数える。
const LAUNCH_TEMPFAIL_EXIT: i32 = 75;

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
    /// エージェントの stdin と出力の扱い（`build_plan` がフラグの付与と同時に決めたもの）。
    pub(super) output: OutputPipeline,
    /// レート制限で中断したらセッションを保存し、次回の実行で再開できるようにする。
    pub(super) save_interrupted: bool,
    /// 保存済みの中断セッションを再開するタスクなら、その記録と継続プロンプト。
    pub(super) resume: Option<TaskResume<'a>>,
    /// 保存済みの記録を使わずに新しいセッションで始めるなら、その古い session_id。
    /// 新しいセッションを起動する直前に破棄する。
    pub(super) discard_session: Option<&'a str>,
}

/// 中断セッションを再開するタスクの情報。
pub(super) struct TaskResume<'a> {
    pub(super) entry: &'a ResumeEntry,
    /// 継続プロンプトのファイル（元の指示は会話履歴に残っているので、元のプロンプトは送らない）。
    pub(super) prompt_file: &'a Path,
}

pub(super) struct WorkerCtx<'a> {
    pub(super) worker_id: usize,
    pub(super) queue_dir: &'a Path,
    pub(super) task_dir: &'a Path,
    pub(super) marker_dir: &'a Path,
    /// 各タスク完了後に実行する usage-gate コマンド（ai-usage 連携時のみ）。
    pub(super) usage_gate_cmd: Option<&'a str>,
    /// 停止判定とキューからの claim をまとめて行うコマンド。claim できた番号を stdout へ
    /// 出し、終了コードで 0=claim 成功 / 10=恒久停止 / 20=pending 無し を返す。
    pub(super) gate_claim_cmd: &'a str,
    /// tmux のペインをすべて作り終えたことを知らせるファイル。指定されていれば、ワーカーは
    /// これが現れるまでタスクを取りに行かない（`None` は待たない）。
    pub(super) start_file: Option<&'a Path>,
}

/// 開始ファイルを待つ上限（0.1 秒単位。60 秒）。
///
/// ペインの作成はふつう 1 秒もかからない。作成に失敗した場合はセッションごと閉じられるので、
/// この上限に達するのは、ペインを作った後で token-burn 自身が落ちたときくらいしかない。
const WORKER_START_WAIT_TICKS: u32 = 600;

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
    // 起動前の一時的な失敗（LAUNCH_TEMPFAIL_EXIT）の表示。リトライ可能エラー（分類 3）と
    // 同じ言い回しにそろえる。
    let tempfail_notice = shell_escape(&format!(
        "━━━ Temporary launch failure (will retry next run): exit {LAUNCH_TEMPFAIL_EXIT} ━━━"
    ));
    let stop_file_escaped = shell_escape(&ctx.stop_file.to_string_lossy());
    let cmd_str = build_shell_command(&ctx.agent.command, &ctx.agent.env, ctx.prompt_file);
    let mark_cmd = format!(
        "{} mark {} {} {}",
        shell_escape(&ctx.exe_path.to_string_lossy()),
        shell_escape(&ctx.agent.name),
        shell_escape(&ctx.task.directory.to_string_lossy()),
        shell_escape(&ctx.state_file.to_string_lossy()),
    );

    let mut script = format!("CURRENT_TASK_LOG={log_file}\n");
    let last_line_cmd = format!(
        "{} last-log-line \"$CURRENT_TASK_LOG\" 2>/dev/null",
        shell_escape(&ctx.exe_path.to_string_lossy())
    );
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

    let tb_cmd = shell_escape(&ctx.exe_path.to_string_lossy());
    // resume-entry に渡す「どのエージェントの、どのターゲットか」。mark と同じ並び。
    let entry_args = format!(
        "{} {} {}",
        shell_escape(&ctx.agent.name),
        shell_escape(&ctx.task.directory.to_string_lossy()),
        shell_escape(&ctx.state_file.to_string_lossy()),
    );

    let is_claude = ctx.output == OutputPipeline::ClaudeStreamJson;
    if is_claude {
        let attempt = ClaudeAttempt {
            tb: &tb_cmd,
            stop_file: &stop_file_escaped,
            threshold: ctx.rate_limit_threshold,
            prefix: &error_prefix,
            error: &error_file,
            failed: &failed_marker,
        };
        match &ctx.resume {
            Some(resume) => {
                let session = shell_escape(&resume.entry.session_id);
                let mut resume_command = ctx.agent.command.clone();
                resume_command.push("--resume".to_string());
                resume_command.push(resume.entry.session_id.clone());
                let resume_cmd =
                    build_shell_command(&resume_command, &ctx.agent.env, resume.prompt_file);
                // RESUMING はワーカーのシェルに残る。再開しないタスクでも必ず値を入れ直す。
                script += "RESUMING=1\n";
                script += &format!(
                    "echo {}\n",
                    shell_escape(&format!(
                        "↻ Resuming session {} ({})",
                        resume.entry.session_id,
                        resume.entry.summary()
                    ))
                );
                script += &attempt.script(&resume_cmd, &jsonl_file, &log_file);
                // セッションが見つからない（transcript が消えている等）ときは、同じタスクの
                // まま元のプロンプトで新規セッションを始める。次回の実行でも結局同じ新規起動に
                // なるだけなので、ここで 1 実行ぶん待たせる理由が無い。会話は 1 ターンも
                // 始まっていない（num_turns:0）ので二重実行にもならない。試行ごとにログを分ける。
                let fresh_jsonl = shell_escape(
                    &ctx.run_dir
                        .join(format!("{log_base}.fresh.jsonl"))
                        .to_string_lossy(),
                );
                let fresh_log = shell_escape(
                    &ctx.run_dir
                        .join(format!("{log_base}.fresh.log"))
                        .to_string_lossy(),
                );
                script += &format!(
                    concat!(
                        "if [ \"$CLASS_CODE\" -eq {unavailable} ] && [ \"$CANCELLED\" -eq 0 ]; then\n",
                        "  {tb} resume-entry forget {args} --session {session} || true\n",
                        "  echo {notice}\n",
                        "  RESUMING=0\n",
                        "  CURRENT_FAILED_MARKER={failed}\n",
                        "{fresh}",
                        "fi\n",
                    ),
                    unavailable =
                        crate::classify::ResultClass::ResumeUnavailable(String::new()).exit_code(),
                    tb = tb_cmd,
                    args = entry_args,
                    session = session,
                    notice = shell_escape(&format!(
                        "━━━ Session {} could not be resumed — starting a new session ━━━",
                        resume.entry.session_id
                    )),
                    failed = failed_marker,
                    fresh = attempt.script(&cmd_str, &fresh_jsonl, &fresh_log),
                );
            }
            None => {
                script += "RESUMING=0\n";
                // 保存済みの記録を使わずに新しいセッションで始める（--no-resume / --fresh /
                // プロンプト変更 等）。新しいセッションが作業を始めた時点で、古い記録の文脈は
                // リポジトリの実態より古い。残すと、このセッションがリトライ可能エラーで
                // 終わった（＝保存されない）ときに、次の実行がより古いセッションを再開する。
                // 保存済みの ID と一致するときだけ消すので、後から保存された記録は消さない。
                if let Some(old) = ctx.discard_session {
                    script += &format!(
                        "{tb} resume-entry forget {args} --session {session} || true\n",
                        tb = tb_cmd,
                        args = entry_args,
                        session = shell_escape(old),
                    );
                }
                script += &attempt.script(&cmd_str, &jsonl_file, &log_file);
            }
        }
    } else {
        // codex は stdin を /dev/null にして起動する。無人実行なので、codex が stdin を読む経路に
        // 入っても（プロンプトを stdin から読む場合など）ペインのキー入力を待って止まったり、
        // ペインに打たれたキー入力を読んだりせず、即座に EOF を受け取るようにする。codex exec は
        // stdin が TTY でないと stdin を読んでプロンプトへ `<stdin>` ブロックとして足すが、
        // /dev/null なら何も足されない（stderr に `Reading additional input from stdin...` と
        // 出るだけ）。claude とその他のエージェントは従来どおりペインの TTY を stdin に持つ。
        let stdin = if ctx.output.closes_stdin() {
            " </dev/null"
        } else {
            ""
        };
        if ctx.output == OutputPipeline::CodexJson {
            // `codex exec --json` の JSONL を format-codex で整形してペインとログへ出し、生の
            // JSONL は claude と同じく `<NNNN>_<name>.jsonl` に残す。整形器は生ログか出力を
            // 書けないと 1 で終わる（入力は最後まで読むので codex は巻き添えにならない）。
            // claude と同じく、ログ・整形のパイプラインが壊れたタスクを成功扱いしない。
            script += &format!(
                "{cmd_str}{stdin} 2>&1 | {tb_cmd} format-codex --raw-output {jsonl_file} 2>&1 | tee {log_file}\n"
            );
            script += "PIPE_STATUS=(\"${PIPESTATUS[@]}\")\n";
            script += "CMD_EXIT=${PIPE_STATUS[0]}\n";
            script += "FORMAT_EXIT=${PIPE_STATUS[1]}\n";
            script += "TEE_EXIT=${PIPE_STATUS[2]}\n";
            script += "CURRENT_FAILED_MARKER=\"\"\n";
            script += &cancel_check(&failed_marker);
            script += &format!(
                concat!(
                    "if [ \"$FORMAT_EXIT\" -ne 0 ] || [ \"$TEE_EXIT\" -ne 0 ]; then\n",
                    "  printf '%slogging pipeline failed (format=%s tee=%s)\\n' {prefix} \"$FORMAT_EXIT\" \"$TEE_EXIT\" > {error}\n",
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
        } else {
            script += &format!("{cmd_str}{stdin} 2>&1 | tee {log_file}\n");
            script += "PIPE_STATUS=(\"${PIPESTATUS[@]}\")\n";
            script += "CMD_EXIT=${PIPE_STATUS[0]}\n";
            script += "TEE_EXIT=${PIPE_STATUS[1]}\n";
            script += "CURRENT_FAILED_MARKER=\"\"\n";
            script += &cancel_check(&failed_marker);
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
    }

    if is_claude {
        // レート制限で中断したセッションを保存し、次回の実行で続きから再開できるようにする。
        // 保存に失敗しても次回が新規セッションになるだけなので、ワーカーは止めない。
        // プロンプトは元の指示（再開時も ctx.prompt_file は元のプロンプト）のハッシュを残す。
        let save_line = if ctx.save_interrupted {
            format!(
                "    {tb} resume-entry save {args} --jsonl \"$JSONL\" --prompt-file {prompt} || echo {warn}\n",
                tb = tb_cmd,
                args = entry_args,
                prompt = shell_escape(&ctx.prompt_file.to_string_lossy()),
                warn = shell_escape(
                    "  ⚠ could not save the session for resuming — the next run starts a new session"
                ),
            )
        } else {
            String::new()
        };
        // 再開したセッションがレート制限以外の理由で失敗したら数える。上限に達すると記録を
        // 捨て、次回は新規セッションで始める（続けられないセッションを延々と再開しない）。
        // 分類に失敗した場合（`*)`）と、result を出さずに異常終了した場合（`0)` の分岐の
        // 非ゼロ終了）も数える。transcript の読み込みで落ち続けるセッションは result を
        // 残さないため、後者を数えないと記録が永久に残る。ただし LAUNCH_TEMPFAIL_EXIT
        // （エージェントが起動していない）は数えない。
        let fail_cmd = ctx.resume.as_ref().map(|resume| {
            format!(
                "if [ \"$RESUMING\" = 1 ]; then {tb} resume-entry fail {args} --session {session} || true; fi\n",
                tb = tb_cmd,
                args = entry_args,
                session = shell_escape(&resume.entry.session_id),
            )
        });
        let indented_fail = |indent: &str| {
            fail_cmd
                .as_deref()
                .map(|cmd| format!("{indent}{cmd}"))
                .unwrap_or_default()
        };
        let fail_line = indented_fail("      ");
        let crash_fail_line = indented_fail("          ");
        script += &format!(
            concat!(
                "case $CLASS_CODE in\n",
                "  2)\n",
                // 後続タスクが誤って「Cancelled」と判定されないよう、ここでフラグを必ずリセットする
                "    CANCELLED=0\n",
                "    echo '━━━ Rate limited - not marking as completed ━━━'\n",
                "{save_line}",
                // モニターがマーカーを見て終了する前に再開情報を保存し終える。
                "    touch {failed}\n",
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
                // 4（再開不能）は新規セッションへ切り替えた後はここに来ない。キャンセルで
                // 切り替えなかった場合と、再開しないタスクで万一返った場合は失敗として扱う。
                "  1|4)\n",
                "    if [ $CANCELLED -eq 1 ]; then\n",
                "      CANCELLED=0\n",
                "      touch {failed}\n",
                "      echo '━━━ Cancelled ━━━'\n",
                "    else\n",
                "      printf '%s%s\\n' {prefix} \"$CLASSIFIED\" > {error}\n",
                "      echo '━━━ Error - continuing ━━━'\n",
                "{fail_line}",
                "      touch {failed}\n",
                "    fi\n",
                "    ;;\n",
                "  0)\n",
                "    if [ \"$CMD_EXIT\" -ne 0 ]; then\n",
                "      if [ $CANCELLED -eq 1 ]; then\n",
                "        CANCELLED=0\n",
                "        touch {failed}\n",
                "        echo '━━━ Cancelled ━━━'\n",
                "      else\n",
                "        ERROR_MSG=$({last_line_cmd})\n",
                "        printf '%s%s\\n' {prefix} \"$ERROR_MSG\" > {error}\n",
                // 起動前の一時的な失敗。エージェントは動いていないので、失敗として数えずに
                // 次の実行で処理し直す（保存済みのセッションもそのまま残す）。
                "        if [ \"$CMD_EXIT\" -eq {tempfail} ]; then\n",
                "          echo {tempfail_notice}\n",
                "          touch {retry}\n",
                "        else\n",
                "          echo '━━━ Error - continuing ━━━'\n",
                "{crash_fail_line}",
                "          touch {failed}\n",
                "        fi\n",
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
                "  *)\n",
                // 分類プロセスのクラッシュ等を、AI コマンドの正常終了だけで成功扱いしない。
                "    if [ $CANCELLED -eq 1 ]; then\n",
                "      CANCELLED=0\n",
                "      echo '━━━ Cancelled ━━━'\n",
                "    else\n",
                "      printf '%sclassification failed (exit %s)\\n' {prefix} \"$CLASS_CODE\" > {error}\n",
                "      echo \"━━━ Error - classification failed (exit $CLASS_CODE) ━━━\"\n",
                "{fail_line}",
                "    fi\n",
                "    touch {failed}\n",
                "    ;;\n",
                "esac\n",
            ),
            prefix = error_prefix,
            error = error_file,
            failed = failed_marker,
            retry = retry_marker,
            done = done_marker,
            mark = mark_cmd,
            save_line = save_line,
            fail_line = fail_line,
            crash_fail_line = crash_fail_line,
            last_line_cmd = last_line_cmd,
            tempfail = LAUNCH_TEMPFAIL_EXIT,
            tempfail_notice = tempfail_notice,
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
                "    ERROR_MSG=$({last_line_cmd})\n",
                "    printf '%s%s\\n' {prefix} \"$ERROR_MSG\" > {error}\n",
                // 起動前の一時的な失敗は claude と同じく、失敗にせず次の実行で処理し直す。
                "    if [ \"$CMD_EXIT\" -eq {tempfail} ]; then\n",
                "      touch {retry}\n",
                "      echo {tempfail_notice}\n",
                "    else\n",
                "      touch {failed}\n",
                "      echo '━━━ Error - continuing ━━━'\n",
                "    fi\n",
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
            retry = retry_marker,
            done = done_marker,
            mark = mark_cmd,
            tempfail = LAUNCH_TEMPFAIL_EXIT,
            tempfail_notice = tempfail_notice,
            last_line_cmd = last_line_cmd,
        );
    }
    script += "echo ''\n";
    script
}

/// パイプラインの直後に、キャンセル（INT / TERM）を終了コードより先に見る。
///
/// bash は前面のパイプラインが終わってから trap を実行し、macOS の `/bin/bash`（3.2）では
/// trap の中のコマンドが `PIPESTATUS` を上書きする。キャンセルされたパイプラインの終了コードは
/// trap の最後のコマンドの値（0 など）に化け、`FORMAT_EXIT` / `TEE_EXIT` は空になる。そのまま
/// 分岐すると、Ctrl-C で止めたタスクを成功として `mark` し、state.json に処理済みと記録して
/// しまう（次の実行で処理し直されない）。キャンセルされたタスクは、終了コードによらず失敗に
/// する（中断セッションの保存も失敗回数の更新もしない）。
fn cancel_check(failed: &str) -> String {
    format!(
        concat!(
            "if [ \"$CANCELLED\" -eq 1 ]; then\n",
            "  touch {failed}\n",
            "  echo '━━━ Cancelled ━━━'\n",
            "  echo ''\n",
            "  return 0\n",
            "fi\n",
        ),
        failed = failed,
    )
}

/// claude の 1 回分の起動（パイプライン → 失敗検査 → 分類）を組み立てる。
///
/// 再開できなかったタスクを同じタスクのまま新規セッションで起動し直すため、同じ形を
/// 2 回出せるように切り出している。分類結果は `CLASS_CODE` / `CLASSIFIED` に、分類した
/// jsonl のパスは `JSONL` に入る（後段の `resume-entry save` が読む）。
struct ClaudeAttempt<'a> {
    tb: &'a str,
    stop_file: &'a str,
    threshold: u8,
    prefix: &'a str,
    error: &'a str,
    failed: &'a str,
}

impl ClaudeAttempt<'_> {
    fn script(&self, cmd_str: &str, jsonl_file: &str, log_file: &str) -> String {
        let mut script = String::new();
        script += &format!("CURRENT_TASK_LOG={log_file}\n");
        script += &format!("JSONL={jsonl_file}\n");
        script += &format!(
            "{cmd_str} 2>&1 | {tb} format-stream --raw-output {jsonl_file} --stop-file {stop} --threshold {threshold} 2>&1 | tee {log_file}\n",
            tb = self.tb,
            stop = self.stop_file,
            threshold = self.threshold,
        );
        script += "PIPE_STATUS=(\"${PIPESTATUS[@]}\")\n";
        script += "CMD_EXIT=${PIPE_STATUS[0]}\n";
        script += "FORMAT_EXIT=${PIPE_STATUS[1]}\n";
        script += "TEE_EXIT=${PIPE_STATUS[2]}\n";
        script += "CURRENT_FAILED_MARKER=\"\"\n";
        script += &cancel_check(self.failed);
        // 停止シグナル（stop / pause file）を書けなかった場合の専用コード。閾値を超えた
        // のに後続を止める手段が無い状態なので、このタスクを失敗として記録するだけでなく
        // ワーカーごと止める。`return 0` で次のタスクへ進むと、止めるべき状況のまま
        // クォータを使い続ける。
        //
        // 空の jsonl はログのパイプラインの故障として扱うが、コマンドが LAUNCH_TEMPFAIL_EXIT で
        // 終わったときは除く。整形器と tee は成功しており、空なのはエージェントが起動せず
        // 何も出力しなかったためなので、分類（result 無し = 0）へ進めて `0)` の分岐で
        // 起動前の一時的な失敗として扱う。
        script += &format!(
            concat!(
                "if [ \"$FORMAT_EXIT\" -eq {signal_exit} ]; then\n",
                "  printf '%scannot write the stop signal — aborting this worker\\n' {prefix} > {error}\n",
                "  touch {failed}\n",
                "  echo '━━━ Error - stop signal unwritable ━━━'\n",
                "  echo ''\n",
                "  WORKER_ABORT=1\n",
                "  return 0\n",
                "fi\n",
                "if [ \"$FORMAT_EXIT\" -ne 0 ] || [ \"$TEE_EXIT\" -ne 0 ] || {{ [ ! -s {jsonl} ] && [ \"$CMD_EXIT\" -ne {tempfail} ]; }}; then\n",
                "  printf '%slogging/classification pipeline failed (format=%s tee=%s)\\n' {prefix} \"$FORMAT_EXIT\" \"$TEE_EXIT\" > {error}\n",
                "  touch {failed}\n",
                "  echo '━━━ Error - logging pipeline failed ━━━'\n",
                "  echo ''\n",
                "  return 0\n",
                "fi\n",
                "CLASSIFIED=$({tb} classify-result {jsonl} 2>/dev/null)\n",
                "CLASS_CODE=$?\n",
            ),
            prefix = self.prefix,
            error = self.error,
            failed = self.failed,
            jsonl = jsonl_file,
            tb = self.tb,
            signal_exit = crate::format_stream::EXIT_STOP_SIGNAL_UNWRITABLE,
            tempfail = LAUNCH_TEMPFAIL_EXIT,
        );
        script
    }
}

/// 共通ワーカースクリプト: queue_dir/pending-* をアトミックに claim しつつタスクを逐次実行する。
pub(super) fn build_worker_script(ctx: &WorkerCtx<'_>) -> String {
    let w = ctx.worker_id + 1;
    let queue_dir = shell_escape(&ctx.queue_dir.to_string_lossy());
    let task_dir = shell_escape(&ctx.task_dir.to_string_lossy());
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
    // tmux のペインをすべて作り終えるまで、タスクを取りに行かない。ペインは 1 つずつ作るので、
    // 待たないと、途中の分割が失敗してセッションを閉じるまでの間に先に起動したワーカーが
    // タスクを claim し、エージェントを起動したところで kill される（実行 1 回ぶんの
    // タスクが記録も残さずに失われる）。
    let start_wait = match ctx.start_file {
        Some(path) => format!(
            concat!(
                "START_WAIT=0\n",
                "while [ ! -e {start} ] && [ $START_WAIT -lt {ticks} ]; do\n",
                "  sleep 0.1\n",
                "  START_WAIT=$((START_WAIT + 1))\n",
                "done\n",
                "if [ ! -e {start} ]; then\n",
                "  echo '━━━ Start signal missing — stopping worker ━━━'\n",
                "  touch {worker_done}\n",
                "  exit 1\n",
                "fi\n",
            ),
            start = shell_escape(&path.to_string_lossy()),
            ticks = WORKER_START_WAIT_TICKS,
            worker_done = worker_done,
        ),
        None => String::new(),
    };

    format!(
        concat!(
            "#!/bin/bash\n",
            "CURRENT_FAILED_MARKER=\"\"\n",
            "CANCELLED=0\n",
            // trap は印を立てるだけにする。マーカーはタスクスクリプトがパイプラインの直後に
            // CANCELLED を見て作る（`cancel_check`）。bash は前面のパイプラインが終わってから
            // trap を実行するので、ここでマーカーを作ると、続く分岐が done / retry も作り、
            // 1 タスクに 2 つのマーカーができる（モニターが早くセッションを閉じる）。
            "handle_cancel() {{\n",
            "  CANCELLED=1\n",
            "}}\n",
            "trap handle_cancel INT TERM\n",
            // ワーカーペインは無人で動く。端末の IXON（XON/XOFF フロー制御）が有効だと、誤って
            // 押された ^S（XOFF）でペインへの出力が止まり、tee が書き込みでブロックし、やがて
            // エージェント自身の出力も詰まって処理全体が止まる。macOS 既定の `ixany` では任意の
            // キーで再開するため、「止まっていたが Enter を連打したら進んだ」という報告と符合する。
            // 出力を遡って読むには tmux のコピーモードがあり、^S で止める必要は無い。stdin が
            // 端末でない場合（テスト等）は stty が失敗するだけなので無視する。
            "stty -ixon 2>/dev/null || true\n",
            "{start_wait}",
            "\n",
            "QUEUE_DIR={queue_dir}\n",
            "TASK_DIR={task_dir}\n",
            "WORKER_ABORT=0\n",
            "WORKER_FINISHED=0\n",
            "\n",
            "while true; do\n",
            // 停止判定と claim を 1 コマンドにまとめる。判定と `mv` をシェル側で 2 段に
            // 分けていた頃は、その隙間に別ワーカーが停止を発行すると停止後にタスクが
            // 開始されていた。gate-claim は両方を同じロックの下で行う。一時停止中なら
            // 再開時刻まで待ち、処理できる pending が無ければ 20 で終わる。
            "  CLAIMED=$({gate_claim_cmd})\n",
            "  CLAIM_RC=$?\n",
            "  if [ $CLAIM_RC -ne 0 ]; then\n",
            "    if [ $CLAIM_RC -eq 10 ]; then\n",
            "      printf '\\033]2;Worker {w} stopped\\033\\\\'\n",
            "      echo '━━━ Stopped ━━━'\n",
            "    elif [ $CLAIM_RC -eq 20 ]; then\n",
            "      WORKER_FINISHED=1\n",
            "    else\n",
            "      echo \"━━━ Gate claim failed (exit $CLAIM_RC) — stopping worker ━━━\"\n",
            "    fi\n",
            "    break\n",
            "  fi\n",
            "  if [ -z \"$CLAIMED\" ]; then\n",
            "    break\n",
            "  fi\n",
            // gate-claim の stdout は claim 番号だけという契約。番号以外が混ざった場合、
            // タスクは既に claimed へ rename 済みでスクリプト名だけが壊れるため、
            // そのまま続けると「マーカーも残さず消えたタスク」が生まれる。異常を
            // 握り潰さずワーカーを止めて、集計と原因表示に残す。
            "  case \"$CLAIMED\" in\n",
            "    *[!0-9]*)\n",
            "      echo \"━━━ Invalid claim output — stopping worker ━━━\"\n",
            "      break\n",
            "      ;;\n",
            "  esac\n",
            // 各タスク開始前に必ずリセットする。直前タスクの実行中に SIGINT/SIGTERM を
            // 受けて CANCELLED=1 が立ったまま成功・早期 return した場合でも、後続タスクの
            // 通常エラーを誤って「Cancelled」と判定しエラー記録を欠落させるのを防ぐ。
            "  CANCELLED=0\n",
            "  TASK_SCRIPT=\"$TASK_DIR/task-$CLAIMED.sh\"\n",
            "  if [ ! -f \"$TASK_SCRIPT\" ]; then\n",
            "    echo \"━━━ Missing task script: $TASK_SCRIPT ━━━\"\n",
            "    continue\n",
            "  fi\n",
            "  # shellcheck disable=SC1090\n",
            "  source \"$TASK_SCRIPT\"\n",
            // 停止シグナルを書けなかったタスクはワーカーごと止める。閾値を超えたのに
            // 後続を止める手段が無い状態で走り続けると、クォータを使い切ってしまう。
            "  if [ \"$WORKER_ABORT\" = 1 ]; then\n",
            "    echo '━━━ Stopped (stop signal unwritable) ━━━'\n",
            "    break\n",
            "  fi\n",
            "{gate_line}",
            "done\n",
            "\n",
            "if [ \"$WORKER_FINISHED\" = 1 ]; then\n",
            "  printf '\\033]2;Worker {w} done\\033\\\\'\n",
            "  echo '━━━ All tasks completed ━━━'\n",
            "else\n",
            "  printf '\\033]2;Worker {w} stopped\\033\\\\'\n",
            "  echo '━━━ Worker stopped ━━━'\n",
            "fi\n",
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
        w = w,
        worker_done = worker_done,
        gate_line = gate_line,
        gate_claim_cmd = ctx.gate_claim_cmd,
        start_wait = start_wait,
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
    // 実行中アカウント行を強調する active 指定（email 非依存の profile/provider）。値は
    // `--<名前>=<値>` の 1 引数で渡す（`-` で始まる名前を ai-usage の clap が別のフラグと
    // 読まないように）。
    parts.push(format!("--active-profile={profile}"));
    parts.push(format!("--active-provider={provider}"));
    // 選択 agent の env を前置きし、statusline の実行文脈を揃える。
    let mut all = env_prefix_parts(env);
    all.extend(parts.iter().map(|s| shell_escape(s)));
    Some(all.join(" "))
}

/// ai-usage の `--json` 取得コマンドに env prefix を付けて bash で実行できる
/// 文字列を組み立てる。monitor が出力で statusline 用のキャッシュを atomic に置き換え、
/// statusline は `--input <cache>` で同じファイルを読む。長時間タスク中に表示が古いまま
/// 固定されないようにするためで、キャッシュは表示専用（停止・一時停止の判定は usage-gate と
/// 実行開始時の判定が `--refresh` で取り直す）。ai-usage の共有キャッシュを通す取得で
/// よいので `--refresh` は付けない。空コマンドなら None。
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

/// 実行中のアカウントだけを ai-usage の共有キャッシュを通さずに取り直すコマンド
/// （[`crate::usage::live_command`]）に env prefix を付けた、bash で実行できる文字列。
/// モニターが一時停止中に、手動リセット等で枠が戻ったかを確かめるのに使う。空コマンドなら None。
pub(super) fn build_live_cmd(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
    profile: &str,
    provider: &str,
) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    let mut all = env_prefix_parts(env);
    all.extend(
        crate::usage::live_command(command, profile, provider)
            .iter()
            .map(|s| shell_escape(s)),
    );
    Some(all.join(" "))
}

/// ai-usage の `--json` 取得コマンドから、ディスク容量を取る `--disk --json` のコマンドを
/// 組み立てる。出力モードの `--json` の前に `--disk` を挿し、無ければ末尾に `--disk --json` を
/// 足す。`env FOO=1 ai-usage --json` のようなラッパーの前置きと、選択中のエージェントの env は
/// statusline と同じく保つ。ディスク容量は使用量の JSON とキャッシュに混ぜず、別に取る
/// （ai-usage 側の方針）。空コマンドなら None。
pub(super) fn build_disk_cmd(
    command: &[String],
    env: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::with_capacity(command.len() + 2);
    let mut inserted = false;
    for arg in command {
        if arg == "--json" && !inserted {
            parts.push("--disk".to_string());
            inserted = true;
        }
        parts.push(arg.clone());
    }
    if !inserted {
        parts.push("--disk".to_string());
        parts.push("--json".to_string());
    }
    let mut all = env_prefix_parts(env);
    all.extend(parts.iter().map(|s| shell_escape(s)));
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
        output: OutputPipeline,
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
            output,
            save_interrupted: output == OutputPipeline::ClaudeStreamJson,
            resume: None,
            discard_session: None,
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
        let ctx = task_ctx_for_test(7, &agent, &task, &tmp, OutputPipeline::ClaudeStreamJson);
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
        let ctx = task_ctx_for_test(1, &agent, &task, &tmp, OutputPipeline::ClaudeStreamJson);
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
        // claude でも codex でもないエージェントは従来どおり: stdin はペインの TTY のまま、
        // 出力は tee で素通しし、結果は終了コードで判定する。
        let agent = agent_for_test("aider", &["aider", "--yes"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(2, &agent, &task, &tmp, OutputPipeline::Raw);
        let script = build_task_script(&ctx);

        assert!(
            script.contains(
                "'aider' '--yes' \"$(cat '/tmp/prompt.txt')\" 2>&1 | tee '/tmp/0002_repo.log'\n"
            ),
            "{script}"
        );
        assert!(
            !script.contains("</dev/null"),
            "codex 以外は stdin を変えない: {script}"
        );
        assert!(!script.contains("format-codex"), "{script}");
        assert!(!script.contains("classify-result"));
        assert!(!script.contains("exec sleep infinity"));
        assert!(script.contains("Error - continuing"));
        assert!(script.contains("TEE_EXIT=${PIPE_STATUS[1]}"));
        assert!(script.contains("logging pipeline failed"));
    }

    #[test]
    fn build_task_script_for_codex_json_formats_with_format_codex() {
        let agent = agent_for_test(
            "codex",
            &["codex", "-c", "approval_policy=never", "exec", "--json"],
        );
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(2, &agent, &task, &tmp, OutputPipeline::CodexJson);
        let script = build_task_script(&ctx);

        // stdin は /dev/null、JSONL は format-codex で整形し、生の JSONL は .jsonl に残す
        assert!(
            script.contains(concat!(
                "'codex' '-c' 'approval_policy=never' 'exec' '--json' \"$(cat '/tmp/prompt.txt')\" </dev/null 2>&1",
                " | '/usr/local/bin/token-burn' format-codex --raw-output '/tmp/0002_repo.jsonl' 2>&1",
                " | tee '/tmp/0002_repo.log'\n",
            )),
            "{script}"
        );
        assert!(script.contains("PIPE_STATUS=(\"${PIPESTATUS[@]}\")\n"));
        assert!(script.contains("CMD_EXIT=${PIPE_STATUS[0]}\n"));
        assert!(script.contains("FORMAT_EXIT=${PIPE_STATUS[1]}\n"));
        assert!(script.contains("TEE_EXIT=${PIPE_STATUS[2]}\n"));
        // 整形・保存のどちらが壊れても成功扱いしない
        assert!(
            script.contains(
                "if [ \"$FORMAT_EXIT\" -ne 0 ] || [ \"$TEE_EXIT\" -ne 0 ]; then\n  printf '%slogging pipeline failed (format=%s tee=%s)\\n' '[repo] ' \"$FORMAT_EXIT\" \"$TEE_EXIT\" > '/tmp/error-2'\n  touch '/tmp/failed-2'\n  echo '━━━ Error - logging pipeline failed ━━━'\n  echo ''\n  return 0\nfi\n"
            ),
            "{script}"
        );
        // codex は stream-json の分類を持たないので、結果は終了コードで判定して mark する
        assert!(!script.contains("classify-result"), "{script}");
        assert!(!script.contains("format-stream"), "{script}");
        assert!(!script.contains("resume-entry"), "{script}");
        assert!(
            script.contains("if [ \"$CMD_EXIT\" -ne 0 ]; then"),
            "{script}"
        );
        assert!(
            script.contains(
                "elif '/usr/local/bin/token-burn' mark 'codex' '/tmp/repo' '/tmp/state.json'; then\n  touch '/tmp/done-2'"
            ),
            "{script}"
        );
        // 整形の検査はコマンドの終了コードによる判定より前
        let format_check = script.find("FORMAT_EXIT\" -ne 0").unwrap();
        let exit_check = script.find("if [ \"$CMD_EXIT\" -ne 0 ]").unwrap();
        assert!(format_check < exit_check, "{script}");
        assert_valid_bash(&script, "codex json task script");
    }

    #[test]
    fn build_task_script_for_codex_raw_closes_stdin_without_formatting() {
        let agent = agent_for_test("codex", &["/opt/bin/codex-wrapper.sh"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(3, &agent, &task, &tmp, OutputPipeline::CodexRaw);
        let script = build_task_script(&ctx);

        assert!(
            script.contains(
                "'/opt/bin/codex-wrapper.sh' \"$(cat '/tmp/prompt.txt')\" </dev/null 2>&1 | tee '/tmp/0003_repo.log'\n"
            ),
            "{script}"
        );
        assert!(script.contains("TEE_EXIT=${PIPE_STATUS[1]}"), "{script}");
        assert!(!script.contains("format-codex"), "{script}");
        assert!(!script.contains("FORMAT_EXIT"), "{script}");
        assert_valid_bash(&script, "codex raw task script");
    }

    #[test]
    fn build_worker_script_consumes_queue_atomically() {
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop",
            start_file: None,
            usage_gate_cmd: None,
        });

        assert!(script.contains("#!/bin/bash"));
        // claim はシェルではなく gate-claim が行う（停止判定と同じロックの下で
        // rename するため。シェルで `mv` していた頃は、停止の確認と claim の隙間に
        // 停止が発行されるとタスクが 1 件余計に始まっていた）。
        assert!(
            !script.contains("mv \"$pending\""),
            "claim をシェルで行ってはいけない: {script}"
        );
        assert!(script.contains("CLAIMED=$(tb gate-claim"));
        // source で個別タスクを取り込む
        assert!(script.contains("source \"$TASK_SCRIPT\""));
        // ワーカー完了マーカー
        assert!(script.contains("worker-done-0"));
        // 停止シグナル対応
        assert!(script.contains("trap handle_cancel INT TERM"));
    }

    #[test]
    fn build_worker_script_disables_xon_xoff_flow_control() {
        // 誤って押された ^S（XOFF）でペインの出力が止まり、tee とエージェントが書き込みで
        // 詰まるのを防ぐ。エージェントの種類に依らず、最初のタスクを始める前に切る。
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop",
            start_file: None,
            usage_gate_cmd: None,
        });
        let stty = script
            .find("\nstty -ixon 2>/dev/null || true\n")
            .unwrap_or_else(|| panic!("IXON を無効化すべき: {script}"));
        let trap = script.find("trap handle_cancel INT TERM").unwrap();
        let loop_start = script.find("while true; do").unwrap();
        assert!(trap < stty && stty < loop_start, "{script}");
    }

    #[test]
    fn build_worker_script_rejects_non_numeric_claim_output() {
        // gate-claim の stdout は claim 番号だけという契約。診断メッセージ等が
        // 混ざるとタスクスクリプト名が壊れ、既に claimed へ rename 済みのタスクが
        // マーカーも残さず消える。番号以外を検出したらワーカーごと止める。
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop",
            start_file: None,
            usage_gate_cmd: None,
        });

        assert!(
            script.contains("*[!0-9]*)"),
            "数字以外の claim 出力を検査すべき: {script}"
        );
        assert!(script.contains("Invalid claim output"), "{script}");

        // 実際に bash で走らせて、数字以外を返す gate-claim ではタスクを開始せずに
        // 抜けることを確かめる。
        if !bash_available() {
            return;
        }
        let harness = script
            .replace(
                "tb gate-claim --stop-file /tmp/burn/stop",
                "printf 'paused message\\n0001'",
            )
            .replace("/tmp/burn/markers", "/tmp/burn-test-markers");
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&harness)
            .output()
            .expect("bash should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Invalid claim output"),
            "混入を検出して止めるべき: {stdout}"
        );
        assert!(
            !stdout.contains("Missing task script"),
            "壊れた名前でタスクを探しに行ってはいけない: {stdout}"
        );
    }

    #[test]
    fn build_worker_script_runs_usage_gate_after_source() {
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop",
            start_file: None,
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
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop",
            start_file: None,
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
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop",
            start_file: None,
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
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop",
            start_file: None,
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
            gate_claim_cmd: "tb gate-claim --stop-file '/tmp/my stop'",
            start_file: None,
            usage_gate_cmd: None,
        });
        assert!(script.contains("QUEUE_DIR='/tmp/my queue'"));
        assert!(script.contains("TASK_DIR='/tmp/my tasks'"));
        assert!(script.contains("'/tmp/my stop'"));
        assert!(script.contains("'/tmp/my markers/worker-done-1'"));
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
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/stop",
            start_file: None,
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
    fn build_disk_cmd_inserts_disk_before_the_json_flag() {
        let to_vec = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut env = std::collections::BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), String::new());
        // ラッパーの前置きと選択中のエージェントの env を保ち、`--json` の前に `--disk` を挿す。
        assert_eq!(
            build_disk_cmd(&to_vec(&["env", "FOO=1", "ai-usage", "--json"]), &env).unwrap(),
            "env -u CLAUDE_CONFIG_DIR 'env' 'FOO=1' 'ai-usage' '--disk' '--json'"
        );
        // `--json` が無い構成でも `--disk --json` を足す。
        assert_eq!(
            build_disk_cmd(&to_vec(&["ai-usage"]), &std::collections::BTreeMap::new()).unwrap(),
            "'ai-usage' '--disk' '--json'"
        );
        assert!(build_disk_cmd(&[], &std::collections::BTreeMap::new()).is_none());
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
            "'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/cache.json' '--active-profile=Work' '--active-provider=claude'"
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
            "'env' 'FOO=1' 'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/c.json' '--active-profile=Home' '--active-provider=codex'"
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
            "'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/c.json' '--active-profile=Work' '--active-provider=claude'"
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
            "CLAUDE_CONFIG_DIR='/home/u/.claude' 'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/c.json' '--active-profile=Work' '--active-provider=claude'"
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
            "env -u CLAUDE_CONFIG_DIR CODEX_HOME='/home/u/.codex' 'ai-usage' '--statusline' '--logos' '--compact' '--input' '/tmp/c.json' '--active-profile=Home' '--active-provider=codex'"
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
        let ctx = task_ctx_for_test(3, &agent, &task, &tmp, OutputPipeline::ClaudeStreamJson);
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
        // 非 claude（codex 等）のパイプラインでも同じガードが必要。cd が失敗した場合に
        // PIPESTATUS の要素数が変わる問題は、format-codex を挟む 3 要素でも素通しの 2 要素でも同じ。
        let agent = agent_for_test("codex", &["codex", "exec"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        for output in [
            OutputPipeline::CodexJson,
            OutputPipeline::CodexRaw,
            OutputPipeline::Raw,
        ] {
            let ctx = task_ctx_for_test(4, &agent, &task, &tmp, output);
            let script = build_task_script(&ctx);

            assert!(
                script.contains("cd '/tmp/repo' || {"),
                "cd はガード付きの独立文であるべき ({output:?}): {script}"
            );
            assert!(
                !script.contains("cd '/tmp/repo' &&"),
                "cd をパイプラインへ && で連結してはいけない ({output:?}): {script}"
            );
            assert!(script.contains("target directory is unavailable"));
            assert!(script.contains("touch '/tmp/failed-4'"));
            let cd_idx = script
                .find("cd '/tmp/repo' || {")
                .expect("cd guard missing");
            let pipe_idx = script.find("| tee ").expect("pipeline missing");
            assert!(
                cd_idx < pipe_idx,
                "cd ガードはパイプラインの手前に出すべき ({output:?})"
            );
        }
    }

    #[test]
    fn build_task_script_cd_guard_escapes_special_characters_in_directory() {
        // ディレクトリ名に空白やシングルクォートが混ざっても cd ガードが壊れないこと。
        // エスケープが崩れると cd 行が構文エラーになり、ガード自体が機能しなくなる。
        let agent = agent_for_test("claude", &["claude", "-p"]);
        let task = target_for_test("/tmp/re po's dir");
        let tmp = std::path::PathBuf::from("/tmp");
        let ctx = task_ctx_for_test(5, &agent, &task, &tmp, OutputPipeline::ClaudeStreamJson);
        let script = build_task_script(&ctx);

        assert!(
            script.contains("cd '/tmp/re po'\\''s dir' || {"),
            "cd の引数はシェルエスケープされるべき: {script}"
        );
        assert_valid_bash(&script, "task script with quoted directory");
    }

    #[test]
    fn build_task_script_is_valid_bash() {
        // cd ガードを差し込んだ後も、すべての出力パイプラインのタスクスクリプトが
        // bash 構文として妥当であること（ワーカーは source して実行する）。
        let claude = agent_for_test("claude", &["claude", "-p"]);
        let codex = agent_for_test("codex", &["codex", "exec", "--json"]);
        let other = agent_for_test("aider", &["aider", "--yes"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");

        for (agent, output) in [
            (&claude, OutputPipeline::ClaudeStreamJson),
            (&codex, OutputPipeline::CodexJson),
            (&codex, OutputPipeline::CodexRaw),
            (&other, OutputPipeline::Raw),
        ] {
            let ctx = task_ctx_for_test(1, agent, &task, &tmp, output);
            let script = build_task_script(&ctx);
            assert_valid_bash(&script, &format!("{output:?}"));
        }
    }

    #[test]
    fn build_worker_script_takes_its_task_from_the_gate() {
        // 停止判定と claim は同じロックの下で行う必要があるため、ワーカーは自分で
        // stop file を見たりキューを rename したりせず、gate-claim の結果に従う。
        let tmp = std::path::PathBuf::from("/tmp/burn");
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &tmp.join("queue"),
            task_dir: &tmp.join("tasks"),
            marker_dir: &tmp.join("markers"),
            gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop --deadline-epoch 123",
            start_file: None,
            usage_gate_cmd: None,
        });
        assert!(
            script.contains(
                "CLAIMED=$(tb gate-claim --stop-file /tmp/burn/stop --deadline-epoch 123)"
            ),
            "claim した番号を gate から受け取るべき: {script}"
        );
        // 恒久停止（10）と pending 無し（20）を区別する。後者で「Stopped」と出すと、
        // 消化し切ったのか止められたのか読み取れない。
        assert!(script.contains("if [ $CLAIM_RC -eq 10 ]; then"), "{script}");
        assert!(
            !script.contains("$QUEUE_DIR\"/pending-*"),
            "キューの走査をシェルに残してはいけない: {script}"
        );
        let gate_idx = script.find("tb gate-claim").expect("gate missing");
        let source_idx = script
            .find("source \"$TASK_SCRIPT\"")
            .expect("source missing");
        assert!(gate_idx < source_idx, "gate はタスク実行より前: {script}");
    }

    /// ワーカースクリプトを実際に bash で走らせて、マーカーの生成結果を返す。
    ///
    /// `gate_claim_cmd` は claim した番号を stdout へ出し、終了コードで分岐する
    /// `token-burn gate-claim` の代役（テストでは実バイナリを呼べないためスタブを使う）。
    fn run_worker_script(
        gate_claim_cmd: &str,
        task_count: usize,
    ) -> (tempfile::TempDir, String, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let queue = tmp.path().join("queue");
        let tasks = tmp.path().join("tasks");
        let markers = tmp.path().join("markers");
        for dir in [&queue, &tasks, &markers] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for idx in 1..=task_count {
            std::fs::write(queue.join(format!("pending-{idx:04}")), "").unwrap();
            std::fs::write(
                tasks.join(format!("task-{idx:04}.sh")),
                format!("touch {}\n", markers.join(format!("done-{idx}")).display()),
            )
            .unwrap();
        }
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &queue,
            task_dir: &tasks,
            marker_dir: &markers,
            gate_claim_cmd,
            usage_gate_cmd: None,
            start_file: None,
        });
        let script_path = tmp.path().join("worker.sh");
        std::fs::write(&script_path, &script).unwrap();
        let out = std::process::Command::new("bash")
            .arg(&script_path)
            .output()
            .expect("bash must run the worker script");
        let listing = std::fs::read_dir(&markers)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            out.status.success(),
            "worker script must exit cleanly: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        (
            tmp,
            listing,
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    /// N 件を順に claim して、その後 20（pending 無し）を返すスタブ。
    fn claim_stub(count: usize) -> String {
        format!(
            "n=$(cat \"$TASK_DIR/../n\" 2>/dev/null || echo 0); n=$((n+1));              echo $n > \"$TASK_DIR/../n\";              if [ $n -le {count} ]; then printf '%04d\\n' $n; exit 0; else exit 20; fi"
        )
    }

    #[test]
    fn worker_script_runs_the_tasks_the_gate_hands_it() {
        // ゲートが番号を返す限りタスクを処理し、pending が尽きたら（20）静かに終わる。
        let (_tmp, markers, output) = run_worker_script(&claim_stub(2), 2);
        assert_eq!(markers, "done-1,done-2,worker-done-0");
        assert!(output.contains("All tasks completed"), "{output}");
    }

    #[test]
    fn worker_script_stops_when_the_gate_reports_a_permanent_stop() {
        // 恒久停止（10）ではタスクを 1 件も処理せず、ワーカー終了マーカーだけを残す。
        let (_tmp, markers, output) = run_worker_script("exit 10", 2);
        assert_eq!(markers, "worker-done-0");
        assert!(!output.contains("All tasks completed"), "{output}");
    }

    #[test]
    fn worker_script_reports_gate_failure_without_claiming_a_task() {
        let (tmp, markers, output) = run_worker_script("exit 1", 1);
        assert_eq!(markers, "worker-done-0");
        assert!(tmp.path().join("queue/pending-0001").exists());
        assert!(output.contains("Gate claim failed (exit 1)"), "{output}");
        assert!(!output.contains("All tasks completed"), "{output}");
    }

    #[test]
    fn worker_script_ends_quietly_when_the_queue_is_empty() {
        // pending 無し（20）は停止ではないので「Stopped」を出さずに終える。ここで
        // 停止と表示すると、正常に消化し切ったのか止められたのか区別できなくなる。
        let tmp = tempfile::TempDir::new().unwrap();
        let queue = tmp.path().join("queue");
        let tasks = tmp.path().join("tasks");
        let markers = tmp.path().join("markers");
        for dir in [&queue, &tasks, &markers] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &queue,
            task_dir: &tasks,
            marker_dir: &markers,
            gate_claim_cmd: "exit 20",
            start_file: None,
            usage_gate_cmd: None,
        });
        let script_path = tmp.path().join("worker.sh");
        std::fs::write(&script_path, &script).unwrap();
        let out = std::process::Command::new("bash")
            .arg(&script_path)
            .output()
            .unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!stdout.contains("Stopped"), "{stdout}");
        assert!(markers.join("worker-done-0").exists());
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
                gate_claim_cmd: "tb gate-claim --stop-file /tmp/burn/stop",
                start_file: None,
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

    // ─────────── 中断セッションの再開（resume） ───────────

    const SESSION: &str = "0633fa18-8643-40f7-9106-a35330ed247d";

    fn saved_session() -> ResumeEntry {
        ResumeEntry {
            session_id: SESSION.to_string(),
            interrupted_at: chrono::DateTime::parse_from_rfc3339("2026-09-23T13:04:23+09:00")
                .unwrap(),
            retry_after: None,
            retry_basis_percent: None,
            prompt_hash: crate::resume::prompt_hash("review"),
            message: None,
            log: None,
            failed_resumes: 0,
        }
    }

    #[test]
    fn claude_task_saves_the_session_when_rate_limited() {
        let agent = agent_for_test("claude", &["claude", "-p"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let script = build_task_script(&task_ctx_for_test(
            1,
            &agent,
            &task,
            &tmp,
            OutputPipeline::ClaudeStreamJson,
        ));

        // レート制限の分岐で、分類した jsonl と元のプロンプトを渡して保存する
        let case2 = script.find("  2)\n").expect("case 2 missing");
        let case3 = script.find("  3)\n").expect("case 3 missing");
        let branch = &script[case2..case3];
        assert!(
            branch.contains(
                "'/usr/local/bin/token-burn' resume-entry save 'claude' '/tmp/repo' '/tmp/state.json' --jsonl \"$JSONL\" --prompt-file '/tmp/prompt.txt' ||"
            ),
            "{branch}"
        );
        // 保存に失敗してもワーカーは止めない（次回が新規セッションになるだけ）
        assert!(branch.contains("could not save the session"), "{branch}");
        // 再開しないタスクでも RESUMING を入れ直す（ワーカーのシェルに前の値が残る）
        assert!(script.contains("RESUMING=0\n"), "{script}");
        assert!(!script.contains("--resume"), "{script}");
        assert!(!script.contains("resume-entry fail"), "{script}");
        assert!(script.contains("  1|4)\n"), "{script}");
        assert_valid_bash(&script, "claude task script");
    }

    /// 保存済みの記録を使わずに新しいセッションで始めるときは、起動の直前に古い記録を
    /// 破棄する（ID が一致するときだけ）。残すと、新しいセッションがリトライ可能エラーで
    /// 終わったとき、次の実行がより古いセッションを再開してしまう。
    #[test]
    fn fresh_start_discards_the_unused_saved_session() {
        let agent = agent_for_test("claude", &["claude", "-p"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let mut ctx = task_ctx_for_test(2, &agent, &task, &tmp, OutputPipeline::ClaudeStreamJson);
        ctx.discard_session = Some(SESSION);
        let script = build_task_script(&ctx);

        let forget = script
            .find(&format!(
                "'/usr/local/bin/token-burn' resume-entry forget 'claude' '/tmp/repo' '/tmp/state.json' --session '{SESSION}' || true\n"
            ))
            .unwrap_or_else(|| panic!("forget missing: {script}"));
        let launch = script
            .find("format-stream --raw-output '/tmp/0002_repo.jsonl'")
            .expect("launch missing");
        assert!(forget < launch, "起動より前に破棄する: {script}");
        assert_valid_bash(&script, "fresh start script");

        // 破棄する記録が無ければ何も足さない
        let plain = build_task_script(&task_ctx_for_test(
            2,
            &agent,
            &task,
            &tmp,
            OutputPipeline::ClaudeStreamJson,
        ));
        assert!(!plain.contains("resume-entry forget"), "{plain}");
    }

    #[test]
    fn claude_task_does_not_save_when_resuming_is_disabled() {
        let agent = agent_for_test("claude", &["claude", "-p"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let mut ctx = task_ctx_for_test(1, &agent, &task, &tmp, OutputPipeline::ClaudeStreamJson);
        ctx.save_interrupted = false;
        let script = build_task_script(&ctx);
        assert!(!script.contains("resume-entry"), "{script}");
    }

    #[test]
    fn resumed_task_passes_the_session_and_the_continuation_prompt() {
        let agent = agent_for_test("claude", &["claude", "-p"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let entry = saved_session();
        let mut ctx = task_ctx_for_test(4, &agent, &task, &tmp, OutputPipeline::ClaudeStreamJson);
        ctx.resume = Some(TaskResume {
            entry: &entry,
            prompt_file: std::path::Path::new("/tmp/resume-prompt.txt"),
        });
        let script = build_task_script(&ctx);

        // 1 回目は保存済みセッションを継続プロンプトで再開する
        assert!(
            script.contains(&format!(
                "'claude' '-p' '--resume' '{SESSION}' \"$(cat '/tmp/resume-prompt.txt')\" 2>&1 | '/usr/local/bin/token-burn' format-stream --raw-output '/tmp/0004_repo.jsonl'"
            )),
            "{script}"
        );
        assert!(script.contains("RESUMING=1\n"), "{script}");
        assert!(
            script.contains(&format!(
                "echo '↻ Resuming session {SESSION} (rate limited "
            )),
            "{script}"
        );
        // 再開できなければ記録を消し、元のプロンプトの新規セッションを別ログで起動する
        let fallback = script
            .find("if [ \"$CLASS_CODE\" -eq 4 ] && [ \"$CANCELLED\" -eq 0 ]; then")
            .expect("fallback missing");
        let rest = &script[fallback..];
        assert!(
            rest.contains(&format!(
                "resume-entry forget 'claude' '/tmp/repo' '/tmp/state.json' --session '{SESSION}' || true"
            )),
            "{rest}"
        );
        assert!(
            rest.contains(
                "'claude' '-p' \"$(cat '/tmp/prompt.txt')\" 2>&1 | '/usr/local/bin/token-burn' format-stream --raw-output '/tmp/0004_repo.fresh.jsonl'"
            ),
            "{rest}"
        );
        assert!(rest.contains("| tee '/tmp/0004_repo.fresh.log'"), "{rest}");
        // 新規セッションの実行中も、キャンセルで失敗マーカーを残せるようにする
        assert!(
            rest.contains("  CURRENT_FAILED_MARKER='/tmp/failed-4'\n"),
            "{rest}"
        );
        // 再開したセッションの失敗は数える
        assert!(
            script.contains(&format!(
                "if [ \"$RESUMING\" = 1 ]; then '/usr/local/bin/token-burn' resume-entry fail 'claude' '/tmp/repo' '/tmp/state.json' --session '{SESSION}' || true; fi"
            )),
            "{script}"
        );
        assert_valid_bash(&script, "resumed task script");
    }

    /// 生成したタスクスクリプトを偽の claude / token-burn で実際に走らせる。
    ///
    /// 偽の claude は `--resume` 付きなら `RESUME_OUT`、無ければ `FRESH_OUT` を出力する。
    /// 偽の token-burn は format-stream を `tee`、classify-result を出力内容の目印で
    /// 分類し、mark / resume-entry の呼び出しを `calls.log` に残す。
    ///
    /// 次の出力は、claude を起動する前にラッパーが終わった形を真似る（result を残さない）。
    /// `TEMPFAIL` は理由を stderr に出して 75、`SILENT_TEMPFAIL` は何も出さずに 75、
    /// `SILENT_CRASH` は何も出さずに 1 で終わる。
    fn run_resumed_task(resume_out: &str, fresh_out: &str) -> (tempfile::TempDir, String, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let repo = root.join("repo");
        let run_dir = root.join("run");
        let markers = root.join("markers");
        for dir in [&repo, &run_dir, &markers] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let calls = root.join("calls.log");
        let prompts = root.join("prompts.log");
        let claude = root.join("claude");
        std::fs::write(
            &claude,
            format!(
                concat!(
                    "#!/bin/bash\n",
                    "for last; do :; done\n",
                    "printf '%s\\n' \"$last\" >> {prompts}\n",
                    "case \" $* \" in\n",
                    "  *' --resume '*) OUT={resume_out} ;;\n",
                    "  *) OUT={fresh_out} ;;\n",
                    "esac\n",
                    "case $OUT in\n",
                    "  TEMPFAIL) echo 'wrapper: setup failed' >&2; exit 75 ;;\n",
                    "  SILENT_TEMPFAIL) exit 75 ;;\n",
                    "  SILENT_CRASH) exit 1 ;;\n",
                    "esac\n",
                    "printf '%s\\n' \"$OUT\"\n",
                    "case $OUT in NOT_FOUND|CRASH) exit 1 ;; esac\n",
                    "exit 0\n",
                ),
                prompts = shell_escape(&prompts.to_string_lossy()),
                resume_out = shell_escape(resume_out),
                fresh_out = shell_escape(fresh_out),
            ),
        )
        .unwrap();
        let tb = root.join("token-burn");
        std::fs::write(
            &tb,
            format!(
                concat!(
                    "#!/bin/bash\n",
                    "case \"$1\" in\n",
                    "  format-stream) tee \"$3\" ;;\n",
                    "  last-log-line) tail -n 1 \"$2\" ;;\n",
                    "  classify-result)\n",
                    "    if grep -q CLASSIFIER_CRASH \"$2\"; then exit 101; fi\n",
                    "    if grep -q NOT_FOUND \"$2\"; then echo 'No conversation found'; exit 4; fi\n",
                    "    if grep -q RATE \"$2\"; then exit 2; fi\n",
                    "    if grep -q FAIL \"$2\"; then echo boom; exit 1; fi\n",
                    "    exit 0 ;;\n",
                    "  *)\n",
                    "    if [ \"$1\" = resume-entry ] && [ -e {failed} ]; then echo premature-marker >> {calls}; fi\n",
                    "    echo \"$*\" >> {calls} ;;\n",
                    "esac\n",
                ),
                calls = shell_escape(&calls.to_string_lossy()),
                failed = shell_escape(&markers.join("failed-1").to_string_lossy()),
            ),
        )
        .unwrap();
        for exe in [&claude, &tb] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prompt_file = root.join("prompt-1.txt");
        std::fs::write(&prompt_file, "original prompt").unwrap();
        let resume_prompt = root.join("resume-prompt.txt");
        std::fs::write(&resume_prompt, "continue please").unwrap();

        let agent = agent_for_test("claude", &[&claude.to_string_lossy(), "-p"]);
        let task = ResolvedTarget {
            directory: repo.clone(),
            display_name: "repo".to_string(),
            prompt: "original prompt".to_string(),
            visibility: Visibility::Public,
            defer: false,
        };
        let entry = saved_session();
        let state_file = root.join("state.json");
        let stop_file = root.join("stop");
        let script = build_task_script(&TaskCtx {
            idx: 1,
            total: 1,
            task: &task,
            agent: &agent,
            prompt_file: &prompt_file,
            run_dir: &run_dir,
            marker_dir: &markers,
            exe_path: &tb,
            state_file: &state_file,
            stop_file: &stop_file,
            rate_limit_threshold: 95,
            output: OutputPipeline::ClaudeStreamJson,
            save_interrupted: true,
            resume: Some(TaskResume {
                entry: &entry,
                prompt_file: &resume_prompt,
            }),
            discard_session: None,
        });
        let script_path = root.join("task.sh");
        std::fs::write(&script_path, &script).unwrap();
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "CANCELLED=0; source {}",
                shell_escape(&script_path.to_string_lossy())
            ))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "task script must finish: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let calls = std::fs::read_to_string(&calls).unwrap_or_default();
        let prompts = std::fs::read_to_string(&prompts).unwrap_or_default();
        (tmp, calls, prompts)
    }

    fn marker_names(tmp: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(tmp.path().join("markers"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// モニターは完了マーカーを見てセッションを閉じる。永続化中にマーカーが見えてはならない。
    #[test]
    fn resumed_task_persists_outcome_before_completion_marker() {
        for outcome in ["RATE", "FAIL", "CRASH"] {
            let (tmp, calls, _) = run_resumed_task(outcome, "unused");
            assert!(calls.contains("resume-entry"), "永続化が必要: {outcome}");
            assert!(
                !calls.contains("premature-marker"),
                "保存前に完了している: {calls}"
            );
            assert!(marker_names(&tmp).contains(&"failed-1".to_string()));
        }
    }

    #[test]
    fn classifier_crash_does_not_mark_successful_command_completed() {
        let (tmp, calls, _) = run_resumed_task("CLASSIFIER_CRASH", "unused");
        assert!(
            !calls.lines().any(|line| line.starts_with("mark ")),
            "分類できていないタスクを完了扱いにしてはいけない"
        );
        assert!(calls.contains("resume-entry fail"));
        assert!(marker_names(&tmp).contains(&"failed-1".to_string()));
    }

    #[test]
    fn resumed_task_that_succeeds_is_marked_done() {
        if !bash_available() {
            return;
        }
        let (tmp, calls, prompts) = run_resumed_task("ok", "unused");
        assert_eq!(
            prompts, "continue please\n",
            "再開には継続プロンプトだけを送る"
        );
        assert!(calls.starts_with("mark claude "), "{calls}");
        assert!(!calls.contains("resume-entry"), "{calls}");
        assert_eq!(marker_names(&tmp), vec!["done-1"]);
    }

    #[test]
    fn missing_session_falls_back_to_a_new_session_in_the_same_task() {
        if !bash_available() {
            return;
        }
        let (tmp, calls, prompts) = run_resumed_task("NOT_FOUND", "ok");
        // 再開を試みてから、元のプロンプトで新規セッションを起動する
        assert_eq!(prompts, "continue please\noriginal prompt\n");
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines.len(), 2, "{calls}");
        assert!(
            lines[0].starts_with("resume-entry forget claude ")
                && lines[0].ends_with(&format!("--session {SESSION}")),
            "{calls}"
        );
        assert!(lines[1].starts_with("mark claude "), "{calls}");
        assert_eq!(marker_names(&tmp), vec!["done-1"]);
        // 試行ごとにログを分ける
        assert!(tmp.path().join("run/0001_repo.jsonl").exists());
        assert!(tmp.path().join("run/0001_repo.fresh.jsonl").exists());
        assert!(tmp.path().join("run/0001_repo.fresh.log").exists());
    }

    #[test]
    fn rate_limited_fallback_saves_the_new_session_log() {
        if !bash_available() {
            return;
        }
        // 新規セッションへ切り替えた後にレート制限で止まったら、新しい方の jsonl を保存する
        let (tmp, calls, _) = run_resumed_task("NOT_FOUND", "RATE");
        let save = calls
            .lines()
            .find(|l| l.starts_with("resume-entry save"))
            .unwrap_or_else(|| panic!("save missing: {calls}"));
        assert!(save.contains("0001_repo.fresh.jsonl"), "{save}");
        assert!(
            save.contains("prompt-1.txt"),
            "元のプロンプトで照合する: {save}"
        );
        assert_eq!(marker_names(&tmp), vec!["failed-1"]);
    }

    #[test]
    fn resumed_task_rate_limited_again_saves_the_session_again() {
        if !bash_available() {
            return;
        }
        let (tmp, calls, prompts) = run_resumed_task("RATE", "unused");
        assert_eq!(prompts, "continue please\n", "新規セッションは起動しない");
        assert_eq!(calls.lines().count(), 1, "{calls}");
        let save = calls.lines().next().unwrap();
        assert!(save.starts_with("resume-entry save claude "), "{save}");
        assert!(
            save.contains("--jsonl ") && save.contains("run/0001_repo.jsonl"),
            "{save}"
        );
        assert!(save.contains("prompt-1.txt"), "{save}");
        assert_eq!(marker_names(&tmp), vec!["failed-1"]);
    }

    #[test]
    fn resumed_task_crash_without_a_result_is_counted() {
        if !bash_available() {
            return;
        }
        // result を出さずに非ゼロ終了（transcript の読み込みで落ちる等）。数えないと、
        // 続けられないセッションの記録が永久に残って毎回再開しては落ちる。数えないのは
        // 起動前の一時的な失敗（75）だけで、それ以外の終了コード（ここでは 1）は数える。
        let (tmp, calls, _) = run_resumed_task("CRASH", "unused");
        assert_eq!(calls.lines().count(), 1, "{calls}");
        assert!(calls.starts_with("resume-entry fail claude "), "{calls}");
        assert_eq!(marker_names(&tmp), vec!["error-1", "failed-1"]);
    }

    /// ラッパーが claude を起動する前に一時的な理由で諦めた（LAUNCH_TEMPFAIL_EXIT）。
    /// エージェントは動いていないので、失敗にも再開の失敗回数にも数えず、保存済みの
    /// セッションをそのまま残して次の実行で処理し直す。理由は他の失敗と同じく error-N に
    /// 残す。何も出力せずに終わった場合も、空の jsonl をログのパイプラインの故障と
    /// 取り違えない。
    #[test]
    fn resumed_task_launch_tempfail_is_retried_without_counting() {
        if !bash_available() {
            return;
        }
        for outcome in ["TEMPFAIL", "SILENT_TEMPFAIL"] {
            let (tmp, calls, prompts) = run_resumed_task(outcome, "unused");
            assert_eq!(marker_names(&tmp), vec!["error-1", "retry-1"], "{outcome}");
            assert_eq!(calls, "", "保存済みのセッションに触れない: {outcome}");
            assert_eq!(
                prompts, "continue please\n",
                "新規セッションは起動しない: {outcome}"
            );
            let error = std::fs::read_to_string(tmp.path().join("markers/error-1")).unwrap();
            assert!(error.starts_with("[repo] "), "{outcome}: {error}");
        }
    }

    /// 再開できずに新規セッションへ切り替えた後の起動が一時的に失敗した場合も、同じく
    /// 次の実行で処理し直す。
    #[test]
    fn launch_tempfail_after_falling_back_to_a_new_session_is_retried() {
        if !bash_available() {
            return;
        }
        let (tmp, calls, prompts) = run_resumed_task("NOT_FOUND", "TEMPFAIL");
        assert_eq!(prompts, "continue please\noriginal prompt\n");
        let lines: Vec<&str> = calls.lines().collect();
        assert_eq!(lines.len(), 1, "{calls}");
        assert!(
            lines[0].starts_with("resume-entry forget claude "),
            "{calls}"
        );
        assert_eq!(marker_names(&tmp), vec!["error-1", "retry-1"]);
    }

    /// 空の jsonl の検査を外すのは 75 だけ。それ以外の終了コードで何も出さずに終わった
    /// タスクは、従来どおり失敗として扱う。
    #[test]
    fn silent_exit_other_than_launch_tempfail_still_fails() {
        if !bash_available() {
            return;
        }
        let (tmp, _, _) = run_resumed_task("SILENT_CRASH", "unused");
        assert_eq!(marker_names(&tmp), vec!["error-1", "failed-1"]);
    }

    #[test]
    fn launch_tempfail_branch_retries_without_counting_the_session() {
        let agent = agent_for_test("claude", &["claude", "-p"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let entry = saved_session();
        let mut ctx = task_ctx_for_test(4, &agent, &task, &tmp, OutputPipeline::ClaudeStreamJson);
        ctx.resume = Some(TaskResume {
            entry: &entry,
            prompt_file: std::path::Path::new("/tmp/resume-prompt.txt"),
        });
        let script = build_task_script(&ctx);

        // result を残さずに終わった（`0)` の分岐）うち 75 だけは retry にし、失敗回数を数えない
        let start = script
            .find("        if [ \"$CMD_EXIT\" -eq 75 ]; then\n")
            .unwrap_or_else(|| panic!("tempfail branch missing: {script}"));
        let end = start
            + script[start..]
                .find("        else\n")
                .expect("tempfail branch must end");
        let branch = &script[start..end];
        assert!(branch.contains("touch '/tmp/retry-4'"), "{branch}");
        assert!(
            branch
                .contains("echo '━━━ Temporary launch failure (will retry next run): exit 75 ━━━'"),
            "{branch}"
        );
        assert!(!branch.contains("resume-entry fail"), "{branch}");
        assert!(!branch.contains("failed-4"), "{branch}");
        // 理由は 75 でも他の失敗と同じく error-N に残す（分岐より前に書く）
        let error_write = script[..start]
            .rfind("printf '%s%s\\n' '[repo] ' \"$ERROR_MSG\" > '/tmp/error-4'")
            .expect("error is written before the branch");
        assert!(error_write < start);
        // 何も出さずに 75 で終わった場合の空の jsonl は、ログのパイプラインの故障にしない
        assert!(
            script.contains("{ [ ! -s '/tmp/0004_repo.jsonl' ] && [ \"$CMD_EXIT\" -ne 75 ]; }"),
            "{script}"
        );
        assert_valid_bash(&script, "resumed task script");
    }

    #[test]
    fn resumed_task_failure_is_counted() {
        if !bash_available() {
            return;
        }
        let (tmp, calls, _) = run_resumed_task("FAIL", "unused");
        assert_eq!(calls.lines().count(), 1, "{calls}");
        assert!(
            calls.starts_with("resume-entry fail claude ")
                && calls.trim_end().ends_with(&format!("--session {SESSION}")),
            "{calls}"
        );
        assert_eq!(marker_names(&tmp), vec!["error-1", "failed-1"]);
    }

    // ─────────── codex（format-codex の整形と stdin の /dev/null） ───────────

    /// claude 以外のタスクスクリプトを偽のエージェント / token-burn で実際に走らせる。
    ///
    /// 偽のエージェントは stdin を読み切って `stdin.log` に残し、JSONL を 1 行出して `agent_exit` で
    /// 終わる。偽の token-burn は format-codex で生ログを保存して `format_exit` で終わり、それ以外
    /// （mark）の呼び出しを `calls.log` に残す。ワーカーのシェルの stdin には、ペインに打たれた
    /// キー入力の代わりの文字列を流し込む。
    fn run_agent_task(
        output: OutputPipeline,
        format_exit: u8,
        agent_exit: u8,
    ) -> (tempfile::TempDir, String, String) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let repo = root.join("repo");
        let run_dir = root.join("run");
        let markers = root.join("markers");
        for dir in [&repo, &run_dir, &markers] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let calls = root.join("calls.log");
        let stdin_log = root.join("stdin.log");
        let agent_exe = root.join("codex");
        std::fs::write(
            &agent_exe,
            format!(
                concat!(
                    "#!/bin/bash\n",
                    "cat > {stdin_log}\n",
                    "printf '%s\\n' '{{\"type\":\"thread.started\"}}'\n",
                    "exit {agent_exit}\n",
                ),
                stdin_log = shell_escape(&stdin_log.to_string_lossy()),
                agent_exit = agent_exit,
            ),
        )
        .unwrap();
        let tb = root.join("token-burn");
        std::fs::write(
            &tb,
            format!(
                concat!(
                    "#!/bin/bash\n",
                    "case \"$1\" in\n",
                    "  format-codex) tee \"$3\"; exit {format_exit} ;;\n",
                    "  *) echo \"$*\" >> {calls} ;;\n",
                    "esac\n",
                ),
                format_exit = format_exit,
                calls = shell_escape(&calls.to_string_lossy()),
            ),
        )
        .unwrap();
        for exe in [&agent_exe, &tb] {
            std::fs::set_permissions(exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prompt_file = root.join("prompt-1.txt");
        std::fs::write(&prompt_file, "original prompt").unwrap();

        let agent = agent_for_test("codex", &[&agent_exe.to_string_lossy(), "exec"]);
        let task = ResolvedTarget {
            directory: repo.clone(),
            display_name: "repo".to_string(),
            prompt: "original prompt".to_string(),
            visibility: Visibility::Public,
            defer: false,
        };
        let state_file = root.join("state.json");
        let stop_file = root.join("stop");
        let script = build_task_script(&TaskCtx {
            idx: 1,
            total: 1,
            task: &task,
            agent: &agent,
            prompt_file: &prompt_file,
            run_dir: &run_dir,
            marker_dir: &markers,
            exe_path: &tb,
            state_file: &state_file,
            stop_file: &stop_file,
            rate_limit_threshold: 95,
            output,
            save_interrupted: false,
            resume: None,
            discard_session: None,
        });
        let script_path = root.join("task.sh");
        std::fs::write(&script_path, &script).unwrap();
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!(
                "CANCELLED=0; source {}",
                shell_escape(&script_path.to_string_lossy())
            ))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // stdin を読まないまま終わった場合の EPIPE は無視してよい（読まなかったこと自体が結果）。
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(b"keys typed into the pane\n");
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "task script must finish: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let calls = std::fs::read_to_string(&calls).unwrap_or_default();
        let stdin_seen = std::fs::read_to_string(&stdin_log).unwrap();
        (tmp, calls, stdin_seen)
    }

    #[test]
    fn codex_json_task_formats_output_and_marks_success() {
        if !bash_available() {
            return;
        }
        let (tmp, calls, stdin_seen) = run_agent_task(OutputPipeline::CodexJson, 0, 0);
        assert_eq!(stdin_seen, "", "codex にペインのキー入力を渡してはいけない");
        assert!(calls.starts_with("mark codex "), "{calls}");
        assert_eq!(marker_names(&tmp), vec!["done-1"]);
        // 生の JSONL は .jsonl に、整形後の出力は .log に残る
        let raw = std::fs::read_to_string(tmp.path().join("run/0001_repo.jsonl")).unwrap();
        assert_eq!(raw, "{\"type\":\"thread.started\"}\n");
        let log = std::fs::read_to_string(tmp.path().join("run/0001_repo.log")).unwrap();
        assert!(log.contains("thread.started"), "{log}");
    }

    #[test]
    fn codex_json_task_fails_when_the_formatter_fails() {
        if !bash_available() {
            return;
        }
        // 整形器が生ログや出力を書けなかった場合（exit 1）は、codex が成功していても
        // 成功扱いせず、次回の実行で処理し直せるよう state.json に記録しない。
        let (tmp, calls, _) = run_agent_task(OutputPipeline::CodexJson, 1, 0);
        assert!(!calls.contains("mark"), "{calls}");
        assert_eq!(marker_names(&tmp), vec!["error-1", "failed-1"]);
        let error = std::fs::read_to_string(tmp.path().join("markers/error-1")).unwrap();
        assert_eq!(error, "[repo] logging pipeline failed (format=1 tee=0)\n");
    }

    #[test]
    fn codex_raw_task_closes_stdin_but_other_agents_keep_it() {
        if !bash_available() {
            return;
        }
        let (tmp, calls, stdin_seen) = run_agent_task(OutputPipeline::CodexRaw, 0, 0);
        assert_eq!(stdin_seen, "", "codex にペインのキー入力を渡してはいけない");
        assert!(calls.starts_with("mark codex "), "{calls}");
        assert_eq!(marker_names(&tmp), vec!["done-1"]);
        assert!(
            !tmp.path().join("run/0001_repo.jsonl").exists(),
            "素通しでは生ログを別に作らない"
        );

        // codex 以外は従来どおり stdin を引き継ぐ（同じハーネスでキー入力が届くことを確かめる）
        let (_tmp, _, stdin_seen) = run_agent_task(OutputPipeline::Raw, 0, 0);
        assert_eq!(stdin_seen, "keys typed into the pane\n");
    }

    /// claude 以外のエージェントも、起動前の一時的な失敗（LAUNCH_TEMPFAIL_EXIT）は失敗にせず
    /// 次の実行で処理し直す。理由は error-N に残す。それ以外の非ゼロ終了は従来どおり失敗。
    #[test]
    fn agent_task_launch_tempfail_is_retried_but_other_exits_fail() {
        if !bash_available() {
            return;
        }
        for output in [
            OutputPipeline::CodexJson,
            OutputPipeline::CodexRaw,
            OutputPipeline::Raw,
        ] {
            let (tmp, calls, _) = run_agent_task(output, 0, 75);
            assert!(!calls.contains("mark"), "{output:?}: {calls}");
            assert_eq!(marker_names(&tmp), vec!["error-1", "retry-1"], "{output:?}");

            let (tmp, calls, _) = run_agent_task(output, 0, 1);
            assert!(!calls.contains("mark"), "{output:?}: {calls}");
            assert_eq!(
                marker_names(&tmp),
                vec!["error-1", "failed-1"],
                "{output:?}"
            );
        }
    }

    #[test]
    fn build_task_script_for_non_claude_retries_launch_tempfail() {
        let agent = agent_for_test("aider", &["aider", "--yes"]);
        let task = target_for_test("/tmp/repo");
        let tmp = std::path::PathBuf::from("/tmp");
        let script = build_task_script(&task_ctx_for_test(
            2,
            &agent,
            &task,
            &tmp,
            OutputPipeline::Raw,
        ));
        assert!(
            script.contains(concat!(
                "    printf '%s%s\\n' '[repo] ' \"$ERROR_MSG\" > '/tmp/error-2'\n",
                "    if [ \"$CMD_EXIT\" -eq 75 ]; then\n",
                "      touch '/tmp/retry-2'\n",
                "      echo '━━━ Temporary launch failure (will retry next run): exit 75 ━━━'\n",
                "    else\n",
                "      touch '/tmp/failed-2'\n",
                "      echo '━━━ Error - continuing ━━━'\n",
                "    fi\n",
            )),
            "{script}"
        );
        assert_valid_bash(&script, "non-claude task script");
    }

    /// tmux のペインをすべて作り終えるまで（開始ファイルが現れるまで）、ワーカーはタスクを
    /// 取りに行かない。分割の途中で失敗したら、先に起動したワーカーも 1 件も claim しないまま
    /// セッションごと閉じられる。開始ファイルのパスはクォートする。
    #[test]
    fn worker_waits_for_the_start_file_before_claiming() {
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: std::path::Path::new("/tmp/queue"),
            task_dir: std::path::Path::new("/tmp/tasks"),
            marker_dir: std::path::Path::new("/tmp/markers"),
            usage_gate_cmd: None,
            gate_claim_cmd: "tb gate-claim",
            start_file: Some(std::path::Path::new("/tmp/run dir/start")),
        });
        let wait = script
            .find("while [ ! -e '/tmp/run dir/start' ]")
            .expect("開始ファイルを待つ");
        let claim = script.find("CLAIMED=$(tb gate-claim)").unwrap();
        assert!(wait < claim, "{script}");
        assert!(
            script.contains("echo '━━━ Start signal missing — stopping worker ━━━'\n  touch '/tmp/markers/worker-done-0'\n  exit 1\n"),
            "{script}"
        );
        assert_valid_bash(&script, "worker script with start file");

        let without = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: std::path::Path::new("/tmp/queue"),
            task_dir: std::path::Path::new("/tmp/tasks"),
            marker_dir: std::path::Path::new("/tmp/markers"),
            usage_gate_cmd: None,
            gate_claim_cmd: "tb gate-claim",
            start_file: None,
        });
        assert!(!without.contains("START_WAIT"), "{without}");
    }

    /// 開始ファイルが既にあれば待たずに処理する（実際に bash で 1 周させる）。
    #[cfg(unix)]
    #[test]
    fn worker_proceeds_once_the_start_file_exists() {
        if !bash_available() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let (queue, tasks, markers) = (
            tmp.path().join("queue"),
            tmp.path().join("tasks"),
            tmp.path().join("markers"),
        );
        for dir in [&queue, &tasks, &markers] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(queue.join("pending-0001"), "").unwrap();
        std::fs::write(
            tasks.join("task-0001.sh"),
            format!(
                "touch {}\n",
                shell_escape(&markers.join("done-1").to_string_lossy())
            ),
        )
        .unwrap();
        let start = tmp.path().join("start");
        std::fs::write(&start, "").unwrap();
        let claim = format!(
            "if mv {q}/pending-0001 {q}/claimed-0001 2>/dev/null; then echo 0001; else exit 20; fi",
            q = shell_escape(&queue.to_string_lossy())
        );
        let script = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &queue,
            task_dir: &tasks,
            marker_dir: &markers,
            usage_gate_cmd: None,
            gate_claim_cmd: &format!("sh -c {}", shell_escape(&claim)),
            start_file: Some(&start),
        });
        let path = tmp.path().join("worker.sh");
        std::fs::write(&path, script).unwrap();
        let output = std::process::Command::new("bash")
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(markers.join("done-1").exists(), "{output:?}");
        assert!(markers.join("worker-done-0").exists(), "{output:?}");
    }

    /// ワーカー 1 つ・タスク 1 つを bash で実際に動かし、残ったタスクのマーカー名と
    /// `mark` が呼ばれた回数を返す。`agent_script` はエージェントの代わりに `sh -c` で動く。
    /// ワーカーは独立したプロセスグループで起動する（エージェントがグループ全体へ送る
    /// シグナルがテストランナーへ届かないようにする）。
    #[cfg(unix)]
    fn run_worker_with_agent(agent_script: &str) -> (Vec<String>, usize, std::process::Output) {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (queue, tasks, markers, run, repo) = (
            root.join("queue"),
            root.join("tasks"),
            root.join("markers"),
            root.join("run"),
            root.join("repo"),
        );
        for dir in [&queue, &tasks, &markers, &run, &repo] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let executable = |name: &str, body: &str| {
            let path = root.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // mark は呼ばれたことを記録して成功する。gate-claim は 1 件だけ claim し、次は
        // pending 無し（20）で終わる。
        let mark_log = root.join("mark.log");
        let exe = executable(
            "token-burn",
            &format!(
                "echo \"$1\" >> {}",
                shell_escape(&mark_log.to_string_lossy())
            ),
        );
        let claim = executable(
            "gate-claim",
            &format!(
                "if mv {q}/pending-0001 {q}/claimed-0001 2>/dev/null; then echo 0001; else exit 20; fi",
                q = shell_escape(&queue.to_string_lossy())
            ),
        );
        let prompt = root.join("prompt.txt");
        std::fs::write(&prompt, "review").unwrap();
        let agent = agent_for_test("agent", &["sh", "-c", agent_script]);
        let task = target_for_test(&repo.to_string_lossy());
        let mut ctx = task_ctx_for_test(1, &agent, &task, &run, OutputPipeline::Raw);
        ctx.marker_dir = &markers;
        ctx.exe_path = &exe;
        ctx.prompt_file = &prompt;
        std::fs::write(tasks.join("task-0001.sh"), build_task_script(&ctx)).unwrap();
        std::fs::write(queue.join("pending-0001"), "").unwrap();
        let worker = build_worker_script(&WorkerCtx {
            worker_id: 0,
            queue_dir: &queue,
            task_dir: &tasks,
            marker_dir: &markers,
            usage_gate_cmd: None,
            gate_claim_cmd: &shell_escape(&claim.to_string_lossy()),
            start_file: None,
        });
        let worker_path = root.join("worker-0.sh");
        std::fs::write(&worker_path, worker).unwrap();

        let output = std::process::Command::new("/bin/bash")
            .arg(&worker_path)
            .stdin(std::process::Stdio::null())
            .process_group(0)
            .output()
            .unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&markers)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with("worker-done-") && !name.starts_with("error-"))
            .collect();
        names.sort();
        let marks = std::fs::read_to_string(&mark_log)
            .map(|log| log.lines().filter(|line| *line == "mark").count())
            .unwrap_or(0);
        (names, marks, output)
    }

    /// ペインでの Ctrl-C（プロセスグループ全体への SIGINT）で止めたタスクは、失敗として残し、
    /// state.json へ処理済みと記録しない。macOS の `/bin/bash`（3.2）では trap の中の
    /// コマンドが `PIPESTATUS` を上書きするため、終了コードで分岐していた頃は
    /// `CMD_EXIT` が 0 に化けて `mark` され、`done` と `failed` の両方のマーカーができていた。
    #[cfg(unix)]
    #[test]
    fn cancelled_task_is_failed_and_not_marked_processed() {
        if !std::path::Path::new("/bin/bash").exists() {
            return;
        }
        let (names, marks, output) = run_worker_with_agent("kill -INT 0; sleep 1; echo never");
        assert_eq!(names, ["failed-1"], "{output:?}");
        assert_eq!(
            marks, 0,
            "キャンセルしたタスクを処理済みにしない: {output:?}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Cancelled"), "{output:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("integer expression expected"),
            "{output:?}"
        );
    }

    /// ワーカーのシェルだけにシグナルが届いた場合（`kill -TERM <ワーカーの PID>`）も、
    /// 1 タスクのマーカーは 1 つだけにする。trap がその場で `failed-N` を作っていた頃は、
    /// 続く分岐が `done-N` も作り、モニターがマーカーを名前の接頭辞で数えるため 1 タスクを
    /// 2 件と数え、早くセッションを閉じて走行中のタスクを巻き添えで終わらせていた。
    #[cfg(unix)]
    #[test]
    fn signal_to_the_worker_shell_leaves_exactly_one_marker_for_the_task() {
        if !std::path::Path::new("/bin/bash").exists() {
            return;
        }
        let (names, marks, output) =
            run_worker_with_agent("kill -TERM $PPID; sleep 0.2; echo finished");
        assert_eq!(names, ["failed-1"], "{output:?}");
        assert_eq!(marks, 0, "{output:?}");
        assert!(output.status.success(), "{output:?}");
    }
}
