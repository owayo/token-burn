#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Section {
    Settings,
    Agents,
    Scan,
    Targets,
    Usage,
    Profiles,
    Prompts,
}

impl Section {
    pub const ALL: [Self; 7] = [
        Self::Settings,
        Self::Agents,
        Self::Scan,
        Self::Targets,
        Self::Usage,
        Self::Profiles,
        Self::Prompts,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Agents => "agents",
            Self::Scan => "scan",
            Self::Targets => "targets",
            Self::Usage | Self::Profiles => "ai_usage",
            Self::Prompts => "prompts",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Settings => "実行設定",
            Self::Agents => "エージェント",
            Self::Scan => "探索先",
            Self::Targets => "個別対象",
            Self::Usage => "ai-usage 連携",
            Self::Profiles => "プロファイル",
            Self::Prompts => "プロンプト",
        }
    }
    pub fn repeated(self) -> bool {
        matches!(
            self,
            Self::Agents | Self::Scan | Self::Targets | Self::Profiles
        )
    }
}

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Text,
    Prompt,
    Number(i64, i64, &'static str),
    Bool(bool),
    Choice(&'static [(&'static str, &'static str)], &'static str),
    Lines,
    Env,
    Profiles,
}

#[derive(Clone, Copy)]
pub(super) struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub optional: bool,
}

impl Field {
    pub fn multiline(self) -> bool {
        matches!(self.kind, Kind::Lines | Kind::Env | Kind::Prompt)
    }
    pub fn hint(self) -> &'static str {
        match self.kind {
            Kind::Lines => {
                "1 行に 1 要素。空白を含む引数も 1 行。空文字・改行入りの値は JSON 文字列で指定。Enter: 改行 / Ctrl-S: 適用"
            }
            Kind::Env => {
                "1 行ずつ KEY = \"value\" と入力。空文字はその変数を unset。Enter: 改行 / Ctrl-S: 適用"
            }
            Kind::Prompt => {
                "本文または .md のパス（設定ファイルからの相対パスも可）。Enter: 改行 / Ctrl-S: 適用"
            }
            _ => "Enter: 適用 / Esc: 取消 / Ctrl-U: カーソルまで消去",
        }
    }
}

const WINDOW: &[(&str, &str)] = &[
    ("weekly", "weekly — 週次リセット"),
    ("five_hour", "five_hour — 5 時間枠"),
    ("nearest", "nearest — 最も近いリセット"),
];
const FALLBACK: &[(&str, &str)] = &[
    ("fixed", "fixed — 固定の曜日・時刻に戻す"),
    ("skip", "skip — そのエージェントを候補から除外"),
    ("error", "error — エラーで終了"),
];
const WEEKDAY: &[(&str, &str)] = &[
    ("monday", "月曜日"),
    ("tuesday", "火曜日"),
    ("wednesday", "水曜日"),
    ("thursday", "木曜日"),
    ("friday", "金曜日"),
    ("saturday", "土曜日"),
    ("sunday", "日曜日"),
];

pub(super) fn fields(section: Section) -> Vec<Field> {
    use Kind::*;
    // Schema is only presentation and input types. Config::parse owns cross-field validation.
    let definitions: &[(&str, &str, &str, Kind, bool)] = match section {
        Section::Settings => &[
            (
                "parallelism",
                "並列ワーカー数",
                "同時に処理するプロジェクト数。1 以上。",
                Number(1, i64::MAX, "3"),
                false,
            ),
            (
                "limit",
                "対象数の上限",
                "1 回で処理する最大数。1 以上。",
                Number(1, i64::MAX, "10"),
                false,
            ),
            (
                "rate_limit_threshold",
                "使用率の停止閾値 (%)",
                "1〜100%。短周期枠は一時停止、週次枠は後続タスクを停止します。",
                Number(1, 100, "95"),
                false,
            ),
            (
                "skip_within",
                "処理済みをスキップする期間",
                "例: 1d / 24h / 1d12h。未設定ならエージェントの前回リセット以降。履歴を共有する場合は必須。",
                Text,
                true,
            ),
            (
                "dedup_scope",
                "処理済み履歴の共有",
                "agent は個別、provider は同じ CLI、global はすべてのエージェント。provider/global はスキップ期間が必要。",
                Choice(
                    &[
                        ("agent", "agent — エージェントごと"),
                        ("provider", "provider — 同じ CLI で共有"),
                        ("global", "global — 全エージェントで共有"),
                    ],
                    "agent",
                ),
                false,
            ),
            (
                "resume_interrupted",
                "中断セッションを再開",
                "レート制限で中断した Claude のセッションを保存し、次回に続きから処理します。",
                Bool(true),
                false,
            ),
            (
                "report_dir",
                "ログの保存先",
                "未設定なら ~/Documents/token-burn。相対パスは実行時の作業ディレクトリ基準です。",
                Text,
                true,
            ),
            (
                "cleanup_after",
                "ログの保持期間",
                "例: 7d / 48h。未設定なら 7d。古いレポートを自動削除します。",
                Text,
                true,
            ),
        ],
        Section::Agents => &[
            (
                "name",
                "名前",
                "CLI の --agent と履歴のキーに使います。変更すると以前の名前の履歴は引き継ぎません。",
                Text,
                false,
            ),
            (
                "provider",
                "CLI の種類",
                "claude / codex / antigravity。独自ラッパーでも指定できます。未設定ならコマンドから判定。ai-usage 連携時は必須。",
                Text,
                true,
            ),
            (
                "command",
                "起動コマンド・引数",
                "先頭行が実行ファイル、以降は 1 行に 1 引数。引用符で空白を囲む必要はありません。無人実行・整形用の必須フラグは自動付与。",
                Lines,
                false,
            ),
            (
                "reset_weekday",
                "リセット曜日",
                "固定スケジュールの曜日。ai-usage から取得できない場合の fixed フォールバックにも使います。",
                Choice(WEEKDAY, ""),
                true,
            ),
            (
                "reset_time",
                "リセット時刻",
                "24 時間表記 HH:MM。例: 09:00。",
                Text,
                true,
            ),
            (
                "timezone",
                "タイムゾーン",
                "例: Asia/Tokyo / UTC / America/New_York。",
                Text,
                true,
            ),
            (
                "ai_usage.profiles",
                "使用するプロファイル",
                "登録済みアカウントから Space で選択。2 つ以上選ぶと、名前にプロファイル名を付けて展開します。連携画面の有効化も必要。",
                Profiles,
                true,
            ),
            (
                "prompt",
                "専用プロンプト",
                "未設定なら共通プロンプトを使用します。本文または .md のパス。",
                Prompt,
                true,
            ),
            (
                "env",
                "環境変数（詳細）",
                "例: CLAUDE_CONFIG_DIR = \"~/.config/claude-work\"。~ は実行時に展開。プロファイル側の変数が優先されます。",
                Env,
                true,
            ),
            (
                "codex_output",
                "Codex の出力（詳細）",
                "Codex 用。auto は exec 検出時のみ整形。json は JSONL を出すラッパー用。human は素通し。",
                Choice(
                    &[
                        ("auto", "auto — 自動判定"),
                        ("json", "json — JSONL として整形"),
                        ("human", "human — そのまま表示"),
                    ],
                    "",
                ),
                true,
            ),
            (
                "ai_usage.window",
                "リセット枠の上書き（詳細）",
                "未設定なら ai-usage 連携の共通設定を使います。",
                Choice(WINDOW, ""),
                true,
            ),
            (
                "ai_usage.fallback",
                "取得失敗時の上書き（詳細）",
                "未設定なら共通設定。fixed は曜日・時刻・タイムゾーンが必須。",
                Choice(FALLBACK, ""),
                true,
            ),
        ],
        Section::Scan => &[
            (
                "base_dirs",
                "探索するディレクトリ",
                "例: ~/GitHub。1 行に 1 パス。直下の Git リポジトリを探します。",
                Lines,
                false,
            ),
            (
                "recursive",
                "子ディレクトリも探索",
                "オンならネストされたディレクトリも再帰的に探します。",
                Bool(false),
                false,
            ),
            (
                "username",
                "リモートのオーナーで絞る",
                "GitHub などのユーザー名。未設定ならオーナーで絞り込みません。",
                Text,
                true,
            ),
            (
                "public_first",
                "公開リポジトリを優先",
                "公開リポジトリを先に処理します。",
                Bool(true),
                false,
            ),
            (
                "exclude",
                "除外するディレクトリ名",
                "1 行に 1 つ。パスではなくディレクトリ名を指定。",
                Lines,
                true,
            ),
        ],
        Section::Targets => &[
            (
                "directory",
                "対象ディレクトリ",
                "例: ~/GitHub/my-project。探索結果と統合し、同じ対象は重複させません。",
                Text,
                false,
            ),
            (
                "prompt",
                "専用プロンプト",
                "未設定ならエージェント側、さらに共通の順でプロンプトを使います。",
                Prompt,
                true,
            ),
            (
                "defer",
                "後回しにする",
                "オンなら通常の対象の後に処理します。",
                Bool(false),
                false,
            ),
        ],
        Section::Usage => &[
            (
                "enabled",
                "ai-usage 連携を有効化",
                "実データからリセット時刻・使用率を取得。プロファイルの登録とエージェント側の選択も必要。",
                Bool(false),
                false,
            ),
            (
                "window",
                "リセット枠",
                "実行期限を決める枠。通常は weekly を使います。",
                Choice(WINDOW, "weekly"),
                false,
            ),
            (
                "fallback",
                "取得に失敗したとき",
                "fixed は各エージェントの固定スケジュールに戻ります。skip は候補から除外、error は終了。",
                Choice(FALLBACK, "fixed"),
                false,
            ),
            (
                "state_window",
                "処理済み判定の枠",
                "weekly は週次基準、selected は実行期限に選ばれた枠を使います。",
                Choice(
                    &[
                        ("weekly", "weekly — 常に週次基準"),
                        ("selected", "selected — 選んだリセット枠"),
                    ],
                    "weekly",
                ),
                false,
            ),
            (
                "command",
                "連携コマンド（詳細）",
                "通常は先頭行 ai-usage、次の行 --json。ここでコマンドを実行することはありません。",
                Lines,
                false,
            ),
        ],
        Section::Profiles => &[
            (
                "name",
                "参照用の名前",
                "エージェントから選ぶ名前。変更するとエージェント側の参照も更新します。",
                Text,
                false,
            ),
            (
                "profile",
                "ai-usage のプロファイル名",
                "ai-usage --json の profile と同じ値。例: Work / Home。大文字と小文字を区別します。",
                Text,
                false,
            ),
            (
                "env",
                "このアカウントの環境変数",
                "例: CLAUDE_CONFIG_DIR = \"~/.config/claude-work\" または CODEX_HOME = \"~/.config/codex-work\"。",
                Env,
                true,
            ),
        ],
        Section::Prompts => &[
            (
                "default",
                "共通プロンプト",
                "全対象の共通指示。本文または .md のパス。ファイルを指定する場合は保存後も元のファイルを使います。",
                Prompt,
                false,
            ),
            (
                "resume",
                "再開時のプロンプト",
                "未設定なら組み込みの継続指示。本文または .md のパス。",
                Prompt,
                true,
            ),
        ],
    };
    definitions
        .iter()
        .map(|&(key, label, help, kind, optional)| Field {
            key,
            label,
            help,
            kind,
            optional,
        })
        .collect()
}
