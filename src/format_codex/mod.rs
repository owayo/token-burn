//! `codex exec --json` の出力を tmux のワーカーペインで読めるテキストに整形する
//! （`token-burn format-codex`。ワーカースクリプト専用）。
//!
//! codex の人間向け出力は、ファイルを変更するたびにターン全体の累積 diff を表示し直す。
//! 実ログでは 141MB のうち 126MB がこの diff（1 回 100〜500KB）で、ペインには常に diff の
//! 末尾しか映らず、止まっているように見えていた。`--json` のイベントからは変更した
//! パスだけを 1 行ずつ出し、diff は一切表示しない。
//!
//! 読み取りと表示を 2 つのスレッドに分ける。読み取りスレッドは標準入力を EOF まで読み、
//! 受け取ったバイト列をそのまま生ログ（`--raw-output`）へ書いてから表示側へ渡す。
//! 表示側（メインスレッド）は行が来なければ 1 秒ごとに起き、codex が長く黙っている
//! ときに生存表示（ハートビート）を出す。表示の書き込みはすべてメインスレッドで行うため、
//! 行が途中で混ざることはない。

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

mod formatter;
mod items;
mod text;

use formatter::CodexFormatter;

/// 読み取りスレッドから表示側へ渡す行のキュー長。
///
/// codex のイベントは疎（数秒に 1 行程度）なので、表示先が一時的に詰まっても、これだけ
/// あれば読み取りは止まらない。上限を設けるのは、表示先が詰まったまま巨大な
/// `aggregated_output` を含む行が溜まり続けてメモリを使い切らないようにするため。
const CHANNEL_CAPACITY: usize = 1024;

/// 表示側が行の到着を待つ最大時間。これを過ぎるたびに無出力の経過を確かめる
/// （ハートビートの判定の粒度）。
const IDLE_TICK: Duration = Duration::from_secs(1);

/// 標準入力の読み取りバッファ。`aggregated_output` を含む行は数百 KB になり得るため、
/// 標準の 8KB より大きく取って `read` の回数を抑える。
const READ_BUFFER_SIZE: usize = 64 * 1024;

/// 入力が一時的に読めない（`WouldBlock`）ときに待つ時間。
///
/// パイプの読み取り口が非ブロッキングにされていると、データが無い間 `read` は待たずに
/// `WouldBlock` を返す。これを致命的なエラーとして読み取りを打ち切ると、上流の `codex` が
/// SIGPIPE で落ちる。少し待ってから読み直す。
const WOULD_BLOCK_BACKOFF: Duration = Duration::from_millis(20);

/// 標準入力の `codex exec --json` を整形して標準出力へ書く。戻り値は終了コード。
///
/// 生ログか表示の書き込みに失敗した場合は 1、それ以外は 0。失敗しても入力は EOF まで
/// 読み切る（壊れた側への書き込みだけを止め、もう一方は続ける）。途中で抜けると
/// パイプが閉じ、上流の `codex` が SIGPIPE で落ちて数時間の作業を巻き添えにする。
pub fn run(raw_output: Option<&Path>) -> i32 {
    let mut raw_create_failed = false;
    let raw = raw_output.and_then(|path| match File::create(path) {
        Ok(file) => Some(file),
        Err(e) => {
            warn(&format!(
                "cannot create the raw output {} ({e}); continuing without it",
                path.display()
            ));
            raw_create_failed = true;
            None
        }
    });
    // 変更したファイルのパスはこのディレクトリからの相対で表示する。タスクスクリプトは
    // 対象リポジトリへ cd してから codex と format-codex を起動する。
    let mut formatter = CodexFormatter::new(Instant::now(), std::env::current_dir().ok());
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut outcome = process(io::stdin(), raw, &mut out, &mut formatter);
    outcome.raw_failed |= raw_create_failed;
    outcome.exit_code()
}

/// 1 回の実行で起きた失敗。どれか 1 つでもあれば終了コードを 1 にする。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Outcome {
    /// 生ログを作れなかった、または途中で書けなくなった。
    raw_failed: bool,
    /// 整形結果を表示先（標準出力）へ書けなくなった。
    display_failed: bool,
    /// 入力を EOF まで読めなかった。
    read_failed: bool,
}

impl Outcome {
    fn exit_code(self) -> i32 {
        if self.raw_failed || self.display_failed || self.read_failed {
            1
        } else {
            0
        }
    }
}

/// 入力を EOF まで読み切り、生ログと整形表示へ流す。
fn process<R, W, O>(
    reader: R,
    raw: Option<W>,
    out: &mut O,
    formatter: &mut CodexFormatter,
) -> Outcome
where
    R: Read + Send,
    W: Write + Send,
    O: Write,
{
    // 読み取り側の資源は共有の置き場に入れておく。スレッドを作れなかったときに、同じ
    // スレッドで読み取りを続けるために取り戻せるようにする（クロージャへ move すると、
    // 起動に失敗した時点でクロージャごと破棄されて二度と使えない）。
    let source = Mutex::new(Some((
        BufReader::with_capacity(READ_BUFFER_SIZE, reader),
        raw,
    )));
    thread::scope(|scope| {
        // チャネルはこのクロージャの中で作る。表示側が万一 panic して巻き戻るとき、
        // 受信側が先に破棄されるので読み取りスレッドの送信が詰まらず、入力を EOF まで
        // 読み進められる（scope は全スレッドの終了を待ってから panic を伝える）。
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let source_ref = &source;
        let spawned = thread::Builder::new()
            .name("format-codex-reader".to_string())
            .spawn_scoped(scope, move || {
                take_source(source_ref).map(|(reader, raw)| {
                    pump_lines(reader, raw, |line| {
                        // 表示側が先に終わっていても読み取りは続ける（生ログと上流のため）。
                        let _ = tx.send(line);
                    })
                })
            });
        match spawned {
            Ok(handle) => {
                let display_failed = drive(&rx, formatter, out);
                // 読み取りスレッドが panic した場合は EOF まで読めたか分からない。
                let mut outcome = handle.join().ok().flatten().unwrap_or(Outcome {
                    read_failed: true,
                    ..Outcome::default()
                });
                outcome.display_failed = display_failed;
                outcome
            }
            Err(e) => {
                warn(&format!(
                    "cannot start the reader thread ({e}); formatting without heartbeats"
                ));
                match take_source(&source) {
                    Some((reader, raw)) => process_inline(reader, raw, out, formatter),
                    None => Outcome {
                        read_failed: true,
                        ..Outcome::default()
                    },
                }
            }
        }
    })
}

/// 読み取りスレッドを作れなかった場合の代替。同じスレッドで読みながら整形する。
///
/// 読み取りで待っている間は表示側が起きられないためハートビートは出せないが、入力を
/// EOF まで読み切ることを優先する。
fn process_inline<R: BufRead, W: Write, O: Write>(
    reader: R,
    raw: Option<W>,
    out: &mut O,
    formatter: &mut CodexFormatter,
) -> Outcome {
    let mut display_failed = false;
    let mut outcome = pump_lines(reader, raw, |line| {
        if !display_failed {
            report_display(
                formatter.on_line(&line, Instant::now(), out),
                &mut display_failed,
            );
        }
    });
    outcome.display_failed = display_failed;
    outcome
}

fn take_source<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// 入力を EOF まで 1 行ずつ読み、受け取ったバイト列をそのまま生ログへ書いてから
/// `deliver` へ渡す。
///
/// 生ログは受け取ったバイト列そのもの（不正な UTF-8 も置換しない）で、1 行ごとに
/// flush する。codex のイベントは疎なので書き込み回数は問題にならず、キャンセルで
/// 打ち切られた実行でもバッファに残った分を失わない。生ログを書けなくなったら以後は
/// 書かずに読み進める（表示と上流の codex を止めないため）。
fn pump_lines<R: BufRead, W: Write>(
    mut reader: R,
    mut raw: Option<W>,
    mut deliver: impl FnMut(Vec<u8>),
) -> Outcome {
    let mut outcome = Outcome::default();
    let mut line = Vec::new();
    loop {
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                write_raw(&mut raw, &line, &mut outcome.raw_failed);
                deliver(std::mem::take(&mut line));
            }
            // 読めた分は `line` に残っているので、同じバッファへ読み足す。
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(WOULD_BLOCK_BACKOFF),
            Err(e) => {
                warn(&format!("cannot read the input ({e}); stopping"));
                outcome.read_failed = true;
                break;
            }
        }
    }
    // EOF の直前や読み取りエラーの時点で改行の無い断片が残っていても捨てない。
    if !line.is_empty() {
        write_raw(&mut raw, &line, &mut outcome.raw_failed);
        deliver(line);
    }
    outcome
}

fn write_raw<W: Write>(raw: &mut Option<W>, bytes: &[u8], failed: &mut bool) {
    let Some(writer) = raw.as_mut() else {
        return;
    };
    if let Err(e) = writer.write_all(bytes).and_then(|()| writer.flush()) {
        warn(&format!(
            "cannot write the raw output ({e}); continuing without it"
        ));
        *failed = true;
        *raw = None;
    }
}

/// 表示側のループ。読み取りスレッドが終わる（送信側が破棄される）まで受け取り続ける。
///
/// 表示先へ書けなくなったら以後は書かないが、受け取りは続ける。ここで抜けると
/// チャネルが満杯になった時点で読み取りスレッドが止まり、上流の codex まで詰まる。
fn drive<O: Write>(rx: &Receiver<Vec<u8>>, formatter: &mut CodexFormatter, out: &mut O) -> bool {
    let mut failed = false;
    loop {
        match rx.recv_timeout(IDLE_TICK) {
            Ok(line) => {
                if !failed {
                    report_display(formatter.on_line(&line, Instant::now(), out), &mut failed);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !failed {
                    report_display(formatter.on_idle(Instant::now(), out), &mut failed);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    failed
}

fn report_display(result: io::Result<()>, failed: &mut bool) {
    if let Err(e) = result {
        *failed = true;
        warn(&format!(
            "cannot write the formatted output ({e}); reading the input to the end without displaying it"
        ));
    }
}

/// format-codex 自身の警告を標準エラーへ書く。
///
/// `eprintln!` は書き込みに失敗すると panic する。ワーカーでは標準エラーも表示と同じ
/// パイプ（`2>&1 | tee`）へ流れるため、`tee` が先に終わった後で panic すると入力を
/// 読み切れないまま終了してしまう。書けなければ黙って諦める。1 回の `write_all` に
/// まとめるのは、標準出力側の行と途中で混ざらないようにするため。
fn warn(message: &str) {
    let line = format!("\x1b[33m\u{26a0} format-codex: {message}\x1b[0m\n");
    #[cfg(not(test))]
    {
        let _ = io::stderr().lock().write_all(line.as_bytes());
    }
    // テストでは出力キャプチャに乗せる（直接書くと、失敗系のテストの警告がテスト結果の
    // 表示に混ざる）。テスト中の標準エラーは閉じないので panic の心配は無い。
    #[cfg(test)]
    eprint!("{line}");
}

#[cfg(test)]
mod tests;
