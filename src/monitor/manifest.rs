//! モニターの起動情報。
//!
//! `execute_plan` が実行用の一時ディレクトリへ書き、tmux のモニターペインで動く
//! `token-burn monitor` が読む。引数で渡さないのは、ai-usage のコマンド列（env の前置きと
//! クォート済みのシェル文字列）を tmux の shell-command へもう一段埋め込むと、クォートが
//! 二重になって壊れやすいため。一時ディレクトリは 0700 なので、ほかのユーザーからは読めない。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// 表示用のエージェント名。
    pub agent: String,
    /// 表示用のコマンド（認証情報は伏せ済み）。
    pub command: String,
    /// 起動時の明示指定と、実行中セッションのモデル・effort を読む設定。
    #[serde(default)]
    pub inference: super::inference::Configuration,
    /// 表示用のリセット時刻とその導出元。
    pub reset: String,
    /// タスク数。
    pub total: usize,
    /// 実行全体のデッドライン（Unix epoch 秒）。ワーカーの gate-claim と同じ絶対時刻。
    pub deadline_epoch: i64,
    /// ワーカーの結果と、runner の worker-done-* を置くディレクトリ。
    pub marker_dir: PathBuf,
    /// tmux のセッション名（`=` を付けない素の名前）。
    pub session: String,
    /// 起動したワーカー数。
    pub worker_count: usize,
    /// 恒久停止のシグナル。一時停止のファイルはこの隣に置かれる。
    pub stop_file: PathBuf,
    /// この実行のログの置き場所。
    pub report_dir: PathBuf,
    /// 使用率の停止閾値（%）。ワーカーの format-stream / usage-gate と同じ値。
    /// 一時停止中に取り直した実データで、枠が回復したかを判定するのに使う。
    pub threshold: u8,
    /// ai-usage 連携が有効なときのコマンド。連携しない構成では ai-usage を一切呼ばない。
    pub ai_usage: Option<AiUsageCommands>,
}

/// `bash -c` で実行する ai-usage のコマンド（選択中のエージェントの env を前置き済み）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiUsageCommands {
    /// `ai-usage --json`。出力で statusline の表示用キャッシュを置き換える（停止の判定には
    /// 使わない。判定は `live` と同じく `--refresh` で取り直す）。
    pub refresh: String,
    /// キャッシュを `--input` で読む statusline。
    pub statusline: String,
    /// `ai-usage --disk --json`。ディスク容量は使用量のキャッシュに混ぜず、別に取る。
    pub disk: String,
    /// 実行中のアカウントだけを ai-usage の共有キャッシュを通さずに取り直すコマンド
    /// （`--refresh --profile=<profile> --only=<provider>` 付き）。一時停止中に、手動リセット等で
    /// 枠が戻ったかを確かめるのに使う。出力は 1 アカウント分なので、全アカウント分の
    /// キャッシュ（`cache_file`）を置き換えない。
    pub live: String,
    /// `ai-usage --json` の `profile`（実行中のアカウント）。
    pub profile: String,
    /// `ai-usage --json` の `provider`（実行中のアカウント）。
    pub provider: String,
    /// statusline が読む `ai-usage --json` のキャッシュ。
    pub cache_file: PathBuf,
}

impl Manifest {
    pub fn write(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_vec_pretty(self).context("failed to serialize the monitor manifest")?;
        std::fs::write(path, json)
            .with_context(|| format!("failed to write the monitor manifest {}", path.display()))
    }

    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read the monitor manifest {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse the monitor manifest {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_the_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("monitor.json");
        let manifest = Manifest {
            agent: "claude".into(),
            command: "claude -p".into(),
            inference: Default::default(),
            reset: "2026/10/27 09:00".into(),
            total: 3,
            deadline_epoch: 1_792_000_000,
            marker_dir: tmp.path().join("markers"),
            session: "token-burn".into(),
            worker_count: 2,
            stop_file: tmp.path().join("stop"),
            report_dir: tmp.path().join("report dir"),
            threshold: 95,
            ai_usage: Some(AiUsageCommands {
                refresh: "'ai-usage' '--json'".into(),
                statusline: "'ai-usage' '--statusline'".into(),
                disk: "'ai-usage' '--disk' '--json'".into(),
                live: "'ai-usage' '--json' '--refresh' '--profile=Work' '--only=claude'".into(),
                profile: "Work".into(),
                provider: "claude".into(),
                cache_file: tmp.path().join("cache.json"),
            }),
        };
        manifest.write(&path).unwrap();
        assert_eq!(Manifest::read(&path).unwrap(), manifest);
        let mut legacy = serde_json::to_value(&manifest).unwrap();
        legacy.as_object_mut().unwrap().remove("inference");
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(Manifest::read(&path).unwrap(), manifest);
    }

    #[test]
    fn unreadable_manifest_names_the_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("missing.json");
        let error = format!("{:#}", Manifest::read(&path).unwrap_err());
        assert!(error.contains("missing.json"), "{error}");
    }
}
