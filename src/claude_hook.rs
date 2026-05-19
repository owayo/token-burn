//! Claude Code の `Stop` / `StopFailure` hook 受け口。
//!
//! `claude --settings <file>` で渡された task-settings.json は、Stop / StopFailure 両方の hook で
//! `token-burn claude-hook --outcome <path>` を呼ぶよう設定されている。本サブコマンドは stdin から
//! hook の JSON ペイロードを読み取り、`<path>` にアトミックに書き出す。後続の
//! `classify-claude-outcome` がこのファイルを読んで分類する。

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

/// stdin から hook JSON を読み、`outcome_path` に書き出す。
///
/// `outcome_path` の親ディレクトリは事前に存在する想定（executor が `marker_dir` を作成済み）。
/// 書き込みは tmpfile + rename のアトミックパターンで行い、競合読み込みでの壊れた JSON を防ぐ。
/// Stop hook では exit code が読まれるが、StopFailure では無視されるため、エラー時も exit 0 で抜ける。
/// hook 失敗で claude プロセスを止めないことを優先する（落ちると classify が hook 不在で failed 扱いになる）。
pub fn run(outcome_path: &Path) -> Result<()> {
    let mut buf = Vec::with_capacity(4096);
    std::io::stdin()
        .read_to_end(&mut buf)
        .context("Failed to read hook JSON from stdin")?;

    // 空入力でも outcome ファイルは作る（classify 側で「hook 発火したが中身なし」を検知できる）。
    write_atomic(outcome_path, &buf)
        .with_context(|| format!("Failed to write outcome: {}", outcome_path.display()))?;

    Ok(())
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.exists() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create parent dir: {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, content)
        .with_context(|| format!("Failed to write tmp: {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_atomic_creates_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested/dir/outcome.json");
        write_atomic(&path, b"{\"hook_event_name\":\"Stop\"}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"hook_event_name\":\"Stop\"}"
        );
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("outcome.json");
        std::fs::write(&path, "old content").unwrap();
        write_atomic(&path, b"new content").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }

    #[test]
    fn write_atomic_with_empty_content_creates_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("outcome.json");
        write_atomic(&path, b"").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }
}
