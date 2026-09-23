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

Claude Code / Codex CLI のトークンは週次でリセットされますが、未使用分は繰り越されません。「もったいない」精神で、**token-burn** はリセット直前の残りトークンを有効活用します。コードレビュー、バグ修正、リファクタリング、テスト改善など、自由に定義したプロンプトをリポジトリ群に対して並列実行します。リセット時刻が来ると、新規タスクの開始を止め、実行中のタスクが完了するまで待機します。

<p align="center">
  <img src="docs/images/screenshot.png" width="800" alt="token-burn 実行中">
</p>

<p align="center">
  <img src="docs/images/deadline.png" width="800" alt="デッドライン到達 — タスク完了を待機中">
</p>

## 特徴

- **自動探索**: ディレクトリをスキャンしてGitリポジトリを検出、remote URLのユーザー名でフィルタ
- **複数スキャンソース**: GitHub用、GitLab用など、スキャン設定を複数定義可能
- **重複スキャン対策**: 複数スキャンソースで同じディレクトリが見つかっても1回だけ処理
- **可視性対応**: 公開リポジトリを優先的に処理（remote のリポジトリ名で照合）
- **マルチエージェント**: Claude Code、Codex CLI、カスタムエージェントに対応
- **スマートスケジューリング**: リセット期限が最も近いエージェントを自動選択
- **ai-usage 連携**: 外部ツール `ai-usage --json` から各エージェントの reset 時刻を実データ（`weekly.resets_at`）で取得し、曜日固定計算に頼らずスケジュールを自動更新（解決失敗時は曜日計算へフォールバック可能）
- **アカウント別展開**: 1 エージェントを複数プロファイル（例: `work` / `home`）へ展開し、それぞれ専用の env（`CLAUDE_CONFIG_DIR` 等）で起動。`state.json` のキーも `<agent>-<profile>` で分離されアカウントごとに処理済み状態を管理
- **アカウント横断の続きから実行**: `dedup_scope` で処理済み履歴を共有し、あるアカウントが処理したリポジトリの続きから別アカウントが実行。どのアカウントが処理したかの記録は残したまま、参照範囲だけを広げる（実行ごとに `--dedup-scope agent` で解除可能）
- **認証情報を伏せたコマンド表示**: ドライランの実行計画と ai-usage の起動エラーでは、環境変数代入と一般的な認証オプションの値を `<redacted>` に置き換えて表示し、実行時は元の値をそのまま使用。環境変数代入として伏せるのは実行ファイルより前に並ぶ `KEY=VALUE`（`env FOO=1 cmd` の前置き）だけで、`codex -c model='gpt-5.3-codex'` や自動注入の `-c approval_policy=never` のようなサブコマンドのオプション値は表示したままにする（伏せるとドライランで何が起動されるか確認できなくなるため）
- **デッドライン制御**: リセット時刻到達時に新規タスクの開始を止め、実行中タスクの完了を待機
- **対象選択 TUI** (`-i` / `--interactive`): 実行前に TUI を開き、処理するリポジトリと実行順をその場で決定。ワーカーはその順でキューを消化する。候補は `limit` で切らず全件表示し、先頭 `limit` 件が初期選択済みなのでそのまま Enter を押せば従来と同じ実行になる
- **並列実行**: tmuxペイン分割とプログレスモニター付きで複数プロンプトを同時実行
- **完了で自動終了**: 処理するタスクが尽きたワーカーは自分のペインを閉じ、全タスクの処理が済むとモニターが tmux セッションを閉じて終了（Ctrl-C 不要）。最終集計とログパスは起動元の端末に再表示
- **tmux デタッチ安全性**: デタッチ時はワーカースクリプトとキューを保持し、tmux セッション終了までバックグラウンドタスクを安全に継続
- **tmux 起動失敗時の後始末**: ペイン構築に失敗した場合、作成途中のセッションと一時実行ディレクトリを削除
- **Claude の無人実行**: Claude Code の `AskUserQuestion` ツールを自動禁止し、token-burn ジョブが対話回答待ちで停止しないようにする
- **サブエージェント監視**: Claude Codeのチーム/エージェントタスクの開始・進捗・状態更新・完了をリアルタイム表示。`task_started` は具体的な `subagent_type` を優先し、失敗通知は要約を併記、`task_updated` の `killed` は失敗として強調表示
- **サブエージェント結果集計**: `result.subagent_stats` から起動・完了・失敗・強制終了・起動拒否の件数、バックグラウンド/入れ子起動数、最大深度を表示。トップレベルが成功でも配下のサブエージェントが失敗した場合は警告表示
- **システム通知の可視化**: stop hook エラーなどの Claude Code システム通知に加え、`hook_progress` / `hook_response` に stderr や output が含まれる場合のフック診断も表示
- **フック差し戻しの可視化**: Stop フックが exit 2 / タイムアウトで終わると Claude Code はその内容を合成 user メッセージ（`isSynthetic`）としてモデルへ差し戻すが、これは `hook_response` にも `notification` にも現れない。第 1 行が `<Event> hook feedback:` のものを `⚠ Hook feedback (Stop): ⏱ Stop hook timed out after 120s: cargo` として表示する。無人実行では Stop フックが自動コミット / push を担うため、落とすと「仕事は終わったのにコミットされていない」理由をログから追えない。同じ合成メッセージでもスキル本文の注入（`Base directory for this skill: ...`）は SKILL.md 全文を含んで巨大なうえ `Skill` ツール行と重複するため非表示
- **長時間ツールの進捗表示**: `tool_progress` の経過時間を表示し、長時間実行中にモニターが停止したように見える状態を防止
- **拒否時モデル切り替えの可視化**: `model_refusal_fallback` の切り替え元・切り替え先モデルとカテゴリを表示し、イベント内の content / explanation は出力しない
- **ツール詳細の強化**: Claude の stream-json に含まれる `Read` の offset/limit/view range、パースできないツール入力（モデルの不正 JSON 出力、またはレート制限・切断による途中切れ）の文字数表示（`unparsed:<n> chars`）、`Edit` の一括置換状態、`Bash` の timeout/background/sandbox 無効化状態、`BashOutput` の対象 background bash id（`bash:<id>`）と任意の filter、`Agent` / `Task` の識別子と説明およびバックグラウンド状態、`Grep`/`Glob` の output mode・type・ignore-case・only-matching・multiline・glob・head/context/offset 制限、`ScheduleWakeup` の待機時間/理由、`WebFetch` の URL とプロンプト要約、`WebSearch` のクエリと include/exclude ドメイン件数、`ToolSearch` のクエリと `max_results`、`Monitor` の説明/タイムアウト/condition/persistent 状態、`TaskStop` の task id（複数指定含む）と理由、`TaskList` 呼び出し、`TaskGet` の task id、`TaskOutput` の task id / `block` / `timeout`、`Workflow` の起動対象（インライン script の `meta.name` から抽出したワークフロー名とスクリプト文字数、または名前指定ワークフロー / スクリプトパス）、`TaskCreate` の `subject` / `description` / `activeForm`、`TaskUpdate` の `taskId` / `status` / `owner` / `subject` / `description`、`SendMessage` の要約、`SlashCommand` の実行コマンド文字列、既存ログなどに含まれる `AskUserQuestion` の質問/選択肢、Tavily search の期間フィルタ（`start=2026-08-01` / `end=...`）とドメイン絞り込み（1 件なら `site=ast-grep.github.io`、複数なら `site=2 domains`、除外は `-site=...`）、Tavily/Codex MCP の model/sandbox/approval 詳細、Context7 MCP ツールの library/query を表示
- **サブエージェント停止の可視化**: `task_notification` の `status="stopped"`（`TaskStop` 等で停止された場合）もモニターに表示。`usage` が無い通知では duration/token を 0 として表示しない
- **ツールエラー要約**: `tool_result` の `is_error:true` を検出すると、エラー内容の先頭の有意な 1 行を 120 文字までに省略してモニターに併記（単一行/複数行の `<tool_use_error>` ラッパーは除去）。jsonl を開かずに失敗の原因が分かる
- **ツール結果メタデータ**: top-level `tool_use_result` に含まれる出力切り詰め、適用 limit、stale read ヒント、Edit/Write が書き込み前にユーザによる変更を検出した場合の `user-modified` マーカー、Edit が古い読み取り状態から自動回復した場合の `stale-recovered` マーカー、Claude Code がメモリ用ディレクトリへ印を付けた場合の `memdir-stamped` マーカー、失敗理由（`error:` / `message:`）、Bash 等の標準出力/標準エラー要約（`stdout:` / `stderr:`）、MCP/Codex の構造化応答要約（`structured:`）、文字列または text ブロック配列で返る MCP 成功結果の要約（`result:`）、Edit 結果のファイルパスと structured patch 規模（`file:<path>`、`patch:<hunks> ... +追加/-削除`、`replace_all`）、自動バックグラウンド化、待機時間の clamp、永続化出力サイズ、戻りコード解釈、Agent の duration/token/tool 数、`ListAgents` の一覧件数（`agents:<n>`）、サブエージェント種別（`agent:`）・解決モデル（`model:`）・サブエージェントの編集行数（`edits:+追加/-削除`）、非同期 Agent の識別子（`agent-id:`）と `SendMessage` で再開した Agent の識別子（`resumed-agent:`）、Grep/ToolSearch の結果件数と mode、WebSearch の結果件数/検索回数/所要時間、WebFetch の HTTP ステータスコードと応答サイズ（`http:200 OK`、`bytes:120.2KB`）、Read の部分読み取り行数（`lines:<n>/<total>`）またはオフセット付き範囲（`lines:<start>-<end>/<total>`）と token cap 切り詰め（`truncated:token-cap`）、git commit 操作（sha/kind）、タスク件数/task id/task type、TaskOutput の取得状態、読み取り可能な Agent 出力ファイル、Monitor の timeout/persistent 状態、TaskUpdate の状態遷移と status 以外の変更フィールド（`updated:<field1>,<field2>`）、async Agent 起動（`run_in_background=true` 時に `async`）、ScheduleWakeup の予定時刻、Skill のコマンド名と許可ツール件数（`allowed-tools:<n>`）、起動したワークフロー名（`workflow:<name>`）などの重要情報を表示
- **セッションヘッダー**: `init` イベントからモデル・Claude Code バージョン・権限モードを 1 行で表示（`ℹ Session <model> (v<version>, <permissionMode>)`）。これらはストリーム中の他のイベントには現れず、`result.modelUsage` からは実際に課金されたモデルしか分からないため、CLI バージョンと `bypassPermissions` で実行したかどうかが失われていた
- **実測したバックグラウンドメタデータ**: バックグラウンド移行時の待機期限を `wait-timeout:<期間>`、作業ディレクトリの注意を `cwd-hint:<要約>`、権限ルールによる未実行を `not-executed:permission-rule` として表示
- **実 stream-json の境界形式**: assistant レベルのモデル切り替え（`from.model` → `to.model`）と、キャッシュミス理由・対象 input token 数を表示し、partial message の繰り返しは message id 単位で重複抑止。モデル関連フィールドで実測した壊れた末尾 SGR 断片（例: `claude-opus-5[1m]`）は、セッションヘッダー・fallback・Agent メタデータ・モデル別使用量の全経路で `claude-opus-5` へ正規化。タスクイベントと重複する高頻度の `background_tasks_changed` スナップショットは非表示にし、表示対象の system / rate-limit 通知が本文・思考 delta の途中へ到着しても、単語や思考行へ連結せず独立した行に表示（無視対象イベントでは改行を増やさない）。JSON として解釈できない行（`2>&1` で合流した stderr の `API Error: ...` など）も同じ扱いで、開いている思考・テキスト行を閉じてから独立行へ出す。Agent 起動時の任意 `model` / `isolation`、`isImage:true` の `image` マーカーを表示。`structuredPatch[].lines` は `+` / `-` で始まる全行を数え、内容自体が `++` / `--` で始まる追加・削除行も取りこぼさない
- **ログパイプラインの安全性**: `format-stream`、`tee`、raw jsonl 保存に失敗したタスクを完了扱いにせず失敗として記録。スキャンから実行までの間に対象ディレクトリが削除・リネームされた場合は、無関係な「ログパイプラインの失敗」ではなく `target directory is unavailable` として正確に記録
- **モデル別使用量**: 結果サマリーにモデルごと（Opus、Haiku等）のトークン使用量・コスト・キャッシュ読み取り/書き込み・Web検索回数、そして各モデルのコンテキスト上限/最大出力上限（例: `ctx:1M` / `max_out:64K`）を表示
- **VCS 状態変更**: git hook による自動 commit / push（`vcs_state_changed`）を `⎇ VCS push (main)` のようにブランチ付きで表示。無人実行中にコミットや push が作られた事実はセッション後の変更追跡の起点で、`main` へ push したのか作業ブランチへ push したのかで影響範囲が全く違う。一方で 1 件数百 KB に達する `commands_changed`（スキル/コマンド一覧のスナップショット）は実行内容と無関係なため非表示
- **サブエージェント込みの総消費量**: `result.usage` はメインループの消費しか含まないため、`modelUsage` の合計が上回るときだけ `📊 total in:<n> out:<n> (thinking:<n>, incl. subagents)` を併記。実ログでは cache_read が `2,110,689` → `220,321,325` と 100 倍以上乖離しており、見出しの `in/out` だけでは実際の消費量を桁違いに過小評価する。併記する思考トークンは `modelUsage[].thinkingTokens` の合計で、実ログではメインループ分 89,291 に対し 1,219,128（総出力の 66%）だった。落とすとサブエージェントへ消えたトークンの最大の内訳が見えなくなる。サブエージェント未使用のセッションでは両者が完全一致するため表示しない
- **ツール完了行のサブエージェント帰属**: サブエージェント内で走ったツールの結果はメインループの出力と同じストリームへ混ざり、実ログでは完了行の 69%（2,309 / 3,350 件）がサブエージェント由来だった。ツール名の直後に ` @<タスク名>`（Agent 起動時の description）を添えるため、22 個が並列で動いていても `✓ Bash` の羅列から誰の作業かを追える
- **サブエージェント内部のツール使用とテキスト出力**: `stream_event` はメインループ専用（実データ 98,061 件すべてが `parent_tool_use_id: null`）で、サブエージェントが何のコマンドを打ったかは `assistant` イベントにしか現れない。これを拾わないとツール完了行の 44%（1,178 / 2,695 件）が `✓ Bash @<タスク名>` だけになる。`🔧 <ツール名> @<タスク名> <詳細>` としてメインループと同じ形で表示し、サブエージェントの最終レポート（実データ 179 ブロック / 363KB）も `💬 @<タスク名> <先頭 1 行>` に畳んで出す
- **入れ子起動の深さ**: サブエージェントがさらにサブエージェントを起動した場合、`task_started` の行に `depth:<n>` を添える。`result.subagent_stats.max_depth` は最終集計しか持たないため、これが無いと「どのタスクが深いのか」を追えない。入れ子は実行時間とトークン消費が指数的に膨らむ要因そのもの。`spawn_depth` を持たないサブエージェント発の background Bash（実データ 84 件）は `owned_by_subagent` から `nested` として印を付ける
- **進捗行の累積トークン**: `task_progress` に `tokens:<n>`（`usage.total_tokens`）を併記。実データ 1,254 件すべてに入り 10 万トークン級が常態で、`result.usage` はメインループ分しか持たないため、これが無いとどのサブエージェントが枠を食っているかを実行中に読み取れない
- **サブエージェント種別の内訳**: `result.subagent_stats.by_type` から `[Explore:5 general-purpose:4 codex:2]` を併記。`codex` が 2 体なのか `Explore` が 5 体なのかでコストの意味が全く違うため、件数だけの `spawned:12` では読み取れない
- **Bash 経由のファイル変更**: `sed -i` / `cargo fmt` / `depup` など Edit/Write を通らない書き換えは `filePath` も `structuredPatch` も出ず痕跡が残らない。`tool_use_result.bashEditDiff` から `bash-edits:<path> +追加/-削除`（複数なら件数）を表示する（実データでは Bash 結果 102 件が該当し、うち 36 件が実変更）
- **セッション失敗の原因**: `result.is_error` が true なら `error (HTTP 429): <本文>` を表示。実データでは 47 ドル・30 分を消費したセッションが `subtype:"success"` のまま支出上限エラーで終わっており、フッターに出ていたのは `terminal api_error` の 1 行だけで、読み落とすと正常完了に見えた
- **思考進捗ドット**: Claude Code は思考本文を伏せるため `thinking_delta` の本文は空文字で届き、進捗は `estimated_tokens`（増分）にだけ入る。50 トークンごとにドットを 1 つ出す。本文のバイト長だけを見ていた頃は実データ 7,516 件すべてでドットが 0 個になり、中身のない `💭 ` 行だけが並んでいた
- **思考トークンの内訳**: `output_tokens_details.thinking_tokens` を `📊 in:<n> out:<n> (thinking:<n>)` として表示。実ログでは出力トークンの 10〜52% を思考が占めるため、内訳が無いと何にトークンを使ったのか分からない
- **API応答時間**: 実行時間に加えてAPI応答時間・初回トークン到達時間（`ttft`）・初回ストリームトークン到達時間（`stream:`。キュー/リトライ待ちを除いた純粋なストリーム遅延）・リクエスト送信までの所要時間（`req:<n>ms`）を表示
- **fast mode 表示**: fast mode が有効な場合は状態を表示し、利用できない理由が返された場合は `fast_mode_disabled_reason` も表示
- **terminal_reason / permission_denials**: 異常終了時の `terminal_reason`（`completed` 以外）と権限拒否されたツール呼び出しの件数/ツール名を結果サマリーに表示
- **結果メタデータ**: `usage.service_tier`、`usage.speed`、空でない推論リージョン、iteration 数、result origin 種別を表示
- **レート制限通知**: 使用率の警告、リクエスト拒否、overage（超過枠）の状態・リセット時刻（警告時と拒否時にも表示）、および `allowed_warning` でサーバー側が通過した警告閾値（例: `warning at 90%`）を表示し、ローカル閾値超過時に後続タスクを停止（7 日枠なら恒久停止、5 時間枠ならその枠のリセットまでの一時停止。stop file の作成が ENOSPC・権限不足等で失敗した場合は握り潰さず、停止シグナルが生成されない旨を出力に明示）
- **自動停止は実行を止める枠だけで判定**: 停止判定に使うのは `unifiedWindows` の 5 時間枠 / 7 日枠の使用率で、月次の追加課金枠（`overage`）の使用率では停止しません。実データでは 5 時間枠が 13% でも `rateLimitType:"overage"` / `utilization:1.03` の警告が届き、これを閾値と比べて全タスクが止まっていました。停止行には判定した枠の名前・使用率・**その枠自身の**リセット時刻を出し、`[5h 13% / 7d 54%]` の形で実測値を併記します。追加課金枠の警告は `(overage, no auto-stop)` として表示のみ行います
- **5 時間枠の枯渇では実行全体を捨てない**: 停止は枠の周期で 2 種類に分かれます。週次枠（デッドラインと同じ周期）が閾値に触れたら恒久停止ですが、5 時間枠のように待てば回復する枠なら**その枠のリセットまでの一時停止**にとどめ、時刻を過ぎたワーカーは残りのタスクを続行します。以前は 5 時間枠が閾値に触れただけで週次リセットまでの実行余力を丸ごと捨てており、実ログでは停止の数分後にその枠がリセットされたのに残りのタスクが 1 件も実行されないまま終了していました（週次枠は 43%、デッドラインまで 4 時間 52 分残っていた）。待つとデッドラインを越える場合、リセット時刻が読めない場合は従来どおり恒久停止します。一時停止から再開する直前には実データで使用率を確かめ直すため、待っている間に状況が変わっていれば待ち直します
- **停止判定とタスク取得は不可分**: ワーカーは「停止していないか確かめる」と「キューからタスクを取る」を 1 つのロックの下で行います。2 段に分かれていた頃は、その隙間に別のワーカーが停止を発行すると、停止後にタスクが開始されていました
- **ai-usage 使用率ゲート**: ai-usage 連携時、各タスク完了後に該当 agent の実使用率（weekly / five_hour）を枠ごとに確認し、`rate_limit_threshold` 以上なら後続タスクを停止または一時停止。stream-json のリアルタイム監視が無い codex でも実使用率で確実に止まり、取得失敗時は fail-closed で安全側に倒す
- **モニター使用量パネル**: ai-usage 連携時、tmux モニターペインに `ai-usage --statusline --logos`（各アカウントの 5h / 週次使用率バー）を 10 秒ごとに表示（`--input` でキャッシュから高速描画、進捗バーは毎秒更新）。取得は `ai-usage` の終了と同時に返るため、更新処理でペインが固まることはなく、進捗バーは毎秒更新を維持
- **上限到達の判定**: `resets 2:30am` のような分を含む時刻表記や、`You've hit your session limit` / `You've hit your org's monthly spend limit` といった上限到達メッセージを、リトライ可能なプロバイダエラーではなくレート制限として扱う（リトライしても回復しないため）
- **リセット時刻の日付表示**: リセットが翌日以降になる場合は `MM/DD HH:MM` 形式で日付も表示（`seven_day` 枠や超過枠は最大 1 か月先になるため、時刻だけだと当日中に回復するように誤読される）
- **接続断の再試行扱い**: `API Error: Connection closed mid-response` のように HTTP ステータスを伴わない一時的な接続障害は恒久エラーではなく再試行可能として扱い、ワーカーを止めずに次のターゲットへ進む（次回実行で再処理される）
- **サブエージェント失敗理由**: サブエージェントが失敗・強制終了したとき、その原因（API エラー等）を完了通知に併記
- **APIリトライ表示**: 一時的な障害時のリトライ試行回数とエラー情報を表示
- **ログ衝突回避**: タスクごとのログに連番を付け、同名リポジトリでも上書きしない
- **プロンプトファイル**: `.md` ファイルまたはインライン文字列でプロンプトを指定可能
- **処理済みターゲットのスキップ**: 処理済みディレクトリを自動スキップ、スキップ期間を設定可能
- **中断セッションの再開**: レート制限（`You've hit your session limit · resets 2:30pm`）で切られた Claude Code のタスクを最初からやり直さない。そのセッション ID を `resume.json` に保存し、次回の実行で同じリポジトリを同じエージェントが選んだときに `claude --resume` と継続プロンプトで続きから再開するため、同じコマンドをもう一度打つだけで作業が続く。再開できるターゲットは先頭へ寄せ、5 時間枠のリセット前に打ち直した場合はリセットまで待ってから始め、Claude Code が既に削除したセッションは同じタスクの中で新しいセッションに切り替える（[詳細](#中断セッションの再開)）
- **状態更新の競合対策**: 並列ワーカーが安定した sidecar lock file の下で `state.json` を atomic rename 更新。既存状態が壊れている場合や読み取れない場合は上書きせず中断し、処理済み履歴を保全
- **ドライラン**: コマンドを実行せずに実行計画をプレビュー

## 動作環境

- **OS**: macOS
- **tmux**: ペイン分割実行に必要
- **Rust**: 1.88以上（ソースからビルドする場合）
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

# 処理する順番のターゲットディレクトリを一覧表示（`--limit` 無視）
token-burn list

# 特定のリポジトリだけを強制実行
token-burn run ~/GitHub/repo-a ./repo-b

# トークン消費を実行
token-burn run
```

### コマンド

| コマンド | 説明 |
|---------|------|
| `run` | トークン消費を実行（デフォルト） |
| `list` | 処理する順番でターゲットディレクトリを一覧表示（`--limit` 無視、実行しない） |
| `status` | エージェントのリセット状況を表示 |
| `init` | 設定ファイルとプロンプトテンプレートを生成 |
| `clean` | 古いレポートディレクトリを削除 |

### オプション

| オプション | 短縮形 | 説明 |
|-----------|-------|------|
| `--config <PATH>` | `-c` | 設定ファイルパス（デフォルト: `~/.config/token-burn/config.toml`） |
| `--agent <NAME>` | | エージェントを強制指定 |
| `--dry-run` | `-n` | 実行せずにプレビュー |
| `--fresh` | | 保存済み状態（処理済み履歴と中断セッションの両方）を無視して全ターゲットを最初から処理 |
| `--no-resume` | | 保存済みの中断セッションを使わず、全ターゲットを新しいセッションで開始（処理済みのスキップは有効なまま） |
| `--limit <N>` | `-l` | 処理するターゲット数の上限（`N >= 1`） |
| `--no-limit` | | 上限なしですべてのターゲットを処理 |
| `--workers <N>` | `-w` | 並列実行するワーカー数（`N >= 1`、`parallelism` を上書き） |
| `--interactive` | `-i` | 実行前に TUI で対象と実行順を選ぶ（`run` のみ。TTY が必要） |
| `--public-only` | | 公開リポジトリとして判定されたもののみ処理 |
| `--dedup-scope <SCOPE>` | | 処理済み履歴の共有範囲: `global` / `provider` / `agent`（`dedup_scope` を上書き） |
| `--help` | `-h` | ヘルプ表示 |
| `--version` | `-V` | バージョン表示 |

`--no-resume` はその実行に限り[保存済みの中断セッション](#中断セッションの再開)を使いません（処理済みターゲットのスキップはそのまま効きます）。保存済みのセッションがあるターゲットは新しいセッションで始まり、その新しいセッションを起動する直前に保存済みのセッションを破棄します（その時点から、古いセッションの文脈はリポジトリの実態より古くなるため）。実行中に新しく中断したセッションは保存されます。`--fresh` はさらに踏み込み、処理済み履歴と保存済みセッションの両方を無視します。

`--dedup-scope` はその実行に限り設定値の [`dedup_scope`](#エージェント間での処理済み履歴の共有) を上書きします。`--dedup-scope agent` を指定すると共有を解除でき、別アカウントが処理済みのリポジトリをこのアカウントでも改めて処理できます。

`--workers` はその実行に限り設定値の `parallelism` を上書きします。実際に起動するワーカー数はタスク数で頭打ちになり、実効値は実行計画に `Workers:` として表示されます（`--dry-run` でも確認できます）。

`--interactive` は実行前に対象選択画面を開きます。候補は先頭 `limit` 件だけでなく全件が並び、先頭 `limit` 件が初期選択済みなので、そのまま Enter を押せば非対話実行と同じ対象になります。キー操作は `↑↓` / `j` `k` で移動、`Space` で選択トグル、`J` / `K`（`Shift+↑↓` も可）で行を上下に動かして順番を変更、`a` / `n` で全選択・全解除、`g` / `G` で先頭・末尾、`Enter` で実行、`q` / `Esc` でキャンセルです。選択済みの行に付く番号が、ワーカーが処理する順番になります。中断セッションを再開する行には `↻` が付きます。実端末が必要なため、標準入出力をリダイレクトするとエラーになります。`--dry-run` と併用すると実行せずに計画だけ確認できます。

`init` は `--force`（`-f`）で既存ファイルを確認なしで上書きできます。

`clean` は `--older-than` で `cleanup_after` の設定値を一時的に変更できます（例: `--older-than 3d`）。

`run` に `PATH` を1つ以上渡した場合、そのディレクトリ群に対して強制実行され、スキャン結果と状態ベースのスキップは使いません。`repo` と `./repo` のような等価なパスは正規化後に重複排除されるため、1回の実行で同じディレクトリが二重実行されることはありません。保存済みの中断セッションは `PATH` 指定時も使われるため、レート制限の後に同じ `token-burn run PATH...` を打ち直せば、中断したセッションの続きから再開します。

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
| `parallelism` | 並列実行数（`>= 1`、実行ごとに `--workers` で上書き可能） | `3` |
| `skip_within` | この期間以内に処理済みならスキップ | `"7d"`, `"24h"`, `"1d12h"` |
| `cleanup_after` | この期間より古いレポートディレクトリを自動削除 | `"7d"`（デフォルト） |
| `report_dir` | 実行ログの保存先ディレクトリ（相対パスは実行時のカレントディレクトリ基準で絶対パスへ解決） | `~/Documents/token-burn`（デフォルト） |
| `limit` | 1回の実行で処理する最大ターゲット数（`>= 1`） | `10`（デフォルト） |
| `rate_limit_threshold` | 5 時間枠 / 7 日枠の使用率がこの閾値（%）以上で後続タスクを止める（`1-100`）。7 日枠なら恒久停止、5 時間枠ならその枠のリセットまでの一時停止。月次の追加課金枠（overage）の使用率では止まらない。Claude の stream-json リアルタイム監視に加え、ai-usage 連携時は各タスク完了後にも該当 agent の実使用率（weekly / five_hour）でチェックされる | `95`（デフォルト） |
| `dedup_scope` | 処理済み履歴を共有する範囲（`global` / `provider` / `agent`） | `agent`（デフォルト） |
| `resume_interrupted` | レート制限で切られた Claude Code のセッションを保存し、次回の実行で最初からやり直さず続きから再開する。`false` にすると中断セッションの保存も再開も行わない。[中断セッションの再開](#中断セッションの再開)を参照 | `true`（デフォルト） |

`skip_within` と `cleanup_after` には、`d`（日）、`h`（時間）、`m`（分）、`s`（秒）を使った期間文字列を指定します。不正な値や期間として表現できない値は設定ファイルの読み込み時点でエラーになります。`skip_within` を省略した場合は前回リセット以降に処理済みのターゲットをスキップします。期間として表現できても日時の計算範囲を超える値ではパニックせず、`skip_within` は警告後に前回リセット時刻へフォールバックし、クリーンアップはエラーを返します。`--fresh` を指定すると保存済み状態（処理済み履歴と中断セッションの両方）を無視して全ターゲットを処理します。

状態ファイル: `<config-dir>/state.json`（有効な設定ファイルと同じディレクトリ）。更新時は同一ディレクトリのテンポラリファイルへ書き出してから `rename` で atomic に差し替え、`.state.json.lock` のような安定した sidecar lock file で並列ワーカーを直列化します。既存ファイルが不正な JSON の場合や権限・I/O エラーで読み取れない場合は、空状態として置き換えず更新を失敗させ、復旧に必要な原本と処理済み履歴を保全します。各エージェント内のエントリは最終処理時刻の降順（同時刻はパス昇順）で書き出されるため、最新の処理がファイルの先頭に来ます。デフォルト設定パスの場合は `~/.config/token-burn/state.json`。レート制限で中断したセッションは、同じディレクトリの別ファイル `resume.json` に保存されます（[中断セッションの再開](#中断セッションの再開)を参照）。

#### エージェント間での処理済み履歴の共有

`state.json` は展開エージェント名ごとに履歴を記録するため、既定では 1 つのアカウントで処理したリポジトリも他のアカウントからは未処理のままです。同じ CLI を 2 アカウントで回すと、2 つ目のアカウントは 1 つ目の続きからではなく同じリポジトリの先頭から始まります。`dedup_scope` はスキップ判定時にどこまでの履歴を参照するかを決めます。

| 値 | スキップ判定で参照する履歴 |
|----|--------------------------|
| `global` | 全エージェント。`state.json` にしか存在しない名前（改名・削除済みのエージェント）も含む。あるアカウントの続きから別アカウントが処理する |
| `provider` | 同じ `provider` のエージェント同士（例: `codex` 系アカウント同士は共有するが `claude` とは共有しない）。`provider` 未設定のエージェント（空文字・空白のみも未設定として扱う）と、設定に無い名前は自分自身の履歴のみ参照する |
| `agent` | 実行中のエージェントのみ（デフォルト、従来の挙動） |

書き込み側は変わりません。完了は常に実際に実行したエージェント名で記録されるため、`state.json` にはアカウントごとの履歴がそのまま残り、スキーマも変わりません。広がるのは参照側だけです。

`global` / `provider` は `skip_within` が必須です。`skip_within` 省略時のカットオフは「実行中のエージェントの前回リセット時刻」でエージェント固有のため、他エージェントの履歴に適用するとスキップ範囲が「どのエージェントで起動したか」次第で揺れてしまいます。共有 scope なのに `skip_within` が無い設定は読み込み時にエラーになります。

`--dedup-scope <global|provider|agent>` でその実行だけ設定値を上書きできます。別アカウントが処理済みのリポジトリを意図的にもう一度回したいときは `--dedup-scope agent` を指定します。スキップ時は scope・窓・どのエージェントの記録で弾いたかを併記します。

```
  Skipped: 8 targets (already processed; scope: global, window: 2d)
    by agent: codex=5, codex-alt=2, claude=1
```

### エージェント

```toml
[[agents]]
name = "claude"
command = ["claude", "--dangerously-skip-permissions", "--model", "opus"]
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
| `provider` | プロバイダ識別子。`ai-usage` の `(profile, provider)` 照合に使用（ai-usage 連携時は必須） | `"claude"` |
| `command` | コマンドと引数 | `["claude"]` |
| `env` | 起動時に付与する環境変数（任意）。プロファイル側の `env` で上書きマージされる | `{ FOO = "bar" }` |
| `reset_weekday` | リセット曜日 | `"monday"` |
| `reset_time` | リセット時刻（HH:MM） | `"09:00"` |
| `timezone` | IANAタイムゾーン | `"Asia/Tokyo"` |
| `prompt` | エージェント固有プロンプト（任意） | `"prompts/test-coverage.md"` |

`name` は空文字不可で、profile 展開後の名前が全体で一意である必要があります。展開名は `state.json` のキー・レポートディレクトリ名・`--agent` の指定子を兼ねるため、重複すると 2 件目のエージェントを選べなくなり、2 つのエージェントの処理済み履歴が黙って混ざります。`command` は1要素以上を指定し、先頭要素には空でない実行ファイル名を指定してください。`prompt` を指定するとグローバルの `[prompts].default` の代わりに使われます。ターゲット固有の `prompt` が最優先です。

`reset_weekday` / `reset_time` / `timezone` は通常は必須ですが、ai-usage 連携（[ai-usage 連携](#ai-usage-連携任意)を参照）を有効化しており、かつ `fallback` が `fixed` 以外の場合のみ `reset_weekday` を省略できます。省略時は ai-usage が解決できなかったときの曜日計算フォールバックが利用できなくなる点に注意してください。`env` のキーは `[A-Za-z_][A-Za-z0-9_]*` に制限され、値は `~`（ホームディレクトリ）が展開されます。

**プロンプト優先順位**: `[[targets]].prompt` > `[[agents]].prompt` > `[prompts].default`

**Claude 必須フラグの自動付与**: コマンドの実行ファイルが `claude` の場合、ログ出力と進捗モニタリングに必要な `-p`、`--verbose`、`--output-format stream-json`、`--include-partial-messages` と、対話回答待ちを防ぐ `--disallowedTools=AskUserQuestion` が必ず有効化されます。未指定フラグは自動追加され、既存の `--output-format` 値（`--output-format=...` 形式を含む）は `stream-json` に正規化されます。既存の `--disallowedTools` / `--disallowed-tools` がある場合は、必要に応じて equals 形式へ正規化して `AskUserQuestion` を追記します。設定ファイルへの記述は不要です。

**Claude 環境変数の自動付与**: Claude プロセスの環境にはデフォルトで `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0` が追加されます。これが無いと `claude -p` はメインターン終了後にバックグラウンドタスク（background 起動のサブエージェント / ワークフロー）を最大 600 秒しか待たず、"Background tasks still running after 600s; terminating." と共に全タスクを強制終了し、仕事が未完のまま成功として報告されます。`0` は無期限待機を意味し、バックグラウンドエージェントの完了通知でメインループが再開して完走できるようになります。agent / profile の `env` で明示すれば上書きできます（空文字を指定すると unset され、Claude 既定の 600 秒に戻ります）。

`reset_weekday` に指定可能な値: `monday` `tuesday` `wednesday` `thursday` `friday` `saturday` `sunday`（短縮形: `mon` `tue` `wed` `thu` `fri` `sat` `sun`）

### ai-usage 連携（任意）

外部ツール `ai-usage --json` と連携すると、各エージェントの reset 時刻を実データ（`weekly.resets_at`）から自動取得できます。従来は `reset_weekday` / `reset_time` / `timezone` から固定計算していましたが、ai-usage 連携を有効化すると実際の利用状況に基づいた reset 時刻が使われます（固定計算は解決失敗時のフォールバックとして引き続き機能します）。

連携が無い、または `enabled = false` の場合は従来どおり曜日計算のみで動作します。

```toml
[ai_usage]                 # 任意。無い or enabled=false なら従来の曜日計算のみ
enabled = true
command = ["ai-usage", "--json"]   # デフォルト
window = "weekly"          # weekly | five_hour | nearest（deadline 算出枠、デフォルト weekly）
fallback = "fixed"         # fixed | skip | error（解決失敗時、デフォルト fixed）
state_window = "weekly"    # weekly | selected（処理済みカットオフ枠、デフォルト weekly）

[[ai_usage.profiles]]
name = "work"              # 内部参照名（展開名 <agent>-<name> に使う）
profile = "Work"           # ai-usage --json の "profile" と照合（大文字小文字を区別）
env = { CLAUDE_CONFIG_DIR = "~/.config/claude-work" }  # そのアカウントでの起動時 env（~ 展開される）

[[ai_usage.profiles]]
name = "home"
profile = "Home"
env = { CLAUDE_CONFIG_DIR = "~/.config/claude-home" }

[[agents]]
name = "claude"
provider = "claude"        # ai-usage の (profile, provider) 照合に使用。ai_usage 連携時は必須
command = ["claude"]
# env = { ... }            # 任意。profile.env で上書きマージされる
reset_weekday = "monday"   # ai_usage 連携かつ fallback != fixed のときは省略可。それ以外は必須
reset_time = "09:00"
timezone = "Asia/Tokyo"
[agents.ai_usage]
profiles = ["work", "home"]    # 参照する profile 名。複数指定でアカウント別に展開
# window = "weekly"            # 任意: グローバル設定の上書き
# fallback = "fixed"           # 任意: グローバル設定の上書き
```

#### `[ai_usage]`（グローバル設定）

| フィールド | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | ai-usage 連携を有効化する。無い or `false` なら従来の曜日計算のみ | `false` |
| `command` | 実行する ai-usage コマンドと引数 | `["ai-usage", "--json"]` |
| `window` | deadline 算出に使う枠。`weekly` / `five_hour` / `nearest` | `"weekly"` |
| `fallback` | 解決失敗時の挙動。`fixed`（曜日計算へフォールバック）/ `skip`（候補から除外）/ `error`（停止） | `"fixed"` |
| `state_window` | 処理済みカットオフの算出枠。`weekly` / `selected` | `"weekly"` |

#### `[[ai_usage.profiles]]`（プロファイル定義）

| フィールド | 説明 | 例 |
|-----------|------|-----|
| `name` | 内部参照名。エージェントの展開名 `<agent>-<name>` に使われる | `"work"` |
| `profile` | `ai-usage --json` の `"profile"` と照合する名前（大文字小文字を区別） | `"Work"` |
| `env` | そのアカウントでの起動時 env（任意）。キーは `[A-Za-z_][A-Za-z0-9_]*`、値は `~` 展開される | `{ CLAUDE_CONFIG_DIR = "~/.config/claude-work" }` |

#### `[agents.ai_usage]`（エージェント側の連携設定）

| フィールド | 説明 | 例 |
|-----------|------|-----|
| `profiles` | 参照する profile 名のリスト。複数指定でアカウント別に展開 | `["work", "home"]` |
| `window` | グローバル `window` の上書き（任意） | `"weekly"` |
| `fallback` | グローバル `fallback` の上書き（任意） | `"fixed"` |

#### 挙動

- 実行時に agent × profile を展開します。例: `claude` + `["work", "home"]` → `claude-work` / `claude-home` の 2 エージェント。各々プロファイルの `env` を付与して起動します。**profile を 1 つだけ参照する場合は展開名が agent 名のまま**（例: `codex` が `["home"]` のみ → `codex`）で、サフィックス `<agent>-<profile>` が付くのは 2 つ以上参照したときだけです。起動コマンドが異なる各アカウントを別 agent として定義しても展開名が冗長にならず、`state.json` キーも安定します。
- 展開名は `state.json` のキーにも使われ、アカウントごとに処理済み状態が分離されます。
- `ai-usage --json` は 1 プロセスにつき 1 回だけ実行されます。
- **使用率ゲート（完了後チェック）**: ai-usage 連携が有効な場合、各タスク完了後に該当 agent の `(profile, provider)` の weekly / five_hour の `used_percent` を ai-usage から取得し、`rate_limit_threshold` 以上の枠があれば後続タスクの開始を止めます。Claude の stream-json `rate_limit_event` によるリアルタイム監視（タスク実行中の停止）に加えてこの完了後チェックが効くため、**従来リアルタイム監視が無かった codex でも実使用率で確実に止まります**（claude / codex 両方に適用）。
  - 止め方は枠の周期で分かれます。週次枠なら恒久停止、5 時間枠（や `kind:"daily"` の 24 時間枠）ならその枠のリセットまでの一時停止です。周期はスロット名ではなく `kind` から導くため、`five_hour` スロットに 24 時間枠を返すプロバイダでも取り違えません。
  - 完了後チェック用の ai-usage 出力は短い TTL（20 秒）でキャッシュされ、並列ワーカーからの重複取得を抑えます。
  - 取得失敗時、および該当アカウントが `ok:false`（認証切れ等で ai-usage がエラー報告）のときは fail-closed（使用率を確認できないため安全側で停止）。該当エントリが無い、または `ok:true` かつ `used_percent` が欠損している場合は過剰停止を避けて続行します。
  - stop file の作成は冪等で、並列ワーカーから同時に呼ばれても安全です。一時停止は排他ロックの下で更新され、既存より再開時刻が遅い場合だけ書き換わります。
- reset 時刻は ai-usage が選択した枠（`weekly` 等）の `resets_at` から取得します。
- `resets_at` が表す瞬間は保持したまま、`status` / `run` 表示では実行環境のローカル固定オフセットへ変換します。ai-usage が UTC で返した時刻もユーザーのローカル時刻として確認できます。
- 解決に失敗した場合（ai-usage コマンドが無い／失敗、該当する `(profile, provider)` が無い、`ok:false`、該当枠が `null`）は `fallback` に従います。
  - `fixed`: 曜日計算に戻ります（source 表示は `fixed fallback: <理由>`）。
  - `skip`: そのエージェントを候補から除外します。
  - `error`: 停止します。
- `status` / `run` は各エージェントのスケジュールの **source**（`ai-usage (weekly)` / `fixed` / `fixed fallback`）を表示します（静かにフォールバックしません）。
- `env` のキーは `[A-Za-z_][A-Za-z0-9_]*` に制限されます。値は `~`（ホームディレクトリ）が展開されます。

### 自動スキャン（複数定義可）

```toml
[[scan]]
base_dirs = ["~/GitHub"]
username = "yourname"
public_first = true
exclude = ["archived-project"]

[[scan]]
base_dirs = ["~/git"]
username = "yourname"
recursive = true
public_first = false
```

| フィールド | 説明 | デフォルト |
|-----------|------|-----------|
| `base_dirs` | Gitリポジトリを探索するディレクトリ | （必須） |
| `username` | remote URLのオーナーがこのユーザー名と一致するリポジトリのみ対象にする | （なし — 全リポジトリ対象） |
| `public_first` | 実行順で公開リポジトリを非公開リポジトリより前にグループ化する。**いずれか 1 つ**の `[[scan]]` で有効なら適用され、全 scan が `false`（または `[[scan]]` が無い）場合は可視性が順序に影響しない | `true` |
| `recursive` | サブディレクトリを再帰的に探索してネストされたGitリポジトリを検出する | `false` |
| `exclude` | スキャン時にスキップするディレクトリ名 | `[]` |

`username` を指定した場合、可視性判定は各リポジトリの `origin` remote URL から取得したリポジトリ名（大文字小文字を無視）で行われます。ローカルのディレクトリ名は一致している必要がありません。

remote URL の owner/repo は末尾 2 セグメントから抽出するため、GitLab のサブグループ（例: `git@gitlab.example.com:group/subgroup/repo.git`）でも直近の親 (`subgroup`) を owner として正しく扱います。

`username` を指定しない通常スキャンでは、`origin` remote がないリポジトリも対象に含まれます。その場合の可視性は `Unknown` になります。

ディレクトリスキャン時にシンボリックリンクはスキップされます（循環リンクによる無限再帰を防止）。

読み取れないディレクトリ（例: 権限の無いサブディレクトリ）は、警告を出してスキップし走査を継続します。存在しない `base_dirs` やシンボリックリンクと同じ扱いです。読めないサブディレクトリが 1 つあるだけで、リポジトリを 1 件も処理しないまま `run` / `list` が中断することはありません。

複数の `[[scan]]` エントリで同じリポジトリディレクトリが検出された場合は、ディレクトリパス単位で重複排除されるため、1回の実行で同じリポジトリが二重実行されることはありません。

ディレクトリパスは重複排除と状態管理の前に絶対パスへ正規化されるため、`repo` と `./repo` のような等価な相対パスは同一ターゲットとして扱われます。

この正規化と重複排除は、`token-burn run PATH...` で特定ディレクトリを強制実行する場合にも適用されます。

### プロンプト

`.md` で終わる値はファイルパスとして読み込まれます。相対パスは設定ファイルのディレクトリから解決されます。

```toml
[prompts]
default = "prompts/default.md"
# resume = "prompts/resume.md"   # 任意: 中断セッションを再開するときに送る継続プロンプト
```

`resume`（任意）は[中断セッションを再開](#中断セッションの再開)するときに送る継続プロンプトです。`default` と同じ規則で解決され、省略時は組み込みの英語プロンプトが使われます。再開時に送るのはこのプロンプトだけです（元の指示はセッションの履歴に残っているため）。

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

### 処理順

ターゲットは **最終ファイル変更日時が古い順** に処理されます。いちばん長く触られていないリポジトリが先頭に来ます。`defer` の優先度はそのまま維持され、可視性による公開リポジトリ優先のグループ化は **いずれか 1 つの `[[scan]]` で `public_first = true` が指定されている場合のみ** 適用されます。並べ替えはそのグループの内側だけで行われ、安定ソートのため、変更日時が同じターゲット同士の順序は元のままです。変更日時を取得できなかったリポジトリは各グループの末尾に置かれます。[中断セッションを再開](#中断セッションの再開)するターゲットは、グループの内側で変更日時の順より前に置かれます。`token-burn run PATH...` で明示指定した場合はコマンドライン指定順が優先されます。

すべての `[[scan]]` が `public_first = false` の場合（または `[[scan]]` が 1 つも無い場合）、可視性はソートキーから完全に外れ、順序は `defer` と変更日時だけで決まります。これは `limit` との併用時に効いてきます。可視性のグループ化が有効な間は、キューに公開リポジトリが `limit` 件以上残っているかぎり非公開リポジトリには到達しません。

これが無いと処理順が固定になり、毎回同じリストの先頭から `limit` 件だけが選ばれてしまいます。処理済みカットオフ（`skip_within` または前回リセット）は絶対時刻の窓なので、窓の外に出た時点で履歴が一斉に無効化され、また先頭のリポジトリが選ばれる一方で末尾には永遠に到達しません。

順序の基準は記録された処理時刻ではなくリポジトリ自身の最終ファイル変更日時です。レート制限で中断されて実際には何も変更できなかった実行を「処理済み」と数えないためです。日時は `git ls-files` が列挙する追跡対象ファイルの mtime の最大値から求めるため、ビルド成果物や `.gitignore` 対象は自然に除外されつつ、未コミットの編集はそのまま反映されます。`list` / `run` のターゲット一覧には `(modified: ...)` として併記され、並び順の根拠をその場で確認できます。

### 中断セッションの再開

Claude Code のタスクがレート制限（例: `You've hit your session limit · resets 2:30pm (Asia/Tokyo)`）で終わると、token-burn はそのセッションの ID を保存します。次の `token-burn run` が同じリポジトリを同じエージェントで選ぶと、作業を最初からやり直さず、`claude --resume <session_id>` と継続プロンプトで中断したセッションの続きから再開します。

1. `token-burn run` — タスクがレート制限で切られる。タスクは従来どおり失敗として報告され、そのセッションの ID が保存される。実行終了時の集計にもその旨が出る（`↻ Saved 1 interrupted session — run the same command again to continue it`。リセット時刻が分かっていれば併記）
2. 同じコマンドをもう一度実行する（`token-burn run`、1 リポジトリだけなら `token-burn run ~/GitHub/my-repo`）。中断時に 5 時間枠のリセット時刻が記録されていて、まだその時刻になっていなければ、実行全体がリセットまで一時停止した状態で始まる（5 時間枠が埋まったときの実行中の一時停止と同じ仕組み。枠はアカウント単位なので、他のタスクもそれまでは通らない）。待つとデッドラインを越える場合は停止し、この保留は実行計画にも表示される
3. ターゲットは `↻ resume` の行（短縮したセッション ID と、レート制限にかかった時刻）付きで一覧に出て、グループの先頭へ寄せられ、中断したセッションの続きから再開する

保存済みのセッションを再開するのは、次をすべて満たす場合だけです。満たさない場合は従来どおり新しいセッションで始め、`list` / `run` に保存済みセッションを使わない理由を表示し、使わなかった保存済みセッションは新しいセッションを起動する直前に破棄します。残しておくと、新しいセッションが保存されずに終わった場合（リトライ可能エラー等）に、後の実行がより古いセッションへ戻ってしまうためです。

- 再開が有効: `--no-resume` も `--fresh` も指定されておらず、`resume_interrupted` が `false` でない
- エージェントの `command` が自前でセッション系のフラグ（`--resume` / `-r`、`--continue` / `-c`、`--session-id`、`--fork-session`、`--no-session-persistence`、`--from-pr`）を渡していない。渡している場合は、足した `--resume` と衝突するため、そのエージェントでは中断セッションの保存も再開もしない（以前に保存したセッションがあれば、使わない理由を一覧に表示する）
- 中断後にターゲットのプロンプトが変わっていない。プロンプトを編集した後で古い指示の続きをさせても意味が無いため
- 現在の [`dedup_scope`](#エージェント間での処理済み履歴の共有) の範囲で、中断後にそのターゲットが処理されていない（例: `dedup_scope = "global"` で、その間に別アカウントが完了させていた場合）

固定の有効期限はありません。Claude Code は自身の保持設定に従って古い transcript を削除するため、セッションがまだ残っているかどうかは再開を試みた結果で判断します。

| 再開したタスクの結果 | 保存済みセッション |
|--------------------|------------------|
| 成功 | 削除。ターゲットは従来どおり `state.json` に記録される |
| 再びレート制限 | 新しい中断時刻で保存し直し、失敗回数も消す。次の実行も同じセッションの続きから再開する |
| セッションが既に無い | 削除し、同じタスクの中で元のプロンプトから新しいセッションをすぐに始める。この試行のログは元のログの隣の `<NNNN>_<name>.fresh.jsonl` / `.fresh.log` |
| その他の失敗（result を残さないクラッシュを含む） | 残す（認証切れのようにセッションと無関係な失敗もあるため）。再開の失敗が 3 回に達したら捨て、次の実行は新しいセッションで始める |
| リトライ可能エラー / キャンセル（Ctrl-C） | 変更しない |

組み込みの継続プロンプト（英語）は、前のセッションがレート制限で切られ、このセッションがその続きであること、まずリポジトリの状態（`git status`、現在のブランチ、`git worktree list`、未コミットの変更）を確かめること、実行中だったサブエージェントやバックグラウンドタスクは止まっているので完了を仮定せず結果を確かめること、完了済みの操作（コミット・push・リリース）を盲目的に繰り返さないこと、そのうえで元の指示の残りを仕上げることを伝えます。[`[prompts].resume`](#プロンプト) で差し替えられます。

対象は `claude` エージェントの、レート制限で終わったタスクだけです。リトライ可能エラー（`API Error: Connection closed mid-response` など）、クラッシュ、キャンセル、ログパイプラインの失敗は従来どおり次の実行で最初から処理し、Codex のセッションは再開しません。セッションはエージェントごとに保存します。transcript はそのアカウントの `CLAUDE_CONFIG_DIR` の下にあり、別アカウントからは続けられないためです。

再開できるターゲットはグループの先頭へ寄せます。`defer` と `public_first` によるグループ分けはこれまでどおり優先され、そのグループの内側で、最終変更が古い順より前に置かれます。中断したリポジトリは中断の直前まで変更されていたため、古い順だけで並べると末尾へ回り、`limit` で切られて再開されないまま終わるからです。`token-burn run PATH...` はコマンドラインの指定順を保ったまま再開します。再開するタスクの実行計画には `Resume: <session id>` が出て、プロンプト行には実際に送る継続プロンプトが表示されます。`-i` の選択画面では該当する行に `↻` が付きます。

保存先は `state.json` と同じディレクトリの `resume.json`（デフォルト `~/.config/token-burn/resume.json`）で、`state.json` と同じ sidecar lock と atomic rename で更新します。`state.json` に同居させないのは、その形式を変えず、旧バージョンの token-burn でも読めるようにするためです。`--no-resume` でその実行だけ保存済みセッションを無視でき、`resume_interrupted = false` で機能ごと無効にできます。

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
