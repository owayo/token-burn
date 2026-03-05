use anyhow::{Context, Result};
use colored::Colorize;
use std::io::{self, Write};
use std::path::Path;

const DEFAULT_CONFIG: &str = r#"[settings]
parallelism = 3                    # 並列実行数
skip_within = "1d"                 # この期間以内に処理済みならスキップ（例: "7d", "24h", "1d12h"）
# report_dir = "~/Documents/token-burn"  # ログ出力先（デフォルト: ~/Documents/token-burn）
cleanup_after = "7d"               # この期間より古いレポートディレクトリを自動削除
limit = 10                         # 1回の実行で処理する最大ターゲット数

[prompts]
default = "prompts/default.md"     # .md で終わる値はファイルパスとして読み込み
# default = "prompts/default.ja.md"

# ---- エージェント定義 ----

[[agents]]
name = "claude"
command = ["claude", "-p", "--dangerously-skip-permissions", "--model", "opus"]
reset_weekday = "monday"           # リセット曜日
reset_time = "09:00"               # リセット時刻
timezone = "Asia/Tokyo"
# prompt = "prompts/test-coverage.md"  # エージェント固有プロンプト（省略時は [prompts].default を使用）

[[agents]]
name = "codex"
command = ["codex", "exec", "--full-auto", "-c", "model='gpt-5.3-codex'", "-c", "model_reasoning_effort='xhigh'"]
reset_weekday = "thursday"
reset_time = "09:00"
timezone = "Asia/Tokyo"
# prompt = "prompts/codex.md"

# ---- ディレクトリ自動スキャン設定（複数定義可） ----

[[scan]]
base_dirs = ["~/GitHub"]           # スキャン対象のベースディレクトリ
recursive = false                  # true: ネストされたサブディレクトリも再帰的に探索
username = "owayo"                 # このユーザーのリポジトリのみ対象（remote URLで判定）
public_first = true                # 公開リポジトリを優先的に処理
exclude = []                       # 除外するディレクトリ名

# [[scan]]
# base_dirs = ["~/git"]
# username = "owayo"
# public_first = false

# ---- 個別ターゲット（任意、スキャン結果とマージ） ----

# [[targets]]
# directory = "~/GitHub/important-project"
# prompt = "prompts/test-coverage.md"
"#;

const DEFAULT_PROMPT: &str = r#"Thoroughly review the entire codebase of this project.

## Policy

**Only fix definite bugs.** Do not make changes based on speculation or stylistic preference.

## Review Perspectives

Analyze deeply from every perspective below:

### Correctness & Logic
- Off-by-one errors, missed boundary conditions
- Type mismatches, unintended behavior from implicit conversions
- Race conditions, potential deadlocks
- Unhandled nil/null/undefined paths

### Security
- Injection vulnerabilities (SQL, command, XSS)
- Authentication/authorization bypass possibilities
- Sensitive data exposure (logs, error messages)
- Unsafe deserialization

### Error Handling
- Swallowed exceptions/errors
- Paths that cause panics or crashes
- Resource leaks (files, connections, memory)

### Performance
- O(n^2) or worse complexity that can be improved
- N+1 queries
- Unnecessary memory allocations

### Test Coverage
- Identify critical logic/functions with no tests
- Find missing boundary, error-path, and edge-case tests
- Write and add the missing test cases

## Output Rules

1. For each issue found, explain why it is definitively a bug
2. Describe the specific reproduction condition or triggering scenario
3. Provide the fix
4. Do not fix "probably" or "might be" level issues — flag them as observations only
5. Do not suggest style changes or refactoring

## Documentation Update

After completing bug fixes and test additions, update documentation to reflect any changes.
Fix any gaps, omissions, or inaccuracies in existing documentation.

Target files:
- `AGENTS.md` or `CLAUDE.md` — If one is a symlink to the other, only edit the real file (the symlink target). Do not edit both.
- `README.md` and other README variants (e.g., `README.ja.md`)
"#;

const DEFAULT_PROMPT_JA: &str = r#"このプロジェクトのコードベース全体を徹底的にレビューしてください。

## 方針

**確実なバグのみ修正する。** 推測や好みに基づく変更は行わない。

## レビュー観点

以下のすべての観点から、時間をかけて深く分析してください:

### 正確性・ロジック
- オフバイワンエラー、境界条件の見落とし
- 型の不整合、暗黙の型変換による意図しない挙動
- 競合状態、デッドロックの可能性
- nil/null/undefined の未処理パス

### セキュリティ
- インジェクション（SQL, コマンド, XSS）
- 認証・認可のバイパス可能性
- 機密情報の露出（ログ、エラーメッセージ）
- 安全でないデシリアライズ

### エラーハンドリング
- 握りつぶされている例外・エラー
- パニック/クラッシュを引き起こすパス
- リソースリーク（ファイル、コネクション、メモリ）

### パフォーマンス
- O(n²) 以上の計算量で改善可能なもの
- N+1 クエリ
- 不要なメモリアロケーション

### テストカバレッジ
- テストが存在しない重要なロジック・関数を特定する
- 境界値、異常系、エッジケースのテストが不足している箇所
- 不足しているテストケースを実際に追加する

## 出力ルール

1. 問題を発見したら、なぜそれがバグと断言できるのか根拠を示す
2. 再現条件または発生シナリオを具体的に説明する
3. 修正コードを提示する
4. 「おそらく」「かもしれない」レベルのものは修正せず、指摘のみに留める
5. スタイルの好みやリファクタリング提案は行わない

## ドキュメント更新

バグ修正・テスト追加の完了後、変更内容を反映してドキュメントを更新する。
ドキュメントに抜け・漏れ・誤りがあれば修正する。

対象ファイル:
- `AGENTS.md` または `CLAUDE.md` — 一方が他方へのシンボリックリンクの場合、実体ファイル（リンク先）のみを編集する。両方は編集しない。
- `README.md` およびその他の README（例: `README.ja.md`）
"#;

pub fn run_init(config_path: &Path, force: bool) -> Result<()> {
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let prompts_dir = config_dir.join("prompts");

    std::fs::create_dir_all(&prompts_dir)
        .with_context(|| format!("Failed to create directory: {}", prompts_dir.display()))?;

    write_file(config_path, DEFAULT_CONFIG, force)?;

    let prompt_path = prompts_dir.join("default.md");
    write_file(&prompt_path, DEFAULT_PROMPT, force)?;

    let prompt_ja_path = prompts_dir.join("default.ja.md");
    write_file(&prompt_ja_path, DEFAULT_PROMPT_JA, force)?;

    println!();
    println!(
        "Edit {} to configure your settings.",
        config_path.display().to_string().cyan()
    );

    Ok(())
}

fn write_file(path: &Path, content: &str, force: bool) -> Result<()> {
    if path.exists() {
        if !force && !confirm_overwrite(path)? {
            println!("{} {} (skipped)", "Skip:".yellow(), path.display());
            return Ok(());
        }
        std::fs::write(path, content)
            .with_context(|| format!("Failed to write: {}", path.display()))?;
        println!("{} {}", "Overwritten:".yellow().bold(), path.display());
    } else {
        std::fs::write(path, content)
            .with_context(|| format!("Failed to write: {}", path.display()))?;
        println!("{} {}", "Created:".green().bold(), path.display());
    }
    Ok(())
}

fn confirm_overwrite(path: &Path) -> Result<bool> {
    eprint!(
        "Config file already exists at {}. Overwrite? [y/N]: ",
        path.display()
    );
    io::stderr().flush().context("Failed to flush stderr")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Failed to read input")?;

    let input = input.trim().to_lowercase();
    Ok(input == "y" || input == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn run_init_creates_config_and_prompts() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        run_init(&config_path, false).expect("init should succeed");

        assert!(config_path.exists(), "config.toml が作成されるべき");
        assert!(
            tmp.path().join("prompts/default.md").exists(),
            "default.md が作成されるべき"
        );
        assert!(
            tmp.path().join("prompts/default.ja.md").exists(),
            "default.ja.md が作成されるべき"
        );

        // 設定ファイルの内容が有効な TOML であることを確認
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("[settings]"));
        assert!(content.contains("[[agents]]"));
    }

    #[test]
    fn run_init_overwrites_with_force() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "original").unwrap();

        run_init(&config_path, true).expect("init with force should succeed");

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("[settings]"),
            "force=true では上書きされるべき"
        );
    }
}
