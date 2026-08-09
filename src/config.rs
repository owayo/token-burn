use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub config_dir: PathBuf,
    pub settings: Settings,
    pub prompts: Prompts,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub scan: Vec<Scan>,
    #[serde(default)]
    pub targets: Vec<Target>,
    /// ai-usage --json 連携設定（省略時は連携無効）。
    #[serde(default)]
    pub ai_usage: Option<AiUsageConfig>,
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub parallelism: usize,
    /// この期間以内に処理済みのディレクトリをスキップ（例: "7d", "24h", "1d12h"）。
    /// 省略時は前回リセット以降に処理済みのディレクトリをスキップ。
    pub skip_within: Option<String>,
    /// 実行ログの保存先ディレクトリ（デフォルト: ~/Documents/token-burn）
    pub report_dir: Option<String>,
    /// この期間より古いレポートディレクトリを自動削除（デフォルト: "7d"）。
    pub cleanup_after: Option<String>,
    /// 1回の実行で処理する最大ターゲット数（デフォルト: 10）。
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// レート制限使用率がこの閾値（%）を超えたら自動停止する（デフォルト: 95）。
    #[serde(default = "default_rate_limit_threshold")]
    pub rate_limit_threshold: u8,
    /// 処理済み判定を共有する範囲（デフォルト: agent = エージェントごとに分離）。
    #[serde(default)]
    pub dedup_scope: DedupScope,
}

/// 処理済み判定（`state.json` の照会）を共有する範囲。
///
/// 記録側は常に実行したエージェント名のキーへ書くため、この設定を変えても
/// `state.json` のスキーマと「どのエージェントが処理したか」の履歴は変わらない。
/// 変わるのは「スキップ対象かどうかを判定するときに、どのエージェントの履歴まで見るか」だけ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum DedupScope {
    /// 全エージェント横断。どのアカウント・どの CLI で処理しても他へ引き継ぐ。
    Global,
    /// 同じ provider（CLI 種別）のエージェント同士で共有する。
    /// provider 未設定のエージェントは自分自身のみを見る。
    Provider,
    /// エージェントごとに完全分離（従来の挙動）。
    #[default]
    Agent,
}

impl DedupScope {
    /// 他エージェントの履歴まで参照する範囲かどうか。
    pub fn is_shared(self) -> bool {
        !matches!(self, DedupScope::Agent)
    }

    /// 表示用のラベル。
    pub fn label(self) -> &'static str {
        match self {
            DedupScope::Global => "global",
            DedupScope::Provider => "provider",
            DedupScope::Agent => "agent",
        }
    }
}

fn default_limit() -> usize {
    10
}

fn default_rate_limit_threshold() -> u8 {
    95
}

#[derive(Debug, Deserialize)]
pub struct Prompts {
    pub default: String,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Agent {
    pub name: String,
    /// provider 明示（"claude" | "codex" | "antigravity"）。
    /// ai-usage 連携時は (profile, provider) 照合に使うため必須。
    pub provider: Option<String>,
    pub command: Vec<String>,
    /// 起動時に付与する環境変数。profile 展開時は profile.env で上書きマージされる。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// fallback=fixed 用のリセット曜日。ai-usage 連携かつ fallback!=fixed なら省略可。
    pub reset_weekday: Option<String>,
    pub reset_time: Option<String>,
    pub timezone: Option<String>,
    /// エージェント固有のプロンプト上書き（[prompts].default より優先）
    pub prompt: Option<String>,
    /// ai-usage 連携設定（参照する profile、window/fallback 上書き）。
    pub ai_usage: Option<AgentAiUsage>,
}

/// ai-usage --json 連携のグローバル設定（`[ai_usage]`）。
#[derive(Debug, Deserialize, Clone)]
pub struct AiUsageConfig {
    /// 連携を有効化する。false なら従来の曜日計算のみで動作する。
    #[serde(default)]
    pub enabled: bool,
    /// ai-usage を起動するコマンド（デフォルト: ["ai-usage", "--json"]）。
    #[serde(default = "default_ai_usage_command")]
    pub command: Vec<String>,
    /// deadline 算出に使う枠（weekly | five_hour | nearest）。
    #[serde(default)]
    pub window: UsageWindowPolicy,
    /// ai-usage 解決失敗時のフォールバック方針（fixed | skip | error）。
    #[serde(default)]
    pub fallback: UsageFallback,
    /// 処理済みカットオフに使う枠（weekly | selected）。
    #[serde(default)]
    pub state_window: StateWindowPolicy,
    /// プロファイル定義（`[[ai_usage.profiles]]`）。
    #[serde(default)]
    pub profiles: Vec<AiUsageProfile>,
}

fn default_ai_usage_command() -> Vec<String> {
    vec!["ai-usage".to_string(), "--json".to_string()]
}

/// ai-usage の Chrome プロファイルと、そのアカウントで起動する環境のマッピング。
#[derive(Debug, Deserialize, Clone)]
pub struct AiUsageProfile {
    /// token-burn 内部で参照する名前。agent から参照し、展開名 `<agent>-<name>` にも使う。
    pub name: String,
    /// ai-usage --json の `profile` フィールドと照合する値（大文字小文字は区別）。
    pub profile: String,
    /// このプロファイルで起動する際に付与する環境変数（agent.env を上書きマージ）。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// agent 側の ai-usage 連携設定（`[agents.ai_usage]`）。
#[derive(Debug, Deserialize, Clone)]
pub struct AgentAiUsage {
    /// 参照する `[[ai_usage.profiles]]` の name 一覧。空なら fixed 計算にフォールバック。
    #[serde(default)]
    pub profiles: Vec<String>,
    /// この agent の window 上書き（省略時はグローバル設定）。
    pub window: Option<UsageWindowPolicy>,
    /// この agent の fallback 上書き（省略時はグローバル設定）。
    pub fallback: Option<UsageFallback>,
}

/// deadline 算出に使う ai-usage の枠。
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageWindowPolicy {
    /// 週次リセットを使う（token-burn の主目的に合致、デフォルト）。
    #[default]
    Weekly,
    /// 5 時間枠リセットを使う。
    FiveHour,
    /// weekly / five_hour のうち近い方を使う。
    Nearest,
}

/// ai-usage 解決に失敗したときの方針。
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageFallback {
    /// reset_weekday/reset_time/timezone による固定計算に戻す（デフォルト、後方互換）。
    #[default]
    Fixed,
    /// その RuntimeAgent を候補から除外する。
    Skip,
    /// 即エラーにする。
    Error,
}

/// 処理済みカットオフ（state_cutoff）に使う枠。
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateWindowPolicy {
    /// 常に週次（resets_at - 7d）を基準にする（デフォルト）。
    #[default]
    Weekly,
    /// deadline に採用した枠の period を基準にする。
    Selected,
}

/// agent × profile を実行時に展開した実体。スケジュール解決・実行の単位。
#[derive(Debug, Clone, Default)]
pub struct RuntimeAgent {
    /// 展開名（例: "claude" または "claude-owa"）。state.json キー・レポート名に使う。
    pub name: String,
    /// 提供元（"claude" | "codex" | "antigravity"）。
    pub provider: Option<String>,
    pub command: Vec<String>,
    /// `~` 展開済みの環境変数。
    pub env: BTreeMap<String, String>,
    pub prompt: Option<String>,
    /// fixed フォールバック用のリセット定義。
    pub reset_weekday: Option<String>,
    pub reset_time: Option<String>,
    pub timezone: Option<String>,
    /// ai-usage 連携情報（None なら常に fixed 計算）。
    pub ai_usage: Option<RuntimeAiUsage>,
    /// 適用する fallback 方針。
    pub fallback: UsageFallback,
    /// 適用する window 方針。
    pub window: UsageWindowPolicy,
}

/// RuntimeAgent に紐づく ai-usage 照合情報。
#[derive(Debug, Clone)]
pub struct RuntimeAiUsage {
    /// ai-usage --json の `profile` と照合する値。
    pub profile: String,
    /// ai-usage --json の `provider` と照合する値。
    pub provider: String,
}

#[derive(Debug, Deserialize)]
pub struct Scan {
    pub base_dirs: Vec<String>,
    #[serde(default)]
    pub recursive: bool,
    pub username: Option<String>,
    #[serde(default = "default_true")]
    pub public_first: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct Target {
    pub directory: String,
    pub prompt: Option<String>,
    /// true の場合、このターゲットを実行リストの末尾に回す（既存順序は安定ソートで維持）
    #[serde(default)]
    pub defer: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config: {}", path.display()))?;
        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config: {}", path.display()))?;
        config.config_dir = path
            .canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        config.validate()?;
        Ok(config)
    }

    /// プロンプト値を解決: `.md` で終わる場合はファイル内容を読み込み、それ以外はそのまま使用。
    /// 相対パスは設定ファイルのディレクトリから解決される。
    pub fn resolve_prompt(&self, value: &str) -> Result<String> {
        if value.ends_with(".md") {
            let expanded = shellexpand::tilde(value);
            let path = PathBuf::from(expanded.as_ref());
            let path = if path.is_absolute() {
                path
            } else {
                self.config_dir.join(path)
            };
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read prompt file: {}", path.display()))?;
            Ok(content.trim().to_string())
        } else {
            Ok(value.to_string())
        }
    }

    fn validate(&self) -> Result<()> {
        if self.agents.is_empty() {
            anyhow::bail!("At least one agent must be configured");
        }
        if self.scan.is_empty() && self.targets.is_empty() {
            anyhow::bail!("Either [[scan]] or [[targets]] must be configured");
        }
        if self.settings.parallelism == 0 {
            anyhow::bail!("parallelism must be at least 1");
        }
        if self.settings.limit == 0 {
            anyhow::bail!("limit must be at least 1");
        }
        if self.settings.rate_limit_threshold == 0 || self.settings.rate_limit_threshold > 100 {
            anyhow::bail!("rate_limit_threshold must be between 1 and 100");
        }
        validate_optional_duration("skip_within", self.settings.skip_within.as_deref())?;
        validate_optional_duration("cleanup_after", self.settings.cleanup_after.as_deref())?;

        // 共有 scope では skip_within を必須にする。
        // skip_within 省略時のカットオフは「実行中のエージェントの前回リセット時刻」であり、
        // エージェント固有の周期に依存する。他エージェントの履歴まで参照する共有 scope で
        // これを使うと、リセット周期の異なるアカウントの履歴へ別アカウントの周期を
        // 当てはめることになり、スキップ範囲が実行するエージェント次第で揺れる。
        // 共有するなら窓は全エージェント共通の絶対時間で決めるべきなので、明示を求める。
        if self.settings.dedup_scope.is_shared() && self.settings.skip_within.is_none() {
            anyhow::bail!(
                "dedup_scope = \"{}\" requires settings.skip_within (the per-agent reset cutoff cannot be shared across agents)",
                self.settings.dedup_scope.label()
            );
        }

        if let Some(global) = &self.ai_usage {
            if global.command.is_empty() {
                anyhow::bail!("ai_usage.command must include at least one element");
            }
            if global.command[0].trim().is_empty() {
                anyhow::bail!("ai_usage.command executable must not be empty");
            }
            let mut seen = std::collections::HashSet::new();
            for p in &global.profiles {
                if p.name.trim().is_empty() {
                    anyhow::bail!("ai_usage profile name must not be empty");
                }
                if !seen.insert(p.name.as_str()) {
                    anyhow::bail!("Duplicate ai_usage profile name: {}", p.name);
                }
                if p.profile.trim().is_empty() {
                    anyhow::bail!(
                        "ai_usage profile '{}' must set a non-empty profile match",
                        p.name
                    );
                }
                for k in p.env.keys() {
                    if !is_valid_env_key(k) {
                        anyhow::bail!("ai_usage profile '{}' has invalid env key: {}", p.name, k);
                    }
                }
            }
        }

        for agent in &self.agents {
            if agent.name.trim().is_empty() {
                anyhow::bail!("Agent name must not be empty");
            }
            if agent.command.is_empty() {
                anyhow::bail!(
                    "Agent '{}' command must include at least one element",
                    agent.name
                );
            }
            if agent.command[0].trim().is_empty() {
                anyhow::bail!("Agent '{}' executable must not be empty", agent.name);
            }
            for k in agent.env.keys() {
                if !is_valid_env_key(k) {
                    anyhow::bail!("Agent '{}' has invalid env key: {}", agent.name, k);
                }
            }

            let uses_ai_usage = self.agent_uses_ai_usage(agent);
            if uses_ai_usage
                && agent
                    .provider
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
            {
                anyhow::bail!(
                    "Agent '{}' uses ai_usage and must set a non-empty provider",
                    agent.name
                );
            }

            // ai-usage 非連携、または fallback=fixed のときは曜日計算が必要。
            let reset_required =
                !uses_ai_usage || self.effective_fallback(agent) == UsageFallback::Fixed;
            if reset_required {
                let wd = agent.reset_weekday.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Agent '{}' requires reset_weekday (no ai-usage or fallback=fixed)",
                        agent.name
                    )
                })?;
                let tm = agent.reset_time.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Agent '{}' requires reset_time (no ai-usage or fallback=fixed)",
                        agent.name
                    )
                })?;
                let tz = agent.timezone.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Agent '{}' requires timezone (no ai-usage or fallback=fixed)",
                        agent.name
                    )
                })?;
                parse_weekday(wd)?;
                parse_time(tm)?;
                tz.parse::<chrono_tz::Tz>()
                    .map_err(|_| anyhow::anyhow!("Invalid timezone: {}", tz))?;
            } else {
                if let Some(wd) = agent.reset_weekday.as_deref() {
                    parse_weekday(wd)?;
                }
                if let Some(tm) = agent.reset_time.as_deref() {
                    parse_time(tm)?;
                }
                if let Some(tz) = agent.timezone.as_deref() {
                    tz.parse::<chrono_tz::Tz>()
                        .map_err(|_| anyhow::anyhow!("Invalid timezone: {}", tz))?;
                }
            }
        }

        // agent × profile の展開が成立するか（未知 profile 参照・env key 不正等を検出）。
        self.expand_runtime_agents()?;
        Ok(())
    }

    /// agent が ai-usage 連携を使うか。
    /// グローバル `[ai_usage] enabled = true` かつ agent が参照 profile を 1 件以上持つ場合のみ。
    /// enabled=false のときは profile 展開せず従来の単一 agent として扱う。
    fn agent_uses_ai_usage(&self, agent: &Agent) -> bool {
        self.ai_usage.as_ref().is_some_and(|g| g.enabled)
            && agent
                .ai_usage
                .as_ref()
                .is_some_and(|u| !u.profiles.is_empty())
    }

    /// agent に適用される fallback 方針（agent 上書き → グローバル → デフォルト）。
    fn effective_fallback(&self, agent: &Agent) -> UsageFallback {
        agent
            .ai_usage
            .as_ref()
            .and_then(|u| u.fallback)
            .or_else(|| self.ai_usage.as_ref().map(|g| g.fallback))
            .unwrap_or_default()
    }

    /// agent に適用される window 方針（agent 上書き → グローバル → デフォルト）。
    fn effective_window(&self, agent: &Agent) -> UsageWindowPolicy {
        agent
            .ai_usage
            .as_ref()
            .and_then(|u| u.window)
            .or_else(|| self.ai_usage.as_ref().map(|g| g.window))
            .unwrap_or_default()
    }

    /// agent × profile を RuntimeAgent に展開する。
    /// ai_usage.profiles が空/未設定の agent は単一の RuntimeAgent（fixed 計算）になる。
    pub fn expand_runtime_agents(&self) -> Result<Vec<RuntimeAgent>> {
        let global = self.ai_usage.as_ref();
        let mut out = Vec::new();
        for agent in &self.agents {
            let window = self.effective_window(agent);
            let fallback = self.effective_fallback(agent);

            if !self.agent_uses_ai_usage(agent) {
                out.push(RuntimeAgent {
                    name: agent.name.clone(),
                    provider: agent.provider.clone(),
                    command: agent.command.clone(),
                    env: expand_env(&agent.env)?,
                    prompt: agent.prompt.clone(),
                    reset_weekday: agent.reset_weekday.clone(),
                    reset_time: agent.reset_time.clone(),
                    timezone: agent.timezone.clone(),
                    ai_usage: None,
                    fallback,
                    window,
                });
                continue;
            }

            let provider = agent
                .provider
                .clone()
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Agent '{}' uses ai_usage and must set a provider",
                        agent.name
                    )
                })?;
            let profile_names = agent
                .ai_usage
                .as_ref()
                .map(|u| u.profiles.as_slice())
                .unwrap_or(&[]);
            // 同一 agent 内での profile 重複参照を弾く。重複すると同名の RuntimeAgent が
            // 複数生成され、status/list での重複表示や list --agent が 2 件目を選べない
            // 挙動を招く。グローバル profiles 側の name 重複は validate() で既に弾いており、
            // 参照側も同じ方針で揃える。
            let mut seen_profiles = std::collections::HashSet::new();
            for pname in profile_names {
                if !seen_profiles.insert(pname.as_str()) {
                    anyhow::bail!(
                        "Agent '{}' references duplicate ai_usage profile '{}'",
                        agent.name,
                        pname
                    );
                }
            }

            // profile を 1 つだけ参照する agent は展開名を agent 名のまま保つ。
            // 「claude-home」が「claude-home-home」になる冗長化を避け、state.json の
            // キー互換（既存の agent 名のまま）も維持する。複数参照時のみサフィックスを付ける。
            let multi_profile = profile_names.len() > 1;

            for pname in profile_names {
                let profile = global
                    .and_then(|g| g.profiles.iter().find(|p| &p.name == pname))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Agent '{}' references unknown ai_usage profile '{}'",
                            agent.name,
                            pname
                        )
                    })?;
                // env マージ: agent.env をベースに profile.env で上書き。
                let mut env = agent.env.clone();
                for (k, v) in &profile.env {
                    env.insert(k.clone(), v.clone());
                }
                let name = if multi_profile {
                    format!("{}-{}", agent.name, profile.name)
                } else {
                    agent.name.clone()
                };
                out.push(RuntimeAgent {
                    name,
                    provider: Some(provider.clone()),
                    command: agent.command.clone(),
                    env: expand_env(&env)?,
                    prompt: agent.prompt.clone(),
                    reset_weekday: agent.reset_weekday.clone(),
                    reset_time: agent.reset_time.clone(),
                    timezone: agent.timezone.clone(),
                    ai_usage: Some(RuntimeAiUsage {
                        profile: profile.profile.clone(),
                        provider: provider.clone(),
                    }),
                    fallback,
                    window,
                });
            }
        }
        if out.is_empty() {
            anyhow::bail!("No runtime agents after expansion");
        }
        Ok(out)
    }
}

/// 環境変数マップの値を `~` 展開し、key を検証する。
/// 値は必ずしもパスとは限らないため、相対パスを config 相対へ解決することはしない。
fn expand_env(env: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (k, v) in env {
        if !is_valid_env_key(k) {
            anyhow::bail!("Invalid environment variable name: {k}");
        }
        out.insert(k.clone(), shellexpand::tilde(v).to_string());
    }
    Ok(out)
}

/// 環境変数名が `[A-Za-z_][A-Za-z0-9_]*` かを判定する。
fn is_valid_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate_optional_duration(field_name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        crate::state::parse_duration(value)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Invalid {field_name}: {e}"))?;
    }
    Ok(())
}

pub fn resolve_directory(dir: &str) -> Result<PathBuf> {
    let expanded = shellexpand::tilde(dir);
    let path = PathBuf::from(expanded.as_ref());
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize_path(&absolute))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

pub fn default_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".config")
        .join("token-burn")
        .join("config.toml")
}

pub fn parse_weekday(s: &str) -> Result<chrono::Weekday> {
    match s.to_lowercase().as_str() {
        "monday" | "mon" => Ok(chrono::Weekday::Mon),
        "tuesday" | "tue" => Ok(chrono::Weekday::Tue),
        "wednesday" | "wed" => Ok(chrono::Weekday::Wed),
        "thursday" | "thu" => Ok(chrono::Weekday::Thu),
        "friday" | "fri" => Ok(chrono::Weekday::Fri),
        "saturday" | "sat" => Ok(chrono::Weekday::Sat),
        "sunday" | "sun" => Ok(chrono::Weekday::Sun),
        _ => anyhow::bail!("Invalid weekday: {}", s),
    }
}

pub fn parse_time(s: &str) -> Result<(u32, u32)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid time format: {} (expected HH:MM)", s);
    }
    let hour: u32 = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid hour: {}", parts[0]))?;
    let minute: u32 = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid minute: {}", parts[1]))?;
    if hour > 23 || minute > 59 {
        anyhow::bail!("Invalid time: {}", s);
    }
    Ok((hour, minute))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn base_config() -> Config {
        Config {
            config_dir: PathBuf::from("."),
            settings: Settings {
                parallelism: 1,
                skip_within: None,
                report_dir: None,
                cleanup_after: None,
                limit: 10,
                rate_limit_threshold: 95,
                dedup_scope: crate::config::DedupScope::Agent,
            },
            prompts: Prompts {
                default: "review".to_string(),
            },
            agents: vec![Agent {
                name: "agent".to_string(),
                command: vec!["echo".to_string()],
                reset_weekday: Some("monday".to_string()),
                reset_time: Some("09:00".to_string()),
                timezone: Some("UTC".to_string()),
                ..Default::default()
            }],
            scan: vec![],
            targets: vec![Target {
                directory: ".".to_string(),
                prompt: None,
                defer: false,
            }],
            ai_usage: None,
        }
    }

    /// 共有 scope は skip_within を必須にする。
    ///
    /// skip_within 省略時のカットオフは実行エージェントの前回リセット時刻であり、
    /// エージェント固有の周期に依存する。共有 scope でこれを使うと、スキップ範囲が
    /// 「どのエージェントで走らせたか」で揺れるため、設定読み込みの時点で弾く。
    #[test]
    fn validate_rejects_shared_dedup_scope_without_skip_within() {
        for scope in [DedupScope::Global, DedupScope::Provider] {
            let mut config = base_config();
            config.settings.dedup_scope = scope;
            config.settings.skip_within = None;

            let err = config
                .validate()
                .expect_err("共有 scope で skip_within 無しは拒否されるべき");
            assert!(err.to_string().contains("skip_within"), "{scope:?}: {err}");
        }
    }

    #[test]
    fn validate_accepts_shared_dedup_scope_with_skip_within() {
        let mut config = base_config();
        config.settings.dedup_scope = DedupScope::Global;
        config.settings.skip_within = Some("2d".to_string());

        config
            .validate()
            .expect("skip_within があれば共有 scope は許可されるべき");
    }

    /// 既定（dedup_scope 省略）は agent = 従来どおり分離。skip_within 無しでも通る。
    #[test]
    fn validate_accepts_default_agent_scope_without_skip_within() {
        let config = base_config();
        assert_eq!(config.settings.dedup_scope, DedupScope::Agent);
        config.validate().expect("既定は skip_within 不要");
    }

    /// dedup_scope は TOML の小文字表記でパースでき、省略時は agent になる。
    #[test]
    fn dedup_scope_parses_from_toml() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            dedup_scope: DedupScope,
        }

        let parsed: Wrapper = toml::from_str("dedup_scope = \"global\"").expect("parse");
        assert_eq!(parsed.dedup_scope, DedupScope::Global);

        let parsed: Wrapper = toml::from_str("dedup_scope = \"provider\"").expect("parse");
        assert_eq!(parsed.dedup_scope, DedupScope::Provider);

        let parsed: Wrapper = toml::from_str("").expect("parse");
        assert_eq!(
            parsed.dedup_scope,
            DedupScope::Agent,
            "省略時は従来の挙動（分離）"
        );
    }

    #[test]
    fn dedup_scope_is_shared_only_for_cross_agent_scopes() {
        assert!(DedupScope::Global.is_shared());
        assert!(DedupScope::Provider.is_shared());
        assert!(!DedupScope::Agent.is_shared());
    }

    #[test]
    fn validate_rejects_empty_agent_command() {
        let mut config = base_config();
        config.agents[0].command = vec![];

        let err = config
            .validate()
            .expect_err("empty command must be rejected");
        assert!(err.to_string().contains("include at least one element"));
    }

    #[test]
    fn validate_rejects_empty_agent_executable() {
        let mut config = base_config();
        config.agents[0].command = vec!["".to_string(), "-p".to_string()];

        let err = config
            .validate()
            .expect_err("empty executable must be rejected");
        assert!(err.to_string().contains("executable must not be empty"));
    }

    #[test]
    fn validate_accepts_non_empty_agent_command() {
        let config = base_config();
        config
            .validate()
            .expect("valid agent command should pass validation");
    }

    #[test]
    fn parse_weekday_accepts_full_and_short_names() {
        assert_eq!(parse_weekday("monday").unwrap(), chrono::Weekday::Mon);
        assert_eq!(parse_weekday("Mon").unwrap(), chrono::Weekday::Mon);
        assert_eq!(parse_weekday("FRIDAY").unwrap(), chrono::Weekday::Fri);
        assert_eq!(parse_weekday("sun").unwrap(), chrono::Weekday::Sun);
    }

    #[test]
    fn parse_weekday_rejects_invalid() {
        assert!(parse_weekday("funday").is_err());
        assert!(parse_weekday("").is_err());
    }

    #[test]
    fn parse_time_valid() {
        assert_eq!(parse_time("09:00").unwrap(), (9, 0));
        assert_eq!(parse_time("23:59").unwrap(), (23, 59));
        assert_eq!(parse_time("00:00").unwrap(), (0, 0));
    }

    #[test]
    fn parse_time_rejects_invalid() {
        assert!(parse_time("24:00").is_err());
        assert!(parse_time("09:60").is_err());
        assert!(parse_time("9").is_err());
        assert!(parse_time("09:00:00").is_err());
    }

    #[test]
    fn resolve_prompt_literal_string() {
        let config = base_config();
        let result = config.resolve_prompt("review code").unwrap();
        assert_eq!(result, "review code");
    }

    #[test]
    fn resolve_prompt_reads_md_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prompt_path = tmp.path().join("test.md");
        std::fs::write(&prompt_path, "  file content  ").unwrap();
        let mut config = base_config();
        config.config_dir = tmp.path().to_path_buf();
        let result = config.resolve_prompt("test.md").unwrap();
        assert_eq!(result, "file content");
    }

    #[test]
    fn resolve_prompt_missing_md_file_returns_error() {
        let config = base_config();
        assert!(config.resolve_prompt("nonexistent.md").is_err());
    }

    #[test]
    fn validate_rejects_empty_agents() {
        let mut config = base_config();
        config.agents = vec![];
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_parallelism() {
        let mut config = base_config();
        config.settings.parallelism = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_rate_limit_threshold() {
        let mut config = base_config();
        config.settings.rate_limit_threshold = 0;
        let err = config.validate().expect_err("threshold=0 は拒否されるべき");
        assert!(
            err.to_string()
                .contains("rate_limit_threshold must be between 1 and 100")
        );
    }

    #[test]
    fn validate_rejects_over_100_rate_limit_threshold() {
        // u8 なので最大 255 だが、101 以上は拒否される
        let mut config = base_config();
        config.settings.rate_limit_threshold = 101;
        let err = config
            .validate()
            .expect_err("threshold=101 は拒否されるべき");
        assert!(
            err.to_string()
                .contains("rate_limit_threshold must be between 1 and 100")
        );
    }

    #[test]
    fn validate_rejects_zero_limit() {
        let mut config = base_config();
        config.settings.limit = 0;
        let err = config.validate().expect_err("limit=0 は拒否されるべき");
        assert!(err.to_string().contains("limit must be at least 1"));
    }

    #[test]
    fn validate_rejects_no_scan_or_targets() {
        let mut config = base_config();
        config.scan = vec![];
        config.targets = vec![];
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_whitespace_only_agent_name() {
        let mut config = base_config();
        config.agents[0].name = "   ".to_string();
        let err = config
            .validate()
            .expect_err("空白のみのエージェント名は拒否されるべき");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_rejects_whitespace_only_executable() {
        let mut config = base_config();
        config.agents[0].command = vec!["  ".to_string()];
        let err = config
            .validate()
            .expect_err("空白のみの実行ファイル名は拒否されるべき");
        assert!(err.to_string().contains("executable must not be empty"));
    }

    #[test]
    fn validate_rejects_invalid_timezone() {
        let mut config = base_config();
        config.agents[0].timezone = Some("Invalid/Zone".to_string());
        let err = config
            .validate()
            .expect_err("無効なタイムゾーンは拒否されるべき");
        assert!(err.to_string().contains("Invalid timezone"));
    }

    #[test]
    fn validate_rejects_invalid_skip_within() {
        let mut config = base_config();
        config.settings.skip_within = Some("broken".to_string());
        let err = config
            .validate()
            .expect_err("無効な skip_within は拒否されるべき");
        assert!(err.to_string().contains("Invalid skip_within"));
    }

    #[test]
    fn validate_rejects_invalid_cleanup_after() {
        let mut config = base_config();
        config.settings.cleanup_after = Some("broken".to_string());
        let err = config
            .validate()
            .expect_err("無効な cleanup_after は拒否されるべき");
        assert!(err.to_string().contains("Invalid cleanup_after"));
    }

    #[test]
    fn parse_time_rejects_whitespace_padded() {
        assert!(parse_time(" 09:00").is_err());
        assert!(parse_time("09:00 ").is_err());
    }

    #[test]
    fn resolve_prompt_empty_string_returns_empty() {
        let config = base_config();
        let result = config.resolve_prompt("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn resolve_directory_absolute_path_unchanged() {
        let path = resolve_directory("/tmp/some-repo").expect("絶対パスは解決されるべき");
        assert_eq!(path, std::path::PathBuf::from("/tmp/some-repo"));
        assert!(path.is_absolute());
    }

    #[test]
    fn resolve_directory_tilde_expansion() {
        let path = resolve_directory("~/test-dir").expect("チルダは展開されるべき");
        assert!(path.is_absolute());
        assert!(!path.to_string_lossy().contains('~'));
        assert!(path.to_string_lossy().ends_with("test-dir"));
    }

    #[test]
    fn normalize_path_handles_parent_dir() {
        let path = normalize_path(Path::new("/a/b/../c"));
        assert_eq!(path, PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_path_handles_current_dir() {
        let path = normalize_path(Path::new("/a/./b/./c"));
        assert_eq!(path, PathBuf::from("/a/b/c"));
    }

    #[test]
    fn normalize_path_parent_at_root() {
        // ルートを超える .. はルートに留まる
        let path = normalize_path(Path::new("/a/../.."));
        assert_eq!(path, PathBuf::from("/"));
    }

    #[test]
    fn default_config_path_is_absolute() {
        let path = default_config_path();
        // ホームディレクトリが取得できない場合は "~" になるが、
        // 通常は絶対パスになる
        assert!(path.to_string_lossy().contains("config.toml"));
        assert!(path.to_string_lossy().contains("token-burn"));
    }

    #[test]
    fn parse_time_rejects_negative_values() {
        // 負の値は数値パースで弾かれる
        assert!(parse_time("-1:00").is_err());
        assert!(parse_time("09:-5").is_err());
    }

    #[test]
    fn parse_time_rejects_non_numeric() {
        assert!(parse_time("ab:cd").is_err());
        assert!(parse_time("9.5:00").is_err());
    }

    #[test]
    fn parse_weekday_case_insensitive_mixed() {
        // 大文字小文字混在でも受け付ける
        assert_eq!(parse_weekday("MoNdAy").unwrap(), chrono::Weekday::Mon);
        assert_eq!(parse_weekday("TUESDAY").unwrap(), chrono::Weekday::Tue);
    }

    #[test]
    fn parse_weekday_rejects_partial_names() {
        // 2文字の短縮形は受け付けない
        assert!(parse_weekday("mo").is_err());
        assert!(parse_weekday("fr").is_err());
    }

    #[test]
    fn validate_allows_duplicate_agent_names() {
        // 同名エージェントは許容されている（重複禁止ルールなし）
        let mut config = base_config();
        config.agents.push(config.agents[0].clone());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let result = Config::load(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn load_invalid_toml_returns_error() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "not valid toml {{{{").unwrap();
        let result = Config::load(&config_path);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_prompt_absolute_md_path() {
        let tmp = TempDir::new().unwrap();
        let prompt_path = tmp.path().join("absolute.md");
        std::fs::write(&prompt_path, "absolute content").unwrap();

        let config = base_config();
        let result = config
            .resolve_prompt(&prompt_path.to_string_lossy())
            .unwrap();
        assert_eq!(result, "absolute content");
    }

    #[test]
    fn resolve_directory_normalizes_relative_segments() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let _cwd_guard = crate::test_support::CwdGuard::switch_to(tmp.path());

        let expected = std::env::current_dir()
            .expect("cwd should be available")
            .join("repo");
        let path = resolve_directory("./nested/../repo").expect("relative path should resolve");

        assert_eq!(path, expected);
        assert!(path.is_absolute());
    }

    #[test]
    // normalize_path に空パス（""）を渡した場合は空の PathBuf を返す
    fn normalize_path_empty_string_returns_empty() {
        let path = normalize_path(Path::new(""));
        assert_eq!(path, PathBuf::new());
    }

    #[test]
    // parse_time の境界値 "00:59" は有効な時刻として受け付ける
    fn parse_time_boundary_00_59_is_valid() {
        let result = parse_time("00:59").unwrap();
        assert_eq!(result, (0, 59));
    }

    #[test]
    // scan と targets が両方存在する場合は validate が成功する（正常系の網羅）
    fn validate_accepts_both_scan_and_targets() {
        let mut config = base_config();
        config.scan = vec![Scan {
            base_dirs: vec![".".to_string()],
            recursive: false,
            username: None,
            public_first: true,
            exclude: vec![],
        }];
        config.targets = vec![Target {
            directory: ".".to_string(),
            prompt: None,
            defer: false,
        }];
        config
            .validate()
            .expect("scan と targets が両方ある場合は成功するべき");
    }

    #[test]
    fn validate_rejects_invalid_reset_weekday() {
        let mut config = base_config();
        config.agents[0].reset_weekday = Some("notaday".to_string());
        let err = config.validate().expect_err("無効な曜日は拒否されるべき");
        assert!(err.to_string().contains("Invalid weekday"));
    }

    #[test]
    fn validate_rejects_invalid_reset_time() {
        let mut config = base_config();
        config.agents[0].reset_time = Some("25:00".to_string());
        let err = config.validate().expect_err("無効な時刻は拒否されるべき");
        assert!(err.to_string().contains("Invalid time"));
    }

    #[test]
    fn resolve_prompt_trims_whitespace_from_md_file() {
        let tmp = TempDir::new().unwrap();
        let prompt_path = tmp.path().join("whitespace.md");
        std::fs::write(&prompt_path, "\n  content with whitespace  \n\n").unwrap();
        let mut config = base_config();
        config.config_dir = tmp.path().to_path_buf();
        let result = config.resolve_prompt("whitespace.md").unwrap();
        assert_eq!(result, "content with whitespace");
    }

    #[test]
    fn normalize_path_multiple_parent_dirs() {
        let path = normalize_path(Path::new("/a/b/c/../../d"));
        assert_eq!(path, PathBuf::from("/a/d"));
    }

    #[test]
    fn normalize_path_only_root() {
        let path = normalize_path(Path::new("/"));
        assert_eq!(path, PathBuf::from("/"));
    }

    #[test]
    // resolve_prompt で .md 以外の拡張子（.txt）はファイル読み込みではなくリテラルとして扱われる
    fn resolve_prompt_non_md_extension_is_treated_as_literal() {
        let tmp = TempDir::new().unwrap();
        let txt_path = tmp.path().join("prompt.txt");
        std::fs::write(&txt_path, "should not be read").unwrap();

        let mut config = base_config();
        config.config_dir = tmp.path().to_path_buf();

        // .txt ファイルへのパス文字列をそのままリテラルとして返す
        let value = txt_path.to_string_lossy().to_string();
        let result = config.resolve_prompt(&value).unwrap();
        assert_eq!(result, value);
    }

    fn ai_usage_global(enabled: bool, profiles: Vec<(&str, &str)>) -> AiUsageConfig {
        AiUsageConfig {
            enabled,
            command: default_ai_usage_command(),
            window: UsageWindowPolicy::Weekly,
            fallback: UsageFallback::Fixed,
            state_window: StateWindowPolicy::Weekly,
            profiles: profiles
                .into_iter()
                .map(|(name, profile)| AiUsageProfile {
                    name: name.to_string(),
                    profile: profile.to_string(),
                    env: BTreeMap::new(),
                })
                .collect(),
        }
    }

    fn agent_ai_usage(profiles: Vec<&str>, fallback: Option<UsageFallback>) -> AgentAiUsage {
        AgentAiUsage {
            profiles: profiles.into_iter().map(String::from).collect(),
            window: None,
            fallback,
        }
    }

    #[test]
    fn expand_runtime_agents_without_ai_usage_returns_one_per_agent() {
        let config = base_config();
        let agents = config.expand_runtime_agents().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "agent");
        assert!(agents[0].ai_usage.is_none());
    }

    #[test]
    fn expand_runtime_agents_expands_profiles() {
        let mut config = base_config();
        config.agents[0].provider = Some("claude".to_string());
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["work", "home"], None));
        config.ai_usage = Some(ai_usage_global(
            true,
            vec![("work", "Work"), ("home", "Home")],
        ));

        let agents = config.expand_runtime_agents().unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "agent-work");
        assert_eq!(agents[1].name, "agent-home");
        assert_eq!(agents[0].ai_usage.as_ref().unwrap().profile, "Work");
        assert_eq!(agents[0].provider.as_deref(), Some("claude"));
    }

    #[test]
    fn expand_merges_profile_env_over_agent_env() {
        let mut config = base_config();
        let mut agent_env = BTreeMap::new();
        agent_env.insert("BASE".to_string(), "1".to_string());
        agent_env.insert("OVERRIDE".to_string(), "agent".to_string());
        config.agents[0].env = agent_env;
        config.agents[0].provider = Some("claude".to_string());
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["work"], None));

        let mut prof_env = BTreeMap::new();
        prof_env.insert("OVERRIDE".to_string(), "profile".to_string());
        prof_env.insert("PROF".to_string(), "2".to_string());
        let mut global = ai_usage_global(true, vec![("work", "Work")]);
        global.profiles[0].env = prof_env;
        config.ai_usage = Some(global);

        let agents = config.expand_runtime_agents().unwrap();
        // agent.env をベースに profile.env が上書きマージされる
        assert_eq!(agents[0].env.get("BASE").map(String::as_str), Some("1"));
        assert_eq!(
            agents[0].env.get("OVERRIDE").map(String::as_str),
            Some("profile")
        );
        assert_eq!(agents[0].env.get("PROF").map(String::as_str), Some("2"));
    }

    #[test]
    fn validate_ai_usage_skip_allows_missing_reset_fields() {
        let mut config = base_config();
        config.agents[0].reset_weekday = None;
        config.agents[0].reset_time = None;
        config.agents[0].timezone = None;
        config.agents[0].provider = Some("claude".to_string());
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["work"], Some(UsageFallback::Skip)));
        config.ai_usage = Some(ai_usage_global(true, vec![("work", "Work")]));
        config
            .validate()
            .expect("fallback=skip では reset_* 省略可");
    }

    #[test]
    fn validate_ai_usage_fixed_requires_reset_fields() {
        let mut config = base_config();
        config.agents[0].reset_weekday = None;
        config.agents[0].provider = Some("claude".to_string());
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["work"], Some(UsageFallback::Fixed)));
        config.ai_usage = Some(ai_usage_global(true, vec![("work", "Work")]));
        let err = config
            .validate()
            .expect_err("fallback=fixed では reset_weekday 必須");
        assert!(err.to_string().contains("reset_weekday"));
    }

    #[test]
    fn validate_rejects_unknown_profile_reference() {
        let mut config = base_config();
        config.agents[0].provider = Some("claude".to_string());
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["nonexistent"], None));
        config.ai_usage = Some(ai_usage_global(true, vec![("work", "Work")]));
        let err = config.validate().expect_err("未知の profile 参照はエラー");
        assert!(err.to_string().contains("unknown ai_usage profile"));
    }

    #[test]
    fn validate_rejects_duplicate_profile_reference() {
        // 同一 agent が同じ profile を 2 回参照すると、同名の RuntimeAgent が二重生成
        // されるため validate で弾く。
        let mut config = base_config();
        config.agents[0].provider = Some("claude".to_string());
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["work", "work"], None));
        config.ai_usage = Some(ai_usage_global(true, vec![("work", "Work")]));
        let err = config
            .validate()
            .expect_err("同一 profile の重複参照はエラー");
        assert!(err.to_string().contains("duplicate ai_usage profile"));
    }

    #[test]
    fn validate_rejects_ai_usage_without_provider() {
        let mut config = base_config();
        config.agents[0].provider = None;
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["work"], None));
        config.ai_usage = Some(ai_usage_global(true, vec![("work", "Work")]));
        let err = config
            .validate()
            .expect_err("provider 無しの ai_usage 連携はエラー");
        assert!(err.to_string().contains("provider"));
    }

    #[test]
    fn validate_rejects_invalid_env_key() {
        let mut config = base_config();
        let mut env = BTreeMap::new();
        env.insert("1BAD".to_string(), "x".to_string());
        config.agents[0].env = env;
        let err = config
            .validate()
            .expect_err("数字始まりの env key はエラー");
        assert!(err.to_string().contains("invalid env key"));
    }

    #[test]
    fn expand_disabled_ai_usage_does_not_expand_profiles() {
        let mut config = base_config();
        config.agents[0].provider = Some("claude".to_string());
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["work"], None));
        config.ai_usage = Some(ai_usage_global(false, vec![("work", "Work")]));
        // enabled=false なので profile 展開せず単一 agent（reset_* は base_config が保持）
        let agents = config.expand_runtime_agents().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "agent");
        assert!(agents[0].ai_usage.is_none());
    }

    #[test]
    fn is_valid_env_key_rules() {
        assert!(is_valid_env_key("CLAUDE_CONFIG_DIR"));
        assert!(is_valid_env_key("_X"));
        assert!(!is_valid_env_key("1A"));
        assert!(!is_valid_env_key("A-B"));
        assert!(!is_valid_env_key(""));
    }

    #[test]
    fn expand_single_profile_keeps_agent_name() {
        // profile を 1 つだけ参照する agent は展開名が agent 名のまま（サフィックス無し）。
        let mut config = base_config();
        config.agents[0].name = "claude-home".to_string();
        config.agents[0].provider = Some("claude".to_string());
        config.agents[0].ai_usage = Some(agent_ai_usage(vec!["home"], None));
        config.ai_usage = Some(ai_usage_global(true, vec![("home", "Home")]));
        let agents = config.expand_runtime_agents().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "claude-home");
        assert_eq!(agents[0].ai_usage.as_ref().unwrap().profile, "Home");
    }
}
