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
│   ├── classify.rs         # 完了 jsonl の分類（success / failed / rate-limited / retryable / resume-unavailable）
│   ├── rate_control.rs     # 枠の周期に応じた停止判定（一時停止 pause / 恒久停止 stop）・pause file・gate-wait
│   ├── cleanup.rs          # レポートディレクトリの自動クリーンアップ
│   ├── state.rs            # 処理済みターゲット状態の永続化
│   ├── resume.rs           # 中断セッション（レート制限で終了）の session_id 保存・再開判定（resume.json）
│   ├── tui.rs              # --interactive の対象選択 TUI（ratatui。選択・並べ替え）
│   └── display.rs          # ステータス表示・プログレス出力
├── Makefile                # ビルドコマンド
└── .github/workflows/      # CI/CD
```

## 技術スタック

- **Rust** (edition 2024)
- clap (CLI), serde + toml (設定), chrono + chrono-tz (日時), tokio (非同期), colored (出力), ratatui (対象選択 TUI)

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
- `[settings]` - 並列実行数、スキップ期間、レポート設定、ターゲット上限、処理済み履歴の共有範囲、中断セッションの保存・再開（`resume_interrupted`）
- `[prompts]` - デフォルトプロンプト（`default`）、中断セッションを再開するときの継続プロンプト（`resume`、任意）
- `[ai_usage]` - ai-usage --json 連携設定（任意。enabled / window / fallback / state_window / `[[ai_usage.profiles]]`）
- `[[agents]]` - エージェント定義（command, provider, env, リセットスケジュール, prompt, ai_usage 連携）
- `[[scan]]` - ディレクトリ自動スキャン設定
- `[[targets]]` - 個別ターゲット（任意）

`[[agents]]` の `name` は空文字不可、`command` は1要素以上必須（先頭要素は実行ファイル名）です。`reset_weekday` / `reset_time` / `timezone` は ai-usage 連携かつ fallback が `fixed` 以外のときは省略可、それ以外（ai-usage 非連携、または fallback=fixed）では必須です。`env`（環境変数マップ）のキーは `[A-Za-z_][A-Za-z0-9_]*` に制限され、値は読み込み時に `~` 展開されます。

展開後の `RuntimeAgent` 名は全体で一意でなければならず、重複すると設定読み込み時にエラーになります。`[[agents]]` の `name` 重複だけでなく、profile 展開名が別 agent の名前と衝突する場合（agent `claude` が profiles `["work", "home"]` を参照して `claude-home` へ展開され、かつ agent `claude-home` も定義されている場合）も同様です。展開名は `state.json` のキー・レポートディレクトリ名・`--agent` の指定子を兼ねるため、重複すると `--agent <name>` が常に先頭の 1 件しか選べず 2 件目が起動不能になり、別々のエージェントが同じ `state.json` キーへ書いて既定の `dedup_scope = "agent"`（エージェントごとに完全分離）が黙って破れます。

実行ファイルが `claude` の場合、`-p`、`--verbose`、`--output-format stream-json`、`--include-partial-messages`、`--disallowedTools=AskUserQuestion` は自動付与されます。`--output-format` が既存でも値は `stream-json` に正規化されます。既存の `--disallowedTools` / `--disallowed-tools` がある場合は、必要に応じて equals 形式へ正規化して `AskUserQuestion` を追記します。さらに env に `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0` をデフォルト注入します。`claude -p` はメインターン終了後にバックグラウンドタスク（background 起動のサブエージェント / Workflow）を既定 600 秒しか待たず強制終了し、未完のまま `is_error:false` で成功終了してしまうため、無期限待機に切り替えて完走させます。agent / profile の `env` に同キーが明示されていれば尊重します（空文字なら unset）。この注入はタスク実行コマンドだけに効かせ、usage-gate / monitor statusline に渡す env スナップショットには含めません。

実行ファイルが `codex` の場合、無人実行で承認待ちにより停止しないよう `-c approval_policy=never` を実行ファイル直後に自動付与します（`codex -c approval_policy=never exec ...`）。`codex exec` には `--ask-for-approval` フラグが無い（0.136.0）ため、サブコマンドのオプション表面に依存しない top-level の config override として挿入します。`--sandbox`（サンドボックス）とは独立した軸のため、サンドボックス指定の有無に関わらず付与します。ユーザーが承認方針を明示済みの場合（`-a` / `--ask-for-approval` / `-c approval_policy=...` / `--dangerously-bypass-approvals-and-sandbox`）は上書きしません。

### ai-usage 連携

`[ai_usage]` を設定すると、各エージェントのリセット時刻を `ai-usage --json` の実データ（`weekly.resets_at` 等）から自動取得します。`[ai_usage]` が無い、または `enabled = false` の場合は従来どおり `reset_weekday` / `reset_time` / `timezone` による曜日ベースの固定計算のみで動作します（後方互換）。

- `[ai_usage]`: `enabled`（連携の有効化）、`command`（デフォルト `["ai-usage", "--json"]`）、`window`（deadline 算出枠。`weekly` | `five_hour` | `nearest`、デフォルト `weekly`）、`fallback`（解決失敗時の方針。`fixed` | `skip` | `error`、デフォルト `fixed`）、`state_window`（処理済みカットオフの枠。`weekly` | `selected`、デフォルト `weekly`）。
- `[[ai_usage.profiles]]`: `name`（内部参照名）、`profile`（`ai-usage --json` の `profile` と大文字小文字を区別して照合）、`env`（そのアカウントで起動する際に付与する環境変数。例: `CLAUDE_CONFIG_DIR`）。
- `[[agents]]` 側: `provider`（`claude` | `codex` | `antigravity`。ai-usage の `(profile, provider)` 照合に使うため連携時は必須）、`[agents.ai_usage]` の `profiles`（参照する profile 名のリスト。同一 agent 内で同じ profile 名を重複参照すると同名の `RuntimeAgent` が二重生成されるため、設定読み込み時にエラーになります）、任意の `window` / `fallback` 上書き。

実行時は agent × profile を `RuntimeAgent` に展開します。例えば agent `claude` が profiles `["work", "home"]` を参照する場合、`claude-work` / `claude-home` の 2 エージェントに展開され、それぞれ profile の `env`（agent の `env` を上書きマージ）を付与して起動します。**profile を 1 つだけ参照する agent は展開名が agent 名のまま**になります（例: agent `codex` が profiles `["home"]` のみ参照 → `codex`）。サフィックス `<agent>-<profile>` が付くのは 2 つ以上参照したときだけで、これにより各アカウントを個別の agent として定義（`claude` / `claude-home` のように起動コマンドが異なるラッパーを使う構成）しても展開名が冗長にならず、`state.json` のキー互換も保たれます。展開名は `state.json` のキーにも使われるため、アカウントごとに処理済み状態が分離されます。スケジュール解決の `ai-usage --json` は `ScheduleResolver` が 1 回だけ実行し、全エージェントで使い回します（エージェントごとには起動しません）。なお `run` では、これとは別に `execute_plan_tmux` が起動時のキャッシュ初期化でもう 1 回起動します（モニターが最初の 10 秒間隔取得を待たずに statusline を描けるようにするため）。以降の取得はモニターが 10 秒ごとに更新する共有キャッシュ（`ai-usage-cache.json`）を usage-gate と共用します。

解決に失敗した場合（ai-usage コマンドが無い/失敗、該当 `(profile, provider)` が無い、`ok:false`、該当枠が `null`）は `fallback` に従います: `fixed` は曜日ベースの固定計算に戻り（`status` / `run` の source 表示は `fixed fallback: <理由>`）、`skip` はそのエージェントを選択候補から除外し、`error` は即エラーで停止します。`window = "nearest"` で `five_hour` が選ばれても、`state_window = "weekly"` のときは処理済みカットオフは weekly（`resets_at - 7d`）を基準にします（weekly が無い場合のみ選択枠の period に落ちます）。

リセット時刻は `DateTime<FixedOffset>` で保持します。ai-usage の `resets_at`（RFC3339、オフセット付き）は瞬間を保ったまま実行環境のローカル固定オフセットへ変換し、固定計算（タイムゾーンのオフセット）と同じ型で統一します。UTC で返る ai-usage 出力も `status` / `run` ではユーザーのローカル時刻として表示されます。`status` と `run` は各エージェントのスケジュールの導出元（`ai-usage (weekly)` / `fixed` / `fixed fallback: <理由>`）を表示し、ai-usage が静かに固定計算へ戻ることはありません。

ドライランの実行計画、および ai-usage コマンドの起動失敗・タイムアウトエラーにコマンド列を表示するときは、環境変数代入と一般的な認証オプションの値を `<redacted>` に置き換えます。実際の子プロセスには元の引数を渡し、表示のために実行内容を変更しません。

環境変数代入として伏せるのは、**実行ファイルより前に並ぶ `KEY=VALUE`**（`env FOO=1 cmd` の前置き）だけです。実行ファイル以降の `key=value` はサブコマンドのオプション値で、既定 config の `codex -c model='gpt-5.3-codex' -c model_reasoning_effort='xhigh'` や自動注入の `-c approval_policy=never` が該当します。位置を見ずに伏せると、ドライランの目的（何がどう起動されるかの確認）そのものが果たせません（モデル・reasoning effort・承認方針が丸ごと `<redacted>` になっていました）。`--api-key=...` のような認証系オプションの `KEY=VALUE` 形式は、位置に関わらず伏せます。

### 停止の 2 種類（一時停止 pause / 恒久停止 stop）

停止シグナルは**枠の周期で 2 種類に分かれます**（`src/rate_control.rs`）。判定は `stream-json` の `rate_limit_event` 経路と `usage-gate` 経路で共通の `rate_control::evaluate` が行い、単位（前者は 0.0〜1.0 の `utilization`、後者は 0〜100 の `used_percent`）と枠名の違いは呼び出し側で `WindowObservation` へ正規化してから渡します。

| 閾値に触れた枠 | 判定 | シグナル |
|---|---|---|
| 週次枠（`seven_day` / `weekly`、および周期が 7 日以上の枠） | 恒久停止 | stop file |
| 短周期の枠（`five_hour`、`kind:"daily"` の 24 時間枠など） | その枠のリセットまで一時停止 | pause file（`<stop file>-pause.json`） |
| 短周期だがリセット時刻が読めない / 枠 1 周期ぶんより先を指す | 恒久停止（fail-closed） | stop file |

**枠の周期はスロット名ではなく `kind` から導きます。** 実データでは antigravity が `five_hour` スロットに `kind:"daily"`（24 時間枠）を、pixellab が `weekly` スロットに `kind:"monthly"` を返すため、名前で決め打ちすると「待てば回復するか」の判断を取り違えます。

**なぜ分けるか。** 停止シグナルが stop file 1 個（作られたら二度と再開しない）だった頃は、5 時間枠が閾値に触れただけで週次デッドラインまでの実行余力を丸ごと捨てていました。実ログでは 5 時間枠が 90%（閾値ちょうど）に達して停止した数分後にその枠がリセットされ、以降 30 分以上リクエストが通り続けたにもかかわらず、残り 2 タスクが 1 件も実行されないまま終了しています（週次枠は 43%、デッドラインまで 4 時間 52 分残っていた）。

pause file は `resume_at` / `window` / `reason` を 1 行 1 キーの平文で持ちます。JSON にしないのは、これを読むのがワーカー（Rust）だけでなく tmux モニターのシェルスクリプトでもあるためで、平文なら `sed -n 's/^resume_at=//p'` で確実に取り出せます。書き込みは sidecar ロック（`.<pause file>.lock`）の下での read-modify-write ＋ atomic rename で、**既存より再開時刻が遅い場合だけ更新**します（早い時刻での上書きは待つべき時間を縮めてしまう）。ロック対象を本体にしない理由は `state.json` と同じで、rename 後にロック対象の inode が古くなって排他が破れるためです。

**stop file の作成（`rate_control::write_stop`）も同じ sidecar ロックの下で行います。** これが無いと、`gate-claim` がロック内で「停止が無い」ことを確認してから `rename` で claim するまでの隙間に停止が発行され、停止後にタスクが 1 件開始され得ます。ロックを取れなかった場合でも停止シグナル自体は書きます（直列化の取りこぼしより、停止を伝えられないまま走り続ける方が危険なため）。`usage-gate` 側の stop file 作成もこの関数へ寄せており、複製を持つと「停止発行を claim と直列化する」ような変更が片方だけに入って再発します。

### ワーカーのゲートと claim（gate-claim）

ワーカーは自分で stop file を見たりキューを `mv` したりせず、内部サブコマンド `token-burn gate-claim` に「停止判定 + claim」をまとめて任せます。claim できた番号を stdout に出し、終了コードで **0 = claim 成功 / 10 = 恒久停止 / 20 = 処理できる pending 無し** を返します（待機中の進捗や警告は stderr へ出すので stdout は番号だけ）。

**stdout が claim 番号専用であることは、`gate-claim` から起動する子プロセスにも及びます。** ワーカーは `CLAIMED=$(token-burn gate-claim ...)` で受けるため、`--revalidate` に渡した `usage-gate` が 1 行でも stdout へ書くと、その内容が claim 番号に連結されます。タスクは既に `claimed-*` へ rename 済みなので、壊れた名前でスクリプトを探して見つからず、`done` / `failed` / `retry` のどのマーカーも残さないまま 1 件（最悪は再検証が走るたびに 1 件ずつ）失われます。そのため (1) `usage-gate` と `gate-wait` の通知はすべて stderr へ書き、(2) `run_revalidation` は子の stdout を `/dev/null` へ捨て、(3) ワーカー側でも claim 出力が数字以外なら `Invalid claim output` を出してワーカーごと止める、の 3 段で守ります。

**判定と claim を同じロックの下で行うのが要点です。** 以前はシェル側で「stop file を見る → `mv` で claim」と 2 段に分かれており、その隙間に別ワーカーが停止を発行すると、停止後にタスクが 1 件（最悪、並列数ぶん）開始されていました。停止の発行（stop file の作成と pause の更新）も同じ sidecar ロック（`.<pause file>.lock`）を取るため、「停止を発行してから claim される」か「claim してから停止が発行される」のどちらかに必ず並びます。

- stop file があれば即停止。`Path::exists()` ではなく `symlink_metadata` で判定し、I/O エラーは「存在する」側（＝停止）へ倒します。`exists()` は metadata 取得エラーを「無い」に潰すため、権限異常で停止シグナルを見落とします。
- **デッドラインの検査はあらゆる「続行」より先**に行います。モニターも到達時に stop file を作りますが、その書き込みが失敗した場合や claim と競合した場合に期限後のタスクが始まってしまいます。ゲート自身が期限を持っているので自分で確かめます。期限は相対秒ではなく**絶対 Unix epoch** で渡します（ワーカーは別プロセスで、しかも待機を挟むため、相対値だと基準時刻がプロセスごとにずれる）。
- pause file の `resume_at` を**過ぎていればそのまま再開**します。これが上記の事故を防ぐ核心で、リセット済みの枠を理由に走らないままになることがなくなりました。
- `resume_at` がまだ先なら、その時刻まで待ってから再開します。待機中も毎秒 stop file と pause の延長を確認するため、他のワーカーが恒久停止を書けばすぐ止まり、より遅い再開時刻が書かれればそちらまで待ちます。
- 待つと実行全体のデッドラインを越える場合は、待たずに恒久停止へ倒します。
- pause file が壊れて読めない場合は恒久停止（fail-closed）。読めないまま走り続けると、止めるべき場面で走ってしまいます。`resume_at` は正の epoch であることも検証します（負値を「期限切れ」と読むと即座に続行してしまう）。
- 待つのは**残 pending がある場合だけ**です。取るものが無いワーカーを待たせると、空のペインがリセット時刻まで居座ります。

**一時停止から再開する直前だけ、実データで使用率を確かめ直します**（`--revalidate` に渡す `usage-gate`。ai-usage 連携が無ければ省略）。待機の根拠は停止時点の観測なので、これが無いと状況が変わっていても 1 件は開始してしまいます。逆に毎 claim で ai-usage を叩くのは無駄なので、一時停止を抜けた場合に限ります。再検証が新しい pause を書けばそれに従って待ち直し、起動に失敗したり非ゼロ終了した場合は恒久停止へ倒します（使用率を確認できないまま走らせない）。**再開が確定したら、期限切れの pause file は削除します。** claim はタスクごとに別プロセスなので、`resume_at` を過ぎた pause file を残すと以後の claim が毎回「一時停止から再開した」と判定し、タスクごとに再検証（ai-usage 起動）が走り続けます。削除は control ロックの下で読み直してから行い、まだ未来を指す pause は消しません（待機中の別ワーカーが延長を読む根拠のため）。ai-usage 出力のキャッシュは TTL 切れの瞬間に並列ワーカーが揃って到達しても 1 回しか取得しないよう、ロックを取ってから TTL を再確認します。

一時停止中のワーカーは生存しており `worker-done-*` を作りません。モニターの早期停止判定（`WORKERS_DONE >= WORKER_COUNT`）は待機中のワーカーを終了と数えないため、待機がそのまま「停止」と誤報告されることはありません。

### 停止シグナルを書けなかった場合

閾値を超えたのに stop / pause file を作れない（ENOSPC・権限不足等）と、後続を止める手段が無いまま走り続けます。`format-stream` はこれを検出したら**専用の終了コード 11** で終了し、タスクスクリプトはそれを見てワーカーごと止めます（通常のログパイプライン失敗は `failed-N` を記録して次のタスクへ進みますが、この場合だけは `WORKER_ABORT=1` を立ててループを抜けます）。

終了は入力を最後まで処理してから行います。検出した時点で打ち切ると、パイプが閉じて実行中の `claude` が SIGPIPE で落ち、表示整形の都合で数時間の実行を巻き添えにします。

### 使用率ゲート（usage-gate）

ai-usage 連携が有効なとき、`rate_limit_threshold`（%）は 2 経路で後続タスクを止めます。1 つは既存の `claude` stream-json `rate_limit_event` によるリアルタイム監視（タスク実行中の `utilization` が閾値超過で stop / pause 作成）。もう 1 つが **usage-gate** で、各タスク完了後（ワーカーが次の pending を claim する前）に内部サブコマンド `token-burn usage-gate` が `ai-usage --json` を実行し、その agent の `(profile, provider)` の weekly / five_hour `used_percent` を**枠ごとに**上記の判定へかけます。`claude` / `codex` 両方に効きます（codex は従来リアルタイム監視が無かったため特に有効）。

使用率を最大値へ潰してはいけません。潰すと「どの枠が閾値に触れたか」と「その枠のリセット時刻」が失われ、5 時間枠のように待てば回復する枠でも恒久停止になります。

- ai-usage 出力は短 TTL（20 秒）でファイルキャッシュし、並列ワーカーからの重複取得を抑えます。キャッシュは同一ディレクトリの `.<cache>.tmp.<PID>` に書き出し → `rename` で本体に置き換える atomic rename で更新するため、別ワーカーが書き込み途中の不完全 JSON を読むことはありません。
- 取得失敗時、および該当アカウントが `ok:false`（認証切れ等で ai-usage がエラーを報告）のときは fail-closed（使用率を確認できない以上、安全側で停止）。`ok:false` は取得成功でも「使用率を確認できない」状態であり、スケジュール解決（`ScheduleResolver`）が `ok:false` を失敗として fallback するのと一貫します（この検査が無いと `ok:false` かつ `used_percent` 欠損で走り続ける fail-open になります）。該当エントリ無し・`used_percent` 欠損（`ok:true`）時は過剰停止を避けて続行します。`stop_file` の作成にも失敗した場合（ディスクフル等）は黙って継続せず、エラーを伝搬してワーカーを止めます。pause file を書けない場合も同様に恒久停止へ倒します（待機の根拠を共有できない以上、走り続けるより止める側が安全）。
- stop file 作成は `create_new` で冪等（並列ワーカーから同時に呼ばれても安全）。既に走行中のタスクは止められませんが、次のタスク開始前チェックで停止・待機します。これは usage-gate と `claude` stream-json の rate_limit_event 経路の両方で共通の挙動です。

### 中断セッションの再開（resume）

`claude` エージェントのタスクがレート制限で終わった場合（`classify-result` の rate-limited。例: `You've hit your session limit · resets 2:30pm (Asia/Tokyo)`）、token-burn はその Claude Code セッションの `session_id` を `resume.json` に保存します（`src/resume.rs`）。次の `token-burn run` が同じターゲットを同じエージェント（展開後の `RuntimeAgent` 名）で選ぶと、元のプロンプトから新しいセッションを始めるのではなく、`claude --resume <session_id> "<継続プロンプト>"` でそのセッションを続きから再開します。レート制限で失敗した後は、同じコマンドをもう一度打つだけで作業が続きます。

**なぜ再開するか。** レート制限で終わったタスクは `state.json` に記録されないため、次の実行で同じターゲットがもう一度選ばれます。以前はそこで元のプロンプトから新しいセッションを始めていたため、中断までに積み上げた調査・判断の文脈（実ログでは 95 分・327 ターン）が丸ごと失われ、同じ調査をやり直すところからトークンを使っていました。transcript はアカウントの config dir に残っており、`--resume` すれば同じ session ID のまま会話を引き継げます（fork しない）。再開後のストリームには新しいイベントだけが流れて履歴は再送されないため、`format-stream` / `classify-result` は手を入れずにそのまま使えます。

**`state.json` に同居させない理由。** `State` は `#[serde(flatten)]` した `HashMap<エージェント名, HashMap<ディレクトリ, 処理時刻>>` で、top-level のキーはすべてエージェント名として読まれます。型の違う top-level キーを足すと、それを知らない旧バージョンのバイナリは `state.json` の解析に失敗して空状態へ落ち（`State::load` は JSON 破損時に警告して空状態で続行する）、全ターゲットを未処理と判断して再処理します（クォータの二重消費）。`resume.json` は `state.json` と同じディレクトリに置き（デフォルト `~/.config/token-burn/resume.json`）、同じく sidecar ロック（`.resume.json.lock`）の下で、同一ディレクトリのテンポラリファイル → `rename` の atomic 更新で書きます。

読み込みの方針は `state.json` と意図的に変えています。計画時に `resume.json` が読めない・壊れている場合は警告を出して空として扱い、その実行では再開しないだけで走らせます。再開は中断した文脈を引き継ぐための最適化で、読めなくても新しいセッションで始まるだけで処理済み判定は狂わないため、実行を止める理由になりません。一方、更新（保存・削除・失敗の記録）のときに既存ファイルが読めない・壊れている場合は、書き戻さずにエラーにします。空として上書きすると、他のターゲットの中断記録を黙って消してしまいます。

**エントリ。** エージェント名 → 絶対パスのディレクトリの 2 段で持ちます（ディレクトリのキーは `state.json` と同じ形）。

| フィールド | 内容 |
|---|---|
| `session_id` | 再開するセッション（UUID）。jsonl の最後の `result` イベントから取り、無ければ `system` / `init` へ落ちる |
| `interrupted_at` | 中断を記録した時刻 |
| `retry_after` | 任意。これより前に再開しても同じ 5 時間枠で拒否される時刻（後述「リセット前の保留」） |
| `prompt_hash` | そのターゲットの**元の**実効プロンプトのハッシュ（`fnv1a64:<16 桁の 16 進>`） |
| `message` | 任意。上限到達メッセージ（表示用） |
| `log` | 任意。中断した実行の jsonl のパス |
| `failed_resumes` | 再開してレート制限以外の理由で失敗した回数 |

- `session_id` は保存時と再開時の両方で UUID 形式かを確かめます。`claude --resume` は値が UUID でなければ検索語として扱い、対話的なピッカーを開こうとするため、無人実行でそれを踏むと止まります。保存時に取り出せなければ保存せず（再開できない記録を残さない）、保存済みの値が壊れていれば再開しません。
- `prompt_hash` に std の `DefaultHasher` は使えません。アルゴリズムが Rust のリリース間で保証されないため、ツールチェーンを上げただけで全エントリが「プロンプトが変わった」と判定されます。暗号学的な強度は要らない（同一性の確認だけ）ので、依存を増やさず FNV-1a（64bit）を使い、将来アルゴリズムを変えても取り違えないよう名前を前置します。
- ハッシュは継続プロンプトではなく元のプロンプトから取ります。再開したセッションが再びレート制限で切られて保存し直すときも同じです。継続プロンプトから取ると、次の実行で「プロンプトが変わった」と判定されて再開されなくなります。再開時に元のプロンプトを送らないのは、元の指示が会話履歴に残っているためです。

**保存するのはレート制限だけ。** リトライ可能エラー（408/429/5xx、`Connection closed mid-response` 等）、`result` を出さないままのクラッシュ、キャンセル（Ctrl-C）、ログパイプラインの失敗は保存せず、従来どおり次の実行で最初から処理します。リトライ可能エラーは原因が多岐にわたり、会話がどこまで永続化されたかもまちまちなので、初版では意図的に外しています。対象は `claude` エージェントだけです（`codex` には stream-json の分類パイプラインが無く、中断を判定できない）。エントリをエージェント単位で持つのは、transcript がそのアカウントの `CLAUDE_CONFIG_DIR` の下にあり、別アカウントからは再開できないためです。`dedup_scope` で処理済み履歴を共有していても、再開するのは中断したエージェント自身だけです。

**再開する条件。** 保存済みエントリを使うのは次をすべて満たすときだけで、1 つでも欠ければ従来どおり新しいセッションで始めます。

- 再開が有効: `--no-resume` / `--fresh` が無く、`[settings] resume_interrupted` が `false` でない（デフォルト `true`）。`--no-resume` / `--fresh` は「今回は再開しない」だけで、実行中の新しい中断は保存します。`resume_interrupted = false` は保存もしません
- エージェントの `command` が自分でセッション系フラグを渡していない: `--resume` / `-r`、`--continue` / `-c`、`--session-id`、`--fork-session`、`--no-session-persistence`、`--from-pr`。渡していればそのエージェントでは中断の保存も再開もしません（以前に保存したエントリがあれば、一覧に使わない理由を出す）。token-burn が足す `--resume <id>` と衝突するためです（`--continue` は直近の別セッションを、`--session-id` は固定 ID を選び、`--no-session-persistence` はそもそも再開できるセッションを残さない）。判定はシェル文字列の部分一致ではなく argv の要素単位で、`--flag=value` 形式も拾い、`--` 以降の位置引数は見ません。部分一致だと `--model=resume-model` のような別オプションの値で誤検知します
- 現在の実効プロンプトが `prompt_hash` と一致する: プロンプトを編集した後で古い指示の続きをさせても意味が無いので、新しいセッションで始めます
- 処理済み判定と同じ範囲（`dedup_scope`）に、`interrupted_at` より新しい処理済み記録が無い: たとえば `dedup_scope = "global"` で中断の後に別のエージェントがそのターゲットを完了していれば、中断したセッションはもう古くなっています

使わない場合は、`list` / `run` のターゲット一覧に理由（再開無効 / プロンプト変更 / より新しい処理済み記録 / 保存された ID の破損）を添えます。黙って新しいセッションで始めると、「再開されるはずだったのに最初からやり直した」理由をログから追えないためです（処理済みスキップの内訳を `SkipSummary` で出すのと同じ理由）。

使わなかったエントリは、そのターゲットで新しいセッションを起動する直前に破棄します（`ResumePlan::discard` → タスクスクリプトの `resume-entry forget --session <古い ID>`）。新しいセッションが作業を始めた時点で、古いセッションの文脈はリポジトリの実態より古くなります。残しておくと、`--no-resume` / `--fresh` で始めた新しいセッションがリトライ可能エラーで終わった（＝保存されない）場合に、次の実行がより古い中断セッションを再開して、新しいセッションの作業と食い違う計画のまま続けてしまいます。ID が一致するときだけ消すので、同じ実行の中で後から保存された記録（新しいセッションが再びレート制限で切られた等）は消しません。計画から外れたターゲット（`limit` / TUI）の記録には触れません。

固定の有効期限は設けません。transcript が残っているかどうかは Claude Code 自身の保持設定で決まり、token-burn からは試すまで分かりません。期限で切れば、残っているセッションを捨てるか、消えたセッションを試すかを必ずどこかで取り違えます。消えていた場合も同じタスクの中で新しいセッションへ切り替えるため（下記）、試すコストは起動 1 回ぶんで済みます。

**継続プロンプト。** 再開時に送るプロンプトは `[prompts] resume` で変えられます（リテラル文字列、または `.md` で終わるパス。`[prompts].default` と同じ規則で解決）。既定は組み込みの英語の文で、次をモデルに伝えます。

- 前のセッションはレート制限で切られ、このセッションはその続きであること（元の指示と進捗は履歴にある）
- まずリポジトリの状態を確かめること（`git status`、現在のブランチ、`git worktree list`、未コミットの変更）
- 実行中だったサブエージェント・バックグラウンドタスクは止まっているので、完了したと仮定せず結果を確かめること
- 完了済みの操作（コミット・push・リリース）を盲目的に繰り返さないこと
- そのうえで元の指示の残りを仕上げること

中断はターンの途中で起きるため、履歴にあるモデルの認識と実際のリポジトリの状態はずれ得ます。背景で動いていたサブエージェントや Bash はセッションと一緒に止まっていますが、履歴には完了の通知が届いていません。中断前に済んでいたコミットや push、リリースを繰り返せば二重の副作用になります。

**再開したタスクの結末。**

| 結末 | `classify-result` | エントリの扱い |
|---|---|---|
| 成功 | 0 | 従来どおり `token-burn mark` が `state.json` へ記録し、続けてエントリを削除する |
| 再びレート制限 | 2 | 上書きする（同じ `session_id`、新しい `interrupted_at` / `retry_after`、`failed_resumes` は 0） |
| セッションが既に無い | 4 | 削除し、同じタスクの中で元のプロンプトから新しいセッションを始め直す |
| その他の失敗（キャンセル以外） | 1、または result 無しの非ゼロ終了 | `failed_resumes` を 1 増やし、3 回に達したら削除する |
| リトライ可能エラー / キャンセル | 3 / — | 変更しない |

- `mark` は `state.json` を先に書き、エントリはその後で消します。逆順だと、エントリを消した直後に落ちたときに処理済み記録も再開情報も残らず、完了済みのターゲットを次の実行でまた最初から処理します。`state.json` を先に書いておけば、消し損ねたエントリは「`interrupted_at` より新しい処理済み記録がある」条件で使われなくなるだけなので、削除の失敗は警告にとどめます。`mark` は session ID を問わずそのエージェント・ディレクトリのエントリを消し、`resume.json` が無ければ何もしません（ロックファイルも作らない。`codex` など再開と無関係なエージェントの `mark` でも呼ばれるため）。
- 再びレート制限なら `failed_resumes` を 0 へ戻します。上限まで走れたこと自体が、そのセッションを続けられる証拠だからです。
- 3 回で捨てるのは、壊れた transcript のように二度と続けられないセッションを、実行のたびに再開しては失敗し続けるのを防ぐためです。1 回で捨てないのは、認証切れのようにセッションと無関係な原因でも失敗するためです。リトライ可能エラーとキャンセルは、そのセッションを続けられるかどうかについて何も示さないので数えません。
- `result` を出さずに非ゼロ終了した再開（`classify-result` は 0 のまま `CMD_EXIT` だけが非ゼロ。タスクスクリプトの `*)` 分岐）も失敗として数えます。transcript の読み込みで落ち続けるセッションは `result` を残さないため、分類 1 だけを数えていると上限に届かず、エントリが永久に残って実行のたびに再開しては落ちます。
- セッションが消えていると、Claude Code は `No conversation found with session ID: <id>` を出し、exit 1 と共に `subtype:"error_during_execution"`・`num_turns:0`・`errors:[...]` の `result`（`result` 文字列は無い）を出します（Claude Code 2.1.280 で実測）。`classify-result` はこの形（`subtype` が `error_during_execution` で、かつ `errors[]` のいずれかが `No conversation found with session ID` を含む）に限って **4 = 再開不能（resume unavailable）** を返し、レート制限の判定より先に見ます。通常の失敗（1）に落とすと `failed_resumes` を数えるだけになり、エントリが捨てられるまで何も起きない再開を 3 回繰り返します。形で絞るのは、任意のエラー文で判定すると、本当に失敗したタスクを「まだ始まっていない」と見なして新しいセッションで二重に実行してしまうためです（`result` 本文に同じ文言があるだけのものや、起動時の別のエラーは 1 のまま）。
- 新しいセッションは次の実行を待たずに同じタスクの中で始めます。次の実行がすることはまさに新しいセッションでの開始なので、今やれば実行 1 回ぶん早く済みます。失敗した再開は 1 ターンも進んでいない（`num_turns:0`）ので二重実行にもなりません。この 2 回目の試行は元の `<NNNN>_<name>.jsonl` / `.log` の隣の `<NNNN>_<name>.fresh.jsonl` / `<NNNN>_<name>.fresh.log` に書き、その結末は通常のタスクと同じに扱います（マーカーも 2 回目の結果で決まり、レート制限なら新しいセッションを保存する）。ログを試行ごとに分けるのは、分類も `session_id` の抽出も jsonl の最後の `result` を読むためです。同じ jsonl へ追記すると、新しいセッションが `result` を出さずに終わったとき（クラッシュ等）に再開失敗側の `result` を読んでしまい、上書きすれば再開に失敗した記録が消えます。キャンセルで 2 回目へ切り替えなかった場合と、再開しないタスクで万一 4 が返った場合は失敗として扱います。

**リセット前の保留（`retry_after`）。** 保存時には jsonl の最後の `status:"rejected"` の `rate_limit_event` を見て、それが 5 時間枠（`rateLimitType:"five_hour"`）の拒否なら、その枠の構造化された `resetsAt` に一時停止と同じ 30 秒の反映猶予を足した時刻を `retry_after` に記録します。本文の `resets 2:30pm (Asia/Tokyo)` は解析しません（日付を持たない時刻表記で、タイムゾーン名の解決と「今日か明日か」の推測が要る）。次の場合は記録しません: overage を使っている拒否（月次の追加課金枠まで使い切った状態で、5 時間枠のリセットを待っても解けない。後述「レート制限の自動停止判定」と同じ理由）、5 時間枠 1 周期より先を指す値（枠の取り違えか壊れた値。一時停止の妥当性検査と同じ許容幅）、既に過ぎている時刻（待つ必要が無い）。

次の実行の計画に `retry_after` がまだ未来のセッションが含まれていれば、その実行全体をその時刻まで一時停止した状態で始めます。時刻は計画に残ったタスク（`limit` や TUI で外れたターゲットは待機の根拠にしない）の `retry_after` の最大値です。5 時間枠はアカウント単位なので、再開以外のタスクもそれまでは通りません。仕組みは実行中に 5 時間枠へ触れたときと同じで、tmux を起動する前にその実行の pause file（`<stop file>-pause.json`）を書いておきます。そのため待機、待つとデッドラインを越える場合の停止、ai-usage 連携時の再開直前の再検証は、すべて既存の `gate-claim` がそのまま扱います。この pause file を書けなければ実行を始めません。実行計画にも保留（`Hold:`）を表示します。保留が無いと、リセット前に同じコマンドを打ち直したとき、再開したセッションが即座にまた同じ枠で弾かれ、そのたびに継続プロンプトだけが transcript へ積み上がります。

**実行順。** 再開できるターゲットは各グループの先頭へ寄せます。ソートキーは `defer` → 可視性（いずれかの `[[scan]]` が `public_first = true` のときだけ）→ 再開の有無 → 最終ファイル変更日時（古い順）です。中断したリポジトリは中断の直前まで変更されていたので、古い順だけで並べると末尾へ回り、`limit` で切られて再開されないまま終わります。一方で `defer` / `public_first` はユーザーが明示した優先度なので、その上には置きません。`token-burn run PATH...` は従来どおり CLI の指定順を保ったまま再開も行います（「同じコマンドをもう一度打つ」の典型例）。

**表示。** `list` / `run` の `=== Targets ===` では、再開するターゲットの下に `↻ resume <短縮 session id> (rate limited <時刻>)` の行を、保存済みセッションを使わないターゲットの下にその理由の行を出します。実行計画はそのタスクの `Resume: <session id>` を出し、`Prompt:` 行には元のプロンプトではなく継続プロンプトを表示します（実際に送る方を見せないと、再開時に何が送られるかをドライランで確かめられない）。`-i` の TUI は再開する行に `↻` を付け、ヘッダーに `↻` の意味と件数を出します。ワーカーペインは `claude` を起動する前にセッションを再開する旨を、2 回目の試行へ切り替えるときはその旨を出します。tmux から戻った後の最終集計には、この実行で保存した中断セッションの件数と「同じコマンドをもう一度打てば続きから再開する」旨を（リセット時刻が分かればそれも）添えます（`saved_sessions_notice`。この実行の開始より後に保存された記録だけを数える）。失敗の集計だけでは、次の実行が最初からやり直しになるように読めるためです。

**CLI。** `--no-resume` はその実行に限り保存済みセッションを無視します。処理済みのスキップは効いたままで、使わなかったエントリは上記のとおり新しいセッションの起動直前に破棄します。`--fresh` は「最初からやり直す」指定なので、処理済み履歴と保存済みセッションの両方を無視します。どちらも実行中の新しい中断は保存します。

**内部サブコマンド（`resume-entry`）。** ワーカースクリプト専用の隠しサブコマンドです。

- `token-burn resume-entry save <agent> <dir> <state_file> --jsonl <jsonl> --prompt-file <file>`: jsonl から `session_id` / 上限メッセージ / `retry_after` を取り出して保存する（同じエージェント・ディレクトリの既存エントリは置き換える）。`--prompt-file` は `prompt_hash` を取る元のプロンプトで、再開したタスクでも元のプロンプトを渡す
- `token-burn resume-entry forget <agent> <dir> <state_file> --session <id>`: 保存中の `session_id` が一致するときだけ削除する（再開不能だったとき、および使わなかったエントリを新しいセッションの起動直前に破棄するとき）
- `token-burn resume-entry fail <agent> <dir> <state_file> --session <id>`: `session_id` が一致すれば `failed_resumes` を 1 増やし、3 回に達したら削除する

引数の並びは `mark` と同じで、`resume.json` のパスは `state_file` の隣として導出します（ワーカーへ新しいパスを配らずに済む）。`forget` / `fail` が `--session` の一致を条件にするのは、遅れて終わった試行が、その後に保存された別セッションのエントリを消したり数えたりしないためです。

これらのコマンドの失敗はワーカーを止めず、警告を出すだけです。再開情報は最適化であり、失っても次の実行が最初からやり直すか再開を 1 回余分に試すだけで、停止シグナル（stop / pause file）のように書けないと安全を損なう性質のものではありません（前述「停止シグナルを書けなかった場合」とは逆の判断）。タスクマーカーも変えません（レート制限で中断したタスクは従来どおり `failed-N`）。

**スコープ外。** 一時停止が明けた後に中断したタスクを同じ実行の中でキューへ戻して再開すること、`codex` セッションの再開、リトライ可能エラーで終わったセッションの再開は扱いません。

### モニターペインの ai-usage 表示

ai-usage 連携が有効なとき、tmux モニターペイン（左）には進捗に加えて `ai-usage --statusline --logos`（各アカウントの 5h / 週次の使用率バー・%・リセット残り）を表示します。**10 秒ごと**にモニター自身が `ai-usage --json` を実行してキャッシュ（`ai-usage-cache.json`）を atomic（`.tmp` に書いてから `mv` で差し替え）に更新し、その直後に `--input <cache>` 付き statusline で描画します。`--input` を介して usage-gate と同じキャッシュを共有するため、長時間タスク中でもモニター表示と並列ワーカーの使用率判定が同じ最新値で同期します（進捗バーは従来どおり毎秒 `\r` 更新）。取得失敗時は直前の表示を保持し（fail-soft）、`tput civis` でカーソルを隠してちらつきを抑えます。モニターは `\033[H\033[J` で全体を再描画する方式のため、エラーは表示済みフラグではなく `error-*` マーカーから毎回再構築して履歴を保ちます。

statusline コマンドは usage-gate / 起動時キャッシュ初期化と同様に `[ai_usage].command` 全体から組み立てます（出力モードの `--json` を `--statusline --logos --compact --input <cache>` に差し替え、無ければ末尾に追加）。`--compact` はモニターペインの横幅が狭いためゲージ幅を半分にする指定です。さらに末尾へ `--active-profile <profile>` / `--active-provider <provider>` を付け、実行中アカウントの行を強調します（`ai-usage` の既定の active 判定は `.claude.json` の email に依存し、email を持たないアカウントでは効かないため、profile / provider で明示します）。先頭要素だけを使う実装ではないため、`["env", "FOO=1", "ai-usage", "--json"]` のようなラッパー前置き構成でも壊れません。進捗バーは `seq` ではなく算術 `while` ループで描画します（BSD seq の `seq 1 0` が降順で `1 0` を返し、macOS で 0% / 100% 時にバーが 2 文字ずれるのを避けるため）。

ワーカーはタスクを消化し切るとそのぶんモニターペインが広がるため、モニターは `trap render WINCH` でペインのリサイズを受けて全体を描き直します。通常の全体再描画は ai-usage の 10 秒間隔取得・`error-*` 件数の変化・状態遷移でしか走らないため、これが無いと旧い幅で折り返した行が残ります。

### 完了時の自動終了

モニターは全タスク完了（`PROCESSED >= TOTAL`）と早期停止（`WORKERS_DONE >= WORKER_COUNT`）のいずれでも集計とログパスを表示した後、`finish_session`（端末状態を復元 → `tmux kill-session` → `exit 0`）でセッションごと閉じて token-burn を終了します。Ctrl-C 待ちはしません。`kill-session` はモニター自身のペインも道連れに殺すため後続の `exit 0` には基本到達しませんが、セッション名の解決に失敗して空振りした場合に取り残されないよう明示的に置いています。`sleep infinity` / `sleep 3600` の待機ループはモニター・ワーカーのどちらにも残しません（回帰防止のためテストで検査しています）。

ワーカーも claim できる pending が尽きたら `worker-done-<w>` を作り、キャンセル trap を外して `exit 0` します。tmux はペインのプロセス終了でペインを閉じるため、完了したワーカーのペインは順次消え、残りのワーカーだけが画面に出ます。ユーザーの `~/.tmux.conf` が `remain-on-exit on` にしていると終了済みペインが "Pane is dead" のまま残ってしまうため、`new-session` 直後（ワーカー用のペイン分割より前、= 最初のタスクが終わる前）にこのセッションに限って `remain-on-exit off` を明示します。

自動終了するとモニターが最後に描いた集計は tmux ごと画面から消えるため、`execute_plan_tmux` は `attach-session` から戻った後（一時ディレクトリの削除より前）に `marker_dir` の `done-*` / `failed-*` / `retry-*` を `collect_run_summary` で数え直し、同じ文言（`✅ All N/N tasks completed` / `⚠ Completed: ...` / `⏹ Stopped: ...`）を呼び出し元の端末へ出します。`worker-done-*` は `done-` 始まりではないためタスク件数には混ざりません。集計に失敗しても 0 件として続行します（実行結果の記録自体は `state.json` が持つため）。デタッチ（`tmux has-session` が成功）時はタスクが走行中なので集計は出しません。

`claude` エージェントのみ出力を `.jsonl` + `format-stream` パイプラインで処理します。`codex` 等の他エージェントは `.log` に直接出力します。

`claude` エージェントでは、`format-stream` / `tee` / raw jsonl 保存のいずれかが失敗した場合、または jsonl が空の場合、そのタスクは `failed-N` として扱い、`state.json` には記録しません。ログ・分類パイプラインが壊れたタスクを成功扱いしないためです。非 `claude` エージェントでも `tee` が失敗した場合は `failed-N` として扱います。

`claude` エージェントのタスク完了後は `token-burn classify-result <jsonl>` により jsonl 最終 `result` イベントの `is_error` / `api_error_status` を解析して分類し、終了コード（**0 = 成功 / 1 = 失敗 / 2 = レート制限 / 3 = リトライ可能 / 4 = 再開不能**）で返します。

- 成功 (`is_error:false`) → `state.json` に記録
- レート制限（下記の判定に合致） → `failed-N` マーカー。`state.json` には記録しない。そのセッションの `session_id` を `resume.json` へ保存し、次の実行で続きから再開する（前述「中断セッションの再開（resume）」）
- プロバイダ側リトライ可能エラー (`api_error_status` が 408/429/5xx) → `retry-N` マーカー。`state.json` には記録しないため次回実行で再処理される。ワーカーは継続
- トランスポート層の一時障害（`api_error_status` が `null` かつ `terminal_reason` が `api_error`）→ `retry-N` マーカー。ワーカーは継続
- 再開不能（`subtype` が `error_during_execution` で、かつ `errors[]` のいずれかが `No conversation found with session ID` を含む）→ 終了コード 4。`--resume` で指定したセッションが既に消えていたことを表し、レート制限の判定より先に見る。タスクはエントリを消し、同じタスクの中で元のプロンプトから新しいセッションを始め直す（マーカーはその結果で決まる。前述「中断セッションの再開（resume）」）
- その他のプロバイダエラー → `failed-N` マーカーとエラーメッセージ（`result` フィールド）を `error-N` へ記録し、`━━━ Error - continuing ━━━` を表示してワーカーは次のタスクへ進む。1 リポジトリ固有の失敗（認証エラー・不正リクエスト等）で残りのターゲットを丸ごと捨てないため。ワーカー自体を止めるのは停止シグナル（stop / pause file）を書けなかったとき（`format-stream` の終了コード 11）だけで、これは後続を止める手段が無いまま走り続けるのを防ぐ唯一の例外（前述「停止シグナルを書けなかった場合」）

`api_error_status` が `null` のまま `terminal_reason` が `api_error` で終わったケースは、HTTP 応答そのものが返っていない = 接続断や名前解決失敗といったトランスポート層の一時障害です。実ログでは `API Error: Connection closed mid-response. The response above may be incomplete.` / `API Error: Unable to connect to API (ENOTFOUND)` が該当し、13 セッション中 5 セッションで発生していました。恒久エラー（`Failed`）に落とすとワーカーごと停止して以降のターゲットが 1 件も処理されないため、再試行可能として扱います。認証エラーや不正リクエストは HTTP ステータスを伴うため、この分岐には入らず従来どおり `Failed` のままです。

レート制限の判定 (`is_rate_limit_message`) は次のいずれかに合致した場合です。上限到達は `api_error_status` が 429 で返るため、この判定を漏らすとリトライ可能エラーへ落ち、回復しないまま残りのターゲット全件にエラー行が出続けます。

- `usage limit reached` を含む
- `hit your` 以降に `limit` を含む（実ログの `You've hit your session limit ...` / `You've hit your org's monthly spend limit ...`）
- `resets ` の直後が時刻表記（`3am` / `12pm` に加え、分を含む `2:30am` 形式にも対応）

`resets` の時刻判定は「時 → 任意の `:<分>` → `am`/`pm`」を明示的に読み進めます。`:` を単なる非数字として打ち切ると `:30am` が残って判定に失敗し、実ログの `resets 2:30am (Asia/Tokyo)` を取りこぼしていました。一方で `resets 5 times max. Please retry tomorrow at 8am` のように数字と `am`/`pm` が離れたメッセージは従来どおり誤検知しません。

jsonl ファイルが存在しない場合は result イベント無しと等価で Success として扱いますが、ファイルは存在するのに権限エラーや I/O エラーで読めない場合は `Failed` として返します（読み込み失敗を Success と誤分類して `state.json` に誤記録するのを防ぐため）。

`result` イベントに `result` 文字列（エラー本文）が無い場合は、`errors[]` の空でない要素を `; ` で連結してメッセージにします（`result` があればそちらを優先）。起動直後に失敗した `result`（`error_during_execution`）は `result` を持たず `errors[]` だけを持つため、`result` だけを見ていた頃はエラー記録が空文字になって原因を追えませんでした。

`format-stream` は `tool_result` の `is_error:true` を検出した場合、エラー内容の先頭の有意な 1 行をサマリーとして表示します（単一行/複数行の `<tool_use_error>...</tool_use_error>` ラッパーは除去）。配列形式の `content` にも対応し、120 文字を超える場合は末尾を `...` で省略します。

`result.subagent_stats` がある場合は、サブエージェントの起動・完了・失敗・強制終了・起動拒否の件数、バックグラウンド/入れ子起動数、最大深度を集計表示します。top-level の `result` が `success` でも配下のサブエージェントが全件失敗した実データがあるため、`failed` / `killed` / `refused` のいずれかが非ゼロなら警告色にします。理由別の `killed` / `refused` は飽和加算し、壊れた巨大値でも debug build でオーバーフローさせません。全カウンタがゼロならノイズを避けて表示しません。

`subagent_stats.by_type` があれば、種別ごとの件数を `[Explore:5 general-purpose:4 codex:2 security-engineer:1]` の形で多い順（同数は名前順）に併記します。実データでは 14 result 中 12 件が非空で、`codex` が 2 体なのか `Explore` が 5 体なのかでコストの意味が全く違うため、件数だけの `spawned:12` では読み取れません。

`result.is_error` が true の場合は、`api_error_status` と `result`（エラー本文）を `error (HTTP 429): <本文>` として表示します。実データでは 47 ドル・30 分を消費したセッションが `subtype:"success"` のまま `is_error:true` / 429 / `You've hit your individual spend limit ...` で終わっていたのに、フッターへ出ていたのは `terminal api_error` の 1 行だけでした。その 1 行を読み落とすと正常完了したように見えます。`rate_limit_event` 由来の `🚫 Rate limited` 行は「five_hour が rejected」としか言わず、真因（個人の支出上限）は伝えないため重複しません。`api_error_status` は `classify-result` が 408/429/5xx で再試行を判断する値そのもので、ログと分類結果を突き合わせられるようになります。成功時の `result` は最終テキストとして既にストリーム済みなので表示しません。

`tool_use_result` の top-level メタデータに `truncated`、`appliedLimit`、`staleReadFileStateHint`、`userModified`（Edit/Write 等で書き込み前にユーザがファイルを変更していた場合。`user-modified` 形式）、`staleRecovered`（Edit が古い読み取り状態から自動回復した場合。`stale-recovered` 形式）、`success:false` / `error` / `message`、`stdout` / `stderr`（Bash 等の標準出力・標準エラー要約。`stdout:<summary>` / `stderr:<summary>` 形式）、Edit/Write 結果の `filePath` / `structuredPatch` / `replaceAll` / `memdirStamped`（`file:<path>`、`patch:<hunks> ... +追加/-削除`、`replace_all`、`memdir-stamped` 形式。`originalFile` / `oldString` / `newString` は巨大化するため表示しない）、`bashEditDiff`（Bash 経由のファイル書き換え。`bash-edits:<path> +追加/-削除` / `bash-edits:<件数> files +追加/-削除 (+<N> more)` 形式）、`assistantAutoBackgrounded`、`backgroundTaskId`、`wasClamped` / `clampedDelaySeconds`、`persistedOutputPath` / `persistedOutputSize`、`returnCodeInterpretation`、`totalDurationMs` / `durationMs` / `totalTokens` / `totalToolUseCount`、`listing`（`ListAgents` の一覧から 2 空白で始まるエージェント行を数え、`agents:<n>` 形式）、`agentType`（Agent のサブエージェント種別。`agent:<type>` 形式）、`agentId`（非同期 Agent の識別子。`agent-id:<id>` 形式）、`resumedAgentId`（`SendMessage` で再開した Agent の識別子。`resumed-agent:<id>` 形式）、`resolvedModel`（Skill / Agent が解決したモデル名。`model:<...>` 形式。末尾の壊れた SGR 断片は除去）、`toolStats`（サブエージェントの編集行数。加除いずれか非ゼロのとき `edits:+<追加>/-<削除>` 形式）、`numFiles` / `numLines`、`file.startLine` / `file.numLines` / `file.totalLines`（Read の部分読み取り。先頭からなら `lines:<n>/<total>`、途中からなら `lines:<start>-<end>/<total>` 形式）、`file.truncatedByTokenCap`（Read の token cap 切り詰め。`truncated:token-cap` 形式）、`matches`（ToolSearch）/ `numMatches`（Grep の count モード）/ `mode` / `total_deferred_tools`、`results` / `searchCount` / `durationSeconds`（WebSearch の結果件数・検索回数・所要時間）、`code` / `codeText` / `bytes`（WebFetch の HTTP ステータス・応答サイズ。`http:<code> <text>` 形式）、`gitOperation`（git commit の sha / kind。`commit:<sha> <kind>` 形式）、`structuredContent.content`（Codex MCP 等の構造化応答。`structured:<summary>` 形式）、`tasks` / `task` / `taskId` / `task_id` / `task_type`、`retrieval_status`、`outputFile` / `canReadOutputFile`、`timeoutMs` / `persistent`、`statusChange`、`updatedFields`（TaskUpdate の変更フィールド一覧。`status` のみのときは `statusChange` と重複するため非表示、それ以外は `updated:<field1>,<field2>` 形式）、`isAsync`（Agent を `run_in_background=true` で起動した async-launched 応答。`async` として表示）、`scheduledFor`、`commandName`、`workflowName`（Workflow 起動結果。どのワークフローが走ったかを `workflow:<name>` 形式で表示。`runId` は内部識別子のため非表示）、`allowedTools`（Skill が許可するツール一覧。非空配列のときに件数を `allowed-tools:<n>` 形式で表示） が含まれる場合は、ツール完了行に短い補足として表示します。`matches` 配列は ToolSearch 専用で Grep の結果には存在しないため、Grep の count モードでは `numMatches` 整数から件数を表示します。

サブエージェント由来のツール完了行には、ツール名の直後へ帰属（` @<タスク名>`）を添えます。サブエージェント内で走ったツールの結果はメインループの出力と同じストリームへ user イベントとしてインラインに混ざり、実ログではツール完了行の 69%（2,309 / 3,350 件）がサブエージェント由来でした。帰属が無いと画面が `✓ Bash` の羅列になり、22 個が並列で動いているときにどの完了行が誰の作業か追えません。user イベント top-level の `task_description`（Agent 起動時の description。実ログの「PHP 版比較のバグ修正」等）は並列タスクを一意に識別できる唯一の手掛かりなので優先し、無ければ `subagent_type` へ落とします。どちらも持たないメインループの結果では従来どおり何も添えません。Read の行数は実データでは `file` オブジェクトに入れ子で入り、部分読み取り（`numLines < totalLines`）のときのみ表示します。`startLine` が 1 または欠損なら `lines:<n>/<total>`、2 以上なら `lines:<start>-<end>/<total>` を使い、全行読み取り時はノイズ回避のため省略します。`file.truncatedByTokenCap` が true の場合は、行数比率とは独立して `truncated:token-cap` を表示します。WebSearch の `searchCount` は通常 1 のため 2 以上のときのみ表示します。

`tool_use_result.type` は実データ（94MB / 7 セッション）で `text`（通常の読み取り、69 件）/ `create`（新規作成、7 件）/ `file_unchanged`（3 件）が現れます。このうち `file_unchanged` を `file-unchanged`、`create` を `created` として表示します。`file_unchanged` は前回読み取りから内容が変わらず本文が返らなかったケースで、パス情報も `file.filePath` に入れ子で入るため top-level メタデータには何も出ず、表示しないと通常の Read 成功と区別できません（サブエージェント出力ファイルをポーリングしている最中の Read が実は何も取得していない、という判断材料を失う）。`create` は Write が既存ファイルの上書きではなく新規作成だったケースで、`file:<path>` はどちらでも出るため、これが無いと既存ファイルを潰したのかどうかが区別できません。

`tool_use_result.bashEditDiff` がある場合は、Bash 経由で書き換わったファイルの規模を `bash-edits:<path> +追加/-削除`（1 ファイル）または `bash-edits:<件数> files +追加/-削除 (+<N> more)`（複数）として表示します。`sed -i` / `cargo fmt` / `depup` / スクリプト経由の書き換えは Edit/Write を通らないため `filePath` も `structuredPatch` も出ず、完了行に痕跡が 1 つも残りませんでした（実データでは Bash 結果 102 件が `bashEditDiff` を持ち、うち 36 件が実際にファイルを変更していた）。無人実行で「いつの間にかファイルが変わっている」理由を追う手掛かりが消えるため、`vcs_state_changed` を表示するのと同じ理由で出します。件数は `changedFiles`（変更された全ファイル）を正とし、加除行数は hunk を持つ `files` から数えます。`moreFiles` が非ゼロなら詳細が間引かれていて加除が一部しか数えられていないため、`(+N more)` で明示します。`files` が空の応答（102 件中 66 件）では何も出しません。

`isImage` / `interrupted` は実データで `false` が常設されるため、`true` の場合だけ `image` / `interrupted` を表示します。`noOutputExpected` も同様に常設ですが、true でも「出力が無いのが正常」という意味しかなく表示価値が無いため出しません。`structuredPatch[].lines` は hunk 内の行だけを保持しファイルヘッダーを含まないため、`+` / `-` で始まる全行を加除として数えます。これにより、内容自体が `++` / `--` で始まる行が diff 上で `+++` / `---` になっても過少計上しません。

`tool_use_result` が object でなく文字列、または Context7 等で見られる `[{"type":"text","text":"..."}]` 配列の応答（実データで確認）は、成功時のみ先頭の有意な 1 行を `result:<要約>` として補足表示します。エラー時は content 側のサマリー表示と同文になるため補足しません。

実データで確認した `timedOutAfterMs` は、コマンド失敗ではなくバックグラウンド移行までの待機期限なので `wait-timeout:<期間>` と表示します。`backgroundCwdHint` は `cwd-hint:<要約>`、top-level `tool_result_meta[].non_execution_kind` は対象の tool use id と照合し、`not-executed:<理由>` として表示します。

モニターペインの進捗は `fail:<n> retry:<n>` を併記し、完了時も `%d succeeded / %d failed / %d retry` の形で表示します。

## 並列実行モデル

`execute_plan_tmux` はタスクキュー方式で並列実行します。

- 各タスクは `queue_dir/pending-<idx>` と `tasks/task-<idx>.sh` として事前に書き出される
- ワーカーは `token-burn gate-claim` に停止判定と claim をまとめて任せ、返ってきた番号の `task-<idx>.sh` を `source` で実行する。claim は同ロック下で `pending-<idx>` → `claimed-<idx>` の rename として行われる。停止判定と claim をシェル側で 2 段に分けていた頃は、その隙間に停止が発行されるとタスクが余計に開始されていた（前述「ワーカーのゲートと claim」）
- 各タスクを `source` する前にワーカーループ先頭で `CANCELLED` フラグを 0 にリセットする。直前タスクの実行中に SIGINT/SIGTERM を受けて `CANCELLED=1` が立ったまま成功・早期 return しても、後続タスクの通常エラーを誤って `Cancelled` 判定しエラー記録を欠落させるのを防ぐ
- タスクがエラー終了してもワーカーは `exec sleep infinity` せず、即座に次の `pending-*` を取りに行く
- ワーカーは claim できる pending が尽きるまで処理を続け、尽きて初めて `worker-done-<w>` を作成して終了する。終了するとそのペインは閉じるため、画面には走行中のワーカーだけが残る
- 全タスクの処理が済むか全ワーカーが終了すると、モニターは集計とログパスを表示してからセッションを閉じ、token-burn を終了する（Ctrl-C 待ちはしない）
- ユーザーが tmux をデタッチした場合、tmux セッションが生存していれば `/tmp/token-burn` は削除しない。ワーカーのキュー・タスクスクリプト・プロンプトファイルを保持し、バックグラウンド実行を継続できるようにする
- tmux セッション作成後のペイン構築に失敗した場合は、作成途中のセッションを kill して一時実行ディレクトリも削除する
- レポートディレクトリ名に使うエージェント名は `sanitize_filename` でパス成分を無害化する

結果として、`parallelism` で指定した並列数はタスクが尽きるまで維持されます（一部タスクが失敗しても他ワーカーは止まらない）。エラーは `marker_dir/error-<idx>` にタスク単位で記録されるため、同一ワーカーで複数エラーが起きてもモニターに全て表示されます。

並列数は CLI の `--workers` / `-w`（1 以上。0 は clap の `value_parser` で拒否）で実行ごとに上書きでき、未指定なら `[settings].parallelism` を使います。実際に起動するワーカー数は `worker_count`（= `parallelism.min(タスク数)`）で頭打ちになり、`print_plan` が同じ関数で算出した実効値を実行計画に `Workers:` として表示します（頭打ちのときは要求値も併記）。表示と `execute_plan_tmux` の起動数が食い違わないよう、両者は必ずこの関数を経由します。

`format-stream` は以下の stream-json イベントを処理します:
- テキスト応答のストリーミング表示
- セッション開始（`system` / `init`）のモデル・CLI バージョン・権限モードを 1 行表示（`ℹ Session <model> (v<version>, <permissionMode>)`）。これらは他のどのイベントにも現れず、`result.modelUsage` からは実際に課金されたモデルしか分からないため、CLI バージョンと `bypassPermissions` で走ったかどうかが完全に失われていた。セッションにつき 1 行のみ。実ログの `init.model` 等に混入する `claude-opus-5[1m]` のような壊れた末尾 SGR 断片はモデル表示の全経路で除去する
- 思考ブロック（`thinking`）のプログレスインジケーター。Claude Code は思考本文を伏せるため `thinking_delta` の `thinking` は空文字で届き、進捗は `estimated_tokens`（累積ではなく**増分**。実データは 50 / 100 / 150 単位で、1 ブロック合計 100〜500 程度）にだけ入る。50 トークンごとにドットを 1 つ出す。本文のバイト長だけを見ていた頃は実データ 7,516 件すべてでドットが 0 個になり、中身のない `💭 ` 行だけが並んでいた。本文が返る形式のために 100 バイトごとのカウントも残すが、情報源（推定トークン / 本文バイト）はブロック単位で最初に観測できた方に固定する（両方を加算するとドットが二重計上される）。ブロック終端に届く `estimated_tokens: null` かつ本文なしのデルタ（実データで 1,284 件）は進捗を報せないので、思考行を開かない
- 長時間ツールの `tool_progress` を経過時間付き（例: `Bash running (1m 30s)`）で表示
- ツール使用（`Read`/`Edit`/`Write`/`Bash`（小文字 `bash` を含む）/`BashOutput`/`Agent`/`Task`/`TaskCreate`/`TaskGet`/`TaskList`/`TaskUpdate`/`TaskStop`/`TaskOutput`/`Workflow`/`TeamCreate`/`Skill`/`SlashCommand`/`TodoWrite`/`Monitor`/`Grep`/`Glob`/`ScheduleWakeup`/`WebFetch`/`WebSearch`/`ToolSearch`/`SendMessage`/`AskUserQuestion`/Context7・Tavily・Codex MCP 等）の詳細表示と差分出力
- サブエージェント内部のツール使用（`🔧 <ツール名> @<タスク名> <詳細>`）とテキスト出力（`💬 @<タスク名> <先頭 1 行>`）。`stream_event` はメインループ専用で、実データ 98,061 件すべてが `parent_tool_use_id: null` だった。サブエージェントのツール使用とテキストは `assistant` イベントにしか現れないため、ここを素通りしていた頃はツール完了行の 44%（1,178 / 2,695 件）が `✓ Bash @<タスク名>` だけになり、何のコマンドを打ったのかがログから完全に消えていた（`tool_id_map` への登録はしていたのでツール名だけは解決できていたが、`input` は `finalize_block` → `extract_tool_detail` を通るストリーム経路にしか繋がっていなかった）。サブエージェントの最終レポート（実データで 179 ブロック / 363KB）も同様に失われる（`task_notification.summary` は 60 文字に切られ、`✓ Agent` の完了行はメタデータを持つため `tool_result_string_summary` のフォールバックにも乗らない）。メインループの assistant イベントは `stream_event` と同じ内容を再送するだけなので対象にしない（両方書くと 1 ツールにつき 🔧 行が 2 本出る）。判定は「帰属（`task_description` / `subagent_type`）があり、かつ `parent_tool_use_id` が非 null」で行う。`--include-partial-messages` は同一 message id の assistant を複数回送るため、ツール使用 id とテキスト内容で重複排除する。Edit の差分はメインループと同じく出す（実データのサブエージェント Edit は 7 セッションで 3 件しかなく出力量は増えない一方、変更内容は他のどの行にも現れない）
- assistant メッセージの `fallback` コンテンツによるモデル切り替え（`from.model` → `to.model`）と、`message.diagnostics.cache_miss_reason` によるキャッシュミス理由・対象 input token 数の表示。`--include-partial-messages` が同一 message id の assistant メッセージを繰り返し出力しても、同じ診断は 1 回だけ表示する。これらの通知は出力がある場合だけ `break_open_line` で開きっぱなしの思考/テキスト行を閉じてから書く（`handle_assistant_event` が通知をいったんバッファへ書き、非空のときだけ行を閉じる）。思考ブロックは `\x1b[2m💭 ` を改行なしで書き進めるため、そのまま通知を書くと同じ行に連結され、通知末尾の `\x1b[0m` が dim を打ち消して以降の進捗ドットが崩れる。毎回無条件に閉じると assistant イベント（1 セッションで数千件）ごとに改行が入って進捗ドット表示自体が壊れるため、出力有無で分岐する
- `model_refusal_fallback` は切り替え元・切り替え先モデルとカテゴリを表示する。拒否対象の内容や explanation はモニターへ出さない
- `Read` の `file_path` と `offset` / `limit` / `view_range`、malformed 入力時の `__unparsedToolInput.len`、`Bash` の `timeout`（1000ms 以上は `timeout=<秒>s`、未満は `timeout=<n>ms` でミリ秒切り捨てによる "0s" 誤表示を回避。同じ整形を `Monitor` / `TaskOutput` の `timeout` でも使用）/ `run_in_background` / `dangerouslyDisableSandbox`、`BashOutput` の `bash_id`（出力取得対象の background bash。`bash:<id>` 形式）と任意の `filter`、`Agent` の `run_in_background` を表示
- `Agent` / `Task` は `name` と `description` の両方があれば `<name> — <description>` として併記し、`subagent_type` などの属性を末尾へ付ける。`name` は `SendMessage` で参照する識別子、`description` は役割であり、どちらか一方を落とすと複数サブエージェント実行時の追跡に必要な情報が欠ける。任意の `model` / `isolation` は指定時だけ `model:<...>` / `isolation:<...>` として表示する
- `Edit` は `new_string` に加えて実データで確認された `new_str` 入力も差分表示に使用し、`replace_all` が true の場合は一括置換として表示する。詳細行の `(+追加/-削除)` は行数差分（new − old）ではなく、共通プレフィックス/サフィックス除去後の実変更行数（表示 diff の `+` / `-` 行数と常に一致）。行数差分だと同一行数の in-place 置換が `(+0/-0)` になり「変更なし」に見える（実ログで確認）。行分割 (`split_lines`) は末尾の改行を「空の最終行」として保持する。`str::lines()` は末尾改行を落とすため、これが無いと `"foo"` と `"foo\n"` が同じ行集合になり、EOF 改行を足すだけの Edit が `(+0/-0)` かつ差分表示なしで「変更なし」に見えてしまう
- `Grep` / `Glob` の検索パターン、対象パス、`output_mode`、`type`、`glob`、`head_limit`、`context`、`offset`、`-A` / `-B` / `-C` / `-n` / `-i` / `-o`、`multiline` を表示
- `ScheduleWakeup` の待機時間と理由を表示
- `WebFetch` の URL とプロンプト要約、`WebSearch` のクエリと include/exclude ドメイン件数、`ToolSearch` のクエリと `max_results` を表示
- `Monitor` の説明・タイムアウト・condition・persistent、`TaskStop` の task id / task ids / reason、`TaskList`、`TaskGet` の task id、`TaskOutput` の task id / `block` / `timeout`、`TaskCreate` の `subject` / `description` / `activeForm`、`TaskUpdate` の `taskId` / `status` / `owner` / `subject` / `description`、`SendMessage` の送信先/要約、`AskUserQuestion` の質問数・選択肢数、`SlashCommand` の実行コマンド文字列（`/<command> ...`）、Tavily search の query/max/time range/search depth/topic/days/start_date/end_date/include_domains/exclude_domains（`topic=news` はニュース索引への切り替え、`days` はその遡及日数で、いずれも検索対象そのものを変える。実データの `topic:news days:8` は `time_range` を伴わないため、落とすと通常の Web 検索と区別できない。`start_date` / `end_date` も同じ期間フィルタで、実データでは `time_range`(2 件)より `start_date`(13 件)の方が主流。ドメイン絞り込みは検索対象そのものを置き換える強いフィルタで、1 件ならドメイン名を `site=<domain>` / `-site=<domain>`、複数なら件数を `site=<n> domains` として出す。`WebSearch` では allowed/blocked の件数を既に出しているのに Tavily 側だけ落ちていた）と Tavily extract の先頭 URL/件数(+N more)/extract_depth（`mcp__tavily__tavily-search` / `mcp__tavily__tavily_search` のハイフン版・アンダースコア版いずれも対応）、Codex MCP の prompt/cwd/model/sandbox/approval-policy、Context7 MCP ツールの library/query を表示
- `Workflow`（マルチエージェント・オーケストレーション）の起動対象を表示。名前指定（保存済みワークフロー）は `name`、インライン `script` は `export const meta = { name: ... }` から抽出したワークフロー名とスクリプト文字数（`<名前> (script:<n> chars)`。抽出できなければ `script:<n> chars`）、`scriptPath` 指定（再実行・resume）はファイル名を表示する
- ツール入力（`input_json_delta` で蓄積した JSON）がパースできない場合は、詳細を空にせず生入力の文字数を `unparsed:<n> chars` として表示し、malformed / truncated を可視化する。これはモデルが不正な JSON をツール入力として出力したケース（`InputValidationError: ... could not be parsed as JSON`）や、レート制限・セッション切断でツール呼び出しがストリーム途中で打ち切られたケースで発生する。`format-stream` はストリーミング経路（`content_block_start` で空入力 → `input_json_delta` で生 JSON 蓄積 → `content_block_stop` で確定）で処理するため、assistant メッセージ最終形の `__unparsedToolInput.len`（`Read` 等の専用ハンドラが別途処理）はストリーミングには現れない。引数なしツールの空入力（`TaskList` 等）は従来どおり空表示を維持する
- サブエージェントの開始・進捗・状態更新・完了通知（`task_started` / `task_progress` / `task_updated` / `task_notification`）。`task_started` は `task_type=local_agent` のような実行方式より、存在する場合は `subagent_type`（`general-purpose` / `Explore` 等）を優先表示し、`spawn_depth` が 2 以上なら `depth:<n>` を添える（入れ子起動は実行時間とトークン消費が指数的に膨らむ一方、`result.subagent_stats.max_depth` は最終集計しか持たないため、どのタスクが深いのかを追えない。深さ 1 は通常のトップレベル起動でノイズになるので出さない）。`owned_by_subagent:true` なら `nested` を添える。`spawn_depth` が付くのは `local_agent` だけ（実データ 30 件）で、サブエージェントが起動した background Bash は `owned_by_subagent:true` の `local_bash`（84 件）として届き深さの手掛かりを一切持たないため、`depth:` と同じ目的でこちらにも印を付ける。`task_progress` は `usage.total_tokens` を `tokens:<n>` として併記する（実データ 1,254 件すべてに入り 10 万トークン級が常態。token-burn はトークン消費の可視化そのものが目的である一方 `result.usage` はメインループ分しか持たないため、これが無いとどのサブエージェントが枠を食っているかを実行中に読み取れない。`tool_uses` / `duration_ms` は完了行の `duration:` / `tokens:` と重複するので出さない）。`is_backgrounded` は表示しない（Claude Code はサブエージェントを常に背景で起動するため実データの `local_agent` では全件 true で情報量が無く、長時間 Bash の自動バックグラウンド移行は完了行の `assistantAutoBackgrounded`、明示的な背景起動は `Agent` ツール行の `run_in_background` で既に見えている）。`task_notification` は `completed` / `failed` / `stopped` を表示し、`failed` の `summary` を失敗原因として併記する。`usage` が無い場合は duration/token を 0 として表示しない。`task_updated` の `killed` は `failed` / `cancelled` と同じ失敗状態として強調表示し、`patch.error`（例: `Agent terminated early due to an API error: ...`）があれば失敗理由として併記する。これを落とすと `Task failed` だけが残り、無人実行でサブエージェントが死んだ理由を後から追えない。`status` を伴わず `patch.is_backgrounded:true` だけの更新は `Task backgrounded` として表示する（以降そのタスクの出力がインラインに出なくなる理由そのもののため）
- `background_tasks_changed` は実行中バックグラウンドタスク一覧の高頻度スナップショットで、個々の開始・進捗・完了は上記タスクイベントにより表示済みのため、重複ノイズとして明示的に無視する
- `commands_changed` は利用可能なスキル/コマンド一覧のスナップショット通知で、1 件で数百 KB（実データで 414KB）に達する。一覧の中身はセッションの実行内容と無関係なため明示的に無視する
- `vcs_state_changed` は git hook（`git-sc` 等）による自動 commit / push の通知で、`⎇ VCS <kind> (<branch>)` として表示する。無人実行中にコミットや push が作られた事実はセッション後の変更追跡の起点であり、落とすと「いつの間にかコミットが増えている」理由をログから追えない。`branch` は実データの `push` に付随し、`main` へ push したのか作業ブランチへ push したのかで影響範囲が全く違うため kind と併記する（`cwd` は対象リポジトリと重複するので表示しない）
- 合成 user メッセージ（`isSynthetic: true`）のうち、フックの差し戻し（第 1 行が `<Event> hook feedback:`）を `⚠ Hook feedback (<Event>): <内容>` として表示する。Stop フックが exit 2 / タイムアウトで終わると Claude Code はその内容を合成 user メッセージとしてモデルへ差し戻すが、これは `system` の `hook_response` にも `notification` にも現れない（実データの `"Stop hook feedback:\n⏱ Stop hook timed out after 120s: cargo"`）。無人実行では Stop フックが自動コミット / push を担うため、落とすと「仕事は終わったのにコミットされていない」理由をログから追えない。同じ `isSynthetic` でもスキル本文の注入（`"Base directory for this skill: ..."`）は SKILL.md 全文を含んで巨大になり `Skill` ツール行と重複するため表示しない
- Claude Code のシステム通知（`notification`。例: stop hook エラー）と、出力を伴う hook 診断（`hook_progress` / `hook_response` の output / stderr / stdout）。候補キーの走査には `first_string` ではなく `first_non_empty_string` を使う。実データの `hook_response` は `output` / `stdout` / `stderr` を常に持ち、失敗時は stderr にだけ内容が入るため、値が文字列でありさえすれば空文字でも確定する `first_string` だと `output:""` が採用されてフォールバックが到達不能になり、診断が最も欲しい場面で "no output" にしかならなかった
- JSON として解釈できない行（`codex` 等のプレーンテキスト出力、claude ラッパーのバナー、`2>&1` で合流した stderr）は内容をそのまま独立行へ出す。このとき system / `rate_limit_event` / `user` / `tool_progress` と同じく、開きっぱなしの思考・テキスト行を閉じてから書く。タスクスクリプトは `claude ... 2>&1 | format-stream` で stderr を同じパイプへ合流させるため、`API Error: Connection closed mid-response.` のような stderr 行が思考ブロックの途中に到着し得る。直接書くと `💭 ..API Error: ...` の形で連結され、行末のリセットが dim を打ち消して以降の進捗ドット表示まで崩れる
- 表示対象の system / `rate_limit_event` / `user`（ツール完了行） / `tool_progress` は先にバッファへ描画し、出力がある場合だけ開いている本文・思考行を閉じてから独立行へ書く。バックグラウンド/非同期ツールの完了とハートビートは次ターンのテキスト・思考 delta の途中にも到着するため、直接書くと開きっぱなしの行へ連結される（実ログの整形結果に `💭   ✓ WebFetch` の形が 38 件あった）。実ログでは text delta の `I` と `'ll` の間に rate-limit 通知が到着し、単語中へ通知が連結されていた。`thinking_tokens` / 詳細のない `allowed` など無視対象イベントではバッファが空なので、本文へ不要な改行を増やさない
- `result.subagent_stats` の起動・完了・失敗・強制終了・起動拒否件数と、バックグラウンド/入れ子起動数、最大深度の集計表示。top-level success でも配下に失敗があれば警告色にする
- `tool_use_result` の出力切り詰め、適用 limit、stale read ヒント、ユーザ変更検出（`user-modified`）、古い読み取り状態からの自動回復（`stale-recovered`）、メモリ用ディレクトリへの印付け（`memdir-stamped`）、失敗理由（`error:`）や結果メッセージ（`message:`）、Bash 等の標準出力/標準エラー要約（`stdout:` / `stderr:`）、構造化応答の要約（`structured:`）、文字列/text ブロック配列で返る成功結果の要約（`result:`）、Edit 結果のファイルパスと structured patch 規模（`file:<path>` / `patch:<hunks> ... +追加/-削除` / `replace_all`）、自動バックグラウンド化、clamp、永続化出力サイズ、戻りコード解釈、Agent の duration/token/tool 数・`ListAgents` の一覧件数（`agents:<n>`）・サブエージェント種別（`agent:`）・識別子（`agent-id:`）・再開した識別子（`resumed-agent:`）・解決モデル（`model:`）・編集行数（`edits:+追加/-削除`）、Grep/ToolSearch の結果件数と mode、WebSearch の結果件数/検索回数/所要時間、WebFetch の HTTP ステータス/応答サイズ、Read の部分読み取り行数（`lines:<n>/<total>`）またはオフセット付き範囲（`lines:<start>-<end>/<total>`）と token cap 切り詰め（`truncated:token-cap`）、タスク件数/task id/task type、TaskOutput の取得状態、Agent 出力ファイル、Monitor の timeout/persistent、TaskUpdate の状態遷移、ScheduleWakeup の予定時刻、Skill のコマンド名、Workflow のワークフロー名（`workflow:<name>`）の補足表示
- トークン使用量、コスト、キャッシュ内訳、Web検索/フェッチ回数の集計表示
- モデル別使用量（`modelUsage`）の内訳表示（キャッシュ読み取り/書き込みトークン、Web検索回数、`contextWindow` / `maxOutputTokens` を `ctx:1M` / `max_out:64K` のような単位付きで表示）。モデル ID の末尾に数字とセミコロンだけの壊れた SGR 断片がある場合は除去し、通常の角括弧を含む名前は保持する
- フッターの `model` 行（`message_start.model` / `result.model` 由来）も同じ `normalize_model_name` を通す。実データでは `init.model`（44 件）と `modelUsage` のキー（30 件）が `claude-opus-5[1m]` の形で断片を持つため、ここだけ素通しすると同じセッションの中でモデル表記が食い違う
- API応答時間（`duration_api_ms`）と初回トークン到達時間（`ttft_ms`）、初回ストリームトークン到達時間（`ttft_stream_ms`。キュー/リトライ待ちを含む `ttft_ms` より小さい純粋なストリーム遅延。`stream:` 形式）、リクエスト送信までの所要時間（`time_to_request_ms`。通常数十〜数百 ms のためミリ秒表記 `req:<n>ms`）の表示
- fast mode 状態（`fast_mode_state` が `off` 以外の場合）と、利用できない理由（空でない `fast_mode_disabled_reason`）の表示
- 異常終了時の `terminal_reason`（`completed` 以外の場合）と `permission_denials` の件数・ツール名表示
- result の `usage.service_tier`、`usage.speed`、空でない `usage.inference_geo`、`usage.iterations` 件数、`origin.kind` の表示
- レート制限警告（`rate_limit_event`）の使用率表示、リクエスト拒否通知、および overage（超過枠）の補足情報表示（`overageStatus` / `overageDisabledReason` / `overageResetsAt` / `isUsingOverage`・`overageInUse`）。補足は `allowed` だけでなく `allowed_warning` と `rejected` にも付ける。自動停止の判定は top-level の `utilization` ではなく `unifiedWindows` の `five_hour` / `seven_day`（＝実際にリクエストを止める枠）の最大使用率で行う（後述「レート制限の自動停止判定」）。実データの `rejected` は `overageStatus` / `overageResetsAt` / `isUsingOverage` を伴い、これらを落とすと「5 時間枠の resets 時刻」だけが残って、実際は超過枠まで使い切って復旧が数週間先でも「その時刻まで待てば再開できる」と誤読される。`isUsingOverage` と `overageInUse` は実データで同義の別キーとして両方現れるため、どちらか一方でも true なら `using_overage` を表示する。`allowed_warning` 時に `surpassedThreshold` が含まれている場合は通過済み警告閾値（例: `warning at 90%`）を併記する
- リセット時刻（`resetsAt` / `overageResetsAt`）は当日中なら `HH:MM`、翌日以降なら `MM/DD HH:MM` で表示する。時刻だけだと `seven_day` 枠（最大 7 日先）や overage 枠（実データで 28 日先）のリセットが「今日のその時刻」に見え、待てば再開できると誤読される（実ログでは復旧が 1 か月先でも `resets 09:00` としか出ていなかった）
- overage の補足を 80 文字へ切り詰める際、枠名（`rateLimitType`）は切り詰めの予算に含めない。`allowed` 行だけが枠名まで同じ予算へ入れており、実データの `five_hour overage:rejected reason:org_level_disabled_until overage_resets:10/01 09:00`（85 文字）が超過して末尾が `overage_resets:10/...` の形で切れていた（実ログの整形結果で 41 件 = overage 付き `allowed` 行の全件）。切り落とされるのは上項で「誤読を防ぐために足した」と決めたリセット日時そのもので、残る `10/` は日付とも時刻とも読める断片になり誤読をむしろ増やす。他の経路（`wrap_overage_details`）は枠名を予算の外へ置いているので、そちらへ揃える
- レート制限使用率が `rate_limit_threshold`（デフォルト: 95%）を超えた場合、stop file を作成して後続タスクを自動停止。stop file の作成は usage-gate と同じく `create_new` で冪等（並列ワーカーから同時に呼ばれても既存内容は上書きしない）。`AlreadyExists`（別ワーカーが作成済み）は正常系として無視するが、ENOSPC・権限不足等で作成に失敗した場合は黙って握り潰さず、停止シグナル（stop file）が生成されない旨を出力に明示する（`format-stream` はパイプ中段のため exit code が観測されない）
- APIリトライ（`api_retry`）の試行回数とエラー情報の表示。実データには `error` フィールドの無い api_retry があり、その場合は "unknown" を補わず試行回数（と `error_status` があればそれ）だけを表示する
- `status`（リクエスト状態通知）と `thinking_tokens`（思考トークンの推定累積値 `estimated_tokens` / `estimated_tokens_delta`）は高頻度（1 セッションで数千件）に出力されるノイズイベントのため、明示的に無視します。思考中の進捗は `thinking_delta` のドット表示、トークン総数は `result.usage` の集計表示で代替するため、これらを表示すると重複・冗長になります

なお `usage` フィールドは各 `message_start` / `message_delta` でその API 呼び出し単独の値を返し、`result` イベントに最終累計が入るため、`format-stream` は `result` の値を最終出力として優先します。

ただし `result.usage` の累計は**メインループの消費だけ**で、サブエージェントの消費を含みません。実ログで検証したところ、`subagent_stats.spawned` が 0 のセッションでは `usage` と `modelUsage` の合計が完全に一致し、サブエージェントを起動したセッションでは `modelUsage` 側が常に大きくなります（cache_read で `2,110,690` → `220,321,325` の 100 倍超の実例あり）。トークン消費量そのものを目的とするツールで見出しの `in/out` が実消費の 1/100 になり得るため、`modelUsage` の合計が `usage` を上回るときだけ `📊 total in:<n> out:<n> (incl. subagents)` を併記します。一致する（＝サブエージェント未使用の）セッションでは重複表示になるので出しません。

この総計行には `modelUsage[].thinkingTokens` の合計も併記します（`📊 total in:<n> out:<n> (thinking:<n>, incl. subagents)`）。`usage.output_tokens_details.thinking_tokens` はメインループ分しか含まず、実ログではメインループ 89,291 に対しサブエージェント込みが 1,219,128（総出力 1,843,731 の 66%）でした。これを落とすと、サブエージェントへ消えたトークンの最大の内訳が丸ごと見えなくなります。`thinkingTokens` を持たない形式では従来どおり `(incl. subagents)` だけを出します。

`usage.output_tokens_details.thinking_tokens` は `output_tokens` の内訳で、実ログでは出力トークンの 10〜52% を占めます。これを表示しないと「何にトークンを使ったのか」の最大の内訳が失われるため、非ゼロのときだけ `📊 in:<n> out:<n> (thinking:<n>)` として括弧で添えます（出力トークンへの二重加算はしません）。

### レート制限の自動停止判定

`rate_limit_event` による自動停止は、**実際にリクエストを止める枠だけ**を基準にします。判定に使うのは `rate_limit_info.unifiedWindows` の `five_hour` / `seven_day` の使用率で、これを枠ごとに `rate_control::evaluate` へかけます（前述「停止の 2 種類」。5 時間枠だけが触れているなら一時停止、7 日枠が触れていれば恒久停止）。top-level の `utilization` は `rateLimitType` が指す枠の値でしかないため、そのまま閾値と比較してはいけません。

実データ（13 セッション）には `rateLimitType:"overage"` / `utilization:1.03` の警告が 188 件あり、同じイベントの `unifiedWindows.five_hour` は 0.13、`status` は `allowed_warning`（リクエストは通っている）でした。overage は月次の追加課金枠で、このアカウントでは `overageDisabledReason:"org_level_disabled"` により組織レベルで無効化されており実行に影響しません。top-level を基準にしていた頃は、5 時間枠が 13% でも `⛔ Rate limit auto-stop: 103% used (overage) (warning at 100%) >= threshold 90% resets 09:00` を出して全タスクを止めていました。top-level の `resetsAt` も overage 枠のもの（09:00）で、5 時間枠の実際のリセット（13:10）とは別物です。

- `seven_day_overage_included` は判定に使いません。overage 込みで分母が変わる派生指標で、実データでは常に `seven_day` より小さくなります（`seven_day:0.31` に対し `0.19`）。
- 停止行には判定に使った枠の名前・使用率・**その枠自身の** `resetsAt` を出し、`[5h 13% / 7d 54%]` の形で実測値を併記します。`surpassedThreshold` は top-level の `rateLimitType` について通過を報告した閾値なので、停止理由が別の枠になったときは併記しません（overage の `warning at 100%` を 5 時間枠の停止行に持ち込むと、その枠の警告閾値だと誤読される）。
- 一時停止の行は `⏸ Rate limit pause: 90% used (five_hour) ... >= threshold 90% — resuming at 11:50` の形で、恒久停止（`⛔ Rate limit auto-stop`）と区別できるようにします。恒久停止を選んだのが閾値超過以外の理由（`reset time unavailable` / `implausible reset <n>s ahead`）のときは括弧で併記します。これを落とすと、なぜ待たずに止めたのかがログから追えません。
- pause file に残す `reason` は、待機の根拠として**実際に成立している条件**を書きます。閾値超過で待つ場合は `Basis::reason`（`five_hour 90% >= threshold 90%`）、`rejected` で待つ場合は `request rejected (five_hour); five_hour at 42%` のように拒否された事実と実測値を書きます。`rejected` は閾値超過とは別の理由で起きる（実データでは 5 時間枠が 42% でも拒否される）ため、閾値の不等式を無条件に書くと、pause file とデッドライン超過時の停止メッセージに**成立していない不等式**が残り、後から「なぜ待っていたのか」を誤読します。
- 停止判定に使わない枠（overage）の警告は `⚠ Rate limit warning: 103% used (overage, no auto-stop) (warning at 100%) [5h 13% / 7d 54%] resets 09:00` として表示だけ行います。判定に使う枠の実測値は主語が何であれ併記します（主語が 5 時間枠でも、警告に出ない 7 日枠の残量や、壊れて判定から外れた枠があることは、なぜ止まった/止まらなかったのかを読むのに要る）。
- 判定は `allowed` と `allowed_warning` の両方で行います。両者の違いはサーバー側が警告閾値を跨いだかどうかだけで、`rate_limit_threshold` をサーバーの警告閾値より低く設定すると `allowed` のまま超過し得ます（実データの `allowed` は最大 5h 89% / 7d 68%）。表示は従来どおり `allowed` では補足情報があるときだけ出します（1 セッションで 480 件の高頻度イベントのため）。
- `unifiedWindows` を持たない形式へのフォールバックでは、`rateLimitType` が `overage` のときだけ判定をスキップし、それ以外は従来どおり top-level `utilization` で判定します。使用率が読めない枠（欠損・NaN・負値）は判定から外し、読める枠だけで判定します。判定基準が無い曖昧な警告では停止しない（fail-open）方針です。本当に枯れていれば `rejected` か、実行を止める枠側の警告として届きます。usage-gate という第 2 の停止経路もあるため、ここで曖昧なイベントを理由に全体を止める必要はありません。
- `rejected` は必ず後続を止めます（fail-closed）。ただし止め方は原因の枠で分けます。`rateLimitType` が `five_hour` で、かつ 7 日枠の実測値が読めて閾値未満で、かつ overage を使っていない（`isUsingOverage` / `overageInUse` がいずれも true でない）ことを確認できたときだけ、その枠のリセットまでの一時停止にします。それ以外は恒久停止です。overage が絡む拒否は月次の追加課金枠まで使い切った状態であり、5 時間枠のリセットを待っても再開できません（実データの `overageResetsAt` は 28 日先）。

処理済み状態は有効な設定ファイルと同じディレクトリの `state.json` に保存されます（デフォルト: `~/.config/token-burn/state.json`）。レート制限で中断したセッションの再開情報は、同じディレクトリの別ファイル `resume.json` に置きます（前述「中断セッションの再開（resume）」。`state.json` に同居させない理由もそこに書いています）。エージェント名は昇順、各エージェント内のエントリは処理時刻の降順（同時刻はパス昇順で安定化）で書き出します。内側のマップを `serde_json::Map` へ `collect()` してはいけません。`preserve_order` feature を有効にしていない serde_json の `Map` は `BTreeMap` であり、collect した時点でキー（パス）昇順へ再ソートされ、並べ替えが丸ごと捨てられます（実際の `state.json` も全エージェントがパスのアルファベット順になっていました）。順序を保つために `OrderedEntries` ラッパーで `serialize_map` を直接使います。

`[settings]` の `limit` は 1 以上である必要があります。
`[settings]` の `parallelism` は 1 以上である必要があります（CLI の `--workers` / `-w` で実行ごとに上書き可能）。
`[settings]` の `rate_limit_threshold` は 1〜100 の範囲で指定する必要があります（デフォルト: 95）。`rate_limit_event` の `unifiedWindows` が示す 5 時間枠 / 7 日枠の使用率がこの閾値以上になると、現在のタスク完了後に後続タスクの実行を止めます（月次の追加課金枠 `overage` の使用率では停止しません。前述「レート制限の自動停止判定」）。止め方は枠の周期で分かれ、**5 時間枠ならその枠のリセットまでの一時停止、7 日枠なら恒久停止**です（前述「停止の 2 種類」）。`rejected` イベント受信時も同様に止めます。ai-usage 連携が有効な場合は、各タスク完了後に該当 agent の実使用率（weekly / five_hour）でも `usage-gate` が同じ判定を行います。
`[settings]` の `skip_within` と `cleanup_after` には `d` / `h` / `m` / `s` を使った有効な期間文字列を指定する必要があり、不正または `chrono::Duration` で表現できない値は設定読み込み時にエラーになります。期間自体は表現できても日時の減算範囲を超える場合、`skip_within` は警告後に前回リセット時刻へフォールバックし、レポートクリーンアップはエラーを返します。
`[settings]` の `resume_interrupted`（デフォルト `true`）を `false` にすると、レート制限で中断したセッションの保存も再開も行いません（前述「中断セッションの再開（resume）」）。

### 処理済み履歴の共有範囲（dedup_scope）

`state.json` は展開エージェント名ごとに履歴を記録するため、既定ではアカウント A で処理したリポジトリもアカウント B からは未処理のままです。同じ CLI を 2 アカウントで回すと、B は A の続きからではなく同じ先頭ターゲットを再処理します。`[settings]` の `dedup_scope` は**スキップ判定で参照する範囲**を決めます（`global` | `provider` | `agent`、デフォルト `agent` = 従来の分離挙動）。

- `global`: 全エージェント横断。`state.json` にしか無い名前（改名・削除済みエージェント）の記録も参照する
- `provider`: 同じ `provider` のエージェント同士のみ共有。`provider` 未設定のエージェント（空文字 `""` や空白のみも未設定として扱う）と、現在の設定に無い名前は自分自身の記録だけを見る。`state.json` は provider を持たず現在の `RuntimeAgent` 一覧からしか復元できないため、別 provider の履歴を誤って引き当てて実行を握り潰すより取りこぼす方へ倒している
- `agent`: 実行中のエージェントのみ（従来どおり）

**書き込み側は変えません**。完了は常に実際に実行したエージェント名のキーへ記録するため、`state.json` のスキーマも「どのアカウントが処理したか」の履歴も保たれ、広がるのは参照側だけです（`State::last_processed_in_scope`）。

共有 scope（`global` / `provider`）は `skip_within` を必須にします。`skip_within` 省略時のカットオフは `sched.state_cutoff` = 実行中エージェントの前回リセット時刻でエージェント固有のため、他エージェントの履歴へ適用するとスキップ範囲が「どのエージェントで起動したか」次第で揺れます。設定側は `Config::validate`、CLI 上書き側は `resolve_dedup_scope` (`main.rs`) が同じ検査をします（CLI で `agent` から `global` へ引き上げた場合は `validate` を通らないため二重に置いています）。

CLI の `--dedup-scope <global|provider|agent>` で実行ごとに上書きできます。別アカウントが処理済みのリポジトリを意図的にもう一度回したいときは `--dedup-scope agent` を指定します。`skip_within` 必須の検査は `run` / `list` の分岐内で行います。共通部で解決すると、処理済み判定を一切使わない `status` / `clean` まで `--dedup-scope global` でエラーになります（`--dedup-scope` はグローバルオプションなのでどのサブコマンドにも付けられる）。スキップ表示は件数だけでなく scope・窓・どのエージェントの記録で弾いたかの内訳（`SkipSummary`）を出します。件数のみだと「統合が効いてスキップされた」のか「ターゲット探索が壊れて候補が消えた」のかを実行ログから切り分けられないためです。

`[[scan]]` で `username` を指定した場合、リポジトリ可視性（public/private）はローカルディレクトリ名ではなく `origin` の remote URL に含まれるリポジトリ名（大文字小文字を無視）で照合されます。`username` を指定しない通常スキャンでは `origin` remote がなくても対象に含まれ、可視性は `Unknown` になります。

remote URL の owner / repo 抽出は末尾 2 セグメントを採用するため、GitLab のサブグループ（例: `git@gitlab.example.com:group/subgroup/repo.git`）でも直近の親 (`subgroup`) を owner、`repo` を repository 名として認識します。GitHub の `owner/repo.git` のような 2 セグメント構成はそのまま機能します。

`[[scan]]` のディレクトリスキャンではシンボリックリンクはスキップされます（循環リンクによる無限再帰を防止）。

読み取りに失敗したディレクトリ（権限不足、走査中の削除等）は警告を出してスキップし、走査を続けます。存在しない `base_dirs`、取得に失敗した `DirEntry`、`origin` remote を取れないリポジトリと同じ「警告して継続」の方針です。走査中の `readdir` 失敗（マウント断・削除・ACL 変更）も `flatten()` で握り潰さず警告します。黙って捨てると、そのリポジトリが警告も無く対象から消え、`Found N repositories` の N が静かに減るだけで理由を追えません。以前は `find_repos` の `read_dir` だけがエラーを `run` / `list` まで伝播していたため、スキャン対象ですらない中間ディレクトリが 1 つ読めないだけでリポジトリを 1 件も処理せず異常終了していました。

複数の `[[scan]]` 設定で同一ディレクトリが重複検出された場合、ターゲットは1件に正規化されます（同一リポジトリの重複実行を防止）。

ディレクトリパスは重複排除と状態管理の前に絶対パスへ正規化されるため、`repo` と `./repo` のような等価な相対パスは同一ターゲットとして扱われます。

この正規化と重複排除は、`token-burn run PATH...` で特定ディレクトリを強制実行する場合にも適用されます。

`[[targets]]` には `defer = true` を指定でき、true のターゲットは実行リストの末尾に集められます（`scan` 由来のターゲットは常に `defer=false`）。`resolve_targets` の最後で `sort_by_key` による安定ソートが行われるため、`scan` 内の Visibility 順や `[[targets]]` 同士の追加順は各グループ (defer=false / defer=true) 内で維持されます。`token-burn run PATH...` で明示指定した場合は CLI 指定順を優先するため `defer` フラグは反映しません。

### 実行順（最終ファイル変更日時が古い順）

処理済みフィルタ (`filter_by_state`) の後、`limit` を適用する前に `sort_by_least_recent` (`main.rs`) が **最終ファイル変更日時の古い順** にターゲットを並べ替えます。`defer` の優先度はそのまま維持し、その内側だけを並べ替える安定ソートのため、変更日時が同じターゲット同士の順序（`scan` 内の Visibility 順 / `[[targets]]` の追加順）は変わりません。変更日時を取得できなかったリポジトリは判断材料が無いので各グループの末尾に置きます。中断セッションを再開できるターゲットは、`defer`（と有効なら可視性）の内側で変更日時より前に並べます（前述「中断セッションの再開（resume）」）。`token-burn run PATH...` で明示指定した場合は CLI 指定順を優先するため並べ替えません。

可視性（`public_first`）でグループ化するかどうかは `public_first_enabled` (`main.rs`) が判定し、**いずれかの `[[scan]]` が `public_first = true` のときだけ** `visibility` をソートキーへ入れます。無条件に入れていた頃は、`public_first` を読むのが `scanner::scan_directories` の 1 箇所だけなのに最終順序が必ず public 優先になり、`public_first = false` が黙って無視されていました（`limit` と併用すると、公開リポジトリが `limit` 件以上ある限り非公開リポジトリへ永久に到達しない）。`[[scan]]` が無い構成（`[[targets]]` のみ）でもグループ化しません。

処理済みカットオフ（`skip_within` / 前回リセット）は絶対時刻の窓であり、窓をまたいだ時点で処理済み履歴が一斉に無効化されます。ターゲット順が固定のままだとそのたびにリストの先頭 `limit` 件だけが再処理され、末尾のリポジトリには永遠に到達しませんでした（実測: 先頭 10 件が 2 日おきに再処理される一方、11 件目以降は 2 か月近く未処理）。古い順に並べ替えることで、カットオフが切れても前回処理した分は後ろへ回り、放置されているリポジトリから消化されます。

順序の基準は `state.json` の処理時刻ではなくリポジトリ自身の最終ファイル変更日時です。レート制限（429）で中断されて実際には何も変更できなかった実行を「処理済み」と数えてしまわないためです。

最終ファイル変更日時は `scanner::repo_last_modified` が `git ls-files` の列挙する追跡対象ファイルの mtime の最大値として求めます。ディレクトリを素朴に走査すると `target/` や `node_modules/` のビルド成果物が混ざり、`cargo build` しただけのリポジトリが「たった今変更された」ように見えてしまいます。追跡対象に限定すればビルド成果物と `.gitignore` 対象は自然に除外され、未コミットの編集は mtime としてそのまま拾えます。1 リポジトリにつき `git ls-files` の子プロセス起動が要るため、`repo_last_modified_map` が blocking タスクとして並行実行します。`list` / `run` のターゲット一覧にはこの日時が `(modified: ...)` としてローカル時刻で併記され、実行順の根拠を目視で確認できます。

### 対象選択 TUI（`--interactive` / `-i`）

`token-burn run -i` は実行前に対象選択 TUI（`src/tui.rs`、ratatui）を開き、**実行するリポジトリと実行順**をユーザーが確定します。ワーカーは `pending-0001..N` を番号順に claim するため、画面で確定した並びがそのまま処理順になります。

- **候補は `limit` で切らず全件表示し、先頭 `limit` 件を初期選択済み**にします。そのまま Enter を押せば非対話実行と同じ対象になり（後方互換）、`limit` の外側（11 件目以降）も選べます。確定後に `limit` を再適用すると、選んだのに実行されないターゲットが黙って落ちるため、選択結果をそのまま実行対象にします
- 初期の並びは既存のソート結果（最終変更が古い順 / 再開できるものは各グループの先頭 / `defer` は後ろ）そのまま。TUI で並べ替えた場合はユーザー指定が優先されます
- 中断セッションを再開する行には `↻` を付けます（前述「中断セッションの再開（resume）」）。選択から外した行は、再開の保留（`retry_after`）の根拠にもしません
- **`J` / `K` で行を動かすと実行順も入れ替わります**（選択済み同士を入れ替えたときだけ番号を交換するので、未選択行を跨いだだけでは選択済み同士の相対順は変わりません）。項目を `swap` するだけだと選択番号ごと運ばれてしまい、画面の番号が降順に乱れる一方で実行順は Space を押した順のまま残ります（並べ替え操作が実質無効になる）
- 実行順は**選択済みの行にだけ 1 から採番**して表示します。番号が出ていないと「並べ替えたつもりの順で処理される」ことを確認できません
- 選択 0 件のまま Enter を押しても実行しません（フッターに理由を表示）。全解除の誤操作をそのまま「対象なし」で走らせると、TUI を出した意味がないまま何も起きずに終わります
- キー: `↑↓` / `j` `k` 移動、`Space` 選択トグル、`J` `K`（`Shift+↑↓` も可）で行を上下に移動して並べ替え、`a` 全選択、`n` 全解除、`g` / `G` 先頭・末尾、`Enter` 決定、`q` / `Esc` / `Ctrl-C` キャンセル。`Shift+矢印` を取れない端末があるため `J` / `K` を主操作として案内します
- **TTY が無ければエラー**にします（`token-burn run -i > log` や CI）。`--interactive` を明示したのに選択画面が出ないまま実行が始まると、意図と違う対象へトークンを使います。TUI は opt-in なので、非対話実行は従来どおりオプション無しで動きます
- 状態遷移（カーソル・選択・並べ替え）は `SelectorState::on_key` に閉じ込め、描画と端末制御から切り離しています。端末を用意せずキー処理をユニットテストするためです
- 列幅は**表示幅（端末セル数、`unicode-width`）**で数えます（`truncate_chars` / `pad_end` / `display_path`）。`format!("{:<width$}")` はバイト幅で数えるため日本語を含むリポジトリ名で列がずれますが、char 数で数えても揃いません。ratatui は `unicode-width` でレイアウトするため、East Asian Wide 文字（1 文字 2 セル）を含む名前では char 数基準だと 1 文字ごとに 1 セルずつ後続の列が右へずれ、狭いペインでは行末がリスト枠で切り落とされます。パスはホームを `~` に畳んだうえで**先頭**を `…` で落とします（末尾にリポジトリ名が来るため、切るなら前を捨てる方が識別しやすい）

TUI での選択は人手なので分単位で止まります。そのため `execute_plan_tmux` に渡すデッドラインは、起動時の `sched.time_until_reset` ではなく確定直前の現在時刻から `remaining_until` (`main.rs`) で引き直します。起動時の値をそのまま渡すと、選択やスキャン（gh CLI / `git ls-files`）に費やした分だけモニターのデッドラインが後ろへずれ、実際のリセット後まで新規タスクを開始してしまいます。

## 実装上の注意点

- tmux へ渡すスクリプトパスは `tmux_script_arg`（= `shell_escape`）でクォートします。tmux は `new-session` / `split-window` の shell-command を `sh -c` 経由で実行するため、`std::env::temp_dir()`（= `TMPDIR`）に空白が含まれる環境では未クォートだと `/tmp/tb` を `space` `test/monitor.sh` を引数に起動しようとしてペインが即死します。しかも **tmux 自身は exit 0 を返す**ため直後の `ensure!(status.success())` では検知できず、後続の split-window が「no such session」で失敗して真因と無関係なエラーになります。生成するシェルスクリプトの内側は `shell_escape` 済みで、この tmux 呼び出しだけが取りこぼしでした。
- ワーカーは全タスク完了後、`CURRENT_FAILED_MARKER` を空にして `trap - INT TERM` でキャンセル trap を外してから `exit 0` します。trap を残したまま終了すると、解除直前に届いた INT/TERM で `handle_cancel` だけが走り、処理するタスクが無いのに直前タスクの failed マーカーを立て直してしまいます。

- レポート出力先 (`resolve_report_dir` / `main.rs`) は設定値を必ず絶対パスへ正規化します（`config::resolve_directory` 経由）。相対パスのまま返すと、レポートディレクトリの作成（`executor` 側。プロセスの cwd で解決）と、そこへ書き込むタスクスクリプトの `tee` / `--raw-output`（対象リポジトリへ `cd` した後で解決）が別ディレクトリを指し、ログのパイプラインが `No such file or directory` で失敗して全ターゲットが `failed-N` になります（`state.json` に 1 件も記録されない）。`report_dir = "reports"` のような素直な設定で踏みます。
- `format-stream` の入力読み取りは `read_lines_lossy` で行い、不正な UTF-8 バイトは U+FFFD へ置換します。`BufRead::lines()` は非 UTF-8 バイトを含む行に `Err(InvalidData)` を返し、そこで中断すると以降の**正常な JSON も含めて**標準出力と `--raw-output` の両方から失われます。タスクスクリプトは `claude ... 2>&1 | token-burn format-stream ... | tee log` で stderr を同じパイプへ合流させており、stream-json の 1 行は macOS の `PIPE_BUF`（512 バイト）を常に超えるため、stdout の途中に stderr の書き込みが割り込んでマルチバイト文字が分断されるだけで不正な UTF-8 が生じ得ます。中断すると `FORMAT_EXIT != 0` で `failed-N` になるうえ、パイプが閉じて `claude` 本体が SIGPIPE で落ち、表示整形の都合で数時間の実行を巻き添えにします。
- モニターは 1 周回の先頭で `worker-done-*` を**タスクマーカーより先に**読みます。ワーカーは `done-*` / `failed-*` / `retry-*` を書き切ってから `worker-done-*` を作るため、この順序なら「worker-done は見えているのにそのワーカーのタスクマーカーが見えていない」状態は起こりません。逆順（タスクマーカー → `fetch_usage` で最大 `AI_USAGE_TIMEOUT` 秒ブロック → worker-done）だと、その待ち時間に最後のワーカーが完走した場合に古い `PROCESSED` と新しい `WORKERS_DONE` が組み合わさり、全件成功でも `⏹ Stopped: 9/10 processed` と誤報告します。
- モニターの ai-usage 再取得スロットル (`LAST_USAGE`) は fetch **完了時刻**を記録します。`fetch_usage` は `run_with_timeout` を 2 回呼ぶため最長 `2*AI_USAGE_TIMEOUT` 秒かかり、開始時刻（ループ先頭の `NOW`）を基準にすると次の周回で即座に条件が成立して間隔を空けずに再取得し続けます。ループ外の初回 `fetch_usage` 直後にも記録し、起動直後の二重取得を防ぎます。
- `ai-usage --json` の枠データは `kind`（`five_hour` / `daily` / `weekly` / `monthly`）を読み、そこから `state_cutoff` の周期を導きます。スロット名（`weekly` / `five_hour`）と実際の枠長は一致しません。実データでは antigravity が `five_hour` スロットに `kind:"daily"`（24 時間枠）を、pixellab が `weekly` スロットに `kind:"monthly"`（月次枠）を返します。スロット名で決め打ちすると 24 時間枠を 5 時間として扱い、`state_cutoff`（= 直前の枠の開始点）が未来へ飛んで `filter_by_state` の `last >= cutoff` が恒偽になり、処理済みフィルタが黙って無効化されます（毎回同じ先頭ターゲットだけを再処理し続ける）。未知の `kind` はスロット名の既定周期へ落とします。
- `scanner` から `git` を起動する箇所は `run_git_capture` に集約し、パスを `OsStr` のまま渡します。`to_string_lossy()` は不正な UTF-8 バイトを U+FFFD へ置換するため、非 UTF-8 のディレクトリ名では存在しないパスを git に渡すことになり、`username` 指定時はそのリポジトリが黙って対象から消えます。`git` 自体が PATH に無い場合は 1 プロセスにつき 1 回だけ警告します（黙って `None` にすると全リポジトリが対象外になって `No targets found` だけが出て、原因の手掛かりがゼロになる）。

- リセット日時計算 (`schedule.rs`) は `naive_local()` をベースに行います。`DateTime::date_naive()` は UTC 日付を返すため、`weekday()` のローカル曜日と整合させるためにローカルタイムゾーンの日付を基準とします。Asia/Tokyo のような UTC+N のタイムゾーンで深夜帯（UTC 前日）に実行しても曜日がずれない設計です。
- リセット時刻が DST（夏時間）遷移に重なる場合も `resolve_local_datetime` (`schedule.rs`) で解決します。曖昧な時刻（秋の繰り戻しで 2 回出現する時刻）は早い方を採用し、存在しない時刻（春の繰り上げでスキップされる時刻）は遷移直後の最初の有効な瞬間にフォールバックします。`from_local_datetime().earliest()` は存在しない時刻に対して `None` を返すため、`America/New_York` の `02:30` のように DST ギャップへ重なるリセット時刻だと、設定読み込みは成功するのに `status` / `run` が実行時に毎回失敗していました。これを防ぐ実装です。
- 状態ファイル (`state.json`) の書き込みは「同一ディレクトリのテンポラリファイル (`.state.json.tmp.<PID>.<nanos>`) に書き出し → `rename` で本体に置き換える」 atomic rename パターンで行います。排他ロックは `state.json` 本体ではなく sidecar の `.state.json.lock` に取ります。本体をロックすると `rename` 後にロック対象 inode が古くなり、別ワーカーが新しい `state.json` を同時ロックできて更新を失うためです。`write_all` 途中の ENOSPC やプロセスクラッシュでも本体が壊れず、書き込み失敗時はテンポラリファイルを掃除します（次回起動時に残骸が積み重ならない）。ロック取得後に既存の JSON が壊れていた場合や、権限・I/O エラーで読み取れない場合は、空状態として上書きせず更新をエラーで中断し、原本と処理済み履歴を保全します。読み取り側の `State::load` も同じ方針で、存在しない場合のみ空状態として扱い、権限・I/O エラーはエラーとして伝搬します（JSON 破損時のみ警告を出して空状態で続行）。ここを空状態へ潰すと `filter_by_state` が全ターゲットを未処理と判断して消化済みリポジトリを再実行しクォータを二重消費するうえ、`token-burn mark` 側は書き込みで正しくエラーになるため `state.json` が更新されず、次回以降も同じ状態が再現して延々と同じターゲットを処理し続けます。`resume.json` の書き込みも同じ方式（sidecar の `.resume.json.lock`）ですが、計画時の読み込みだけは壊れていても警告して空として扱います（前述「中断セッションの再開（resume）」）。
- tmux ワーカー / モニター起動前の `chmod +x` は終了コードを検証します。`output()` の戻り値だけ確認する旧実装では `chmod` が非ゼロで終了しても無視されてしまい、`permission denied` が tmux ペイン内で初めて顕在化していました。
- tmux セッション作成後にペイン分割・ワーカー起動が失敗した場合は、そのセッションを kill して一時実行ディレクトリを削除します。セッションだけ作成されて後続コマンドが失敗すると、従来は孤立セッションと `/tmp/token-burn` 配下の実行資産が残っていました。
- `kill-session` / `has-session` のセッション指定は `=` を付けた**厳密一致ターゲット**（`-t =token-burn`）で行います。tmux の `-t` は「完全一致 → 前方一致 → fnmatch」の順に解決するため、素の `token-burn` は目的のセッションが存在しないとき `token-burn-notes` のような無関係なセッションへ当たります（実測で確認）。起動時の「既存セッションがあれば終了」がそれを掴むと**ユーザーの別セッションを黙って破壊し**、`attach-session` 後の生存確認が掴むと「デタッチされた」と誤判定して最終集計を出さずに終わります。モニタースクリプトの `finish_session` / 強制終了も同じ理由で `-t "=$SESSION"` を使います。`new-session -s` は新しい名前の指定であってターゲットではないため素の名前を渡します。`split-window` / `select-pane` / `set-option` などはセッション作成後にしか走らず、完全一致が最優先で解決されるため素の名前のままです。
- 起動時キャッシュ初期化で ai-usage を同期起動する `spawn_ai_usage_sync_with_timeout` (`executor/mod.rs`) は、env を「空文字の値は unset」という設定側の規約（シェル経路の `env_prefix_parts` が `env -u KEY` へ変換するのと同じ意味）で適用します。`Command::envs()` へ素通しすると空文字がそのまま子プロセスへ渡り、`CLAUDE_CONFIG_DIR = ""` を「既定に戻す」つもりで書いた profile で ai-usage が cwd 相対の設定ディレクトリを見に行って別アカウントの使用率を拾います。この結果は起動時キャッシュ（`ai-usage-cache.json`）として usage-gate と共有されるため、誤った使用率で停止判定が走ります。
- 起動時キャッシュ初期化で ai-usage を同期起動する `spawn_ai_usage_sync_with_timeout` (`executor/mod.rs`) は、子プロセスの stdout/stderr を**別スレッドで並行に drain** します。子の終了を待ってからまとめて読む実装では、出力がパイプバッファ（macOS では 16KB 程度）を超えたとき子の `write(2)` がブロックして終了できず、`try_wait` が永遠に `None` を返してタイムアウトまでハングするデッドロックに陥ります（大きな JSON や stderr へのログ出力で発生）。読み取りを終了監視から分離することでこれを防ぎます。
- `format-stream` の `truncate_str` は「返却文字列の char 数を `max` 以下に保つ」契約を満たします。省略記号 `"..."` を付ける余地が無い `max <= 3` の場合は先頭から `max` 文字までで切り詰めます（実コードの呼び出しサイトは最小でも 30 程度のため、契約強化に伴う表示変更はありません）。
- 同じ `truncate_str` は**改行（LF / CR）を空白へ畳んでから**切り詰めます。呼び出し元はすべて「1 行の中へ埋め込む」用途ですが、実データの Bash `command` はヒアドキュメントや複数行スクリプトで改行を含み、2,797 件中 284 件が先頭 60 文字以内で改行していました。そのまま埋め込むと 1 行のはずのツール行が複数行へ割れ、`\x1b[2m` を開いたまま改行して閉じる `\x1b[0m` が最終行にしか出ません（実ログの整形結果で 145 件）。このモジュールは `break_open_line` まで作り込んで「1 イベント 1 行」を守っているので、切り詰め側でも行を割らないことを保証します。`task_notification` の `summary` のように別のフィールドが複数行になる場合も同じ経路で守られます。
- 合成 user メッセージの hook feedback を切り出す `split_hook_feedback` は、`to_lowercase()` した文字列のバイト位置を元文字列へ流用しません。小文字化はバイト長を保存せず、`İ`(U+0130, 2 バイト) は 3 バイトへ伸び、`K`(U+212A KELVIN SIGN, 3 バイト) は 1 バイトへ縮むため、伸びた側では範囲外スライス、縮んだ側では非文字境界スライスで **panic** します。マーカーは純 ASCII なので、元文字列上で ASCII の大文字小文字だけ無視して探す（`find_ascii_case_insensitive`）と位置が常に正しく、UTF-8 では ASCII バイトが多バイト列の内部に現れないため結果は必ず文字境界になります。`format-stream` はパイプ中段なので、panic はパイプを閉じて上流の `claude` を SIGPIPE で道連れにし、数時間の実行を巻き添えにします。
- レポートディレクトリのクリーンアップ (`cleanup.rs`) はシンボリックリンクをスキップします。`Path::is_dir()` はリンクを追跡するため、リンク先のディレクトリを誤って削除しないよう `is_symlink()` で除外します。
- モニタースクリプトのエラーマーカー走査は `while IFS= read -r ... < <(find ...)` 方式を使用しており、`TMPDIR` のパスに空白が含まれる環境でもワードスプリットが発生しません。エラー内容の表示は `printf '%s'` 経由で行い、ファイル内容を `echo` のダブルクォート内で再解釈しないようにしています。
- デタッチ実行後のログを整形する `strip_ansi` (`executor/util.rs`) は、charset designation エスケープ（`\x1b(B` = G0 を ASCII 集合に指定、`\x1b(0` = DEC 罫線集合等）を introducer（`( ) * + - . /`）＋終端バイトの 3 バイトとして扱い両方を除去します。introducer だけをスキップする実装では終端バイト（`\x1b(B` の `B` 等）が通常文字としてログに漏れます。その他の 2 バイトエスケープ（`\x1b=` / `\x1b>` / `\x1bM` 等）は従来どおり ESC ＋ 1 文字だけスキップします。
- モニタースクリプトの `run_with_timeout` (`executor/scripts.rs`) は、監視サブシェルの stdout を必ず `>/dev/null 2>&1` で捨てます。呼び出し側の stdout を継承したままだと、コマンドが即座に終わってもサブシェルの子 `sleep $secs` がコマンド置換のパイプ書き込み端を握ったまま孤児化し（`kill -TERM $wpid` はサブシェル本体しか殺せない）、`new=$(run_with_timeout ...)` が EOF を待って **timeout 秒まるごとブロック**します。ハング対策のはずが、10 秒ごとの ai-usage 取得で毎回 `AI_USAGE_MONITOR_TIMEOUT_SECS`（30 秒）固まり、毎秒更新のはずの進捗バーとデッドライン残り時間が止まっていました（実測: 即終了コマンドに 8 秒指定 → 8 秒）。
- 同じ `run_with_timeout` の `wait` は**ループで待ち直します**。bash の `wait` は trap を設定したシグナルを受けると 128 超の終了ステータスで即座に返るため、モニターが張っている `trap 'RESIZED=1' WINCH`（ワーカーのペインが閉じるたびに届く）と 10 秒ごとの ai-usage 取得が重なると、(1) 取得済みの JSON を捨てて `rm -f <cache>.tmp` へ落ち、その周回のキャッシュ更新が飛ぶ、(2) `kill -TERM $wpid` は監視サブシェルしか殺さないため ai-usage 本体が削除済み tmp の fd を掴んだまま孤児化する、の 2 つが起きます（実測 rc=156）。`rc <= 128` か、子が既に消えているときだけループを抜けます。statusline 取得側（`new=$(run_with_timeout ...)`）はコマンド置換でサブシェルの trap がリセットされるため元から影響を受けませんが、キャッシュ更新はメインシェルで走るので直撃していました。
- タスクスクリプトは対象ディレクトリへの `cd` をパイプラインと分けて発行します。`build_shell_command` は `cd` を含めず、`build_task_script` が手前で `cd <dir> || { ...; return 0; }` を出します。`cd X && cmd 2>&1 | format-stream | tee log` と書くと bash は `cd X && (3 要素パイプライン)` と解釈するため、cd 失敗時はパイプラインが実行されず `PIPESTATUS` が cd の 1 要素だけになります。すると `FORMAT_EXIT` / `TEE_EXIT` が空文字に展開されて `[ "" -ne 0 ]` が `integer expression expected` を吐き（ワーカーペインに漏れる）、記録されるエラーも真因と無関係な「logging pipeline failed」になっていました。スキャンから実行までの間に対象リポジトリが削除・リネームされると発生します。
- 実行用一時ディレクトリの準備は `prepare_run_tmp_dir` (`executor/mod.rs`) が行い、`remove_dir_all` の失敗を（`NotFound` を除き）エラーとして伝播したうえで、作成後に unix では `0o700` を設定します。旧実装の `let _ = remove_dir_all(...)` は削除失敗を握り潰して、消せなかったディレクトリをそのまま再利用していました。`temp_dir()` が共有の `/tmp` になる環境（Linux。macOS は `TMPDIR` がユーザーごと）では、他ユーザーが先に `/tmp/token-burn` を作っておくと sticky bit により削除が失敗する一方 `create_dir_all` は成功するため、他人の所有ディレクトリへワーカースクリプトやプロンプトを書き込んでしまいます。
