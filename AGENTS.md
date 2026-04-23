# token-burn

週次リセット前にAIコーディングアシスタントのトークンを消費するCLIツール。

## プロジェクト構成

```
token-burn/
├── Cargo.toml              # 依存クレート定義
├── src/
│   ├── main.rs             # エントリポイント、clap CLI定義
│   ├── init.rs             # config/prompt 雛形の初期化
│   ├── config.rs           # TOML設定ファイルの読み込み・バリデーション
│   ├── scanner.rs          # ディレクトリスキャン・リポジトリ探索・gh CLI連携
│   ├── schedule.rs         # リセット日時計算、最寄りエージェント選択
│   ├── executor.rs         # プロセス起動・並列実行管理（tokio）
│   ├── format_stream.rs    # claude stream-json出力のフォーマッター
│   ├── classify.rs         # 完了 jsonl の分類（success / failed / rate-limited / retryable）
│   ├── cleanup.rs          # レポートディレクトリの自動クリーンアップ
│   ├── state.rs            # 処理済みターゲット状態の永続化
│   └── display.rs          # ステータス表示・プログレス出力
├── Makefile                # ビルドコマンド
└── .github/workflows/      # CI/CD
```

## 技術スタック

- **Rust** (edition 2024)
- clap (CLI), serde + toml (設定), chrono + chrono-tz (日時), tokio (非同期), colored (出力)

## 開発コマンド

```bash
make build    # デバッグビルド
make test     # テスト
make check    # clippy + fmt チェック
make release  # リリースビルド
```

## 設定ファイル

デフォルトパス: `~/.config/token-burn/config.toml`

主要セクション:
- `[settings]` - 並列実行数、スキップ期間、レポート設定、ターゲット上限
- `[prompts]` - デフォルトプロンプト
- `[[agents]]` - エージェント定義（command, リセットスケジュール, prompt）
- `[[scan]]` - ディレクトリ自動スキャン設定
- `[[targets]]` - 個別ターゲット（任意）

`[[agents]]` の `name` は空文字不可、`command` は1要素以上必須（先頭要素は実行ファイル名）です。

実行ファイルが `claude` の場合、`--verbose`、`--output-format stream-json`、`--include-partial-messages` は自動付与されます。`--output-format` が既存でも値は `stream-json` に正規化されます。

`claude` エージェントのみ出力を `.jsonl` + `format-stream` パイプラインで処理します。`codex` 等の他エージェントは `.log` に直接出力します。

`claude` エージェントのタスク完了後は `token-burn classify-result <jsonl>` により jsonl 最終 `result` イベントの `is_error` / `api_error_status` を解析して分類します。

- 成功 (`is_error:false`) → `state.json` に記録
- レート制限 (`resets <h><am|pm>` 等) → `failed-N` マーカー。`state.json` には記録しない
- プロバイダ側リトライ可能エラー (`api_error_status` が 408/429/5xx) → `retry-N` マーカー。`state.json` には記録しないため次回実行で再処理される。ワーカーは継続
- その他のプロバイダエラー → `failed-N` マーカーとエラーメッセージ（`result` フィールド）を表示し、ワーカーは停止

モニターペインの進捗は `fail:<n> retry:<n>` を併記し、完了時も `%d succeeded / %d failed / %d retry` の形で表示します。

`format-stream` は以下の stream-json イベントを処理します:
- テキスト応答のストリーミング表示
- 思考ブロック（`thinking`）のプログレスインジケーター
- ツール使用（`Read`/`Edit`/`Write`/`Bash`/`Agent`/`Task`/`TeamCreate`/`Skill`/`TodoWrite`/`Grep`/`Glob`/`ScheduleWakeup` 等）の詳細表示と差分出力
- `Grep` の検索パターンと対象パス、`ScheduleWakeup` の待機時間と理由を表示
- サブエージェントの開始・進捗・完了通知（`task_started` / `task_progress` / `task_notification`）
- トークン使用量、コスト、キャッシュ内訳、Web検索/フェッチ回数の集計表示
- モデル別使用量（`modelUsage`）の内訳表示（キャッシュ読み取り/書き込みトークン、Web検索回数を含む）
- API応答時間（`duration_api_ms`）の表示
- fast mode 状態の表示（`fast_mode_state` が `off` 以外の場合）
- レート制限警告（`rate_limit_event`）の使用率表示、リクエスト拒否通知、および `allowed` 時の補足情報表示（`resetsAt` / overage 情報がある場合）
- レート制限使用率が `rate_limit_threshold`（デフォルト: 95%）を超えた場合、stop file を作成して後続タスクを自動停止
- APIリトライ（`api_retry`）の試行回数とエラー情報の表示

処理済み状態は有効な設定ファイルと同じディレクトリの `state.json` に保存されます（デフォルト: `~/.config/token-burn/state.json`）。

`[settings]` の `limit` は 1 以上である必要があります。
`[settings]` の `rate_limit_threshold` は 1〜100 の範囲で指定する必要があります（デフォルト: 95）。レート制限使用率がこの閾値を超えると、現在のタスク完了後に後続タスクの実行を停止します。`rejected` イベント受信時も同様に停止します。
`[settings]` の `skip_within` と `cleanup_after` には `d` / `h` / `m` / `s` を使った有効な期間文字列を指定する必要があり、不正な値は設定読み込み時にエラーになります。

`[[scan]]` で `username` を指定した場合、リポジトリ可視性（public/private）はローカルディレクトリ名ではなく `origin` の remote URL に含まれるリポジトリ名（大文字小文字を無視）で照合されます。`username` を指定しない通常スキャンでは `origin` remote がなくても対象に含まれ、可視性は `Unknown` になります。

`[[scan]]` のディレクトリスキャンではシンボリックリンクはスキップされます（循環リンクによる無限再帰を防止）。

複数の `[[scan]]` 設定で同一ディレクトリが重複検出された場合、ターゲットは1件に正規化されます（同一リポジトリの重複実行を防止）。

ディレクトリパスは重複排除と状態管理の前に絶対パスへ正規化されるため、`repo` と `./repo` のような等価な相対パスは同一ターゲットとして扱われます。

この正規化と重複排除は、`token-burn run PATH...` で特定ディレクトリを強制実行する場合にも適用されます。
