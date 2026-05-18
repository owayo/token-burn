# token-burn

週次リセット前にAIコーディングアシスタントのトークンを消費するCLIツール。

## プロジェクト構成

```
token-burn/
├── Cargo.toml              # 依存クレート定義
├── src/
│   ├── main.rs             # エントリポイント、clap CLI定義
│   ├── init.rs             # config/prompt 雛形の初期化
│   ├── config.rs           # TOML設定ファイルの読み込み・バリデーション・AgentMode
│   ├── scanner.rs          # ディレクトリスキャン・リポジトリ探索・gh CLI連携
│   ├── schedule.rs         # リセット日時計算、最寄りエージェント選択
│   ├── executor.rs         # プロセス起動・並列実行管理・mode 別タスクスクリプト生成
│   ├── format_stream.rs    # claude stream-json出力のフォーマッター (claude-print 用)
│   ├── classify.rs         # 完了 jsonl / outcome JSON の分類（success / failed / rate-limited / retryable）
│   ├── claude_hook.rs      # Stop / StopFailure hook 受け口（claude-interactive 用）
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

## エージェント `mode`（2026-06-15 Anthropic 制限への対応）

`[[agents]]` には `mode` を指定できます (`auto` / `generic` / `claude-print` / `claude-interactive`、デフォルト `auto`)。

**背景**: 2026-06-15 以降、Anthropic は `claude -p` / Claude Agent SDK / GitHub Actions を「Agent SDK 専用月次クレジット」（Pro $20/月、Max 5x $100/月、Max 20x $200/月）に分離します。token-burn は「プラン使用枠を使い切る」のが目的なので、プラン枠を消費する **対話的 Claude Code 経路** = `mode = "claude-interactive"` をデフォルト推奨にしています。**対象外の対話モード経路は技術的にプラン枠を消費しますが、Anthropic 側のポリシーは今後変わる可能性があり、保証はありません**。

各モードの動作:

- `claude-interactive`: `claude "prompt"` を tmux 実 TTY で起動。`--settings task-settings.json` で Stop / StopFailure hooks を注入し、hook 経由で書き出された outcome JSON を `classify-claude-outcome` が分類する。tmux pipe-pane でログ取得。
- `claude-print`: 既存の `claude -p` 経路。`--verbose`、`--output-format stream-json`、`--include-partial-messages` を強制付与し、`format-stream` + `classify-result` で jsonl を分類。2026-06-15 以降は Agent SDK クレジット消費のため、明示 opt-in 扱い。
- `generic`: codex 等。stdout/stderr を `tee` でログに保存し、終了コードで成否を判定。
- `auto`: command の内容で自動判定。`-p` / `--print` / `--output-format` / `--include-partial-messages` / `--input-format` のいずれかを含む claude → `claude-print`、それ以外の claude → `claude-interactive`、claude 以外 → `generic`。

`mode = "claude-interactive"` 設定時は、validate 段階で `command` に `-p` / `--print` / `--output-format` / `--input-format` / `--include-partial-messages` / `--max-budget-usd` / `--no-session-persistence` / `--include-hook-events` / `--json-schema` が含まれていれば設定読み込み時にエラーになります（これらが指定されると print 経路に切り替わり Agent SDK クレジットを消費してしまうため）。

また、claude 経路 (`claude-print` / `claude-interactive` / Auto with claude 実行ファイル) では `command` に `--settings` / `--settings=...` を直接書くことも禁止されます。token-burn は `--settings` を必ず 1 個だけ渡す方針（[[agents]].claude_settings で user 設定を集約）なので、wrapper 内で `--settings` を渡している場合は wrapper から外し、`claude_settings` に移行してください。

## `claude_settings`: user の Claude settings を統合する仕組み

`[[agents]]` に `claude_settings` を指定すると、token-burn が user の Claude settings JSON を 1 つ以上読み込み、token-burn の `Stop` / `StopFailure` hooks を **prepend** で挿入してから 1 つの merged JSON ファイルとして書き出し、`claude --settings <merged-path>` で渡します。

サポートするソース:

```toml
[[agents]]
name = "claude"
command = ["claude", "--dangerously-skip-permissions", "--model", "opus"]
mode = "claude-interactive"

# 定義順に deep merge される。後勝ち。
claude_settings = [
    # (1) ファイル経路: ~ 展開対応、中身は valid な JSON object
    { file = "~/.config/claude/plugin-settings.json" },

    # (2) コマンド経路: shell コマンドを実行し stdout を JSON object として読む
    #     動的判定（cwd 依存等）はこの経路で実現する
    { command = ["bash", "-lc", "~/bin/claude-plugin-settings.sh"] },

    # (3) inline 経路: TOML 上で直接書く JSON object
    { inline = { enabledPlugins = { "my-plugin@org" = true } } },
]
```

**merge 規則**:
- object 同士は **再帰 deep merge**（同じキーがあれば後の source が勝つ）
- 配列は完全置換（hooks の matcher 配列を除く）
- `hooks.Stop` / `hooks.StopFailure` 配列は token-burn の hook entry が user の hook entries の **先頭** に prepend される。これにより token-burn の outcome 書き出しが先に走り、その後 user hooks が走る（user hooks の `decision: "block"` を尊重）

**wrapper script からの移行**: 既存の `claude-wrapper.sh` などで `--settings "$JSON"` を渡している場合は、wrapper 内の `--settings` を **削除** し、生成ロジックを `claude_settings = [{ command = [...] }]` に移植してください。理由は token-burn が `--settings` を必ず自前で 1 個だけ渡すためです（複数指定時の claude の挙動は公式非明記）。validate でも `command` 内の `--settings` 直書きは拒否されます。

**source の制約**: `file` / `command` / `inline` のソースは valid な JSON **object** を返す必要があります。array / scalar / null はエラーになります。`command` ソースの非ゼロ終了 / 空 stdout もエラーです。

**mode 制限**: `claude_settings` は claude エージェント (`mode = "claude-print"` / `"claude-interactive"` / Auto with claude 実行ファイル) でのみ指定可能。`generic` モードや非 claude 実行ファイルで指定するとエラーになります。

## モード別の分類

### `claude-print` 経路

`format-stream` / `tee` / raw jsonl 保存のいずれかが失敗した場合、または jsonl が空の場合、そのタスクは `failed-N` として扱い、`state.json` には記録しません。タスク完了後は `token-burn classify-result <jsonl>` により jsonl 最終 `result` イベントの `is_error` / `api_error_status` を解析して分類します。

- 成功 (`is_error:false`) → `state.json` に記録
- レート制限 (`resets <h><am|pm>` 等) → `failed-N` マーカー。`state.json` には記録しない
- プロバイダ側リトライ可能エラー (`api_error_status` が 408/429/5xx) → `retry-N` マーカー。次回実行で再処理。ワーカーは継続
- その他のプロバイダエラー → `failed-N` マーカーとエラーメッセージ（`result` フィールド）を表示

### `claude-interactive` 経路

タスクごとに `task-settings.json` を生成し、`claude --settings <path>` で Stop / StopFailure hooks を注入します。hooks は `token-burn claude-hook --outcome <path>` を呼び、stdin の hook JSON ペイロードを outcome ファイルへアトミック書き出しします。タスク完了後は `token-burn classify-claude-outcome <outcome>` で分類します。

- `Stop` hook 発火 → `ResultClass::Success` → `state.json` に記録
- `StopFailure.error == "rate_limit"` → `ResultClass::RateLimited` → `failed-N`
- `StopFailure.error == "server_error"` → `ResultClass::Retryable` → `retry-N`、次回再処理
- `StopFailure.error == "billing_error" / "authentication_failed" / "oauth_org_not_allowed" / "invalid_request" / "max_output_tokens"` → `ResultClass::Failed` → `failed-N`
- `StopFailure.error == "unknown"` → `error_details` / `last_assistant_message` のテキストで判定（rate-limit シグネチャ含めば `RateLimited`、それ以外は `Failed`）
- outcome ファイルが書き出されない (hook 不発火) → `failed-N` として「hook did not fire (claude crashed or --settings ignored)」を記録

stream-json の `rate_limit_event` が使えないため、`settings.rate_limit_threshold` による 95% 自動停止は `claude-interactive` モードでは機能しません。`StopFailure(error=rate_limit)` 受信時のみ停止します。

### `generic` 経路

`tee` が失敗した場合は `failed-N` として扱います。終了コード 0 で `state.json` に記録、非 0 でペインの末尾出力をエラーとして `failed-N`。

モニターペインの進捗は `fail:<n> retry:<n>` を併記し、完了時も `%d succeeded / %d failed / %d retry` の形で表示します。

## 並列実行モデル

`execute_plan_tmux` はタスクキュー方式で並列実行します。

- 各タスクは `queue_dir/pending-<idx>` と `tasks/task-<idx>.sh` として事前に書き出される
- ワーカーは `pending-<idx>` を `mv` でアトミックに `claimed-<idx>` にリネームして claim し、対応する `task-<idx>.sh` を `source` で実行する
- タスクがエラー終了してもワーカーは `exec sleep infinity` せず、即座に次の `pending-*` を取りに行く
- ワーカーは claim できる pending が尽きるまで処理を続け、尽きて初めて `worker-done-<w>` を作成して終了する
- ユーザーが tmux をデタッチした場合、tmux セッションが生存していれば `/tmp/token-burn` は削除しない。ワーカーのキュー・タスクスクリプト・プロンプトファイルを保持し、バックグラウンド実行を継続できるようにする
- レポートディレクトリ名に使うエージェント名は `sanitize_filename` でパス成分を無害化する

結果として、`parallelism` で指定した並列数はタスクが尽きるまで維持されます（一部タスクが失敗しても他ワーカーは止まらない）。エラーは `marker_dir/error-<idx>` にタスク単位で記録されるため、同一ワーカーで複数エラーが起きてもモニターに全て表示されます。

`format-stream` は以下の stream-json イベントを処理します:
- テキスト応答のストリーミング表示
- 思考ブロック（`thinking`）のプログレスインジケーター
- ツール使用（`Read`/`Edit`/`Write`/`Bash`/`Agent`/`Task`/`TaskStop`/`TaskOutput`/`TeamCreate`/`Skill`/`TodoWrite`/`Monitor`/`Grep`/`Glob`/`ScheduleWakeup`/`WebFetch`/`WebSearch`/`ToolSearch`/`SendMessage`/`AskUserQuestion`/Context7・Tavily・Codex MCP 等）の詳細表示と差分出力
- `Read` の `file_path` と `offset` / `limit`、`Bash` の `timeout` / `run_in_background`、`Agent` の `run_in_background` を表示
- `Edit` は `new_string` に加えて実データで確認された `new_str` 入力も差分表示に使用し、`replace_all` が true の場合は一括置換として表示する
- `Grep` / `Glob` の検索パターン、対象パス、`output_mode`、`glob`、`head_limit`、`context`、`-A` / `-B` / `-C` / `-n` / `-i` を表示
- `ScheduleWakeup` の待機時間と理由を表示
- `WebFetch` の URL とプロンプト要約、`WebSearch` のクエリと include/exclude ドメイン件数、`ToolSearch` のクエリと `max_results` を表示
- `Monitor` の説明とタイムアウト、`TaskStop` の task id、`TaskOutput` の task id / `block` / `timeout`、`SendMessage` の送信先/要約、`AskUserQuestion` の質問数・選択肢数、Tavily の query/max/time range/search depth、Codex MCP の prompt/cwd/sandbox/approval-policy、Context7 MCP ツールの library/query を表示
- サブエージェントの開始・進捗・状態更新・完了通知（`task_started` / `task_progress` / `task_updated` / `task_notification`）。`task_updated` は `is_backgrounded` と `killed` も表示し、`task_notification` は `completed` / `failed` に加え `stopped` も表示する
- Claude Code のシステム通知（`notification`。例: stop hook エラー）と、出力を伴う hook 診断（`hook_progress` / `hook_response` の stderr / output）
- トークン使用量、コスト、キャッシュ内訳、Web検索/フェッチ回数の集計表示
- モデル別使用量（`modelUsage`）の内訳表示（キャッシュ読み取り/書き込みトークン、Web検索回数、`contextWindow` / `maxOutputTokens` を `ctx:1M` / `max_out:64K` のような単位付きで表示）
- API応答時間（`duration_api_ms`）の表示
- fast mode 状態の表示（`fast_mode_state` が `off` 以外の場合）
- 異常終了時の `terminal_reason`（`completed` 以外の場合）と `permission_denials` の件数表示
- レート制限警告（`rate_limit_event`）の使用率表示、リクエスト拒否通知、および `allowed` 時の補足情報表示（`resetsAt` / `overageResetsAt` / overage 情報がある場合）。`allowed_warning` 時に `surpassedThreshold` が含まれている場合は通過済み警告閾値（例: `warning at 90%`）を併記する
- レート制限使用率が `rate_limit_threshold`（デフォルト: 95%）を超えた場合、stop file を作成して後続タスクを自動停止
- APIリトライ（`api_retry`）の試行回数とエラー情報の表示

なお `usage` フィールドは各 `message_start` / `message_delta` でその API 呼び出し単独の値を返し、`result` イベントに最終累計が入るため、`format-stream` は `result` の値を最終出力として優先します。

処理済み状態は有効な設定ファイルと同じディレクトリの `state.json` に保存されます（デフォルト: `~/.config/token-burn/state.json`）。

`[settings]` の `limit` は 1 以上である必要があります。
`[settings]` の `rate_limit_threshold` は 1〜100 の範囲で指定する必要があります（デフォルト: 95）。レート制限使用率がこの閾値を超えると、現在のタスク完了後に後続タスクの実行を停止します。`rejected` イベント受信時も同様に停止します。
`[settings]` の `skip_within` と `cleanup_after` には `d` / `h` / `m` / `s` を使った有効な期間文字列を指定する必要があり、不正な値は設定読み込み時にエラーになります。

`[[scan]]` で `username` を指定した場合、リポジトリ可視性（public/private）はローカルディレクトリ名ではなく `origin` の remote URL に含まれるリポジトリ名（大文字小文字を無視）で照合されます。`username` を指定しない通常スキャンでは `origin` remote がなくても対象に含まれ、可視性は `Unknown` になります。

`[[scan]]` のディレクトリスキャンではシンボリックリンクはスキップされます（循環リンクによる無限再帰を防止）。

複数の `[[scan]]` 設定で同一ディレクトリが重複検出された場合、ターゲットは1件に正規化されます（同一リポジトリの重複実行を防止）。

ディレクトリパスは重複排除と状態管理の前に絶対パスへ正規化されるため、`repo` と `./repo` のような等価な相対パスは同一ターゲットとして扱われます。

この正規化と重複排除は、`token-burn run PATH...` で特定ディレクトリを強制実行する場合にも適用されます。

`[[targets]]` には `defer = true` を指定でき、true のターゲットは実行リストの末尾に集められます（`scan` 由来のターゲットは常に `defer=false`）。`resolve_targets` の最後で `sort_by_key` による安定ソートが行われるため、`scan` 内の Visibility 順や `[[targets]]` 同士の追加順は各グループ (defer=false / defer=true) 内で維持されます。`token-burn run PATH...` で明示指定した場合は CLI 指定順を優先するため `defer` フラグは反映しません。
