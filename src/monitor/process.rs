//! モニターが外部コマンド（ai-usage）を起動する処理。
//!
//! コマンドは `bash -c` で実行する（env の前置きとクォートを済ませた文字列を、実行計画と
//! 同じ規則で組み立てているため）。ハングしても描画と停止判定を止めないよう、すべて
//! 期限付きで待ち、期限を過ぎたらプロセスグループごと終了させる。bash の子（ai-usage）や
//! その子まで残さないためで、直接の子だけを殺すと孫が stdout を握ったまま残り、EOF を
//! 待つ読み取りが終わらない。
//!
//! 別のプロセスグループにしたぶん、tmux がペインを閉じたときの SIGHUP はこれらの子に届かない
//! （bash 版のモニターでは子が同じグループにいたので、ペインと一緒に止まっていた）。そのため
//! 起動中のグループを [`RUNNING`] に記録し、モニターが終わるときに [`terminate_all`] で止める。

use std::process::{ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// 読み取る stdout の上限。ai-usage の出力は数 KB なので、壊れたコマンドが大量に出力しても
/// メモリを使い切らないための保険。
const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
/// SIGTERM から SIGKILL までの猶予。
const TERM_GRACE: Duration = Duration::from_secs(1);
/// SIGKILL 後に終了を待つ時間。これを過ぎても終わらない（戻らない I/O で止まっている）
/// ときは、裏で回収を待ち、その間は次を起動しない。
const REAP_GRACE: Duration = Duration::from_secs(2);

/// 起動中のプロセスグループ（期限切れで止めた後の回収待ちを含む）。
static RUNNING: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// [`RUNNING`] への登録。プロセスを回収し終えたら（このガードを捨てたら）外す。
struct Registration(Option<i32>);

impl Registration {
    fn new(pid: Option<u32>) -> Self {
        let pid = pid.and_then(|pid| i32::try_from(pid).ok());
        if let Some(pid) = pid
            && let Ok(mut running) = RUNNING.lock()
        {
            running.push(pid);
        }
        Self(pid)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(pid) = self.0
            && let Ok(mut running) = RUNNING.lock()
        {
            running.retain(|&running| running != pid);
        }
    }
}

/// 起動中のコマンドをプロセスグループごと止める。モニターが終わる直前に呼ぶ（終了処理は
/// `std::process::exit` で、Drop も `kill_on_drop` も動かないため）。
pub fn terminate_all() {
    let groups: Vec<i32> = match RUNNING.lock() {
        Ok(running) => running.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    terminate_groups(&groups);
}

fn terminate_groups(groups: &[i32]) {
    if groups.is_empty() {
        return;
    }
    for &pid in groups {
        signal_group(u32::try_from(pid).ok(), Signal::Term);
    }
    // ai-usage はキャッシュを一時ファイルからの rename で書くので、途中で止めても壊れない。
    // TERM で片付く猶予を少しだけ置いてから、残りを止める。
    std::thread::sleep(Duration::from_millis(200));
    for &pid in groups {
        signal_group(u32::try_from(pid).ok(), Signal::Kill);
    }
}

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

#[derive(Debug)]
pub enum RunError {
    /// 起動に失敗した。
    Spawn(std::io::Error),
    /// 期限内に終わらなかった。
    TimedOut,
    /// 出力の読み取りか終了の待機に失敗した。
    Io(std::io::Error),
    /// 前回の期限切れのプロセスがまだ終了していない。増やさないために起動しない。
    StillRunning,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(error) => write!(f, "failed to start: {error}"),
            RunError::TimedOut => f.write_str("timed out"),
            RunError::Io(error) => write!(f, "failed to read the output: {error}"),
            RunError::StillRunning => f.write_str("the previous run has not exited yet"),
        }
    }
}

/// 同じ種類のコマンドを繰り返し起動する。期限切れで終了させたプロセスがまだ回収できて
/// いない間は、次を起動しない（戻らない I/O で止まったプロセスを増殖させない）。
#[derive(Default)]
pub struct Runner {
    reaping: Option<JoinHandle<()>>,
}

impl Runner {
    pub async fn run(&mut self, command: &str, timeout: Duration) -> Result<Output, RunError> {
        if self
            .reaping
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Err(RunError::StillRunning);
        }
        self.reaping = None;

        let mut child = spawn(command).map_err(RunError::Spawn)?;
        let pid = child.id();
        let registration = Registration::new(pid);
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let finished = tokio::time::timeout(timeout, async {
            let mut bytes = Vec::new();
            (&mut stdout)
                .take(MAX_OUTPUT)
                .read_to_end(&mut bytes)
                .await?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, bytes))
        })
        .await;
        match finished {
            Ok(Ok((status, stdout))) => Ok(Output { status, stdout }),
            Ok(Err(error)) => {
                terminate(&mut child, pid).await;
                Err(RunError::Io(error))
            }
            Err(_) => {
                if !terminate(&mut child, pid).await {
                    self.reaping = Some(tokio::spawn(async move {
                        let _ = child.wait().await;
                        drop(registration);
                    }));
                }
                Err(RunError::TimedOut)
            }
        }
    }
}

fn spawn(command: &str) -> std::io::Result<Child> {
    let mut builder = Command::new("bash");
    builder
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // 診断はペインに出さない（ratatui の描画を壊す）。失敗は終了コードと出力で判断する。
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // 子を新しいプロセスグループの先頭にして、期限切れのときに孫までまとめて終了させる。
    #[cfg(unix)]
    builder.process_group(0);
    builder.spawn()
}

/// プロセスグループへ SIGTERM、猶予の後に SIGKILL を送り、直接の子の終了を待つ。
/// 待つ時間内に終わったら true。
async fn terminate(child: &mut Child, pid: Option<u32>) -> bool {
    signal_group(pid, Signal::Term);
    if tokio::time::timeout(TERM_GRACE, child.wait()).await.is_ok() {
        // 子が終わっても、孫が同じグループに残っていれば止める。
        signal_group(pid, Signal::Kill);
        return true;
    }
    signal_group(pid, Signal::Kill);
    let _ = child.start_kill();
    tokio::time::timeout(REAP_GRACE, child.wait()).await.is_ok()
}

enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_group(pid: Option<u32>, signal: Signal) {
    let Some(pid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) else {
        return;
    };
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: pid は process_group(0) で作った自分の子のプロセスグループ ID。負値で
    // グループ全体を指定する。グループが既に無ければ ESRCH が返るだけで害は無い。
    unsafe {
        libc::kill(-pid, signal);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: Option<u32>, _signal: Signal) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn returns_the_output_of_a_fast_command_without_waiting() {
        let start = Instant::now();
        let output = Runner::default()
            .run("printf ready", Duration::from_secs(10))
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn propagates_a_nonzero_exit_code() {
        let output = Runner::default()
            .run("echo partial; exit 3", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"partial\n");
    }

    #[tokio::test]
    async fn stops_a_hanging_command_and_its_children_at_the_deadline() {
        // 孫（バックグラウンドの sleep）が stdout を握っていても、期限で戻る。
        let tmp = tempfile::TempDir::new().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let command = format!(
            "sleep 30 & echo $! > '{}'; wait",
            pid_file.to_string_lossy()
        );
        let start = Instant::now();
        let result = Runner::default()
            .run(&command, Duration::from_millis(500))
            .await;
        assert!(matches!(result, Err(RunError::TimedOut)), "{result:?}");
        assert!(start.elapsed() < Duration::from_secs(8));
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // 孫もグループごと終了している（回収は init が行うので、少し待って確かめる）。
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: シグナル 0 は存在確認だけで、プロセスに影響しない。
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(Instant::now() < deadline, "grandchild {pid} survived");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn running_groups_are_recorded_until_reaped_and_can_be_stopped_at_exit() {
        // 登録表はテスト全体で共有されるので、自分のグループだけを止めて確かめる
        // （terminate_all を呼ぶと、並行するほかのテストのコマンドまで止めてしまう）。
        let tmp = tempfile::TempDir::new().unwrap();
        let pid_file = tmp.path().join("group.pid");
        let command = format!(
            "echo $$ > '{}'; sleep 30 & wait",
            pid_file.to_string_lossy()
        );
        let task = tokio::spawn(async move {
            Runner::default()
                .run(&command, Duration::from_secs(60))
                .await
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let pid: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the command did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(RUNNING.lock().unwrap().contains(&pid));
        tokio::task::spawn_blocking(move || terminate_groups(&[pid]))
            .await
            .unwrap();
        let output = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the stopped command must end")
            .unwrap()
            .unwrap();
        assert!(!output.status.success());
        assert!(!RUNNING.lock().unwrap().contains(&pid));
    }
}
