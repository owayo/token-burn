# 設定

デフォルトの設定ファイルパス: `~/.config/token-burn/config.toml`

`token-burn init` で設定テンプレートを生成してください。最小の設定は [README](../README.ja.md#設定) にあり、このページではすべてのセクションを説明します。

## 基本設定

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
| `rate_limit_threshold` | 5 時間枠 / 7 日枠の使用率がこの閾値（%）以上で後続タスクを止める（`1-100`）。7 日枠なら恒久停止、5 時間枠ならその枠のリセットまでの一時停止。月次の追加課金枠（overage）の使用率では止まらない。Claude の stream-json リアルタイム監視に加え、ai-usage 連携時は各タスク完了後にも該当 agent の実使用率（weekly / five_hour）でチェックされる。[レート制限](rate-limits.ja.md)を参照 | `95`（デフォルト） |
| `dedup_scope` | 処理済み履歴を共有する範囲（`global` / `provider` / `agent`） | `agent`（デフォルト） |
| `resume_interrupted` | レート制限で切られた Claude Code のセッションを保存し、次回の実行で最初からやり直さず続きから再開する。`false` にすると中断セッションの保存も再開も行わない。[中断セッションの再開](usage.ja.md#中断セッションの再開)を参照 | `true`（デフォルト） |

`skip_within` と `cleanup_after` には、`d`（日）、`h`（時間）、`m`（分）、`s`（秒）を使った期間文字列を指定します。不正な値や期間として表現できない値は設定ファイルの読み込み時点でエラーになります。`skip_within` を省略した場合は前回リセット以降に処理済みのターゲットをスキップします。期間として表現できても日時の計算範囲を超える値ではパニックせず、`skip_within` は警告後に前回リセット時刻へフォールバックし、クリーンアップはエラーを返します。`--fresh` を指定すると保存済み状態（処理済み履歴と中断セッションの両方）を無視して全ターゲットを処理します。

状態ファイル: `<config-dir>/state.json`（有効な設定ファイルと同じディレクトリ）。更新時は同一ディレクトリのテンポラリファイルへ書き出してから `rename` で atomic に差し替え、`.state.json.lock` のような安定した sidecar lock file で並列ワーカーを直列化します。既存ファイルが不正な JSON の場合や権限・I/O エラーで読み取れない場合は、空状態として置き換えず更新を失敗させ、復旧に必要な原本と処理済み履歴を保全します。各エージェント内のエントリは最終処理時刻の降順（同時刻はパス昇順）で書き出されるため、最新の処理がファイルの先頭に来ます。デフォルト設定パスの場合は `~/.config/token-burn/state.json`。レート制限で中断したセッションは、同じディレクトリの別ファイル `resume.json` に保存されます（[中断セッションの再開](usage.ja.md#中断セッションの再開)を参照）。

### エージェント間での処理済み履歴の共有

`state.json` は展開エージェント名ごとに履歴を記録するため、既定では 1 つのアカウントで処理したリポジトリも他のアカウントからは未処理のままです。同じ CLI を 2 アカウントで回すと、2 つ目のアカウントは 1 つ目の続きからではなく同じリポジトリの先頭から始まります。`dedup_scope` はスキップ判定時にどこまでの履歴を参照するかを決めます。

| 値 | スキップ判定で参照する履歴 |
|----|--------------------------|
| `global` | 全エージェント。`state.json` にしか存在しない名前（改名・削除済みのエージェント）も含む。あるアカウントの続きから別アカウントが処理する |
| `provider` | 同じ `provider` のエージェント同士（例: `codex` 系アカウント同士は共有するが `claude` とは共有しない）。`provider` 未設定のエージェント（空文字・空白のみも未設定として扱う）と、設定に無い名前は自分自身の履歴のみ参照する |
| `agent` | 実行中のエージェントのみ（デフォルト） |

書き込み側は変わりません。完了は常に実際に実行したエージェント名で記録されるため、`state.json` にはアカウントごとの履歴がそのまま残り、スキーマも変わりません。広がるのは参照側だけです。

`global` / `provider` は `skip_within` が必須です。`skip_within` 省略時のカットオフは「実行中のエージェントの前回リセット時刻」でエージェント固有のため、他エージェントの履歴に適用するとスキップ範囲が「どのエージェントで起動したか」次第で揺れてしまいます。共有 scope なのに `skip_within` が無い設定は読み込み時にエラーになります。

`--dedup-scope <global|provider|agent>` でその実行だけ設定値を上書きできます。別アカウントが処理済みのリポジトリを意図的にもう一度回したいときは `--dedup-scope agent` を指定します。スキップ時は scope・窓・どのエージェントの記録で弾いたかを併記します。

```text
  Skipped: 8 targets (already processed; scope: global, window: 2d)
    by agent: codex=5, codex-alt=2, claude=1
```

## エージェント

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

## ai-usage 連携（任意）

既定では、各エージェントの reset 時刻を `reset_weekday` / `reset_time` / `timezone` から固定計算します。外部ツール `ai-usage --json` と連携すると、reset 時刻を実データ（`weekly.resets_at`）から自動取得し、実際の利用状況に基づいた reset 時刻を使います（固定計算は解決失敗時のフォールバックとして残ります）。

連携が無い、または `enabled = false` の場合は曜日計算だけで動作します。

```toml
[ai_usage]                 # 任意。無い or enabled=false なら曜日計算のみ
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

### `[ai_usage]`（グローバル設定）

| フィールド | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | ai-usage 連携を有効化する。無い or `false` なら曜日計算のみ | `false` |
| `command` | 実行する ai-usage コマンドと引数 | `["ai-usage", "--json"]` |
| `window` | deadline 算出に使う枠。`weekly` / `five_hour` / `nearest` | `"weekly"` |
| `fallback` | 解決失敗時の挙動。`fixed`（曜日計算へフォールバック）/ `skip`（候補から除外）/ `error`（停止） | `"fixed"` |
| `state_window` | 処理済みカットオフの算出枠。`weekly` / `selected` | `"weekly"` |

### `[[ai_usage.profiles]]`（プロファイル定義）

| フィールド | 説明 | 例 |
|-----------|------|-----|
| `name` | 内部参照名。エージェントの展開名 `<agent>-<name>` に使われる | `"work"` |
| `profile` | `ai-usage --json` の `"profile"` と照合する名前（大文字小文字を区別） | `"Work"` |
| `env` | そのアカウントでの起動時 env（任意）。キーは `[A-Za-z_][A-Za-z0-9_]*`、値は `~` 展開される | `{ CLAUDE_CONFIG_DIR = "~/.config/claude-work" }` |

### `[agents.ai_usage]`（エージェント側の連携設定）

| フィールド | 説明 | 例 |
|-----------|------|-----|
| `profiles` | 参照する profile 名のリスト。複数指定でアカウント別に展開 | `["work", "home"]` |
| `window` | グローバル `window` の上書き（任意） | `"weekly"` |
| `fallback` | グローバル `fallback` の上書き（任意） | `"fixed"` |

### 挙動

- 実行時に agent × profile を展開します。例: `claude` + `["work", "home"]` → `claude-work` / `claude-home` の 2 エージェント。各々プロファイルの `env` を付与して起動します。**profile を 1 つだけ参照する場合は展開名が agent 名のまま**（例: `codex` が `["home"]` のみ → `codex`）で、サフィックス `<agent>-<profile>` が付くのは 2 つ以上参照したときだけです。起動コマンドが異なる各アカウントを別 agent として定義しても展開名が冗長にならず、`state.json` キーも安定します。
- 展開名は `state.json` のキーにも使われ、アカウントごとに処理済み状態が分離されます。
- `ai-usage --json` は 1 プロセスにつき 1 回だけ実行されます。
- **使用率ゲート（完了後チェック）**: ai-usage 連携が有効な場合、各タスク完了後に該当 agent の `(profile, provider)` の weekly / five_hour の `used_percent` を ai-usage から取得し、`rate_limit_threshold` 以上の枠があれば後続タスクの開始を止めます。Claude の stream-json `rate_limit_event` によるリアルタイム監視（タスク実行中の停止）に加えてこの完了後チェックが効くため、**リアルタイムの監視が無い codex でも実使用率で確実に止まります**（claude / codex 両方に適用）。
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

## 自動スキャン（複数定義可）

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

## プロンプト

`.md` で終わる値はファイルパスとして読み込まれます。相対パスは設定ファイルのディレクトリから解決されます。

```toml
[prompts]
default = "prompts/default.md"
# resume = "prompts/resume.md"   # 任意: 中断セッションを再開するときに送る継続プロンプト
```

`resume`（任意）は[中断セッションを再開](usage.ja.md#中断セッションの再開)するときに送る継続プロンプトです。`default` と同じ規則で解決され、省略時は組み込みの英語プロンプトが使われます。再開時に送るのはこのプロンプトだけです（元の指示はセッションの履歴に残っているため）。

## 個別ターゲット（スキャン結果とマージ）

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
