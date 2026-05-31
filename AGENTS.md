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

`claude` エージェントでは、`format-stream` / `tee` / raw jsonl 保存のいずれかが失敗した場合、または jsonl が空の場合、そのタスクは `failed-N` として扱い、`state.json` には記録しません。ログ・分類パイプラインが壊れたタスクを成功扱いしないためです。非 `claude` エージェントでも `tee` が失敗した場合は `failed-N` として扱います。

`claude` エージェントのタスク完了後は `token-burn classify-result <jsonl>` により jsonl 最終 `result` イベントの `is_error` / `api_error_status` を解析して分類します。

- 成功 (`is_error:false`) → `state.json` に記録
- レート制限 (`resets <h><am|pm>` 等) → `failed-N` マーカー。`state.json` には記録しない
- プロバイダ側リトライ可能エラー (`api_error_status` が 408/429/5xx) → `retry-N` マーカー。`state.json` には記録しないため次回実行で再処理される。ワーカーは継続
- その他のプロバイダエラー → `failed-N` マーカーとエラーメッセージ（`result` フィールド）を表示し、ワーカーは停止

`format-stream` は `tool_result` の `is_error:true` を検出した場合、エラー内容の先頭の有意な 1 行をサマリーとして表示します（単一行/複数行の `<tool_use_error>...</tool_use_error>` ラッパーは除去）。配列形式の `content` にも対応し、120 文字を超える場合は末尾を `...` で省略します。

`tool_use_result` の top-level メタデータに `truncated`、`appliedLimit`、`staleReadFileStateHint`、`assistantAutoBackgrounded`、`backgroundTaskId`、`wasClamped` / `clampedDelaySeconds`、`persistedOutputPath` / `persistedOutputSize`、`returnCodeInterpretation`、`totalDurationMs` / `totalTokens` / `totalToolUseCount`、`numFiles` / `numLines`、`matches` / `total_deferred_tools`、`statusChange`、`scheduledFor`、`commandName` が含まれる場合は、ツール完了行に短い補足として表示します。

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
- ツール使用（`Read`/`Edit`/`Write`/`Bash`/`Agent`/`Task`/`TaskCreate`/`TaskUpdate`/`TaskStop`/`TaskOutput`/`TeamCreate`/`Skill`/`TodoWrite`/`Monitor`/`Grep`/`Glob`/`ScheduleWakeup`/`WebFetch`/`WebSearch`/`ToolSearch`/`SendMessage`/`AskUserQuestion`/Context7・Tavily・Codex MCP 等）の詳細表示と差分出力
- `Read` の `file_path` と `offset` / `limit`、`Bash` の `timeout` / `run_in_background`、`Agent` の `run_in_background` を表示
- `Edit` は `new_string` に加えて実データで確認された `new_str` 入力も差分表示に使用し、`replace_all` が true の場合は一括置換として表示する
- `Grep` / `Glob` の検索パターン、対象パス、`output_mode`、`type`、`glob`、`head_limit`、`context`、`-A` / `-B` / `-C` / `-n` / `-i`、`multiline` を表示
- `ScheduleWakeup` の待機時間と理由を表示
- `WebFetch` の URL とプロンプト要約、`WebSearch` のクエリと include/exclude ドメイン件数、`ToolSearch` のクエリと `max_results` を表示
- `Monitor` の説明とタイムアウト、`TaskStop` の task id、`TaskOutput` の task id / `block` / `timeout`、`TaskCreate` の `subject` / `description` / `activeForm`、`TaskUpdate` の `taskId` / `status` / `owner` / `subject` / `description`、`SendMessage` の送信先/要約、`AskUserQuestion` の質問数・選択肢数、Tavily の query/max/time range/search depth、Codex MCP の prompt/cwd/model/sandbox/approval-policy、Context7 MCP ツールの library/query を表示
- サブエージェントの開始・進捗・状態更新・完了通知（`task_started` / `task_progress` / `task_updated` / `task_notification`）。`task_notification` は `completed` / `failed` に加え `stopped` も表示し、`usage` が無い場合は duration/token を 0 として表示しない
- Claude Code のシステム通知（`notification`。例: stop hook エラー）と、出力を伴う hook 診断（`hook_progress` / `hook_response` の stderr / output）
- `tool_use_result` の出力切り詰め、適用 limit、stale read ヒント、自動バックグラウンド化、clamp、永続化出力サイズ、戻りコード解釈、Agent の duration/token/tool 数、Grep/ToolSearch の結果件数、TaskUpdate の状態遷移、ScheduleWakeup の予定時刻、Skill のコマンド名の補足表示
- トークン使用量、コスト、キャッシュ内訳、Web検索/フェッチ回数の集計表示
- モデル別使用量（`modelUsage`）の内訳表示（キャッシュ読み取り/書き込みトークン、Web検索回数、`contextWindow` / `maxOutputTokens` を `ctx:1M` / `max_out:64K` のような単位付きで表示）
- API応答時間（`duration_api_ms`）と初回トークン到達時間（`ttft_ms`）の表示
- fast mode 状態の表示（`fast_mode_state` が `off` 以外の場合）
- 異常終了時の `terminal_reason`（`completed` 以外の場合）と `permission_denials` の件数・ツール名表示
- result の `usage.service_tier`、`usage.speed`、空でない `usage.inference_geo`、`usage.iterations` 件数、`origin.kind` の表示
- レート制限警告（`rate_limit_event`）の使用率表示、リクエスト拒否通知、および `allowed` 時の補足情報表示（`resetsAt` / `overageResetsAt` / overage 情報がある場合）。`allowed_warning` 時に `surpassedThreshold` が含まれている場合は通過済み警告閾値（例: `warning at 90%`）を併記する
- レート制限使用率が `rate_limit_threshold`（デフォルト: 95%）を超えた場合、stop file を作成して後続タスクを自動停止
- APIリトライ（`api_retry`）の試行回数とエラー情報の表示
- `status`（リクエスト状態通知）と `thinking_tokens`（思考トークンの推定累積値 `estimated_tokens` / `estimated_tokens_delta`）は高頻度（1 セッションで数千件）に出力されるノイズイベントのため、明示的に無視します。思考中の進捗は `thinking_delta` のドット表示、トークン総数は `result.usage` の集計表示で代替するため、これらを表示すると重複・冗長になります

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

## 実装上の注意点

- リセット日時計算 (`schedule.rs`) は `naive_local()` をベースに行います。`DateTime::date_naive()` は UTC 日付を返すため、`weekday()` のローカル曜日と整合させるためにローカルタイムゾーンの日付を基準とします。Asia/Tokyo のような UTC+N のタイムゾーンで深夜帯（UTC 前日）に実行しても曜日がずれない設計です。
- リセット時刻が DST（夏時間）遷移に重なる場合も `resolve_local_datetime` (`schedule.rs`) で解決します。曖昧な時刻（秋の繰り戻しで 2 回出現する時刻）は早い方を採用し、存在しない時刻（春の繰り上げでスキップされる時刻）は遷移直後の最初の有効な瞬間にフォールバックします。`from_local_datetime().earliest()` は存在しない時刻に対して `None` を返すため、`America/New_York` の `02:30` のように DST ギャップへ重なるリセット時刻だと、設定読み込みは成功するのに `status` / `run` が実行時に毎回失敗していました。これを防ぐ実装です。
- 状態ファイル (`state.json`) の書き込みは `write_all` 完了後に `set_len(written_len)` で末尾を切り詰める順序で行います。途中で書き込みが失敗してもファイル全消失（旧実装の `set_len(0)` 先行による data loss）が起きないようにしています。
- レポートディレクトリのクリーンアップ (`cleanup.rs`) はシンボリックリンクをスキップします。`Path::is_dir()` はリンクを追跡するため、リンク先のディレクトリを誤って削除しないよう `is_symlink()` で除外します。
- モニタースクリプトのエラーマーカー走査は `while IFS= read -r ... < <(find ...)` 方式を使用しており、`TMPDIR` のパスに空白が含まれる環境でもワードスプリットが発生しません。エラー内容の表示は `printf '%s'` 経由で行い、ファイル内容を `echo` のダブルクォート内で再解釈しないようにしています。
