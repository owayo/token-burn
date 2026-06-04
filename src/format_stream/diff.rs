//! Edit ツールの新旧文字列からカラー差分を生成するモジュール。

use crate::format_stream::util::first_string;

pub(crate) fn format_tool_diff(tool_name: &str, input_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(input_json).ok()?;

    match tool_name {
        "Edit" => {
            let old = first_string(&v, &["old_string", "old_str"]);
            let new = first_string(&v, &["new_string", "new_str"]);
            if old.is_empty() && new.is_empty() {
                return None;
            }
            let diff = format_diff_lines(old, new);
            if diff.is_empty() { None } else { Some(diff) }
        }
        _ => None,
    }
}

/// 新旧テキスト間のカラー差分を生成する。
/// 共通のプレフィックス/サフィックス行を検出し、変更部分のみ表示する。
pub(crate) fn format_diff_lines(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        old.lines().collect()
    };
    let new_lines: Vec<&str> = if new.is_empty() {
        Vec::new()
    } else {
        new.lines().collect()
    };

    // 共通プレフィックス行を検出
    let prefix_len = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();

    // 共通サフィックス行を検出（プレフィックス以降）
    let old_rest = &old_lines[prefix_len..];
    let new_rest = &new_lines[prefix_len..];
    let suffix_len = old_rest
        .iter()
        .rev()
        .zip(new_rest.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let old_changed = &old_lines[prefix_len..old_lines.len() - suffix_len];
    let new_changed = &new_lines[prefix_len..new_lines.len() - suffix_len];

    if old_changed.is_empty() && new_changed.is_empty() {
        return String::new();
    }

    let max_context = 2;
    let max_changed = 12;
    let mut result = String::new();

    // プレフィックスからのコンテキスト（末尾N行）
    let ctx_start = prefix_len.saturating_sub(max_context);
    for line in &old_lines[ctx_start..prefix_len] {
        result.push_str(&format!("\x1b[2m    {}\x1b[0m\n", line));
    }

    // 削除行
    for (i, line) in old_changed.iter().enumerate() {
        if i >= max_changed {
            result.push_str(&format!(
                "\x1b[31m  ... ({} more)\x1b[0m\n",
                old_changed.len() - max_changed
            ));
            break;
        }
        result.push_str(&format!("\x1b[31m  - {}\x1b[0m\n", line));
    }

    // 追加行
    for (i, line) in new_changed.iter().enumerate() {
        if i >= max_changed {
            result.push_str(&format!(
                "\x1b[32m  ... ({} more)\x1b[0m\n",
                new_changed.len() - max_changed
            ));
            break;
        }
        result.push_str(&format!("\x1b[32m  + {}\x1b[0m\n", line));
    }

    // サフィックスからのコンテキスト（先頭N行）
    let suffix_start = old_lines.len() - suffix_len;
    let suffix_show = std::cmp::min(suffix_len, max_context);
    for line in &old_lines[suffix_start..suffix_start + suffix_show] {
        result.push_str(&format!("\x1b[2m    {}\x1b[0m\n", line));
    }

    result
}
