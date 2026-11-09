use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value, value};

use super::Page;
use super::schema::{Field, Kind, Section};

const STARTER: &str = r#"# Edit with: token-burn config
[settings]
parallelism = 3
limit = 10
rate_limit_threshold = 95
skip_within = "1d"
dedup_scope = "agent"

[prompts]
default = "Review this project, fix definite bugs, and add tests for the fixes."

[[agents]]
name = "claude"
provider = "claude"
command = ["claude", "--dangerously-skip-permissions", "--model", "opus"]
reset_weekday = "monday"
reset_time = "09:00"
timezone = "UTC"

[[scan]]
base_dirs = ["~/GitHub"]
recursive = false
public_first = true
"#;

pub(super) struct Draft {
    pub path: PathBuf,
    original: Option<String>,
    baseline: String,
    crlf: bool,
    pub doc: DocumentMut,
}

impl Draft {
    pub fn open(path: &Path) -> Result<Self> {
        // Follow existing symlinks so atomic replacement preserves the link itself.
        let path = match fs::symlink_metadata(path) {
            Ok(_) => path
                .canonicalize()
                .context("設定ファイルの実体を解決できません")?,
            Err(error) if error.kind() == ErrorKind::NotFound => std::path::absolute(path)?,
            Err(error) => return Err(error).context("設定ファイルを確認できません"),
        };
        let original = read_optional(&path)?;
        let crlf = original
            .as_ref()
            .is_some_and(|text| text.contains("\r\n") && !text.replace("\r\n", "").contains('\n'));
        let doc = original
            .as_deref()
            .unwrap_or(STARTER)
            .parse::<DocumentMut>()
            .map_err(|error| {
                // Never quote source lines; they may include passwords.
                anyhow::anyhow!("設定の TOML を読めません: {}", error.message())
            })?;
        let baseline = doc.to_string();
        Ok(Self {
            path,
            original,
            baseline,
            crlf,
            doc,
        })
    }

    pub fn is_new(&self) -> bool {
        self.original.is_none()
    }
    pub fn dirty(&self) -> bool {
        self.is_new() || self.baseline != self.doc.to_string()
    }

    fn root(&self, section: Section) -> Option<&Item> {
        let root = self.doc.get(section.key())?;
        if section == Section::Profiles {
            root.get("profiles")
        } else {
            Some(root)
        }
    }

    fn root_mut(&mut self, section: Section) -> Result<&mut Item> {
        let item = self
            .doc
            .as_item_mut()
            .get_mut(section.key())
            .context("設定セクションがテーブルではありません")?;
        if item.is_none() {
            *item = if section.repeated() && section != Section::Profiles {
                Item::ArrayOfTables(ArrayOfTables::new())
            } else {
                Item::Table(Table::new())
            };
        }
        if section == Section::Profiles {
            let item = item
                .get_mut("profiles")
                .context("ai_usage がテーブルではありません")?;
            if item.is_none() {
                *item = Item::ArrayOfTables(ArrayOfTables::new());
            }
            Ok(item)
        } else {
            Ok(item)
        }
    }

    pub fn count(&self, section: Section) -> usize {
        self.root(section).map_or(0, |item| {
            item.as_array_of_tables()
                .map(ArrayOfTables::len)
                .or_else(|| item.as_array().map(Array::len))
                .unwrap_or(0)
        })
    }

    fn group(&self, page: Page) -> Option<&Item> {
        let item = self.root(page.section)?;
        if let Some(index) = page.index {
            item.get(index)
        } else {
            Some(item)
        }
    }

    fn group_mut(&mut self, page: Page) -> Result<&mut Item> {
        let item = self.root_mut(page.section)?;
        if let Some(index) = page.index {
            item.get_mut(index).context("編集対象がありません")
        } else {
            Ok(item)
        }
    }

    fn item(&self, page: Page, key: &str) -> Option<&Item> {
        let mut item = self.group(page)?;
        for key in key.split('.') {
            item = item.get(key)?;
        }
        Some(item)
    }

    pub fn name(&self, page: Page) -> String {
        let key = match page.section {
            Section::Agents | Section::Profiles => "name",
            Section::Scan => "base_dirs",
            Section::Targets => "directory",
            _ => return page.section.title().into(),
        };
        let name = self
            .item(page, key)
            .and_then(|item| {
                item.as_str().map(str::to_string).or_else(|| {
                    item.as_array()
                        .and_then(|items| items.get(0))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
            })
            .unwrap_or_else(|| "未設定".into());
        format!("{}: {name}", page.section.title())
    }

    pub fn profile_options(&self) -> Vec<(String, String)> {
        (0..self.count(Section::Profiles))
            .filter_map(|index| {
                let page = Page {
                    section: Section::Profiles,
                    index: Some(index),
                };
                let name = self.item(page, "name")?.as_str()?;
                let profile = self
                    .item(page, "profile")
                    .and_then(Item::as_str)
                    .unwrap_or("");
                Some((name.into(), format!("{name} ({profile})")))
            })
            .collect()
    }

    pub fn input(&self, page: Page, field: Field) -> Result<String> {
        let item = self.item(page, field.key);
        Ok(match field.kind {
            Kind::Number(_, _, default) => item
                .and_then(Item::as_integer)
                .map(|value| value.to_string())
                .unwrap_or(default.into()),
            Kind::Bool(default) => item.and_then(Item::as_bool).unwrap_or(default).to_string(),
            Kind::Choice(_, default) => item.and_then(Item::as_str).unwrap_or(default).into(),
            Kind::Lines | Kind::Profiles => {
                let defaults = if page.section == Section::Usage && field.key == "command" {
                    vec!["ai-usage".into(), "--json".into()]
                } else {
                    Vec::new()
                };
                let values = item
                    .and_then(Item::as_array)
                    .map(|array| {
                        array
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or(defaults);
                lines_text(&values)
            }
            Kind::Env => item
                .and_then(Item::as_table_like)
                .map(|table| {
                    table
                        .iter()
                        .map(|(key, value)| {
                            format!(
                                "{} = {}",
                                toml_edit::Key::new(key),
                                toml_edit::Value::from(value.as_str().unwrap_or(""))
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            _ => item.and_then(Item::as_str).unwrap_or("").into(),
        })
    }

    pub fn display(&self, page: Page, field: Field) -> String {
        if self.item(page, field.key).is_none() && field.optional {
            return match field.key {
                "skip_within" => "未設定（前回リセット以降）".into(),
                "report_dir" => "未設定（~/Documents/token-burn）".into(),
                "cleanup_after" => "未設定（7d）".into(),
                "provider" => "未設定（コマンドから判定）".into(),
                "codex_output" => "未設定（auto）".into(),
                "ai_usage.profiles" => "未設定（固定スケジュール）".into(),
                "ai_usage.window" | "ai_usage.fallback" => {
                    let key = field.key.trim_start_matches("ai_usage.");
                    let default = if key == "window" { "weekly" } else { "fixed" };
                    let value = self
                        .doc
                        .get("ai_usage")
                        .and_then(|item| item.get(key))
                        .and_then(Item::as_str)
                        .unwrap_or(default);
                    format!("共通設定を使用（{value}）")
                }
                "env" => "未設定（起動元の環境を継承）".into(),
                "username" => "未設定（絞り込みなし）".into(),
                "exclude" => "未設定（除外なし）".into(),
                "prompt" => "未設定（共通・エージェントの指示を使用）".into(),
                "resume" => "未設定（組み込みの継続指示）".into(),
                _ => "未設定".into(),
            };
        }
        let text = self.input(page, field).unwrap_or_default();
        match field.kind {
            Kind::Bool(_) => if text == "true" { "オン" } else { "オフ" }.into(),
            Kind::Env => format!("{} 個の変数（Enter で編集）", text.lines().count()),
            Kind::Lines if field.key == "command" => self
                .item(page, field.key)
                .and_then(Item::as_array)
                .map(|array| {
                    crate::display::format_command(
                        &array
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap_or_else(|| "ai-usage --json".into()),
            _ => text.replace('\n', " / "),
        }
    }

    pub fn set(&mut self, page: Page, field: Field, text: Option<&str>) -> Result<()> {
        let replacement = match text {
            None => {
                ensure!(field.optional, "必須項目は削除できません");
                None
            }
            Some("") if field.optional => None,
            Some(text) => Some(match field.kind {
                Kind::Number(min, max, _) => {
                    let number: i64 = text.trim().parse().context("整数を入力してください")?;
                    ensure!(
                        (min..=max).contains(&number),
                        "{min}〜{max} の範囲で指定してください"
                    );
                    value(number)
                }
                Kind::Bool(_) => value(text.parse::<bool>()?),
                Kind::Lines | Kind::Profiles => {
                    let mut array = Array::new();
                    for value in parse_lines(text)? {
                        array.push(value);
                    }
                    ensure!(
                        field.optional || !array.is_empty(),
                        "1 つ以上の要素を指定してください"
                    );
                    Item::Value(Value::Array(array))
                }
                Kind::Env => {
                    let variables: BTreeMap<String, String> =
                        toml::from_str(text).map_err(|error| {
                            anyhow::anyhow!(
                                "環境変数は KEY = \"value\" の形式です: {}",
                                error.message()
                            )
                        })?;
                    let mut table = InlineTable::new();
                    for (key, value) in variables {
                        table.insert(&key, Value::from(value));
                    }
                    Item::Value(Value::InlineTable(table))
                }
                _ => {
                    ensure!(
                        field.optional || !text.trim().is_empty(),
                        "空にはできません"
                    );
                    value(text)
                }
            }),
        };
        // Returning without assignment also preserves the original formatting on Enter.
        if let Some(text) = text {
            let typed = self
                .item(page, field.key)
                .is_some_and(|item| match field.kind {
                    Kind::Number(..) => item.as_integer().is_some(),
                    Kind::Bool(_) => item.as_bool().is_some(),
                    Kind::Lines | Kind::Profiles => item
                        .as_array()
                        .is_some_and(|array| array.iter().all(|value| value.as_str().is_some())),
                    Kind::Env => item.as_table_like().is_some_and(|table| {
                        table.iter().all(|(_, value)| value.as_str().is_some())
                    }),
                    _ => item.as_str().is_some(),
                });
            if typed && text == self.input(page, field)? {
                return Ok(());
            }
        } else if self.item(page, field.key).is_none() {
            return Ok(());
        }

        let old_name = if page.section == Section::Profiles && field.key == "name" {
            self.item(page, "name")
                .and_then(Item::as_str)
                .map(str::to_string)
        } else {
            None
        };
        if let (Some(old), Some(new)) = (old_name.as_deref(), text) {
            ensure!(
                !self
                    .profile_options()
                    .iter()
                    .any(|(name, _)| name == new && name != old),
                "同じ参照名のプロファイルが既にあります"
            );
        }
        let mut parts = field.key.split('.').peekable();
        let mut item = self.group_mut(page)?;
        while let Some(key) = parts.next() {
            if parts.peek().is_none() {
                let table = item
                    .as_table_like_mut()
                    .context("編集対象がテーブルではありません")?;
                if let Some(mut replacement) = replacement {
                    if let (Some(old), Some(new)) = (
                        table.get(key).and_then(Item::as_value),
                        replacement.as_value_mut(),
                    ) {
                        *new.decor_mut() = old.decor().clone();
                    }
                    table.insert(key, replacement);
                } else {
                    table.remove(key);
                }
                break;
            }
            if item.get(key).is_none() && replacement.is_none() {
                return Ok(());
            }
            item = item
                .get_mut(key)
                .context("入れ子の設定がテーブルではありません")?;
            if item.is_none() {
                let mut table = Table::new();
                table.set_implicit(true);
                *item = Item::Table(table);
            }
        }
        if let (Some(old), Some(new)) = (old_name, text) {
            self.rename_references(&old, new);
        }
        Ok(())
    }

    fn rename_references(&mut self, old: &str, new: &str) {
        for index in 0..self.count(Section::Agents) {
            if let Ok(agent) = self.group_mut(Page {
                section: Section::Agents,
                index: Some(index),
            }) && let Some(values) = agent
                .get_mut("ai_usage")
                .and_then(|item| item.get_mut("profiles"))
                .and_then(Item::as_array_mut)
            {
                for item in values.iter_mut() {
                    if item.as_str() == Some(old) {
                        *item = Value::from(new);
                    }
                }
            }
        }
    }

    pub fn add(&mut self, section: Section, preset: &str) -> Result<usize> {
        ensure!(section.repeated(), "このセクションは追加できません");
        let mut table = Table::new();
        match section {
            Section::Agents => {
                let existing: Vec<_> = (0..self.count(section))
                    .filter_map(|index| {
                        self.item(
                            Page {
                                section,
                                index: Some(index),
                            },
                            "name",
                        )
                        .and_then(Item::as_str)
                    })
                    .collect();
                let mut name = preset.to_string();
                let mut suffix = 2;
                while existing.contains(&name.as_str()) {
                    name = format!("{preset}-{suffix}");
                    suffix += 1;
                }
                table.insert("name", value(name));
                let args: &[&str] = match preset {
                    "claude" => &[
                        "claude",
                        "--dangerously-skip-permissions",
                        "--model",
                        "opus",
                    ],
                    "codex" => &["codex", "exec", "--sandbox", "workspace-write"],
                    _ => &["my-agent"],
                };
                if preset != "custom" {
                    table.insert("provider", value(preset));
                }
                table.insert(
                    "command",
                    Item::Value(Value::Array(args.iter().copied().collect())),
                );
                table.insert("reset_weekday", value("monday"));
                table.insert("reset_time", value("09:00"));
                table.insert("timezone", value("UTC"));
            }
            Section::Scan => {
                table.insert(
                    "base_dirs",
                    Item::Value(Value::Array(["~/GitHub"].into_iter().collect())),
                );
                table.insert("recursive", value(false));
                table.insert("public_first", value(true));
            }
            Section::Targets => {
                table.insert("directory", value("~/GitHub/my-project"));
            }
            Section::Profiles => {
                let existing = self.profile_options();
                let mut name = "work".to_string();
                let mut suffix = 2;
                while existing.iter().any(|(value, _)| *value == name) {
                    name = format!("work-{suffix}");
                    suffix += 1;
                }
                table.insert("name", value(name));
                table.insert("profile", value("Work"));
            }
            _ => unreachable!(),
        }
        let root = self.root_mut(section)?;
        if let Some(array) = root.as_array_of_tables_mut() {
            let index = array.len();
            array.push(table);
            Ok(index)
        } else if let Some(array) = root.as_array_mut() {
            let index = array.len();
            array.push(table.into_inline_table());
            Ok(index)
        } else {
            bail!("追加先がテーブルの配列ではありません")
        }
    }

    pub fn remove(&mut self, page: Page) -> Result<()> {
        let index = page.index.context("削除する対象がありません")?;
        if page.section == Section::Profiles {
            let name = self.item(page, "name").and_then(Item::as_str).unwrap_or("");
            for index in 0..self.count(Section::Agents) {
                let agent = Page {
                    section: Section::Agents,
                    index: Some(index),
                };
                if self
                    .item(agent, "ai_usage.profiles")
                    .and_then(Item::as_array)
                    .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(name)))
                {
                    bail!(
                        "このプロファイルはエージェントが参照しています。先に選択を解除してください。"
                    );
                }
            }
        }
        let root = self.root_mut(page.section)?;
        if let Some(array) = root.as_array_of_tables_mut() {
            array.remove(index);
        } else if let Some(array) = root.as_array_mut() {
            array.remove(index);
        } else {
            bail!("削除先がテーブルの配列ではありません");
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        crate::config::Config::parse(&self.doc.to_string(), &self.path)?;
        Ok(())
    }

    pub fn changes(&self) -> Vec<String> {
        let mut before = BTreeMap::new();
        let mut after = BTreeMap::new();
        if let Some(original) = &self.original
            && let Ok(value) = toml::from_str::<toml::Value>(original)
        {
            flatten("", &value, &mut before);
        }
        if let Ok(value) = toml::from_str::<toml::Value>(&self.doc.to_string()) {
            flatten("", &value, &mut after);
        }
        let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
        keys.into_iter()
            .filter_map(|key| {
                let old = before.get(key);
                let new = after.get(key);
                (old != new).then(|| {
                    format!(
                        "{key}: {} → {}",
                        old.map(|value| preview(key, value))
                            .unwrap_or_else(|| "未設定".into()),
                        new.map(|value| preview(key, value))
                            .unwrap_or_else(|| "削除".into())
                    )
                })
            })
            .collect()
    }

    pub fn save(&mut self) -> Result<Option<PathBuf>> {
        self.validate()?;
        let parent = self.path.parent().context("保存先がありません")?;
        fs::create_dir_all(parent)?;
        let name = self
            .path
            .file_name()
            .context("ファイル名がありません")?
            .to_string_lossy();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(parent.join(format!(".{name}.lock")))?;
        lock.try_lock_exclusive()
            .context("別の設定画面が保存中です。もう一度保存してください")?;
        ensure!(
            !fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.file_type().is_symlink()),
            "保存先が外部でシンボリックリンクに変わりました。開き直してください"
        );
        ensure!(
            read_optional(&self.path)? == self.original,
            "設定が外部で変更されました。上書きせず中止しました。画面を開き直してください"
        );
        let permissions = fs::metadata(&self.path).ok().map(|meta| meta.permissions());
        let backup = if let Some(original) = &self.original {
            let mut backup = tempfile::Builder::new()
                .prefix(&format!("{name}.bak."))
                .tempfile_in(parent)?;
            backup.write_all(original.as_bytes())?;
            backup.as_file().sync_all()?;
            Some(backup.keep().context("バックアップを保存できません")?.1)
        } else {
            None
        };
        let content = if self.crlf {
            self.doc
                .to_string()
                .replace("\r\n", "\n")
                .replace('\n', "\r\n")
        } else {
            self.doc.to_string()
        };
        let mut pending = tempfile::Builder::new()
            .prefix(&format!(".{name}.tmp."))
            .tempfile_in(parent)?;
        if let Some(permissions) = permissions {
            pending.as_file().set_permissions(permissions)?;
        }
        pending.write_all(content.as_bytes())?;
        pending.as_file().sync_all()?;
        pending
            .persist(&self.path)
            .context("設定ファイルを置き換えられません")?;
        self.original = Some(content);
        self.baseline = self.doc.to_string();
        Ok(backup)
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("設定を読み取れません: {}", path.display()))
        }
    }
}

fn flatten(prefix: &str, value: &toml::Value, output: &mut BTreeMap<String, toml::Value>) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                flatten(
                    &if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    },
                    value,
                    output,
                );
            }
        }
        toml::Value::Array(values) if values.iter().any(toml::Value::is_table) => {
            for (index, value) in values.iter().enumerate() {
                flatten(&format!("{prefix}[{}]", index + 1), value, output);
            }
        }
        _ => {
            output.insert(prefix.into(), value.clone());
        }
    }
}

fn preview(key: &str, value: &toml::Value) -> String {
    if key.contains(".env.") {
        "<非表示>".into()
    } else if key.ends_with(".command") {
        value
            .as_array()
            .map(|values| {
                crate::display::format_command(
                    &values
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_else(|| "<コマンド>".into())
    } else {
        value.to_string().replace('\n', " ")
    }
}

pub(super) fn lines_text(values: &[String]) -> String {
    values
        .iter()
        .map(|text| {
            if text.is_empty() || text.contains(['\r', '\n']) || text.starts_with('"') {
                serde_json::to_string(text).unwrap()
            } else {
                text.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn parse_lines(text: &str) -> Result<Vec<String>> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            if line.starts_with('"') {
                serde_json::from_str(line)
                    .context("ダブルクォートで始まる要素は JSON 文字列で指定してください")
            } else {
                Ok(line.into())
            }
        })
        .collect()
}
