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
│   ├── schedule.rs         # 固定リセット計算（曜日ベース）・AgentSchedule/ScheduleSource
│   ├── usage.rs            # ai-usage --json 連携・ScheduleResolver（スケジュール解決・最寄り選択）
│   ├── executor.rs         # プロセス起動・並列実行管理（tokio）
│   ├── format_stream/      # claude stream-json出力のフォーマッター（モジュール分割）
│   │   ├── mod.rs          # pub run / process（JSON行のトップレベル dispatch）
│   │   ├── state.rs        # StreamState / StreamSummary / UsageSummary
│   │   ├── blocks.rs       # ContentBlockState・ブロック確定（finalize_block 等）
│   │   ├── stream.rs       # handle_stream_event（content_block_* ハンドラ）
│   │   ├── system.rs       # handle_system_event（task通知 / hook / api_retry）
│   │   ├── result.rs       # handle_result（コスト・トークン・モデル別使用量等の各行生成）
│   │   ├── rate_limit.rs   # handle_rate_limit_event（reset時刻 / stop_file）
│   │   ├── diff.rs         # format_tool_diff / format_diff_lines
│   │   ├── util.rs         # truncate_str / format_number / first_string 等の小ヘルパー
│   │   ├── tools/          # ツール詳細・結果メタデータ表示
│   │   │   ├── mod.rs
│   │   │   ├── detail.rs   # tool_specific_detail / extract_tool_detail / detail_* 系
│   │   │   └── metadata.rs # tool_result_metadata
│   │   └── tests/          # 機能別に分割した #[cfg(test)] テスト群
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
- `[ai_usage]` - ai-usage --json 連携設定（任意。enabled / window / fallback / state_window / `[[ai_usage.profiles]]`）
- `[[agents]]` - エージェント定義（command, provider, env, リセットスケジュール, prompt, ai_usage 連携）
- `[[scan]]` - ディレクトリ自動スキャン設定
- `[[targets]]` - 個別ターゲット（任意）

`[[agents]]` の `name` は空文字不可、`command` は1要素以上必須（先頭要素は実行ファイル名）です。`reset_weekday` / `reset_time` / `timezone` は ai-usage 連携かつ fallback が `fixed` 以外のときは省略可、それ以外（ai-usage 非連携、または fallback=fixed）では必須です。`env`（環境変数マップ）のキーは `[A-Za-z_][A-Za-z0-9_]*` に制限され、値は読み込み時に `~` 展開されます。

実行ファイルが `claude` の場合、`-p`、`--verbose`、`--output-format stream-json`、`--include-partial-messages`、`--disallowedTools=AskUserQuestion` は自動付与されます。`--output-format` が既存でも値は `stream-json` に正規化されます。既存の `--disallowedTools` / `--disallowed-tools` がある場合は、必要に応じて equals 形式へ正規化して `AskUserQuestion` を追記します。

実行ファイルが `codex` の場合、無人実行で承認待ちにより停止しないよう `-c approval_policy=never` を実行ファイル直後に自動付与します（`codex -c approval_policy=never exec ...`）。`codex exec` には `--ask-for-approval` フラグが無い（0.136.0）ため、サブコマンドのオプション表面に依存しない top-level の config override として挿入します。`--sandbox`（サンドボックス）とは独立した軸のため、サンドボックス指定の有無に関わらず付与します。ユーザーが承認方針を明示済みの場合（`-a` / `--ask-for-approval` / `-c approval_policy=...` / `--dangerously-bypass-approvals-and-sandbox`）は上書きしません。

### ai-usage 連携

`[ai_usage]` を設定すると、各エージェントのリセット時刻を `ai-usage --json` の実データ（`weekly.resets_at` 等）から自動取得します。`[ai_usage]` が無い、または `enabled = false` の場合は従来どおり `reset_weekday` / `reset_time` / `timezone` による曜日ベースの固定計算のみで動作します（後方互換）。

- `[ai_usage]`: `enabled`（連携の有効化）、`command`（デフォルト `["ai-usage", "--json"]`）、`window`（deadline 算出枠。`weekly` | `five_hour` | `nearest`、デフォルト `weekly`）、`fallback`（解決失敗時の方針。`fixed` | `skip` | `error`、デフォルト `fixed`）、`state_window`（処理済みカットオフの枠。`weekly` | `selected`、デフォルト `weekly`）。
- `[[ai_usage.profiles]]`: `name`（内部参照名）、`profile`（`ai-usage --json` の `profile` と大文字小文字を区別して照合）、`env`（そのアカウントで起動する際に付与する環境変数。例: `CLAUDE_CONFIG_DIR`）。
- `[[agents]]` 側: `provider`（`claude` | `codex` | `antigravity`。ai-usage の `(profile, provider)` 照合に使うため連携時は必須）、`[agents.ai_usage]` の `profiles`（参照する profile 名のリスト）、任意の `window` / `fallback` 上書き。

実行時は agent × profile を `RuntimeAgent` に展開します。例えば agent `claude` が profiles `["work", "home"]` を参照する場合、`claude-work` / `claude-home` の 2 エージェントに展開され、それぞれ profile の `env`（agent の `env` を上書きマージ）を付与して起動します。**profile を 1 つだけ参照する agent は展開名が agent 名のまま**になります（例: agent `codex` が profiles `["home"]` のみ参照 → `codex`）。サフィックス `<agent>-<profile>` が付くのは 2 つ以上参照したときだけで、これにより各アカウントを個別の agent として定義（`claude` / `claude-home` のように起動コマンドが異なるラッパーを使う構成）しても展開名が冗長にならず、`state.json` のキー互換も保たれます。展開名は `state.json` のキーにも使われるため、アカウントごとに処理済み状態が分離されます。`ai-usage --json` は 1 プロセスにつき 1 回だけ実行され（`ScheduleResolver`）、全エージェントで使い回します。

解決に失敗した場合（ai-usage コマンドが無い/失敗、該当 `(profile, provider)` が無い、`ok:false`、該当枠が `null`）は `fallback` に従います: `fixed` は曜日ベースの固定計算に戻り（`status` / `run` の source 表示は `fixed fallback: <理由>`）、`skip` はそのエージェントを選択候補から除外し、`error` は即エラーで停止します。`window = "nearest"` で `five_hour` が選ばれても、`state_window = "weekly"` のときは処理済みカットオフは weekly（`resets_at - 7d`）を基準にします（weekly が無い場合のみ選択枠の period に落ちます）。

リセット時刻は `DateTime<FixedOffset>` で保持します。ai-usage の `resets_at`（RFC3339、オフセット付き）と固定計算（タイムゾーンのオフセット）を同じ型で統一し、ローカル時刻成分を保つためです。`status` と `run` は各エージェントのスケジュールの導出元（`ai-usage (weekly)` / `fixed` / `fixed fallback: <理由>`）を表示し、ai-usage が静かに固定計算へ戻ることはありません。

### 使用率ゲート（usage-gate）

ai-usage 連携が有効なとき、`rate_limit_threshold`（%）は 2 経路で後続タスクを止めます。1 つは既存の `claude` stream-json `rate_limit_event` によるリアルタイム監視（タスク実行中の `utilization` が閾値超過で stop file 作成）。もう 1 つが **usage-gate** で、各タスク完了後（ワーカーが次の pending を claim する前）に内部サブコマンド `token-burn usage-gate` が `ai-usage --json` を実行し、その agent の `(profile, provider)` の weekly / five_hour `used_percent` のうち**いずれかが `rate_limit_threshold` 以上なら stop file を作成**して後続を停止します。`claude` / `codex` 両方に効きます（codex は従来リアルタイム監視が無かったため特に有効）。

- ai-usage 出力は短 TTL（20 秒）でファイルキャッシュし、並列ワーカーからの重複取得を抑えます。
- 取得失敗時は fail-closed（使用率を確認できない以上、安全側で停止）。該当エントリ無し・`used_percent` 欠損時は過剰停止を避けて続行します。
- stop file 作成は `create_new` で冪等（並列ワーカーから同時に呼ばれても安全）。既に走行中のタスクは止められませんが、次のタスク開始前チェックで停止します。

### モニターペインの ai-usage 表示

ai-usage 連携が有効なとき、tmux モニターペイン（左）には進捗に加えて `ai-usage --statusline --logos`（各アカウントの 5h / 週次の使用率バー・%・リセット残り）を表示します。`--input` で起動時および usage-gate が更新するキャッシュ（`ai-usage-cache.json`）から高速描画し、**10 秒ごとに再描画**します（進捗バーは従来どおり毎秒 `\r` 更新）。取得失敗時は直前の表示を保持し（fail-soft）、`tput civis` でカーソルを隠してちらつきを抑えます。モニターは `\033[H\033[J` で全体を再描画する方式に変更したため、エラーは表示済みフラグではなく `error-*` マーカーから毎回再構築して履歴を保ちます。

statusline コマンドは usage-gate / 起動時キャッシュ初期化と同様に `[ai_usage].command` 全体から組み立てます（出力モードの `--json` を `--statusline --logos --input <cache>` に差し替え、無ければ末尾に追加）。先頭要素だけを使う実装ではないため、`["env", "FOO=1", "ai-usage", "--json"]` のようなラッパー前置き構成でも壊れません。完了/停止後にモニターが `exec sleep infinity` で待機する際は EXIT trap が発火しないため、`exec` の直前で端末状態（カーソル `tput cnorm` と自動折り返し `\033[?7h`）を明示的に復元します。進捗バーは `seq` ではなく算術 `while` ループで描画します（BSD seq の `seq 1 0` が降順で `1 0` を返し、macOS で 0% / 100% 時にバーが 2 文字ずれるのを避けるため）。

`claude` エージェントのみ出力を `.jsonl` + `format-stream` パイプラインで処理します。`codex` 等の他エージェントは `.log` に直接出力します。

`claude` エージェントでは、`format-stream` / `tee` / raw jsonl 保存のいずれかが失敗した場合、または jsonl が空の場合、そのタスクは `failed-N` として扱い、`state.json` には記録しません。ログ・分類パイプラインが壊れたタスクを成功扱いしないためです。非 `claude` エージェントでも `tee` が失敗した場合は `failed-N` として扱います。

`claude` エージェントのタスク完了後は `token-burn classify-result <jsonl>` により jsonl 最終 `result` イベントの `is_error` / `api_error_status` を解析して分類します。

- 成功 (`is_error:false`) → `state.json` に記録
- レート制限 (`resets <h><am|pm>` 等) → `failed-N` マーカー。`state.json` には記録しない
- プロバイダ側リトライ可能エラー (`api_error_status` が 408/429/5xx) → `retry-N` マーカー。`state.json` には記録しないため次回実行で再処理される。ワーカーは継続
- その他のプロバイダエラー → `failed-N` マーカーとエラーメッセージ（`result` フィールド）を表示し、ワーカーは停止

`format-stream` は `tool_result` の `is_error:true` を検出した場合、エラー内容の先頭の有意な 1 行をサマリーとして表示します（単一行/複数行の `<tool_use_error>...</tool_use_error>` ラッパーは除去）。配列形式の `content` にも対応し、120 文字を超える場合は末尾を `...` で省略します。

`tool_use_result` の top-level メタデータに `truncated`、`appliedLimit`、`staleReadFileStateHint`、`assistantAutoBackgrounded`、`backgroundTaskId`、`wasClamped` / `clampedDelaySeconds`、`persistedOutputPath` / `persistedOutputSize`、`returnCodeInterpretation`、`totalDurationMs` / `durationMs` / `totalTokens` / `totalToolUseCount`、`agentType`（Agent のサブエージェント種別。`agent:<type>` 形式）、`resolvedModel`（Skill / Agent が解決したモデル名。`model:<...>` 形式）、`toolStats`（サブエージェントの編集行数。加除いずれか非ゼロのとき `edits:+<追加>/-<削除>` 形式）、`numFiles` / `numLines`、`file.numLines` / `file.totalLines`（Read の部分読み取り。`lines:<n>/<total>` 形式）、`file.truncatedByTokenCap`（Read の token cap 切り詰め。`truncated:token-cap` 形式）、`matches`（ToolSearch）/ `numMatches`（Grep の count モード）/ `mode` / `total_deferred_tools`、`results` / `searchCount` / `durationSeconds`（WebSearch の結果件数・検索回数・所要時間）、`code` / `codeText` / `bytes`（WebFetch の HTTP ステータス・応答サイズ。`http:<code> <text>` 形式）、`gitOperation`（git commit の sha / kind。`commit:<sha> <kind>` 形式）、`tasks` / `task`、`retrieval_status`、`outputFile` / `canReadOutputFile`、`timeoutMs` / `persistent`、`statusChange`、`scheduledFor`、`commandName`、`allowedTools`（Skill が許可するツール一覧。非空配列のときに件数を `allowed-tools:<n>` 形式で表示） が含まれる場合は、ツール完了行に短い補足として表示します。`matches` 配列は ToolSearch 専用で Grep の結果には存在しないため、Grep の count モードでは `numMatches` 整数から件数を表示します。Read の行数は実データでは `file` オブジェクトに入れ子で入り、部分読み取り（`numLines < totalLines`）のときのみ `lines:<n>/<total>` を表示します（全行読み取り時はノイズ回避のため省略）。`file.truncatedByTokenCap` が true の場合は、行数比率とは独立して `truncated:token-cap` を表示します。WebSearch の `searchCount` は通常 1 のため 2 以上のときのみ表示します。

モニターペインの進捗は `fail:<n> retry:<n>` を併記し、完了時も `%d succeeded / %d failed / %d retry` の形で表示します。

## 並列実行モデル

`execute_plan_tmux` はタスクキュー方式で並列実行します。

- 各タスクは `queue_dir/pending-<idx>` と `tasks/task-<idx>.sh` として事前に書き出される
- ワーカーは `pending-<idx>` を `mv` でアトミックに `claimed-<idx>` にリネームして claim し、対応する `task-<idx>.sh` を `source` で実行する
- 各タスクを `source` する前にワーカーループ先頭で `CANCELLED` フラグを 0 にリセットする。直前タスクの実行中に SIGINT/SIGTERM を受けて `CANCELLED=1` が立ったまま成功・早期 return しても、後続タスクの通常エラーを誤って `Cancelled` 判定しエラー記録を欠落させるのを防ぐ
- タスクがエラー終了してもワーカーは `exec sleep infinity` せず、即座に次の `pending-*` を取りに行く
- ワーカーは claim できる pending が尽きるまで処理を続け、尽きて初めて `worker-done-<w>` を作成して終了する
- ユーザーが tmux をデタッチした場合、tmux セッションが生存していれば `/tmp/token-burn` は削除しない。ワーカーのキュー・タスクスクリプト・プロンプトファイルを保持し、バックグラウンド実行を継続できるようにする
- レポートディレクトリ名に使うエージェント名は `sanitize_filename` でパス成分を無害化する

結果として、`parallelism` で指定した並列数はタスクが尽きるまで維持されます（一部タスクが失敗しても他ワーカーは止まらない）。エラーは `marker_dir/error-<idx>` にタスク単位で記録されるため、同一ワーカーで複数エラーが起きてもモニターに全て表示されます。

`format-stream` は以下の stream-json イベントを処理します:
- テキスト応答のストリーミング表示
- 思考ブロック（`thinking`）のプログレスインジケーター
- ツール使用（`Read`/`Edit`/`Write`/`Bash`/`Agent`/`Task`/`TaskCreate`/`TaskGet`/`TaskList`/`TaskUpdate`/`TaskStop`/`TaskOutput`/`TeamCreate`/`Skill`/`TodoWrite`/`Monitor`/`Grep`/`Glob`/`ScheduleWakeup`/`WebFetch`/`WebSearch`/`ToolSearch`/`SendMessage`/`AskUserQuestion`/Context7・Tavily・Codex MCP 等）の詳細表示と差分出力
- `Read` の `file_path` と `offset` / `limit`、`Bash` の `timeout` / `run_in_background` / `dangerouslyDisableSandbox`、`Agent` の `run_in_background` を表示
- `Edit` は `new_string` に加えて実データで確認された `new_str` 入力も差分表示に使用し、`replace_all` が true の場合は一括置換として表示する
- `Grep` / `Glob` の検索パターン、対象パス、`output_mode`、`type`、`glob`、`head_limit`、`context`、`offset`、`-A` / `-B` / `-C` / `-n` / `-i` / `-o`、`multiline` を表示
- `ScheduleWakeup` の待機時間と理由を表示
- `WebFetch` の URL とプロンプト要約、`WebSearch` のクエリと include/exclude ドメイン件数、`ToolSearch` のクエリと `max_results` を表示
- `Monitor` の説明・タイムアウト・condition・persistent、`TaskStop` の task id / task ids / reason、`TaskList`、`TaskGet` の task id、`TaskOutput` の task id / `block` / `timeout`、`TaskCreate` の `subject` / `description` / `activeForm`、`TaskUpdate` の `taskId` / `status` / `owner` / `subject` / `description`、`SendMessage` の送信先/要約、`AskUserQuestion` の質問数・選択肢数、Tavily search の query/max/time range/search depth と Tavily extract の先頭 URL/件数(+N more)/extract_depth（`mcp__tavily__tavily-search` / `mcp__tavily__tavily_search` のハイフン版・アンダースコア版いずれも対応）、Codex MCP の prompt/cwd/model/sandbox/approval-policy、Context7 MCP ツールの library/query を表示
- サブエージェントの開始・進捗・状態更新・完了通知（`task_started` / `task_progress` / `task_updated` / `task_notification`）。`task_notification` は `completed` / `failed` に加え `stopped` も表示し、`usage` が無い場合は duration/token を 0 として表示しない
- Claude Code のシステム通知（`notification`。例: stop hook エラー）と、出力を伴う hook 診断（`hook_progress` / `hook_response` の stderr / output）
- `tool_use_result` の出力切り詰め、適用 limit、stale read ヒント、自動バックグラウンド化、clamp、永続化出力サイズ、戻りコード解釈、Agent の duration/token/tool 数・サブエージェント種別（`agent:`）・解決モデル（`model:`）・編集行数（`edits:+追加/-削除`）、Grep/ToolSearch の結果件数と mode、WebSearch の結果件数/検索回数/所要時間、WebFetch の HTTP ステータス/応答サイズ、Read の部分読み取り行数（`lines:<n>/<total>`）と token cap 切り詰め（`truncated:token-cap`）、タスク件数/task id、TaskOutput の取得状態、Agent 出力ファイル、Monitor の timeout/persistent、TaskUpdate の状態遷移、ScheduleWakeup の予定時刻、Skill のコマンド名の補足表示
- トークン使用量、コスト、キャッシュ内訳、Web検索/フェッチ回数の集計表示
- モデル別使用量（`modelUsage`）の内訳表示（キャッシュ読み取り/書き込みトークン、Web検索回数、`contextWindow` / `maxOutputTokens` を `ctx:1M` / `max_out:64K` のような単位付きで表示）
- API応答時間（`duration_api_ms`）と初回トークン到達時間（`ttft_ms`）、初回ストリームトークン到達時間（`ttft_stream_ms`。キュー/リトライ待ちを含む `ttft_ms` より小さい純粋なストリーム遅延。`stream:` 形式）、リクエスト送信までの所要時間（`time_to_request_ms`。通常数十〜数百 ms のためミリ秒表記 `req:<n>ms`）の表示
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
`[settings]` の `rate_limit_threshold` は 1〜100 の範囲で指定する必要があります（デフォルト: 95）。レート制限使用率がこの閾値を超えると、現在のタスク完了後に後続タスクの実行を停止します。`rejected` イベント受信時も同様に停止します。ai-usage 連携が有効な場合は、各タスク完了後に該当 agent の実使用率（weekly / five_hour の最大）でも `usage-gate` が判定し、閾値以上なら停止します。
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
