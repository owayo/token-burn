# ライブモニター

実行中に token-burn が表示する内容です。各ワーカーのペインは Claude Code の stream-json 出力を読める行に整形して表示し、モニターのペインは実行全体の進捗を表示します。tmux のペインの構成は [usage.ja.md](usage.ja.md#tmux-での実行) で説明しています。

## モニターのペイン

- **モニター使用量パネル**: ai-usage 連携時、tmux モニターペインに `ai-usage --statusline --logos`（各アカウントの 5h / 週次使用率バー）を 10 秒ごとに表示（`--input` でキャッシュから高速描画、進捗バーは毎秒更新）。取得は `ai-usage` の終了と同時に返るため、更新処理でペインが固まることはなく、進捗バーは毎秒更新を維持

## セッションと結果

- **セッションヘッダー**: `init` イベントからモデル・Claude Code バージョン・権限モードを 1 行で表示（`ℹ Session <model> (v<version>, <permissionMode>)`）。これらはストリーム中の他のイベントには現れず、`result.modelUsage` からは実際に課金されたモデルしか分からないため、CLI バージョンと `bypassPermissions` で実行したかどうかが失われていた
- **モデル別使用量**: 結果サマリーにモデルごと（Opus、Haiku等）のトークン使用量・コスト・キャッシュ読み取り/書き込み・Web検索回数、そして各モデルのコンテキスト上限/最大出力上限（例: `ctx:1M` / `max_out:64K`）を表示
- **サブエージェント込みの総消費量**: `result.usage` はメインループの消費しか含まないため、`modelUsage` の合計が上回るときだけ `📊 total in:<n> out:<n> (thinking:<n>, incl. subagents)` を併記。実ログでは cache_read が `2,110,689` → `220,321,325` と 100 倍以上乖離しており、見出しの `in/out` だけでは実際の消費量を桁違いに過小評価する。併記する思考トークンは `modelUsage[].thinkingTokens` の合計で、実ログではメインループ分 89,291 に対し 1,219,128（総出力の 66%）だった。落とすとサブエージェントへ消えたトークンの最大の内訳が見えなくなる。サブエージェント未使用のセッションでは両者が完全一致するため表示しない
- **思考進捗ドット**: Claude Code は思考本文を伏せるため `thinking_delta` の本文は空文字で届き、進捗は `estimated_tokens`（増分）にだけ入る。50 トークンごとにドットを 1 つ出す。本文のバイト長だけを見ていた頃は実データ 7,516 件すべてでドットが 0 個になり、中身のない `💭 ` 行だけが並んでいた
- **思考トークンの内訳**: `output_tokens_details.thinking_tokens` を `📊 in:<n> out:<n> (thinking:<n>)` として表示。実ログでは出力トークンの 10〜52% を思考が占めるため、内訳が無いと何にトークンを使ったのか分からない
- **API応答時間**: 実行時間に加えてAPI応答時間・初回トークン到達時間（`ttft`）・初回ストリームトークン到達時間（`stream:`。キュー/リトライ待ちを除いた純粋なストリーム遅延）・リクエスト送信までの所要時間（`req:<n>ms`）を表示
- **fast mode 表示**: fast mode が有効な場合は状態を表示し、利用できない理由が返された場合は `fast_mode_disabled_reason` も表示
- **terminal_reason / permission_denials**: 異常終了時の `terminal_reason`（`completed` 以外）と権限拒否されたツール呼び出しの件数/ツール名を結果サマリーに表示
- **結果メタデータ**: `usage.service_tier`、`usage.speed`、空でない推論リージョン、iteration 数、result origin 種別を表示
- **セッション失敗の原因**: `result.is_error` が true なら `error (HTTP 429): <本文>` を表示。実データでは 47 ドル・30 分を消費したセッションが `subtype:"success"` のまま支出上限エラーで終わっており、フッターに出ていたのは `terminal api_error` の 1 行だけで、読み落とすと正常完了に見えた
- **VCS 状態変更**: git hook による自動 commit / push（`vcs_state_changed`）を `⎇ VCS push (main)` のようにブランチ付きで表示。無人実行中にコミットや push が作られた事実はセッション後の変更追跡の起点で、`main` へ push したのか作業ブランチへ push したのかで影響範囲が全く違う。一方で 1 件数百 KB に達する `commands_changed`（スキル/コマンド一覧のスナップショット）は実行内容と無関係なため非表示

## ツール

- **ツール詳細の強化**: Claude の stream-json に含まれる `Read` の offset/limit/view range、パースできないツール入力（モデルの不正 JSON 出力、またはレート制限・切断による途中切れ）の文字数表示（`unparsed:<n> chars`）、`Edit` の一括置換状態、`Bash` の timeout/background/sandbox 無効化状態、`BashOutput` の対象 background bash id（`bash:<id>`）と任意の filter、`Agent` / `Task` の識別子と説明およびバックグラウンド状態、`Grep`/`Glob` の output mode・type・ignore-case・only-matching・multiline・glob・head/context/offset 制限、`ScheduleWakeup` の待機時間/理由、`WebFetch` の URL とプロンプト要約、`WebSearch` のクエリと include/exclude ドメイン件数、`ToolSearch` のクエリと `max_results`、`Monitor` の説明/タイムアウト/condition/persistent 状態、`TaskStop` の task id（複数指定含む）と理由、`TaskList` 呼び出し、`TaskGet` の task id、`TaskOutput` の task id / `block` / `timeout`、`Workflow` の起動対象（インライン script の `meta.name` から抽出したワークフロー名とスクリプト文字数、または名前指定ワークフロー / スクリプトパス）、`TaskCreate` の `subject` / `description` / `activeForm`、`TaskUpdate` の `taskId` / `status` / `owner` / `subject` / `description`、`SendMessage` の要約、`SlashCommand` の実行コマンド文字列、既存ログなどに含まれる `AskUserQuestion` の質問/選択肢、Tavily search の期間フィルタ（`start=2026-08-01` / `end=...`）とドメイン絞り込み（1 件なら `site=ast-grep.github.io`、複数なら `site=2 domains`、除外は `-site=...`）、Tavily/Codex MCP の model/sandbox/approval 詳細、Context7 MCP ツールの library/query を表示
- **ツールエラー要約**: `tool_result` の `is_error:true` を検出すると、エラー内容の先頭の有意な 1 行を 120 文字までに省略してモニターに併記（単一行/複数行の `<tool_use_error>` ラッパーは除去）。jsonl を開かずに失敗の原因が分かる
- **ツール結果メタデータ**: top-level `tool_use_result` に含まれる出力切り詰め、適用 limit、stale read ヒント、Edit/Write が書き込み前にユーザによる変更を検出した場合の `user-modified` マーカー、Edit が古い読み取り状態から自動回復した場合の `stale-recovered` マーカー、Claude Code がメモリ用ディレクトリへ印を付けた場合の `memdir-stamped` マーカー、失敗理由（`error:` / `message:`）、Bash 等の標準出力/標準エラー要約（`stdout:` / `stderr:`）、MCP/Codex の構造化応答要約（`structured:`）、文字列または text ブロック配列で返る MCP 成功結果の要約（`result:`）、Edit 結果のファイルパスと structured patch 規模（`file:<path>`、`patch:<hunks> ... +追加/-削除`、`replace_all`）、自動バックグラウンド化、待機時間の clamp、永続化出力サイズ、戻りコード解釈、Agent の duration/token/tool 数、`ListAgents` の一覧件数（`agents:<n>`）、サブエージェント種別（`agent:`）・解決モデル（`model:`）・サブエージェントの編集行数（`edits:+追加/-削除`）、非同期 Agent の識別子（`agent-id:`）と `SendMessage` で再開した Agent の識別子（`resumed-agent:`）、Grep/ToolSearch の結果件数と mode、WebSearch の結果件数/検索回数/所要時間、WebFetch の HTTP ステータスコードと応答サイズ（`http:200 OK`、`bytes:120.2KB`）、Read の部分読み取り行数（`lines:<n>/<total>`）またはオフセット付き範囲（`lines:<start>-<end>/<total>`）と token cap 切り詰め（`truncated:token-cap`）、git commit 操作（sha/kind）、タスク件数/task id/task type、TaskOutput の取得状態、読み取り可能な Agent 出力ファイル、Monitor の timeout/persistent 状態、TaskUpdate の状態遷移と status 以外の変更フィールド（`updated:<field1>,<field2>`）、async Agent 起動（`run_in_background=true` 時に `async`）、ScheduleWakeup の予定時刻、Skill のコマンド名と許可ツール件数（`allowed-tools:<n>`）、起動したワークフロー名（`workflow:<name>`）などの重要情報を表示
- **長時間ツールの進捗表示**: `tool_progress` の経過時間を表示し、長時間実行中にモニターが停止したように見える状態を防止
- **Bash 経由のファイル変更**: `sed -i` / `cargo fmt` / `depup` など Edit/Write を通らない書き換えは `filePath` も `structuredPatch` も出ず痕跡が残らない。`tool_use_result.bashEditDiff` から `bash-edits:<path> +追加/-削除`（複数なら件数）を表示する（実データでは Bash 結果 102 件が該当し、うち 36 件が実変更）
- **実測したバックグラウンドメタデータ**: バックグラウンド移行時の待機期限を `wait-timeout:<期間>`、作業ディレクトリの注意を `cwd-hint:<要約>`、権限ルールによる未実行を `not-executed:permission-rule` として表示
- **拒否時モデル切り替えの可視化**: `model_refusal_fallback` の切り替え元・切り替え先モデルとカテゴリを表示し、イベント内の content / explanation は出力しない
- **実 stream-json の境界形式**: assistant レベルのモデル切り替え（`from.model` → `to.model`）と、キャッシュミス理由・対象 input token 数を表示し、partial message の繰り返しは message id 単位で重複抑止。モデル関連フィールドで実測した壊れた末尾 SGR 断片（例: `claude-opus-5[1m]`）は、セッションヘッダー・fallback・Agent メタデータ・モデル別使用量の全経路で `claude-opus-5` へ正規化。タスクイベントと重複する高頻度の `background_tasks_changed` スナップショットは非表示にし、表示対象の system / rate-limit 通知が本文・思考 delta の途中へ到着しても、単語や思考行へ連結せず独立した行に表示（無視対象イベントでは改行を増やさない）。JSON として解釈できない行（`2>&1` で合流した stderr の `API Error: ...` など）も同じ扱いで、開いている思考・テキスト行を閉じてから独立行へ出す。Agent 起動時の任意 `model` / `isolation`、`isImage:true` の `image` マーカーを表示。`structuredPatch[].lines` は `+` / `-` で始まる全行を数え、内容自体が `++` / `--` で始まる追加・削除行も取りこぼさない

## サブエージェント

- **サブエージェント監視**: Claude Codeのチーム/エージェントタスクの開始・進捗・状態更新・完了をリアルタイム表示。`task_started` は具体的な `subagent_type` を優先し、失敗通知は要約を併記、`task_updated` の `killed` は失敗として強調表示
- **サブエージェント結果集計**: `result.subagent_stats` から起動・完了・失敗・強制終了・起動拒否の件数、バックグラウンド/入れ子起動数、最大深度を表示。トップレベルが成功でも配下のサブエージェントが失敗した場合は警告表示
- **サブエージェント停止の可視化**: `task_notification` の `status="stopped"`（`TaskStop` 等で停止された場合）もモニターに表示。`usage` が無い通知では duration/token を 0 として表示しない
- **サブエージェント失敗理由**: サブエージェントが失敗・強制終了したとき、その原因（API エラー等）を完了通知に併記
- **ツール完了行のサブエージェント帰属**: サブエージェント内で走ったツールの結果はメインループの出力と同じストリームへ混ざり、実ログでは完了行の 69%（2,309 / 3,350 件）がサブエージェント由来だった。ツール名の直後に ` @<タスク名>`（Agent 起動時の description）を添えるため、22 個が並列で動いていても `✓ Bash` の羅列から誰の作業かを追える
- **サブエージェント内部のツール使用とテキスト出力**: `stream_event` はメインループ専用（実データ 98,061 件すべてが `parent_tool_use_id: null`）で、サブエージェントが何のコマンドを打ったかは `assistant` イベントにしか現れない。これを拾わないとツール完了行の 44%（1,178 / 2,695 件）が `✓ Bash @<タスク名>` だけになる。`🔧 <ツール名> @<タスク名> <詳細>` としてメインループと同じ形で表示し、サブエージェントの最終レポート（実データ 179 ブロック / 363KB）も `💬 @<タスク名> <先頭 1 行>` に畳んで出す
- **入れ子起動の深さ**: サブエージェントがさらにサブエージェントを起動した場合、`task_started` の行に `depth:<n>` を添える。`result.subagent_stats.max_depth` は最終集計しか持たないため、これが無いと「どのタスクが深いのか」を追えない。入れ子は実行時間とトークン消費が指数的に膨らむ要因そのもの。`spawn_depth` を持たないサブエージェント発の background Bash（実データ 84 件）は `owned_by_subagent` から `nested` として印を付ける
- **進捗行の累積トークン**: `task_progress` に `tokens:<n>`（`usage.total_tokens`）を併記。実データ 1,254 件すべてに入り 10 万トークン級が常態で、`result.usage` はメインループ分しか持たないため、これが無いとどのサブエージェントが枠を食っているかを実行中に読み取れない
- **サブエージェント種別の内訳**: `result.subagent_stats.by_type` から `[Explore:5 general-purpose:4 codex:2]` を併記。`codex` が 2 体なのか `Explore` が 5 体なのかでコストの意味が全く違うため、件数だけの `spawned:12` では読み取れない

## 通知とリトライ

- **システム通知の可視化**: stop hook エラーなどの Claude Code システム通知に加え、`hook_progress` / `hook_response` に stderr や output が含まれる場合のフック診断も表示
- **フック差し戻しの可視化**: Stop フックが exit 2 / タイムアウトで終わると Claude Code はその内容を合成 user メッセージ（`isSynthetic`）としてモデルへ差し戻すが、これは `hook_response` にも `notification` にも現れない。第 1 行が `<Event> hook feedback:` のものを `⚠ Hook feedback (Stop): ⏱ Stop hook timed out after 120s: cargo` として表示する。無人実行では Stop フックが自動コミット / push を担うため、落とすと「仕事は終わったのにコミットされていない」理由をログから追えない。同じ合成メッセージでもスキル本文の注入（`Base directory for this skill: ...`）は SKILL.md 全文を含んで巨大なうえ `Skill` ツール行と重複するため非表示
- **APIリトライ表示**: 一時的な障害時のリトライ試行回数とエラー情報を表示

レート制限の警告と停止は [rate-limits.ja.md](rate-limits.ja.md) にまとめています。
