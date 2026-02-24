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
│   ├── cleanup.rs          # レポートディレクトリの自動クリーンアップ
│   ├── state.rs            # 処理済みターゲット状態の永続化
│   └── display.rs          # ステータス表示・プログレス出力
├── Makefile                # ビルドコマンド
└── .github/workflows/      # CI/CD
```

## 技術スタック

- **Rust** (edition 2021)
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
- `[settings]` - 並列実行数
- `[prompts]` - デフォルトプロンプト
- `[[agents]]` - エージェント定義（command, リセットスケジュール, prompt）
- `[[scan]]` - ディレクトリ自動スキャン設定
- `[[targets]]` - 個別ターゲット（任意）

`[[agents]]` の `name` は空文字不可、`command` は1要素以上必須（先頭要素は実行ファイル名）です。

実行ファイルが `claude` の場合、`--verbose`、`--output-format stream-json`、`--include-partial-messages` は自動付与されます。`--output-format` が既存でも値は `stream-json` に正規化されます。

`claude` エージェントのみ出力を `.jsonl` + `format-stream` パイプラインで処理します。`codex` 等の他エージェントは `.log` に直接出力します。
