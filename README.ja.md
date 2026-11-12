<p align="center">
  <img src="docs/images/app.png" width="128" alt="token-burn">
</p>

<h1 align="center">token-burn</h1>

<p align="center">
  リセット前の Claude Code / Codex CLI の残りトークンで、複数のリポジトリにプロンプトを並列実行する CLI
</p>

<!-- standard:badges:start -->
<h3 align="center">対応プラットフォーム</h3>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/token-burn/actions/workflows/ci.yml"><img src="https://github.com/owayo/token-burn/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/token-burn/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/token-burn" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/token-burn" alt="License"></a>
</p>

<p align="center">
  <a href="README.md">English</a> |
  <a href="README.ja.md">日本語</a>
</p>
<!-- standard:badges:end -->

---

**token-burn** は、週次リセット前の Claude Code / Codex CLI の残りトークンで、リポジトリ群にプロンプトを並列実行する CLI です。「もったいない」精神で、コードレビュー、バグ修正、リファクタリング、テスト改善などに活用します。リセット時刻が来ると、新しいタスクの開始を止め、実行中のタスクが終わるのを待ちます。

[ai-usage](https://github.com/owayo/ai-usage) と連携すると、アカウントごとの実際のリセット時刻と使用率を取得し、実行するエージェントと実行期限を自動で決められます。曜日・時刻を指定する固定スケジュールにも対応しています。

<p align="center"><img src="docs/images/screenshot.png" width="800" alt="token-burn 実行中"></p>

## 機能

- ai-usage から実際のリセット時刻を取得し、実行期限を自動管理します。週次枠・5 時間枠・最も近い枠を選べます。取得に失敗した場合の動作は、固定スケジュール・対象エージェントの除外・エラー終了から指定できます。
- Claude Code、Codex CLI、任意のエージェントを並列実行できます。Claude と Codex には、無人実行と出力整形に必要なフラグを自動で付与します。
- 分割 TUI のワーカー見出しに対象パスを表示します。モニターには、取得できたモデルと effort、連携時の使用率、CPU・メモリのグラフ、スワップ、ディスクの空き容量が並びます。
- 実行中に `a` で対象ディレクトリを追加できます。`d` でデタッチしても処理は続き、`token-burn attach` で再接続できます。
- `run -i` で対象と実行順を選び、ワーカー数・停止閾値・ドライランも変更してから実行できます。
- `token-burn config` でエージェント、プロンプト、探索先、ai-usage 連携を編集し、保存前に検証します。
- 複数の探索先から Git リポジトリを集め、最終変更が古い順に処理します。最近の処理をスキップする期間と、公開リポジトリを優先するかどうかを設定できます。
- 複数アカウントの環境変数と履歴を分離できます。`dedup_scope` を使うと、アカウント間や同じプロバイダー内で処理済み履歴を共有できます。
- 長い周期の利用枠が閾値に達したら後続タスクを停止し、短い周期の枠ならリセットまで一時停止します。連携時は手動リセット後の回復を確認して早めに再開でき、レート制限で中断した Claude Code セッションも次回の実行で引き継げます。
- Claude と Codex の出力を整形し、ツール呼び出し、サブエージェント、使用トークン、コスト、エラーを表示します。タスクごとのログを保存し、失敗理由は `errors.txt` にまとめます。実行計画とモニターのコマンド表示では、環境変数代入と認証オプションの値を伏せます。

## 動作環境

- Windows は Windows 10 バージョン 1809 以降、または Windows 11 の x64 環境と [Git for Windows](https://gitforwindows.org/) が必要です。Windows Terminal や PowerShell から実行できます。Bash の探索先とパスの設定は [Windows での実行](docs/windows.ja.md) を参照してください。
- Claude Code や Codex CLI など、実行するエージェントの CLI が必要です。
- 探索設定に `username` を指定して GitHub のリポジトリ情報と可視性を取得する場合は、`gh` CLI も使います。
- リセット時刻の自動取得と使用率の確認には [ai-usage](https://github.com/owayo/ai-usage) が必要です。固定スケジュールだけで使う場合は省略できます。

## インストール

<!-- standard:install:start -->
### Homebrew (macOS)

```bash
brew install owayo/token-burn/token-burn
```

### Cargo

Rust 1.99 以上が必要です。

```bash
cargo install --git https://github.com/owayo/token-burn --locked
```

### GitHub Releases から

[Releases](https://github.com/owayo/token-burn/releases/latest) から自分の環境のアーカイブを取得して展開し、`token-burn` を `PATH` の通った場所に置きます。各リリースには、取得したファイルを確かめるための `SHA256SUMS` も添付しています。

| プラットフォーム | ファイル |
|---|---|
| macOS (Intel) | `token-burn-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `token-burn-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `token-burn-x86_64-pc-windows-msvc.zip` |

macOS でブラウザから取得した場合は、実行の前に隔離属性を外します: `xattr -d com.apple.quarantine token-burn`。

### ソースから

[mise](https://mise.jdx.dev/) が必要です (Rust のツールチェーンは `mise.toml` で固定しています)。

```bash
git clone https://github.com/owayo/token-burn.git
cd token-burn
make install
```

`make install` は `/usr/local/bin` に入れます。場所を変えるときは `INSTALL_PATH` を指定します (例: `make install INSTALL_PATH="$HOME/.local/bin"`)。
<!-- standard:install:end -->

## クイックスタート

設定画面を開きます。設定ファイルが無ければ新規作成、あれば既存の設定を編集します。

```bash
token-burn config
```

エージェントのコマンドと探索先を設定し、[ai-usage 連携](#ai-usage-でリセット時刻を自動管理する)か固定スケジュールを選びます。`s` で変更を確認して保存してください。固定スケジュールを使う場合は、仮の月曜 09:00 UTC を実際の曜日・時刻・タイムゾーンに変更します。初めて実行する前に、リセット時刻と取得元、実行計画を確かめます。

```bash
token-burn status
token-burn run -n
```

TOML を直接編集する場合は、`token-burn init` で設定とプロンプトファイルの雛形を生成できます。設定画面の操作は [設定 TUI](docs/configuration.ja.md#設定-tui) を参照してください。

## 使い方

```bash
# トークン消費を実行する (最大 `limit` 件のターゲット)
token-burn run

# 先に TUI で対象と実行順を選ぶ
token-burn run -i

# デタッチした実行へ再接続する
token-burn attach

# 特定のリポジトリだけを実行する (スキャンとスキップの規則を使わない)
token-burn run ~/GitHub/repo-a ./repo-b

# 何も実行せずに、全ターゲットを処理する順に一覧する
token-burn list

# 3 日より古いレポートディレクトリを削除する
token-burn clean --older-than 3d
```

実行画面の通常操作で使う主なキーです。

| キー | 操作 |
|---|---|
| `Tab` / `Shift-Tab` | ペインを選ぶ |
| `z` | 選択したペインを拡大・元に戻す |
| `a` | ディレクトリを追加する。Enter で確定、Esc で取り消す |
| `PageUp` / `PageDown` | ワーカーの出力をスクロールする |
| `i` / `Ctrl-G` | ワーカーの入力モードに入る・通常操作へ戻る |
| `d` / `q` | デタッチする。実行は継続する |
| `Ctrl-C` | 新しいタスクの開始を止め、実行中のタスクを待つ。再度押すと強制終了する |

ワーカーの見出しには対象パスを表示します。モニターではモデルと effort の取得値を確認でき、ai-usage 連携時は使用率も表示されます。マシンの負荷も同じ画面で確認できます。対象の追加と入力操作の詳細は [分割画面での実行](docs/usage.ja.md#分割画面での実行) を参照してください。

リセット時刻が来ると、token-burn は新しいタスクの開始を止め、実行中のタスクが終わるまで待ちます。

<p align="center"><img src="docs/images/deadline.png" width="800" alt="デッドライン到達 — タスク完了を待機中"></p>

詳しい説明:

- [CLI リファレンス](docs/cli-reference.ja.md): すべてのコマンドとオプション
- [使い方の詳細](docs/usage.ja.md): 処理順、中断セッションの再開、分割画面での実行、ログと状態
- [レート制限](docs/rate-limits.ja.md): 実行が止まる場合と、一時停止で済む場合
- [ライブモニター](docs/monitor.ja.md): ワーカーとモニターのペインに出る内容

## 設定

設定ファイルの既定の場所は `~/.config/token-burn/config.toml` です。別のファイルを使うときは `-c` / `--config` を渡します。`state.json`（処理済みのターゲット）と `resume.json`（中断したセッション）も同じディレクトリに置かれます。

### ai-usage でリセット時刻を自動管理する

`ai-usage --json` を実行し、使いたいアカウントの `profile` を確認します。次の例の `profile = "Work"` を、その値と大文字小文字まで一致させてください。この例では週次リセットの時刻を取得するので、`reset_weekday`・`reset_time`・`timezone` の指定は不要です。

```toml
[settings]
parallelism = 3
skip_within = "1d"
limit = 10

[prompts]
default = "Review this repository and improve its tests."

[ai_usage]
enabled = true
window = "weekly"
fallback = "error"

[[ai_usage.profiles]]
name = "work"
profile = "Work"

[[agents]]
name = "claude"
provider = "claude"
command = ["claude", "--dangerously-skip-permissions", "--model", "opus"]
[agents.ai_usage]
profiles = ["work"]

[[scan]]
base_dirs = ["~/GitHub"]
```

`name = "work"` は `profiles = ["work"]` から参照する名前で、`profile = "Work"` は ai-usage 上のアカウントを指定します。CLI のログイン先も同じアカウントにそろえてください。切り替えが必要な場合は、profile の `env` で `CLAUDE_CONFIG_DIR` や `CODEX_HOME` を指定します。

取得した時刻と取得元は `status` と `run` に表示されます。この例の `fallback = "error"` は、リセット時刻を取得できなければエラーで終了する設定です。固定スケジュールへ戻す場合は `fallback = "fixed"` に変更し、連携するエージェントに `reset_weekday`・`reset_time`・`timezone` を追加します。対象エージェントを除外する場合は `fallback = "skip"` を使います。複数 profile、アカウントごとの環境変数、使用する枠の指定は [ai-usage 連携](docs/configuration.ja.md#ai-usage-連携任意) で説明しています。

### 固定スケジュールを使う

ai-usage を使わない場合は、上の例から `[ai_usage]`・`[[ai_usage.profiles]]`・`[agents.ai_usage]` を削除し、エージェントの定義を次に置き換えます。

```toml
[[agents]]
name = "claude"
command = ["claude", "--dangerously-skip-permissions", "--model", "opus"]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "Asia/Tokyo"
```

`.md` で終わるプロンプトの値は Markdown ファイルを読み込みます。相対パスの基準は設定ファイルのディレクトリです。優先順位は `[[targets]].prompt` > `[[agents]].prompt` > `[prompts].default` です。

すべてのセクションと設定項目は [docs/configuration.ja.md](docs/configuration.ja.md) を参照してください。

## 開発

<!-- standard:dev:start -->
[mise](https://mise.jdx.dev/) が必要です。ツールの版は `mise.toml` で固定しています。

```bash
make setup   # ツールチェーン (mise) と依存を取得する
make ci      # CI と同じ検査 (書き換えない)
```

| コマンド | 説明 |
|---|---|
| `make setup` | ツールチェーン (mise) と依存を取得する |
| `make build` | デバッグ版をビルドする |
| `make release` | リリース版をビルドする |
| `make run` | デバッグ版を実行する (引数は ARGS="...") |
| `make test` | テストを実行する |
| `make lint` | clippy を警告ゼロで通す |
| `make fmt` | コードを整形する (書き換える) |
| `make fmt-check` | 整形済みかを確かめる (書き換えない) |
| `make check` | 整形と静的検査 (書き換えない) |
| `make ci` | CI と同じ検査 (書き換えない) |
| `make install` | リリース版を INSTALL_PATH (既定 /usr/local/bin) に入れる |
| `make uninstall` | INSTALL_PATH から取り除く |
| `make clean` | ビルド成果物を消す |

`make` でターゲットの一覧を表示します。リリースは GitHub Actions で行います (**Actions → Release → Run workflow**)。
<!-- standard:dev:end -->

## ライセンス

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
