//! ai-usage の使用量の表示を 10 秒ごとに更新する。
//!
//! `ai-usage --json` の出力でキャッシュを置き換え、その後 statusline を `--input <cache>` で
//! 描かせる。キャッシュは usage-gate と共有しているため、モニターの更新が並列ワーカーの
//! 使用率の判定にも効く。取得に失敗したら直前の表示を残す（fail-soft）。

use std::path::Path;
use std::time::Duration;

use ratatui::text::Line;
use tokio::sync::mpsc;

use super::Event;
use super::manifest::AiUsageCommands;
use super::process::Runner;

/// 取得が終わってから次の取得を始めるまでの間隔。間隔は開始ではなく完了から数える。
/// 開始から数えると、取得が間隔より長くかかったときに休みなく取得し続ける。
pub const INTERVAL: Duration = Duration::from_secs(10);
/// 1 回のコマンドの期限。
pub const TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(commands: AiUsageCommands, events: mpsc::Sender<Event>) {
    let mut refresh = Runner::default();
    let mut statusline = Runner::default();
    loop {
        refresh_cache(&mut refresh, &commands.refresh, &commands.cache_file).await;
        if let Some(lines) = render_statusline(&mut statusline, &commands.statusline).await
            && events.send(Event::Usage(lines)).await.is_err()
        {
            return;
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

/// `ai-usage --json` の出力でキャッシュを置き換える。JSON として読めない出力では置き換えない
/// （usage-gate が壊れたキャッシュを読んで止まるのを防ぐ）。
async fn refresh_cache(runner: &mut Runner, command: &str, cache_file: &Path) {
    let Ok(output) = runner.run(command, TIMEOUT).await else {
        return;
    };
    if !output.status.success()
        || serde_json::from_slice::<serde_json::Value>(&output.stdout).is_err()
    {
        return;
    }
    let _ = write_atomically(cache_file, &output.stdout);
}

async fn render_statusline(runner: &mut Runner, command: &str) -> Option<Vec<Line<'static>>> {
    let output = runner.run(command, TIMEOUT).await.ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(super::ansi::parse_lines(&output.stdout))
}

/// 同じディレクトリの一時ファイルへ書いてから置き換える。読み手（statusline・usage-gate）が
/// 書きかけの JSON を読まないようにするため。一時ファイル名にプロセス ID を入れ、
/// usage-gate が同時に書く一時ファイルと衝突させない。
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ai-usage-cache.json".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.monitor.{}", std::process::id()));
    if let Err(error) = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
    }

    #[tokio::test]
    async fn refresh_replaces_the_cache_only_with_valid_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache.json");
        std::fs::write(&cache, r#"{"old":true}"#).unwrap();
        let mut runner = Runner::default();

        refresh_cache(&mut runner, "printf 'not json'", &cache).await;
        assert_eq!(std::fs::read_to_string(&cache).unwrap(), r#"{"old":true}"#);

        refresh_cache(&mut runner, r#"printf '{"new":1}'; exit 1"#, &cache).await;
        assert_eq!(std::fs::read_to_string(&cache).unwrap(), r#"{"old":true}"#);

        refresh_cache(&mut runner, r#"printf '{"new":1}'"#, &cache).await;
        assert_eq!(std::fs::read_to_string(&cache).unwrap(), r#"{"new":1}"#);
        // 一時ファイルを残さない。
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(leftovers, [std::ffi::OsString::from("cache.json")]);
    }

    #[tokio::test]
    async fn statusline_keeps_the_previous_display_on_failure() {
        let mut runner = Runner::default();
        assert!(render_statusline(&mut runner, "exit 1").await.is_none());
        assert!(render_statusline(&mut runner, "true").await.is_none());
        let lines = render_statusline(&mut runner, r"printf '\033[38;5;35m a\033[0m\n b\n'")
            .await
            .unwrap();
        assert_eq!(lines.len(), 2);
    }

    #[tokio::test]
    async fn the_loop_sends_the_statusline_after_refreshing_the_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = tmp.path().join("cache.json");
        let commands = AiUsageCommands {
            refresh: r#"printf '{"accounts":[]}'"#.to_string(),
            // statusline はキャッシュを読む。更新後の中身が出ることで順序を確かめる。
            statusline: format!("cat {}", quote(&cache)),
            disk: String::new(),
            cache_file: cache.clone(),
        };
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(run(commands, tx));
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        task.abort();
        let Event::Usage(lines) = event else {
            panic!("unexpected event");
        };
        assert_eq!(lines[0].spans[0].content, r#"{"accounts":[]}"#);
    }
}
