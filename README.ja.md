<p align="center">
  <img src="docs/images/app.png" width="128" alt="token-burn">
</p>

<h1 align="center">token-burn</h1>

<p align="center">
  <strong>週次リセット前にAIコーディングアシスタントのトークンを消費するCLIツール</strong>
</p>

<p align="center">
  <a href="https://github.com/owayo/token-burn/actions/workflows/ci.yml">
    <img alt="CI" src="https://github.com/owayo/token-burn/actions/workflows/ci.yml/badge.svg?branch=main">
  </a>
  <a href="https://github.com/owayo/token-burn/releases/latest">
    <img alt="Version" src="https://img.shields.io/github/v/release/owayo/token-burn">
  </a>
  <a href="LICENSE">
    <img alt="License" src="https://img.shields.io/github/license/owayo/token-burn">
  </a>
</p>

<p align="center">
  <a href="README.md">English</a> | 日本語
</p>

---

## 概要

Claude Code / Codex CLI のトークンは週次でリセットされますが、未使用分は繰り越されません。「もったいない」精神で、**token-burn** はリセット直前の残りトークンを有効活用します。コードレビュー、バグ修正、リファクタリング、テスト改善など、自由に定義したプロンプトをリポジトリ群に対して並列実行します。リセット時刻が来ると、実行中のプロセスは自動的に終了されます。

<p align="center">
  <img src="docs/images/screenshot.png" width="800" alt="token-burn 実行中">
</p>

<p align="center">
  <img src="docs/images/deadline.png" width="800" alt="デッドライン到達 — タスク完了を待機中">
</p>

## 特徴

- **自動探索**: ディレクトリをスキャンしてGitリポジトリを検出、remote URLのユーザー名でフィルタ
- **複数スキャンソース**: GitHub用、GitLab用など、スキャン設定を複数定義可能
- **可視性対応**: 公開リポジトリを優先的に処理（remote のリポジトリ名で照合）
- **マルチエージェント**: Claude Code、Codex CLI、カスタムエージェントに対応
- **スマートスケジューリング**: リセット期限が最も近いエージェントを自動選択
- **デッドライン制御**: リセット時刻到達時に全子プロセスを自動終了
- **並列実行**: tmuxペイン分割とプログレスモニター付きで複数プロンプトを同時実行
- **ログ衝突回避**: タスクごとのログに連番を付け、同名リポジトリでも上書きしない
- **プロンプトファイル**: `.md` ファイルまたはインライン文字列でプロンプトを指定可能
- **レジューム**: 処理済みディレクトリを自動スキップ、スキップ期間を設定可能
- **状態更新の競合対策**: 並列ワーカーが `state.json` をファイルロック付きで原子的に更新
- **ドライラン**: コマンドを実行せずに実行計画をプレビュー

## 動作環境

- **OS**: macOS
- **tmux**: ペイン分割実行に必要
- **Rust**: 1.85以上（ソースからビルドする場合）
- **gh CLI**: リポジトリ可視性の検出に必要
- **Claude Code** および/または **Codex CLI**: 少なくとも1つのエージェントが必要

## インストール

### Homebrew (macOS/Linux)

```bash
brew install owayo/token-burn/token-burn
```

### ソースからビルド

```bash
git clone https://github.com/owayo/token-burn.git
cd token-burn
make install
```

### バイナリダウンロード

[Releases](https://github.com/owayo/token-burn/releases) から最新バイナリをダウンロード。

#### macOS (Apple Silicon)

```bash
curl -L https://github.com/owayo/token-burn/releases/latest/download/token-burn-aarch64-apple-darwin.tar.gz | tar xz
sudo mv token-burn /usr/local/bin/
```

#### macOS (Intel)

```bash
curl -L https://github.com/owayo/token-burn/releases/latest/download/token-burn-x86_64-apple-darwin.tar.gz | tar xz
sudo mv token-burn /usr/local/bin/
```

## 使い方

### クイックスタート

```bash
# 設定ファイルとデフォルトプロンプトを生成
token-burn init

# エージェントのリセット状況を確認
token-burn status

# 実行計画のプレビュー
token-burn run -n

# トークン消費を実行
token-burn run
```

### コマンド

| コマンド | 説明 |
|---------|------|
| `run` | トークン消費を実行（デフォルト） |
| `status` | エージェントのリセット状況を表示 |
| `init` | 設定ファイルとプロンプトテンプレートを生成 |
| `clean` | 古いレポートディレクトリを削除 |

### オプション

| オプション | 短縮形 | 説明 |
|-----------|-------|------|
| `--config <PATH>` | `-c` | 設定ファイルパス（デフォルト: `~/.config/token-burn/config.toml`） |
| `--agent <NAME>` | | エージェントを強制指定 |
| `--dry-run` | `-n` | 実行せずにプレビュー |
| `--fresh` | | 保存済み状態を無視して全ターゲットを処理 |
| `--limit <N>` | `-l` | 処理するターゲット数の上限 |
| `--no-limit` | | 上限なしですべてのターゲットを処理 |
| `--public-only` | | 公開リポジトリとして判定されたもののみ処理 |
| `--help` | `-h` | ヘルプ表示 |
| `--version` | `-V` | バージョン表示 |

`init` は `--force`（`-f`）で既存ファイルを確認なしで上書きできます。

`clean` は `--older-than` で `cleanup_after` の設定値を一時的に変更できます（例: `--older-than 3d`）。

## 設定

デフォルトの設定ファイルパス: `~/.config/token-burn/config.toml`

`token-burn init` で設定テンプレートを生成してください。

### 基本設定

```toml
[settings]
parallelism = 3
skip_within = "7d"    # 任意
```

| フィールド | 説明 | 例 |
|-----------|------|-----|
| `parallelism` | 並列実行数 | `3` |
| `skip_within` | この期間以内に処理済みならスキップ | `"7d"`, `"24h"`, `"1d12h"` |
| `cleanup_after` | この期間より古いレポートディレクトリを自動削除 | `"7d"`（デフォルト） |
| `report_dir` | 実行ログの保存先ディレクトリ | `~/Documents/token-burn`（デフォルト） |

`skip_within` には期間文字列を指定します: `d`（日）、`h`（時間）、`m`（分）、`s`（秒）。省略時は前回リセット以降に処理済みのターゲットをスキップします。過大な値はエラーになります。`--fresh` を指定すると保存済み状態を無視して全ターゲットを処理します。

状態ファイル: `<config-dir>/state.json`（有効な設定ファイルと同じディレクトリ。並列実行時の取りこぼしを防ぐため原子的に更新）。デフォルト設定パスの場合は `~/.config/token-burn/state.json`。

### エージェント

```toml
[[agents]]
name = "claude"
command = ["claude", "-p", "--dangerously-skip-permissions", "--model", "opus"]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "Asia/Tokyo"
prompt = "prompts/test-coverage.md"  # 任意

[[agents]]
name = "codex"
command = ["codex", "exec", "--full-auto", "-c", "model='gpt-5.3-codex'", "-c", "model_reasoning_effort='xhigh'"]
reset_weekday = "thursday"
reset_time = "09:00"
timezone = "Asia/Tokyo"
# prompt = "prompts/codex.md"
```

| フィールド | 説明 | 例 |
|-----------|------|-----|
| `name` | エージェント識別名 | `"claude"` |
| `command` | コマンドと引数 | `["claude", "-p"]` |
| `reset_weekday` | リセット曜日 | `"monday"` |
| `reset_time` | リセット時刻（HH:MM） | `"09:00"` |
| `timezone` | IANAタイムゾーン | `"Asia/Tokyo"` |
| `prompt` | エージェント固有プロンプト（任意） | `"prompts/test-coverage.md"` |

`name` は空文字不可です。`command` は1要素以上を指定し、先頭要素には空でない実行ファイル名を指定してください。`prompt` を指定するとグローバルの `[prompts].default` の代わりに使われます。ターゲット固有の `prompt` が最優先です。

**プロンプト優先順位**: `[[targets]].prompt` > `[[agents]].prompt` > `[prompts].default`

**Claude 必須フラグの自動付与**: コマンドの実行ファイルが `claude` の場合、ログ出力と進捗モニタリングに必要な `--verbose`、`--output-format stream-json`、`--include-partial-messages` が必ず有効化されます。未指定フラグは自動追加され、既存の `--output-format` 値（`--output-format=...` 形式を含む）は `stream-json` に正規化されます。設定ファイルへの記述は不要です。

`reset_weekday` に指定可能な値: `monday` `tuesday` `wednesday` `thursday` `friday` `saturday` `sunday`（短縮形: `mon` `tue` `wed` `thu` `fri` `sat` `sun`）

### 自動スキャン（複数定義可）

```toml
[[scan]]
base_dirs = ["~/GitHub"]
username = "owayo"
public_first = true
exclude = ["archived-project"]

[[scan]]
base_dirs = ["~/git"]
username = "owayo"
recursive = true
public_first = false
```

| フィールド | 説明 | デフォルト |
|-----------|------|-----------|
| `base_dirs` | Gitリポジトリを探索するディレクトリ | （必須） |
| `username` | remote URLのオーナーがこのユーザー名と一致するリポジトリのみ対象にする | （なし — 全リポジトリ対象） |
| `public_first` | 公開リポジトリを優先的に先に処理する | `true` |
| `recursive` | サブディレクトリを再帰的に探索してネストされたGitリポジトリを検出する | `false` |
| `exclude` | スキャン時にスキップするディレクトリ名 | `[]` |

`username` を指定した場合、可視性判定は各リポジトリの `origin` remote URL から取得したリポジトリ名（大文字小文字を無視）で行われます。ローカルのディレクトリ名は一致している必要がありません。

### プロンプト

`.md` で終わる値はファイルパスとして読み込まれます。相対パスは設定ファイルのディレクトリから解決されます。

```toml
[prompts]
default = "prompts/default.md"
```

### 個別ターゲット（スキャン結果とマージ）

```toml
[[targets]]
directory = "~/GitHub/important-project"
prompt = "prompts/test-coverage.md"
```

| フィールド | 説明 |
|-----------|------|
| `directory` | ターゲットディレクトリのパス（必須）。既存のディレクトリを指定 |
| `prompt` | このターゲット専用のプロンプト。省略時は `[prompts].default` を使用 |

スキャン結果と同じディレクトリの場合、個別ターゲットの設定が優先されます。

## 開発

```bash
# ビルド
make build

# テスト実行
make test

# clippy とフォーマットチェック
make check

# リリースビルド
make release
```

## ライセンス

[MIT](LICENSE)
