use anyhow::Result;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::Path;

mod assistant;
mod blocks;
mod diff;
mod rate_limit;
mod result;
mod state;
mod stream;
pub(crate) mod system;
mod tool_result;
mod tools;
mod util;

use assistant::handle_assistant_event;
use blocks::{ContentBlockState, break_open_line, finalize_open_blocks};
use rate_limit::handle_rate_limit_event;
use result::handle_result;
use state::{StreamState, StreamSummary};
use stream::handle_stream_event;
use system::handle_system_event;
use tool_result::{handle_synthetic_user_event, handle_tool_result_event};
use tools::progress::handle_tool_progress;

/// `claude -p` の stream-json 出力を読みやすいテキストに変換する。
/// JSON以外の行はそのまま出力（任意のエージェントで動作）。
pub fn run(raw_output: Option<&Path>, stop_file: Option<&Path>, threshold: u8) -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let out = stdout.lock();
    process(stdin.lock(), out, raw_output, stop_file, threshold)
}

fn process(
    reader: impl BufRead,
    mut out: impl Write,
    raw_output: Option<&Path>,
    stop_file: Option<&Path>,
    threshold: u8,
) -> Result<()> {
    let mut tool_id_map: HashMap<String, String> = HashMap::new();
    let mut shown_notices = std::collections::HashSet::new();
    let mut blocks: HashMap<usize, ContentBlockState> = HashMap::new();
    let mut summary = StreamSummary::default();
    let mut raw_writer = match raw_output {
        Some(path) => Some(io::BufWriter::new(File::create(path)?)),
        None => None,
    };

    for line in read_lines_lossy(reader) {
        let line = line?;
        if let Some(writer) = raw_writer.as_mut() {
            writeln!(writer, "{}", line)?;
        }
        if line.is_empty() {
            continue;
        }

        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                // JSON 以外 — そのまま出力（例: codex のプレーンテキスト出力、
                // claude のラッパーバナー、`2>&1` で合流した stderr）。
                //
                // 開きっぱなしの思考/テキスト行を閉じてから独立行へ書く。タスク
                // スクリプトは `claude ... 2>&1 | format-stream` で stderr を同じ
                // パイプへ合流させるため、`API Error: Connection closed mid-response.`
                // のような stderr 行が思考ブロックの途中に到着し得る。直接書くと
                // `💭 ..API Error: ...` の形で開いている行へ連結され、通知末尾の
                // リセットが dim を打ち消して以降の進捗ドットまで崩れる。
                // system / rate_limit / user / tool_progress と同じ扱いに揃える。
                render_out_of_band_event(&mut out, &mut blocks, |pending| {
                    writeln!(pending, "{}", line)?;
                    Ok(())
                })?;
                out.flush()?;
                continue;
            }
        };

        let msg_type = v["type"].as_str().unwrap_or("");

        match msg_type {
            "system" => {
                summary.update_from_system(&v);
                render_out_of_band_event(&mut out, &mut blocks, |pending| {
                    handle_system_event(&v, pending)
                })?;
            }
            "stream_event" => {
                handle_stream_event(
                    &v["event"],
                    &mut out,
                    &mut StreamState {
                        blocks: &mut blocks,
                        tool_id_map: &mut tool_id_map,
                        summary: &mut summary,
                    },
                )?;
            }
            "assistant" => {
                handle_assistant_event(
                    &v,
                    &mut out,
                    &mut tool_id_map,
                    &mut shown_notices,
                    &mut blocks,
                )?;
            }
            "user" => {
                // ツール結果 — 完了したツール名を表示。
                // バックグラウンド/非同期ツールの完了は次ターンのテキスト・思考 delta の
                // 途中にも到着するため、直接書くと開きっぱなしの行へ連結される
                // （実ログで `💭   ✓ WebFetch` の形が 38 件）。
                render_out_of_band_event(&mut out, &mut blocks, |pending| {
                    handle_tool_result_event(&v, pending, &tool_id_map)?;
                    // フックの差し戻し（Stop hook feedback 等）は合成 user メッセージ
                    // として届き、tool_result を持たないため上の経路では拾えない。
                    handle_synthetic_user_event(&v, pending)
                })?;
            }
            "result" => {
                summary.update_from_result(v.as_object());
                finalize_open_blocks(&mut out, &mut blocks)?;
                handle_result(&v, &summary, &mut out)?;
            }
            "rate_limit_event" => {
                render_out_of_band_event(&mut out, &mut blocks, |pending| {
                    handle_rate_limit_event(&v, pending, stop_file, threshold)
                })?;
            }
            "tool_progress" => {
                // 長時間ツールのハートビートも本文ストリームと非同期に届くため、
                // system / rate_limit と同じく行を閉じてから書く。
                render_out_of_band_event(&mut out, &mut blocks, |pending| {
                    handle_tool_progress(&v, pending)
                })?;
            }
            _ => {} // message_stop 等
        }
    }

    finalize_open_blocks(&mut out, &mut blocks)?;
    out.flush()?;
    if let Some(writer) = raw_writer.as_mut() {
        writer.flush()?;
    }

    Ok(())
}

/// 入力を 1 行ずつ読み、不正な UTF-8 バイトは U+FFFD へ置換して返す。
///
/// `BufRead::lines()` は非 UTF-8 バイトを 1 つでも含む行に対して `Err(InvalidData)` を
/// 返す。そこで `?` すると `process` がその場で中断し、以降の**正常な JSON も含めて**
/// 標準出力と `--raw-output` の両方から失われる。タスクスクリプトのパイプラインは
/// `claude ... 2>&1 | token-burn format-stream ... | tee log` で stderr を同じパイプへ
/// 合流させており、しかも stream-json の 1 行は macOS の `PIPE_BUF`（512 バイト）を
/// 常に超えるため、stdout の途中に stderr の書き込みが割り込んでマルチバイト文字が
/// 分断されるだけで不正な UTF-8 が生じ得る。中断すると `FORMAT_EXIT != 0` で
/// `failed-N` になるうえ、パイプが閉じて `claude` 本体が SIGPIPE で落ちるため、
/// 表示整形の都合で数時間の実行を巻き添えにしてしまう。整形器が UTF-8 を要求する
/// 必然性は無いので、該当行だけ置換文字にして読み進める。
fn read_lines_lossy(mut reader: impl BufRead) -> impl Iterator<Item = io::Result<String>> {
    std::iter::from_fn(move || {
        let mut buf = Vec::new();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => None,
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                }
                Some(Ok(String::from_utf8_lossy(&buf).into_owned()))
            }
            Err(e) => Some(Err(e)),
        }
    })
}

/// stream の本文・思考とは独立した通知を、開いている行へ連結せずに出力する。
///
/// 無視対象のイベントでは余計な改行を増やさないよう、先にバッファへ描画し、実際に
/// 表示内容がある場合だけ現在の行を閉じる。system のタスク進捗や rate-limit 通知は
/// テキスト delta の途中にも到着するため、直接書くと本文の単語中へ通知が混入する。
fn render_out_of_band_event(
    out: &mut impl Write,
    blocks: &mut HashMap<usize, ContentBlockState>,
    render: impl FnOnce(&mut Vec<u8>) -> Result<()>,
) -> Result<()> {
    let mut pending = Vec::new();
    render(&mut pending)?;
    if !pending.is_empty() {
        break_open_line(out, blocks)?;
        out.write_all(&pending)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
