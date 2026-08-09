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
│   ├── executor/           # プロセス起動・並列実行管理（tokio、モジュール分割）
│   │   ├── mod.rs          # ExecutionPlan / build_plan / print_plan / execute_plan_tmux / ai-usage 同期起動
│   │   ├── flags.rs        # claude/codex 判定と必須フラグ・env（CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS 等）の自動注入
│   │   ├── scripts.rs      # tmux 用シェルスクリプト生成（task/worker/monitor/statusline、shell_escape/env 前置き）
│   │   └── util.rs         # sanitize_filename / task_log_base / strip_ansi / truncate
│   ├── format_stream/      # claude stream-json出力のフォーマッター（モジュール分割）
│   │   ├── mod.rs          # pub run / process（JSON行のトップレベル dispatch）
│   │   ├── assistant.rs    # assistant メッセージのモデル切替・キャッシュミス診断
│   │   ├── state.rs        # StreamState / StreamSummary / UsageSummary
│   │   ├── blocks.rs       # ContentBlockState・ブロック確定（finalize_block 等）
│   │   ├── stream.rs       # handle_stream_event（content_block_* ハンドラ）
│   │   ├── system.rs       # handle_system_event（task通知 / hook / api_retry / model_refusal_fallback）
│   │   ├── result.rs       # handle_result（コスト・トークン・モデル別使用量等の各行生成）
│   │   ├── tool_result.rs  # handle_tool_result_event（user イベントのツール完了行）
│   │   ├── rate_limit.rs   # handle_rate_limit_event（reset時刻 / stop_file）
│   │   ├── diff.rs         # format_tool_diff / format_diff_lines
│   │   ├── util.rs         # truncate_str / format_number / first_string 等の小ヘルパー
│   │   ├── tools/          # ツール詳細・結果メタデータ表示
│   │   │   ├── mod.rs
│   │   │   ├── detail.rs   # tool_specific_detail / extract_tool_detail / detail_* 系
│   │   │   ├── metadata.rs # tool_result_metadata
│   │   │   └── progress.rs # tool_progress の経過時間表示
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
- `[settings]` - 並列実行数、スキップ期間、レポート設定、ターゲット上限、処理済み履歴の共有範囲
- `[prompts]` - デフォルトプロンプト
- `[ai_usage]` - ai-usage --json 連携設定（任意。enabled / window / fallback / state_window / `[[ai_usage.profiles]]`）
- `[[agents]]` - エージェント定義（command, provider, env, リセットスケジュール, prompt, ai_usage 連携）
- `[[scan]]` - ディレクトリ自動スキャン設定
- `[[targets]]` - 個別ターゲット（任意）

`[[agents]]` の `name` は空文字不可、`command` は1要素以上必須（先頭要素は実行ファイル名）です。`reset_weekday` / `reset_time` / `timezone` は ai-usage 連携かつ fallback が `fixed` 以外のときは省略可、それ以外（ai-usage 非連携、または fallback=fixed）では必須です。`env`（環境変数マップ）のキーは `[A-Za-z_][A-Za-z0-9_]*` に制限され、値は読み込み時に `~` 展開されます。

実行ファイルが `claude` の場合、`-p`、`--verbose`、`--output-format stream-json`、`--include-partial-messages`、`--disallowedTools=AskUserQuestion` は自動付与されます。`--output-format` が既存でも値は `stream-json` に正規化されます。既存の `--disallowedTools` / `--disallowed-tools` がある場合は、必要に応じて equals 形式へ正規化して `AskUserQuestion` を追記します。さらに env に `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0` をデフォルト注入します。`claude -p` はメインターン終了後にバックグラウンドタスク（background 起動のサブエージェント / Workflow）を既定 600 秒しか待たず強制終了し、未完のまま `is_error:false` で成功終了してしまうため、無期限待機に切り替えて完走させます。agent / profile の `env` に同キーが明示されていれば尊重します（空文字なら unset）。この注入はタスク実行コマンドだけに効かせ、usage-gate / monitor statusline に渡す env スナップショットには含めません。

実行ファイルが `codex` の場合、無人実行で承認待ちにより停止しないよう `-c approval_policy=never` を実行ファイル直後に自動付与します（`codex -c approval_policy=never exec ...`）。`codex exec` には `--ask-for-approval` フラグが無い（0.136.0）ため、サブコマンドのオプション表面に依存しない top-level の config override として挿入します。`--sandbox`（サンドボックス）とは独立した軸のため、サンドボックス指定の有無に関わらず付与します。ユーザーが承認方針を明示済みの場合（`-a` / `--ask-for-approval` / `-c approval_policy=...` / `--dangerously-bypass-approvals-and-sandbox`）は上書きしません。

### ai-usage 連携

`[ai_usage]` を設定すると、各エージェントのリセット時刻を `ai-usage --json` の実データ（`weekly.resets_at` 等）から自動取得します。`[ai_usage]` が無い、または `enabled = false` の場合は従来どおり `reset_weekday` / `reset_time` / `timezone` による曜日ベースの固定計算のみで動作します（後方互換）。

- `[ai_usage]`: `enabled`（連携の有効化）、`command`（デフォルト `["ai-usage", "--json"]`）、`window`（deadline 算出枠。`weekly` | `five_hour` | `nearest`、デフォルト `weekly`）、`fallback`（解決失敗時の方針。`fixed` | `skip` | `error`、デフォルト `fixed`）、`state_window`（処理済みカットオフの枠。`weekly` | `selected`、デフォルト `weekly`）。
- `[[ai_usage.profiles]]`: `name`（内部参照名）、`profile`（`ai-usage --json` の `profile` と大文字小文字を区別して照合）、`env`（そのアカウントで起動する際に付与する環境変数。例: `CLAUDE_CONFIG_DIR`）。
- `[[agents]]` 側: `provider`（`claude` | `codex` | `antigravity`。ai-usage の `(profile, provider)` 照合に使うため連携時は必須）、`[agents.ai_usage]` の `profiles`（参照する profile 名のリスト。同一 agent 内で同じ profile 名を重複参照すると同名の `RuntimeAgent` が二重生成されるため、設定読み込み時にエラーになります）、任意の `window` / `fallback` 上書き。

実行時は agent × profile を `RuntimeAgent` に展開します。例えば agent `claude` が profiles `["work", "home"]` を参照する場合、`claude-work` / `claude-home` の 2 エージェントに展開され、それぞれ profile の `env`（agent の `env` を上書きマージ）を付与して起動します。**profile を 1 つだけ参照する agent は展開名が agent 名のまま**になります（例: agent `codex` が profiles `["home"]` のみ参照 → `codex`）。サフィックス `<agent>-<profile>` が付くのは 2 つ以上参照したときだけで、これにより各アカウントを個別の agent として定義（`claude` / `claude-home` のように起動コマンドが異なるラッパーを使う構成）しても展開名が冗長にならず、`state.json` のキー互換も保たれます。展開名は `state.json` のキーにも使われるため、アカウントごとに処理済み状態が分離されます。`ai-usage --json` は 1 プロセスにつき 1 回だけ実行され（`ScheduleResolver`）、全エージェントで使い回します。

解決に失敗した場合（ai-usage コマンドが無い/失敗、該当 `(profile, provider)` が無い、`ok:false`、該当枠が `null`）は `fallback` に従います: `fixed` は曜日ベースの固定計算に戻り（`status` / `run` の source 表示は `fixed fallback: <理由>`）、`skip` はそのエージェントを選択候補から除外し、`error` は即エラーで停止します。`window = "nearest"` で `five_hour` が選ばれても、`state_window = "weekly"` のときは処理済みカットオフは weekly（`resets_at - 7d`）を基準にします（weekly が無い場合のみ選択枠の period に落ちます）。

リセット時刻は `DateTime<FixedOffset>` で保持します。ai-usage の `resets_at`（RFC3339、オフセット付き）は瞬間を保ったまま実行環境のローカル固定オフセットへ変換し、固定計算（タイムゾーンのオフセット）と同じ型で統一します。UTC で返る ai-usage 出力も `status` / `run` ではユーザーのローカル時刻として表示されます。`status` と `run` は各エージェントのスケジュールの導出元（`ai-usage (weekly)` / `fixed` / `fixed fallback: <理由>`）を表示し、ai-usage が静かに固定計算へ戻ることはありません。

ドライランの実行計画、および ai-usage コマンドの起動失敗・タイムアウトエラーにコマンド列を表示するときは、環境変数代入と一般的な認証オプションの値を `<redacted>` に置き換えます。実際の子プロセスには元の引数を渡し、表示のために実行内容を変更しません。

### 使用率ゲート（usage-gate）

ai-usage 連携が有効なとき、`rate_limit_threshold`（%）は 2 経路で後続タスクを止めます。1 つは既存の `claude` stream-json `rate_limit_event` によるリアルタイム監視（タスク実行中の `utilization` が閾値超過で stop file 作成）。もう 1 つが **usage-gate** で、各タスク完了後（ワーカーが次の pending を claim する前）に内部サブコマンド `token-burn usage-gate` が `ai-usage --json` を実行し、その agent の `(profile, provider)` の weekly / five_hour `used_percent` のうち**いずれかが `rate_limit_threshold` 以上なら stop file を作成**して後続を停止します。`claude` / `codex` 両方に効きます（codex は従来リアルタイム監視が無かったため特に有効）。

- ai-usage 出力は短 TTL（20 秒）でファイルキャッシュし、並列ワーカーからの重複取得を抑えます。キャッシュは同一ディレクトリの `.<cache>.tmp.<PID>` に書き出し → `rename` で本体に置き換える atomic rename で更新するため、別ワーカーが書き込み途中の不完全 JSON を読むことはありません。
- 取得失敗時、および該当アカウントが `ok:false`（認証切れ等で ai-usage がエラーを報告）のときは fail-closed（使用率を確認できない以上、安全側で停止）。`ok:false` は取得成功でも「使用率を確認できない」状態であり、スケジュール解決（`ScheduleResolver`）が `ok:false` を失敗として fallback するのと一貫します（この検査が無いと `ok:false` かつ `used_percent` 欠損で `max_used=None` となり走り続ける fail-open になります）。該当エントリ無し・`used_percent` 欠損（`ok:true`）時は過剰停止を避けて続行します。`stop_file` の作成にも失敗した場合（ディスクフル等）は黙って継続せず、エラーを伝搬してワーカーを止めます。
- stop file 作成は `create_new` で冪等（並列ワーカーから同時に呼ばれても安全）。既に走行中のタスクは止められませんが、次のタスク開始前チェックで停止します。これは usage-gate と `claude` stream-json の rate_limit_event 経路の両方で共通の挙動です。

### モニターペインの ai-usage 表示

ai-usage 連携が有効なとき、tmux モニターペイン（左）には進捗に加えて `ai-usage --statusline --logos`（各アカウントの 5h / 週次の使用率バー・%・リセット残り）を表示します。**10 秒ごと**にモニター自身が `ai-usage --json` を実行してキャッシュ（`ai-usage-cache.json`）を atomic（`.tmp` に書いてから `mv` で差し替え）に更新し、その直後に `--input <cache>` 付き statusline で描画します。`--input` を介して usage-gate と同じキャッシュを共有するため、長時間タスク中でもモニター表示と並列ワーカーの使用率判定が同じ最新値で同期します（進捗バーは従来どおり毎秒 `\r` 更新）。取得失敗時は直前の表示を保持し（fail-soft）、`tput civis` でカーソルを隠してちらつきを抑えます。モニターは `\033[H\033[J` で全体を再描画する方式のため、エラーは表示済みフラグではなく `error-*` マーカーから毎回再構築して履歴を保ちます。

statusline コマンドは usage-gate / 起動時キャッシュ初期化と同様に `[ai_usage].command` 全体から組み立てます（出力モードの `--json` を `--statusline --logos --input <cache>` に差し替え、無ければ末尾に追加）。先頭要素だけを使う実装ではないため、`["env", "FOO=1", "ai-usage", "--json"]` のようなラッパー前置き構成でも壊れません。完了/停止後の待機は `wait_for_close`（`sleep 3600` を回すループ）で行い、待機に入る直前に端末状態（カーソル `tput cnorm` と自動折り返し `\033[?7h`）を明示的に復元します。`exec sleep infinity` は 2 つの理由で使えません。1 つは macOS の BSD `sleep` が `infinity` を受け付けず usage エラーで即座に終了する（`exit 1`）ため、完了直後にペインが閉じて集計・ログパスを読み返せなくなること。もう 1 つは `exec` の時点で catch 済みの INT/TERM が `SIG_DFL` へ戻り（POSIX）、画面に出す `Press Ctrl-C to close session.` が機能しなくなることです。`wait_for_close` は自前の INT/TERM trap で端末を復元し、待機中の `sleep` を片付けてから `tmux kill-session` します。ワーカーペインの完了後待機も同じ理由で `while true; do sleep 3600; done` にしています。進捗バーは `seq` ではなく算術 `while` ループで描画します（BSD seq の `seq 1 0` が降順で `1 0` を返し、macOS で 0% / 100% 時にバーが 2 文字ずれるのを避けるため）。

`claude` エージェントのみ出力を `.jsonl` + `format-stream` パイプラインで処理します。`codex` 等の他エージェントは `.log` に直接出力します。

`claude` エージェントでは、`format-stream` / `tee` / raw jsonl 保存のいずれかが失敗した場合、または jsonl が空の場合、そのタスクは `failed-N` として扱い、`state.json` には記録しません。ログ・分類パイプラインが壊れたタスクを成功扱いしないためです。非 `claude` エージェントでも `tee` が失敗した場合は `failed-N` として扱います。

`claude` エージェントのタスク完了後は `token-burn classify-result <jsonl>` により jsonl 最終 `result` イベントの `is_error` / `api_error_status` を解析して分類します。

- 成功 (`is_error:false`) → `state.json` に記録
- レート制限（下記の判定に合致） → `failed-N` マーカー。`state.json` には記録しない
- プロバイダ側リトライ可能エラー (`api_error_status` が 408/429/5xx) → `retry-N` マーカー。`state.json` には記録しないため次回実行で再処理される。ワーカーは継続
- トランスポート層の一時障害（`api_error_status` が `null` かつ `terminal_reason` が `api_error`）→ `retry-N` マーカー。ワーカーは継続
- その他のプロバイダエラー → `failed-N` マーカーとエラーメッセージ（`result` フィールド）を表示し、ワーカーは停止

`api_error_status` が `null` のまま `terminal_reason` が `api_error` で終わったケースは、HTTP 応答そのものが返っていない = 接続断や名前解決失敗といったトランスポート層の一時障害です。実ログでは `API Error: Connection closed mid-response. The response above may be incomplete.` / `API Error: Unable to connect to API (ENOTFOUND)` が該当し、13 セッション中 5 セッションで発生していました。恒久エラー（`Failed`）に落とすとワーカーごと停止して以降のターゲットが 1 件も処理されないため、再試行可能として扱います。認証エラーや不正リクエストは HTTP ステータスを伴うため、この分岐には入らず従来どおり `Failed` のままです。

レート制限の判定 (`is_rate_limit_message`) は次のいずれかに合致した場合です。上限到達は `api_error_status` が 429 で返るため、この判定を漏らすとリトライ可能エラーへ落ち、回復しないまま残りのターゲット全件にエラー行が出続けます。

- `usage limit reached` を含む
- `hit your` 以降に `limit` を含む（実ログの `You've hit your session limit ...` / `You've hit your org's monthly spend limit ...`）
- `resets ` の直後が時刻表記（`3am` / `12pm` に加え、分を含む `2:30am` 形式にも対応）

`resets` の時刻判定は「時 → 任意の `:<分>` → `am`/`pm`」を明示的に読み進めます。`:` を単なる非数字として打ち切ると `:30am` が残って判定に失敗し、実ログの `resets 2:30am (Asia/Tokyo)` を取りこぼしていました。一方で `resets 5 times max. Please retry tomorrow at 8am` のように数字と `am`/`pm` が離れたメッセージは従来どおり誤検知しません。

jsonl ファイルが存在しない場合は result イベント無しと等価で Success として扱いますが、ファイルは存在するのに権限エラーや I/O エラーで読めない場合は `Failed` として返します（読み込み失敗を Success と誤分類して `state.json` に誤記録するのを防ぐため）。

`format-stream` は `tool_result` の `is_error:true` を検出した場合、エラー内容の先頭の有意な 1 行をサマリーとして表示します（単一行/複数行の `<tool_use_error>...</tool_use_error>` ラッパーは除去）。配列形式の `content` にも対応し、120 文字を超える場合は末尾を `...` で省略します。

`tool_use_result` の top-level メタデータに `truncated`、`appliedLimit`、`staleReadFileStateHint`、`userModified`（Edit/Write 等で書き込み前にユーザがファイルを変更していた場合。`user-modified` 形式）、`staleRecovered`（Edit が古い読み取り状態から自動回復した場合。`stale-recovered` 形式）、`success:false` / `error` / `message`、`stdout` / `stderr`（Bash 等の標準出力・標準エラー要約。`stdout:<summary>` / `stderr:<summary>` 形式）、Edit/Write 結果の `filePath` / `structuredPatch` / `replaceAll` / `memdirStamped`（`file:<path>`、`patch:<hunks> ... +追加/-削除`、`replace_all`、`memdir-stamped` 形式。`originalFile` / `oldString` / `newString` は巨大化するため表示しない）、`assistantAutoBackgrounded`、`backgroundTaskId`、`wasClamped` / `clampedDelaySeconds`、`persistedOutputPath` / `persistedOutputSize`、`returnCodeInterpretation`、`totalDurationMs` / `durationMs` / `totalTokens` / `totalToolUseCount`、`agentType`（Agent のサブエージェント種別。`agent:<type>` 形式）、`agentId`（非同期 Agent の識別子。`agent-id:<id>` 形式）、`resumedAgentId`（`SendMessage` で再開した Agent の識別子。`resumed-agent:<id>` 形式）、`resolvedModel`（Skill / Agent が解決したモデル名。`model:<...>` 形式）、`toolStats`（サブエージェントの編集行数。加除いずれか非ゼロのとき `edits:+<追加>/-<削除>` 形式）、`numFiles` / `numLines`、`file.numLines` / `file.totalLines`（Read の部分読み取り。`lines:<n>/<total>` 形式）、`file.truncatedByTokenCap`（Read の token cap 切り詰め。`truncated:token-cap` 形式）、`matches`（ToolSearch）/ `numMatches`（Grep の count モード）/ `mode` / `total_deferred_tools`、`results` / `searchCount` / `durationSeconds`（WebSearch の結果件数・検索回数・所要時間）、`code` / `codeText` / `bytes`（WebFetch の HTTP ステータス・応答サイズ。`http:<code> <text>` 形式）、`gitOperation`（git commit の sha / kind。`commit:<sha> <kind>` 形式）、`structuredContent.content`（Codex MCP 等の構造化応答。`structured:<summary>` 形式）、`tasks` / `task` / `taskId` / `task_id` / `task_type`、`retrieval_status`、`outputFile` / `canReadOutputFile`、`timeoutMs` / `persistent`、`statusChange`、`updatedFields`（TaskUpdate の変更フィールド一覧。`status` のみのときは `statusChange` と重複するため非表示、それ以外は `updated:<field1>,<field2>` 形式）、`isAsync`（Agent を `run_in_background=true` で起動した async-launched 応答。`async` として表示）、`scheduledFor`、`commandName`、`workflowName`（Workflow 起動結果。どのワークフローが走ったかを `workflow:<name>` 形式で表示。`runId` は内部識別子のため非表示）、`allowedTools`（Skill が許可するツール一覧。非空配列のときに件数を `allowed-tools:<n>` 形式で表示） が含まれる場合は、ツール完了行に短い補足として表示します。`matches` 配列は ToolSearch 専用で Grep の結果には存在しないため、Grep の count モードでは `numMatches` 整数から件数を表示します。Read の行数は実データでは `file` オブジェクトに入れ子で入り、部分読み取り（`numLines < totalLines`）のときのみ `lines:<n>/<total>` を表示します（全行読み取り時はノイズ回避のため省略）。`file.truncatedByTokenCap` が true の場合は、行数比率とは独立して `truncated:token-cap` を表示します。WebSearch の `searchCount` は通常 1 のため 2 以上のときのみ表示します。

`tool_use_result.type` は実データで `text`（通常の読み取り）/ `update`（書き込み）/ `file_unchanged` の 3 種類が現れます。このうち `file_unchanged` のみ `file-unchanged` として表示します。前回読み取りから内容が変わらず本文が返らなかったケースで、パス情報も `file.filePath` に入れ子で入るため top-level メタデータには何も出ず、表示しないと通常の Read 成功と区別できません（サブエージェント出力ファイルをポーリングしている最中の Read が実は何も取得していない、という判断材料を失う）。

`isImage` / `interrupted` は実データで `false` が常設されるため、`true` の場合だけ `image` / `interrupted` を表示します。`noOutputExpected` も同様に常設ですが、true でも「出力が無いのが正常」という意味しかなく表示価値が無いため出しません。`structuredPatch[].lines` は hunk 内の行だけを保持しファイルヘッダーを含まないため、`+` / `-` で始まる全行を加除として数えます。これにより、内容自体が `++` / `--` で始まる行が diff 上で `+++` / `---` になっても過少計上しません。

`tool_use_result` が object でなく文字列、または Context7 等で見られる `[{"type":"text","text":"..."}]` 配列の応答（実データで確認）は、成功時のみ先頭の有意な 1 行を `result:<要約>` として補足表示します。エラー時は content 側のサマリー表示と同文になるため補足しません。

実データで確認した `timedOutAfterMs` は、コマンド失敗ではなくバックグラウンド移行までの待機期限なので `wait-timeout:<期間>` と表示します。`backgroundCwdHint` は `cwd-hint:<要約>`、top-level `tool_result_meta[].non_execution_kind` は対象の tool use id と照合し、`not-executed:<理由>` として表示します。

モニターペインの進捗は `fail:<n> retry:<n>` を併記し、完了時も `%d succeeded / %d failed / %d retry` の形で表示します。

## 並列実行モデル

`execute_plan_tmux` はタスクキュー方式で並列実行します。

- 各タスクは `queue_dir/pending-<idx>` と `tasks/task-<idx>.sh` として事前に書き出される
- ワーカーは `pending-<idx>` を `mv` でアトミックに `claimed-<idx>` にリネームして claim し、対応する `task-<idx>.sh` を `source` で実行する
- 各タスクを `source` する前にワーカーループ先頭で `CANCELLED` フラグを 0 にリセットする。直前タスクの実行中に SIGINT/SIGTERM を受けて `CANCELLED=1` が立ったまま成功・早期 return しても、後続タスクの通常エラーを誤って `Cancelled` 判定しエラー記録を欠落させるのを防ぐ
- タスクがエラー終了してもワーカーは `exec sleep infinity` せず、即座に次の `pending-*` を取りに行く
- ワーカーは claim できる pending が尽きるまで処理を続け、尽きて初めて `worker-done-<w>` を作成して終了する
- ユーザーが tmux をデタッチした場合、tmux セッションが生存していれば `/tmp/token-burn` は削除しない。ワーカーのキュー・タスクスクリプト・プロンプトファイルを保持し、バックグラウンド実行を継続できるようにする
- tmux セッション作成後のペイン構築に失敗した場合は、作成途中のセッションを kill して一時実行ディレクトリも削除する
- レポートディレクトリ名に使うエージェント名は `sanitize_filename` でパス成分を無害化する

結果として、`parallelism` で指定した並列数はタスクが尽きるまで維持されます（一部タスクが失敗しても他ワーカーは止まらない）。エラーは `marker_dir/error-<idx>` にタスク単位で記録されるため、同一ワーカーで複数エラーが起きてもモニターに全て表示されます。

並列数は CLI の `--workers` / `-w`（1 以上。0 は clap の `value_parser` で拒否）で実行ごとに上書きでき、未指定なら `[settings].parallelism` を使います。実際に起動するワーカー数は `worker_count`（= `parallelism.min(タスク数)`）で頭打ちになり、`print_plan` が同じ関数で算出した実効値を実行計画に `Workers:` として表示します（頭打ちのときは要求値も併記）。表示と `execute_plan_tmux` の起動数が食い違わないよう、両者は必ずこの関数を経由します。

`format-stream` は以下の stream-json イベントを処理します:
- テキスト応答のストリーミング表示
- セッション開始（`system` / `init`）のモデル・CLI バージョン・権限モードを 1 行表示（`ℹ Session <model> (v<version>, <permissionMode>)`）。これらは他のどのイベントにも現れず、`result.modelUsage` からは実際に課金されたモデルしか分からないため、CLI バージョンと `bypassPermissions` で走ったかどうかが完全に失われていた。セッションにつき 1 行のみ
- 思考ブロック（`thinking`）のプログレスインジケーター
- 長時間ツールの `tool_progress` を経過時間付き（例: `Bash running (1m 30s)`）で表示
- ツール使用（`Read`/`Edit`/`Write`/`Bash`（小文字 `bash` を含む）/`BashOutput`/`Agent`/`Task`/`TaskCreate`/`TaskGet`/`TaskList`/`TaskUpdate`/`TaskStop`/`TaskOutput`/`Workflow`/`TeamCreate`/`Skill`/`SlashCommand`/`TodoWrite`/`Monitor`/`Grep`/`Glob`/`ScheduleWakeup`/`WebFetch`/`WebSearch`/`ToolSearch`/`SendMessage`/`AskUserQuestion`/Context7・Tavily・Codex MCP 等）の詳細表示と差分出力
- assistant メッセージの `fallback` コンテンツによるモデル切り替え（`from.model` → `to.model`）と、`message.diagnostics.cache_miss_reason` によるキャッシュミス理由・対象 input token 数の表示。`--include-partial-messages` が同一 message id の assistant メッセージを繰り返し出力しても、同じ診断は 1 回だけ表示する。これらの通知は出力がある場合だけ `break_open_line` で開きっぱなしの思考/テキスト行を閉じてから書く（`handle_assistant_event` が通知をいったんバッファへ書き、非空のときだけ行を閉じる）。思考ブロックは `\x1b[2m💭 ` を改行なしで書き進めるため、そのまま通知を書くと同じ行に連結され、通知末尾の `\x1b[0m` が dim を打ち消して以降の進捗ドットが崩れる。毎回無条件に閉じると assistant イベント（1 セッションで数千件）ごとに改行が入って進捗ドット表示自体が壊れるため、出力有無で分岐する
- `model_refusal_fallback` は切り替え元・切り替え先モデルとカテゴリを表示する。拒否対象の内容や explanation はモニターへ出さない
- `Read` の `file_path` と `offset` / `limit` / `view_range`、malformed 入力時の `__unparsedToolInput.len`、`Bash` の `timeout`（1000ms 以上は `timeout=<秒>s`、未満は `timeout=<n>ms` でミリ秒切り捨てによる "0s" 誤表示を回避。同じ整形を `Monitor` / `TaskOutput` の `timeout` でも使用）/ `run_in_background` / `dangerouslyDisableSandbox`、`BashOutput` の `bash_id`（出力取得対象の background bash。`bash:<id>` 形式）と任意の `filter`、`Agent` の `run_in_background` を表示
- `Agent` の任意 `model` / `isolation` を指定時だけ `model:<...>` / `isolation:<...>` として表示
- `Edit` は `new_string` に加えて実データで確認された `new_str` 入力も差分表示に使用し、`replace_all` が true の場合は一括置換として表示する。詳細行の `(+追加/-削除)` は行数差分（new − old）ではなく、共通プレフィックス/サフィックス除去後の実変更行数（表示 diff の `+` / `-` 行数と常に一致）。行数差分だと同一行数の in-place 置換が `(+0/-0)` になり「変更なし」に見える（実ログで確認）。行分割 (`split_lines`) は末尾の改行を「空の最終行」として保持する。`str::lines()` は末尾改行を落とすため、これが無いと `"foo"` と `"foo\n"` が同じ行集合になり、EOF 改行を足すだけの Edit が `(+0/-0)` かつ差分表示なしで「変更なし」に見えてしまう
- `Grep` / `Glob` の検索パターン、対象パス、`output_mode`、`type`、`glob`、`head_limit`、`context`、`offset`、`-A` / `-B` / `-C` / `-n` / `-i` / `-o`、`multiline` を表示
- `ScheduleWakeup` の待機時間と理由を表示
- `WebFetch` の URL とプロンプト要約、`WebSearch` のクエリと include/exclude ドメイン件数、`ToolSearch` のクエリと `max_results` を表示
- `Monitor` の説明・タイムアウト・condition・persistent、`TaskStop` の task id / task ids / reason、`TaskList`、`TaskGet` の task id、`TaskOutput` の task id / `block` / `timeout`、`TaskCreate` の `subject` / `description` / `activeForm`、`TaskUpdate` の `taskId` / `status` / `owner` / `subject` / `description`、`SendMessage` の送信先/要約、`AskUserQuestion` の質問数・選択肢数、`SlashCommand` の実行コマンド文字列（`/<command> ...`）、Tavily search の query/max/time range/search depth/topic/days（`topic=news` はニュース索引への切り替え、`days` はその遡及日数で、いずれも検索対象そのものを変える。実データの `topic:news days:8` は `time_range` を伴わないため、落とすと通常の Web 検索と区別できない）と Tavily extract の先頭 URL/件数(+N more)/extract_depth（`mcp__tavily__tavily-search` / `mcp__tavily__tavily_search` のハイフン版・アンダースコア版いずれも対応）、Codex MCP の prompt/cwd/model/sandbox/approval-policy、Context7 MCP ツールの library/query を表示
- `Workflow`（マルチエージェント・オーケストレーション）の起動対象を表示。名前指定（保存済みワークフロー）は `name`、インライン `script` は `export const meta = { name: ... }` から抽出したワークフロー名とスクリプト文字数（`<名前> (script:<n> chars)`。抽出できなければ `script:<n> chars`）、`scriptPath` 指定（再実行・resume）はファイル名を表示する
- ツール入力（`input_json_delta` で蓄積した JSON）がパースできない場合は、詳細を空にせず生入力の文字数を `unparsed:<n> chars` として表示し、malformed / truncated を可視化する。これはモデルが不正な JSON をツール入力として出力したケース（`InputValidationError: ... could not be parsed as JSON`）や、レート制限・セッション切断でツール呼び出しがストリーム途中で打ち切られたケースで発生する。`format-stream` はストリーミング経路（`content_block_start` で空入力 → `input_json_delta` で生 JSON 蓄積 → `content_block_stop` で確定）で処理するため、assistant メッセージ最終形の `__unparsedToolInput.len`（`Read` 等の専用ハンドラが別途処理）はストリーミングには現れない。引数なしツールの空入力（`TaskList` 等）は従来どおり空表示を維持する
- サブエージェントの開始・進捗・状態更新・完了通知（`task_started` / `task_progress` / `task_updated` / `task_notification`）。`task_started` は `task_type=local_agent` のような実行方式より、存在する場合は `subagent_type`（`general-purpose` / `Explore` 等）を優先表示する。`task_notification` は `completed` / `failed` / `stopped` を表示し、`failed` の `summary` を失敗原因として併記する。`usage` が無い場合は duration/token を 0 として表示しない。`task_updated` の `killed` は `failed` / `cancelled` と同じ失敗状態として強調表示し、`patch.error`（例: `Agent terminated early due to an API error: ...`）があれば失敗理由として併記する。これを落とすと `Task failed` だけが残り、無人実行でサブエージェントが死んだ理由を後から追えない。`status` を伴わず `patch.is_backgrounded:true` だけの更新は `Task backgrounded` として表示する（以降そのタスクの出力がインラインに出なくなる理由そのもののため）
- `background_tasks_changed` は実行中バックグラウンドタスク一覧の高頻度スナップショットで、個々の開始・進捗・完了は上記タスクイベントにより表示済みのため、重複ノイズとして明示的に無視する
- Claude Code のシステム通知（`notification`。例: stop hook エラー）と、出力を伴う hook 診断（`hook_progress` / `hook_response` の output / stderr / stdout）。候補キーの走査には `first_string` ではなく `first_non_empty_string` を使う。実データの `hook_response` は `output` / `stdout` / `stderr` を常に持ち、失敗時は stderr にだけ内容が入るため、値が文字列でありさえすれば空文字でも確定する `first_string` だと `output:""` が採用されてフォールバックが到達不能になり、診断が最も欲しい場面で "no output" にしかならなかった
- 表示対象の system / `rate_limit_event` / `user`（ツール完了行） / `tool_progress` は先にバッファへ描画し、出力がある場合だけ開いている本文・思考行を閉じてから独立行へ書く。バックグラウンド/非同期ツールの完了とハートビートは次ターンのテキスト・思考 delta の途中にも到着するため、直接書くと開きっぱなしの行へ連結される（実ログの整形結果に `💭   ✓ WebFetch` の形が 38 件あった）。実ログでは text delta の `I` と `'ll` の間に rate-limit 通知が到着し、単語中へ通知が連結されていた。`thinking_tokens` / 詳細のない `allowed` など無視対象イベントではバッファが空なので、本文へ不要な改行を増やさない
- `tool_use_result` の出力切り詰め、適用 limit、stale read ヒント、ユーザ変更検出（`user-modified`）、古い読み取り状態からの自動回復（`stale-recovered`）、メモリ用ディレクトリへの印付け（`memdir-stamped`）、失敗理由（`error:`）や結果メッセージ（`message:`）、Bash 等の標準出力/標準エラー要約（`stdout:` / `stderr:`）、構造化応答の要約（`structured:`）、文字列/text ブロック配列で返る成功結果の要約（`result:`）、Edit 結果のファイルパスと structured patch 規模（`file:<path>` / `patch:<hunks> ... +追加/-削除` / `replace_all`）、自動バックグラウンド化、clamp、永続化出力サイズ、戻りコード解釈、Agent の duration/token/tool 数・サブエージェント種別（`agent:`）・識別子（`agent-id:`）・再開した識別子（`resumed-agent:`）・解決モデル（`model:`）・編集行数（`edits:+追加/-削除`）、Grep/ToolSearch の結果件数と mode、WebSearch の結果件数/検索回数/所要時間、WebFetch の HTTP ステータス/応答サイズ、Read の部分読み取り行数（`lines:<n>/<total>`）と token cap 切り詰め（`truncated:token-cap`）、タスク件数/task id/task type、TaskOutput の取得状態、Agent 出力ファイル、Monitor の timeout/persistent、TaskUpdate の状態遷移、ScheduleWakeup の予定時刻、Skill のコマンド名、Workflow のワークフロー名（`workflow:<name>`）の補足表示
- トークン使用量、コスト、キャッシュ内訳、Web検索/フェッチ回数の集計表示
- モデル別使用量（`modelUsage`）の内訳表示（キャッシュ読み取り/書き込みトークン、Web検索回数、`contextWindow` / `maxOutputTokens` を `ctx:1M` / `max_out:64K` のような単位付きで表示）
- API応答時間（`duration_api_ms`）と初回トークン到達時間（`ttft_ms`）、初回ストリームトークン到達時間（`ttft_stream_ms`。キュー/リトライ待ちを含む `ttft_ms` より小さい純粋なストリーム遅延。`stream:` 形式）、リクエスト送信までの所要時間（`time_to_request_ms`。通常数十〜数百 ms のためミリ秒表記 `req:<n>ms`）の表示
- fast mode 状態（`fast_mode_state` が `off` 以外の場合）と、利用できない理由（空でない `fast_mode_disabled_reason`）の表示
- 異常終了時の `terminal_reason`（`completed` 以外の場合）と `permission_denials` の件数・ツール名表示
- result の `usage.service_tier`、`usage.speed`、空でない `usage.inference_geo`、`usage.iterations` 件数、`origin.kind` の表示
- レート制限警告（`rate_limit_event`）の使用率表示、リクエスト拒否通知、および overage（超過枠）の補足情報表示（`overageStatus` / `overageDisabledReason` / `overageResetsAt` / `isUsingOverage`・`overageInUse`）。補足は `allowed` だけでなく `allowed_warning` と `rejected` にも付ける。実データの `rejected` は `overageStatus` / `overageResetsAt` / `isUsingOverage` を伴い、これらを落とすと「5 時間枠の resets 時刻」だけが残って、実際は超過枠まで使い切って復旧が数週間先でも「その時刻まで待てば再開できる」と誤読される。`isUsingOverage` と `overageInUse` は実データで同義の別キーとして両方現れるため、どちらか一方でも true なら `using_overage` を表示する。`allowed_warning` 時に `surpassedThreshold` が含まれている場合は通過済み警告閾値（例: `warning at 90%`）を併記する
- リセット時刻（`resetsAt` / `overageResetsAt`）は当日中なら `HH:MM`、翌日以降なら `MM/DD HH:MM` で表示する。時刻だけだと `seven_day` 枠（最大 7 日先）や overage 枠（実データで 28 日先）のリセットが「今日のその時刻」に見え、待てば再開できると誤読される（実ログでは復旧が 1 か月先でも `resets 09:00` としか出ていなかった）
- レート制限使用率が `rate_limit_threshold`（デフォルト: 95%）を超えた場合、stop file を作成して後続タスクを自動停止。stop file の作成は usage-gate と同じく `create_new` で冪等（並列ワーカーから同時に呼ばれても既存内容は上書きしない）。`AlreadyExists`（別ワーカーが作成済み）は正常系として無視するが、ENOSPC・権限不足等で作成に失敗した場合は黙って握り潰さず、停止シグナル（stop file）が生成されない旨を出力に明示する（`format-stream` はパイプ中段のため exit code が観測されない）
- APIリトライ（`api_retry`）の試行回数とエラー情報の表示。実データには `error` フィールドの無い api_retry があり、その場合は "unknown" を補わず試行回数（と `error_status` があればそれ）だけを表示する
- `status`（リクエスト状態通知）と `thinking_tokens`（思考トークンの推定累積値 `estimated_tokens` / `estimated_tokens_delta`）は高頻度（1 セッションで数千件）に出力されるノイズイベントのため、明示的に無視します。思考中の進捗は `thinking_delta` のドット表示、トークン総数は `result.usage` の集計表示で代替するため、これらを表示すると重複・冗長になります

なお `usage` フィールドは各 `message_start` / `message_delta` でその API 呼び出し単独の値を返し、`result` イベントに最終累計が入るため、`format-stream` は `result` の値を最終出力として優先します。

処理済み状態は有効な設定ファイルと同じディレクトリの `state.json` に保存されます（デフォルト: `~/.config/token-burn/state.json`）。エージェント名は昇順、各エージェント内のエントリは処理時刻の降順（同時刻はパス昇順で安定化）で書き出します。内側のマップを `serde_json::Map` へ `collect()` してはいけません。`preserve_order` feature を有効にしていない serde_json の `Map` は `BTreeMap` であり、collect した時点でキー（パス）昇順へ再ソートされ、並べ替えが丸ごと捨てられます（実際の `state.json` も全エージェントがパスのアルファベット順になっていました）。順序を保つために `OrderedEntries` ラッパーで `serialize_map` を直接使います。

`[settings]` の `limit` は 1 以上である必要があります。
`[settings]` の `parallelism` は 1 以上である必要があります（CLI の `--workers` / `-w` で実行ごとに上書き可能）。
`[settings]` の `rate_limit_threshold` は 1〜100 の範囲で指定する必要があります（デフォルト: 95）。レート制限使用率がこの閾値を超えると、現在のタスク完了後に後続タスクの実行を停止します。`rejected` イベント受信時も同様に停止します。ai-usage 連携が有効な場合は、各タスク完了後に該当 agent の実使用率（weekly / five_hour の最大）でも `usage-gate` が判定し、閾値以上なら停止します。
`[settings]` の `skip_within` と `cleanup_after` には `d` / `h` / `m` / `s` を使った有効な期間文字列を指定する必要があり、不正または `chrono::Duration` で表現できない値は設定読み込み時にエラーになります。期間自体は表現できても日時の減算範囲を超える場合、`skip_within` は警告後に前回リセット時刻へフォールバックし、レポートクリーンアップはエラーを返します。

### 処理済み履歴の共有範囲（dedup_scope）

`state.json` は展開エージェント名ごとに履歴を記録するため、既定ではアカウント A で処理したリポジトリもアカウント B からは未処理のままです。同じ CLI を 2 アカウントで回すと、B は A の続きからではなく同じ先頭ターゲットを再処理します。`[settings]` の `dedup_scope` は**スキップ判定で参照する範囲**を決めます（`global` | `provider` | `agent`、デフォルト `agent` = 従来の分離挙動）。

- `global`: 全エージェント横断。`state.json` にしか無い名前（改名・削除済みエージェント）の記録も参照する
- `provider`: 同じ `provider` のエージェント同士のみ共有。`provider` 未設定のエージェントと、現在の設定に無い名前は自分自身の記録だけを見る。`state.json` は provider を持たず現在の `RuntimeAgent` 一覧からしか復元できないため、別 provider の履歴を誤って引き当てて実行を握り潰すより取りこぼす方へ倒している
- `agent`: 実行中のエージェントのみ（従来どおり）

**書き込み側は変えません**。完了は常に実際に実行したエージェント名のキーへ記録するため、`state.json` のスキーマも「どのアカウントが処理したか」の履歴も保たれ、広がるのは参照側だけです（`State::last_processed_in_scope`）。

共有 scope（`global` / `provider`）は `skip_within` を必須にします。`skip_within` 省略時のカットオフは `sched.state_cutoff` = 実行中エージェントの前回リセット時刻でエージェント固有のため、他エージェントの履歴へ適用するとスキップ範囲が「どのエージェントで起動したか」次第で揺れます。設定側は `Config::validate`、CLI 上書き側は `resolve_dedup_scope` (`main.rs`) が同じ検査をします（CLI で `agent` から `global` へ引き上げた場合は `validate` を通らないため二重に置いています）。

CLI の `--dedup-scope <global|provider|agent>` で実行ごとに上書きできます。別アカウントが処理済みのリポジトリを意図的にもう一度回したいときは `--dedup-scope agent` を指定します。スキップ表示は件数だけでなく scope・窓・どのエージェントの記録で弾いたかの内訳（`SkipSummary`）を出します。件数のみだと「統合が効いてスキップされた」のか「ターゲット探索が壊れて候補が消えた」のかを実行ログから切り分けられないためです。

`[[scan]]` で `username` を指定した場合、リポジトリ可視性（public/private）はローカルディレクトリ名ではなく `origin` の remote URL に含まれるリポジトリ名（大文字小文字を無視）で照合されます。`username` を指定しない通常スキャンでは `origin` remote がなくても対象に含まれ、可視性は `Unknown` になります。

remote URL の owner / repo 抽出は末尾 2 セグメントを採用するため、GitLab のサブグループ（例: `git@gitlab.example.com:group/subgroup/repo.git`）でも直近の親 (`subgroup`) を owner、`repo` を repository 名として認識します。GitHub の `owner/repo.git` のような 2 セグメント構成はそのまま機能します。

`[[scan]]` のディレクトリスキャンではシンボリックリンクはスキップされます（循環リンクによる無限再帰を防止）。

読み取りに失敗したディレクトリ（権限不足、走査中の削除等）は警告を出してスキップし、走査を続けます。存在しない `base_dirs`、取得に失敗した `DirEntry`、`origin` remote を取れないリポジトリと同じ「警告して継続」の方針です。以前は `find_repos` の `read_dir` だけがエラーを `run` / `list` まで伝播していたため、スキャン対象ですらない中間ディレクトリが 1 つ読めないだけでリポジトリを 1 件も処理せず異常終了していました。

複数の `[[scan]]` 設定で同一ディレクトリが重複検出された場合、ターゲットは1件に正規化されます（同一リポジトリの重複実行を防止）。

ディレクトリパスは重複排除と状態管理の前に絶対パスへ正規化されるため、`repo` と `./repo` のような等価な相対パスは同一ターゲットとして扱われます。

この正規化と重複排除は、`token-burn run PATH...` で特定ディレクトリを強制実行する場合にも適用されます。

`[[targets]]` には `defer = true` を指定でき、true のターゲットは実行リストの末尾に集められます（`scan` 由来のターゲットは常に `defer=false`）。`resolve_targets` の最後で `sort_by_key` による安定ソートが行われるため、`scan` 内の Visibility 順や `[[targets]]` 同士の追加順は各グループ (defer=false / defer=true) 内で維持されます。`token-burn run PATH...` で明示指定した場合は CLI 指定順を優先するため `defer` フラグは反映しません。

### 実行順（最終ファイル変更日時が古い順）

処理済みフィルタ (`filter_by_state`) の後、`limit` を適用する前に `sort_by_least_recent` (`main.rs`) が **最終ファイル変更日時の古い順** にターゲットを並べ替えます。`defer` の優先度はそのまま維持し、その内側だけを並べ替える安定ソートのため、変更日時が同じターゲット同士の順序（`scan` 内の Visibility 順 / `[[targets]]` の追加順）は変わりません。変更日時を取得できなかったリポジトリは判断材料が無いので各グループの末尾に置きます。`token-burn run PATH...` で明示指定した場合は CLI 指定順を優先するため並べ替えません。

可視性（`public_first`）でグループ化するかどうかは `public_first_enabled` (`main.rs`) が判定し、**いずれかの `[[scan]]` が `public_first = true` のときだけ** `visibility` をソートキーへ入れます。無条件に入れていた頃は、`public_first` を読むのが `scanner::scan_directories` の 1 箇所だけなのに最終順序が必ず public 優先になり、`public_first = false` が黙って無視されていました（`limit` と併用すると、公開リポジトリが `limit` 件以上ある限り非公開リポジトリへ永久に到達しない）。`[[scan]]` が無い構成（`[[targets]]` のみ）でもグループ化しません。

処理済みカットオフ（`skip_within` / 前回リセット）は絶対時刻の窓であり、窓をまたいだ時点で処理済み履歴が一斉に無効化されます。ターゲット順が固定のままだとそのたびにリストの先頭 `limit` 件だけが再処理され、末尾のリポジトリには永遠に到達しませんでした（実測: 先頭 10 件が 2 日おきに再処理される一方、11 件目以降は 2 か月近く未処理）。古い順に並べ替えることで、カットオフが切れても前回処理した分は後ろへ回り、放置されているリポジトリから消化されます。

順序の基準は `state.json` の処理時刻ではなくリポジトリ自身の最終ファイル変更日時です。レート制限（429）で中断されて実際には何も変更できなかった実行を「処理済み」と数えてしまわないためです。

最終ファイル変更日時は `scanner::repo_last_modified` が `git ls-files` の列挙する追跡対象ファイルの mtime の最大値として求めます。ディレクトリを素朴に走査すると `target/` や `node_modules/` のビルド成果物が混ざり、`cargo build` しただけのリポジトリが「たった今変更された」ように見えてしまいます。追跡対象に限定すればビルド成果物と `.gitignore` 対象は自然に除外され、未コミットの編集は mtime としてそのまま拾えます。1 リポジトリにつき `git ls-files` の子プロセス起動が要るため、`repo_last_modified_map` が blocking タスクとして並行実行します。`list` / `run` のターゲット一覧にはこの日時が `(modified: ...)` としてローカル時刻で併記され、実行順の根拠を目視で確認できます。

## 実装上の注意点

- tmux へ渡すスクリプトパスは `tmux_script_arg`（= `shell_escape`）でクォートします。tmux は `new-session` / `split-window` の shell-command を `sh -c` 経由で実行するため、`std::env::temp_dir()`（= `TMPDIR`）に空白が含まれる環境では未クォートだと `/tmp/tb` を `space` `test/monitor.sh` を引数に起動しようとしてペインが即死します。しかも **tmux 自身は exit 0 を返す**ため直後の `ensure!(status.success())` では検知できず、後続の split-window が「no such session」で失敗して真因と無関係なエラーになります。生成するシェルスクリプトの内側は `shell_escape` 済みで、この tmux 呼び出しだけが取りこぼしでした。
- ワーカーは全タスク完了後に `trap - INT TERM` でキャンセル trap を外してから待機します。待機を `exec` で置き換えていた頃は exec が catch 済みシグナルを `SIG_DFL` へ戻していたため、待機中の Ctrl-C は既定動作（ペインを閉じる）でした。有限 sleep のループへ変えた分、明示的に解除して同じ挙動を保ちます（残すと処理するタスクが無いのに `handle_cancel` だけが走り、ペインも閉じません）。

- レポート出力先 (`resolve_report_dir` / `main.rs`) は設定値を必ず絶対パスへ正規化します（`config::resolve_directory` 経由）。相対パスのまま返すと、レポートディレクトリの作成（`executor` 側。プロセスの cwd で解決）と、そこへ書き込むタスクスクリプトの `tee` / `--raw-output`（対象リポジトリへ `cd` した後で解決）が別ディレクトリを指し、ログのパイプラインが `No such file or directory` で失敗して全ターゲットが `failed-N` になります（`state.json` に 1 件も記録されない）。`report_dir = "reports"` のような素直な設定で踏みます。
- `format-stream` の入力読み取りは `read_lines_lossy` で行い、不正な UTF-8 バイトは U+FFFD へ置換します。`BufRead::lines()` は非 UTF-8 バイトを含む行に `Err(InvalidData)` を返し、そこで中断すると以降の**正常な JSON も含めて**標準出力と `--raw-output` の両方から失われます。タスクスクリプトは `claude ... 2>&1 | token-burn format-stream ... | tee log` で stderr を同じパイプへ合流させており、stream-json の 1 行は macOS の `PIPE_BUF`（512 バイト）を常に超えるため、stdout の途中に stderr の書き込みが割り込んでマルチバイト文字が分断されるだけで不正な UTF-8 が生じ得ます。中断すると `FORMAT_EXIT != 0` で `failed-N` になるうえ、パイプが閉じて `claude` 本体が SIGPIPE で落ち、表示整形の都合で数時間の実行を巻き添えにします。
- モニターは 1 周回の先頭で `worker-done-*` を**タスクマーカーより先に**読みます。ワーカーは `done-*` / `failed-*` / `retry-*` を書き切ってから `worker-done-*` を作るため、この順序なら「worker-done は見えているのにそのワーカーのタスクマーカーが見えていない」状態は起こりません。逆順（タスクマーカー → `fetch_usage` で最大 `AI_USAGE_TIMEOUT` 秒ブロック → worker-done）だと、その待ち時間に最後のワーカーが完走した場合に古い `PROCESSED` と新しい `WORKERS_DONE` が組み合わさり、全件成功でも `⏹ Stopped: 9/10 processed` と誤報告します。
- モニターの ai-usage 再取得スロットル (`LAST_USAGE`) は fetch **完了時刻**を記録します。`fetch_usage` は `run_with_timeout` を 2 回呼ぶため最長 `2*AI_USAGE_TIMEOUT` 秒かかり、開始時刻（ループ先頭の `NOW`）を基準にすると次の周回で即座に条件が成立して間隔を空けずに再取得し続けます。ループ外の初回 `fetch_usage` 直後にも記録し、起動直後の二重取得を防ぎます。
- `ai-usage --json` の枠データは `kind`（`five_hour` / `daily` / `weekly` / `monthly`）を読み、そこから `state_cutoff` の周期を導きます。スロット名（`weekly` / `five_hour`）と実際の枠長は一致しません。実データでは antigravity が `five_hour` スロットに `kind:"daily"`（24 時間枠）を、pixellab が `weekly` スロットに `kind:"monthly"`（月次枠）を返します。スロット名で決め打ちすると 24 時間枠を 5 時間として扱い、`state_cutoff`（= 直前の枠の開始点）が未来へ飛んで `filter_by_state` の `last >= cutoff` が恒偽になり、処理済みフィルタが黙って無効化されます（毎回同じ先頭ターゲットだけを再処理し続ける）。未知の `kind` はスロット名の既定周期へ落とします。
- `scanner` から `git` を起動する箇所は `run_git_capture` に集約し、パスを `OsStr` のまま渡します。`to_string_lossy()` は不正な UTF-8 バイトを U+FFFD へ置換するため、非 UTF-8 のディレクトリ名では存在しないパスを git に渡すことになり、`username` 指定時はそのリポジトリが黙って対象から消えます。`git` 自体が PATH に無い場合は 1 プロセスにつき 1 回だけ警告します（黙って `None` にすると全リポジトリが対象外になって `No targets found` だけが出て、原因の手掛かりがゼロになる）。

- リセット日時計算 (`schedule.rs`) は `naive_local()` をベースに行います。`DateTime::date_naive()` は UTC 日付を返すため、`weekday()` のローカル曜日と整合させるためにローカルタイムゾーンの日付を基準とします。Asia/Tokyo のような UTC+N のタイムゾーンで深夜帯（UTC 前日）に実行しても曜日がずれない設計です。
- リセット時刻が DST（夏時間）遷移に重なる場合も `resolve_local_datetime` (`schedule.rs`) で解決します。曖昧な時刻（秋の繰り戻しで 2 回出現する時刻）は早い方を採用し、存在しない時刻（春の繰り上げでスキップされる時刻）は遷移直後の最初の有効な瞬間にフォールバックします。`from_local_datetime().earliest()` は存在しない時刻に対して `None` を返すため、`America/New_York` の `02:30` のように DST ギャップへ重なるリセット時刻だと、設定読み込みは成功するのに `status` / `run` が実行時に毎回失敗していました。これを防ぐ実装です。
- 状態ファイル (`state.json`) の書き込みは「同一ディレクトリのテンポラリファイル (`.state.json.tmp.<PID>.<nanos>`) に書き出し → `rename` で本体に置き換える」 atomic rename パターンで行います。排他ロックは `state.json` 本体ではなく sidecar の `.state.json.lock` に取ります。本体をロックすると `rename` 後にロック対象 inode が古くなり、別ワーカーが新しい `state.json` を同時ロックできて更新を失うためです。`write_all` 途中の ENOSPC やプロセスクラッシュでも本体が壊れず、書き込み失敗時はテンポラリファイルを掃除します（次回起動時に残骸が積み重ならない）。ロック取得後に既存の JSON が壊れていた場合や、権限・I/O エラーで読み取れない場合は、空状態として上書きせず更新をエラーで中断し、原本と処理済み履歴を保全します。読み取り側の `State::load` も同じ方針で、存在しない場合のみ空状態として扱い、権限・I/O エラーはエラーとして伝搬します（JSON 破損時のみ警告を出して空状態で続行）。ここを空状態へ潰すと `filter_by_state` が全ターゲットを未処理と判断して消化済みリポジトリを再実行しクォータを二重消費するうえ、`token-burn mark` 側は書き込みで正しくエラーになるため `state.json` が更新されず、次回以降も同じ状態が再現して延々と同じターゲットを処理し続けます。
- tmux ワーカー / モニター起動前の `chmod +x` は終了コードを検証します。`output()` の戻り値だけ確認する旧実装では `chmod` が非ゼロで終了しても無視されてしまい、`permission denied` が tmux ペイン内で初めて顕在化していました。
- tmux セッション作成後にペイン分割・ワーカー起動が失敗した場合は、そのセッションを kill して一時実行ディレクトリを削除します。セッションだけ作成されて後続コマンドが失敗すると、従来は孤立セッションと `/tmp/token-burn` 配下の実行資産が残っていました。
- 起動時キャッシュ初期化で ai-usage を同期起動する `spawn_ai_usage_sync_with_timeout` (`executor/mod.rs`) は、子プロセスの stdout/stderr を**別スレッドで並行に drain** します。子の終了を待ってからまとめて読む実装では、出力がパイプバッファ（macOS では 16KB 程度）を超えたとき子の `write(2)` がブロックして終了できず、`try_wait` が永遠に `None` を返してタイムアウトまでハングするデッドロックに陥ります（大きな JSON や stderr へのログ出力で発生）。読み取りを終了監視から分離することでこれを防ぎます。
- `format-stream` の `truncate_str` は「返却文字列の char 数を `max` 以下に保つ」契約を満たします。省略記号 `"..."` を付ける余地が無い `max <= 3` の場合は先頭から `max` 文字までで切り詰めます（実コードの呼び出しサイトは最小でも 30 程度のため、契約強化に伴う表示変更はありません）。
- レポートディレクトリのクリーンアップ (`cleanup.rs`) はシンボリックリンクをスキップします。`Path::is_dir()` はリンクを追跡するため、リンク先のディレクトリを誤って削除しないよう `is_symlink()` で除外します。
- モニタースクリプトのエラーマーカー走査は `while IFS= read -r ... < <(find ...)` 方式を使用しており、`TMPDIR` のパスに空白が含まれる環境でもワードスプリットが発生しません。エラー内容の表示は `printf '%s'` 経由で行い、ファイル内容を `echo` のダブルクォート内で再解釈しないようにしています。
- デタッチ実行後のログを整形する `strip_ansi` (`executor/util.rs`) は、charset designation エスケープ（`\x1b(B` = G0 を ASCII 集合に指定、`\x1b(0` = DEC 罫線集合等）を introducer（`( ) * + - . /`）＋終端バイトの 3 バイトとして扱い両方を除去します。introducer だけをスキップする実装では終端バイト（`\x1b(B` の `B` 等）が通常文字としてログに漏れます。その他の 2 バイトエスケープ（`\x1b=` / `\x1b>` / `\x1bM` 等）は従来どおり ESC ＋ 1 文字だけスキップします。
- モニタースクリプトの `run_with_timeout` (`executor/scripts.rs`) は、監視サブシェルの stdout を必ず `>/dev/null 2>&1` で捨てます。呼び出し側の stdout を継承したままだと、コマンドが即座に終わってもサブシェルの子 `sleep $secs` がコマンド置換のパイプ書き込み端を握ったまま孤児化し（`kill -TERM $wpid` はサブシェル本体しか殺せない）、`new=$(run_with_timeout ...)` が EOF を待って **timeout 秒まるごとブロック**します。ハング対策のはずが、10 秒ごとの ai-usage 取得で毎回 `AI_USAGE_MONITOR_TIMEOUT_SECS`（30 秒）固まり、毎秒更新のはずの進捗バーとデッドライン残り時間が止まっていました（実測: 即終了コマンドに 8 秒指定 → 8 秒）。
- タスクスクリプトは対象ディレクトリへの `cd` をパイプラインと分けて発行します。`build_shell_command` は `cd` を含めず、`build_task_script` が手前で `cd <dir> || { ...; return 0; }` を出します。`cd X && cmd 2>&1 | format-stream | tee log` と書くと bash は `cd X && (3 要素パイプライン)` と解釈するため、cd 失敗時はパイプラインが実行されず `PIPESTATUS` が cd の 1 要素だけになります。すると `FORMAT_EXIT` / `TEE_EXIT` が空文字に展開されて `[ "" -ne 0 ]` が `integer expression expected` を吐き（ワーカーペインに漏れる）、記録されるエラーも真因と無関係な「logging pipeline failed」になっていました。スキャンから実行までの間に対象リポジトリが削除・リネームされると発生します。
- 実行用一時ディレクトリの準備は `prepare_run_tmp_dir` (`executor/mod.rs`) が行い、`remove_dir_all` の失敗を（`NotFound` を除き）エラーとして伝播したうえで、作成後に unix では `0o700` を設定します。旧実装の `let _ = remove_dir_all(...)` は削除失敗を握り潰して、消せなかったディレクトリをそのまま再利用していました。`temp_dir()` が共有の `/tmp` になる環境（Linux。macOS は `TMPDIR` がユーザーごと）では、他ユーザーが先に `/tmp/token-burn` を作っておくと sticky bit により削除が失敗する一方 `create_dir_all` は成功するため、他人の所有ディレクトリへワーカースクリプトやプロンプトを書き込んでしまいます。
