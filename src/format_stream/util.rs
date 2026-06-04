//! 表示整形に使う小さなヘルパー群。
//! 文字列の切り詰め・数値整形・時刻整形など、副作用のない純粋関数のみを置く。

pub(crate) fn first_string<'a>(value: &'a serde_json::Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|key| value[*key].as_str())
        .unwrap_or("")
}

pub(crate) fn truncate_inline(s: &str, max: usize) -> String {
    let normalized = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_str(&normalized, max)
}

/// tool_result の `content` からサマリー（1 行・短縮済み）を抽出する。
///
/// 失敗時に診断が分かりやすくなるよう、`is_error: true` の tool_result から
/// 先頭の有意な 1 行を取り出して併記する用途で使う。文字列・配列いずれの
/// `content` 形式にも対応する。
pub(crate) fn extract_tool_result_summary(content: &serde_json::Value) -> String {
    let raw = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            // 配列形式の場合は `text` フィールドを連結する
            arr.iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => String::new(),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // <tool_use_error> が複数行を包む実データでもタグを残さない。
    let without_open = trimmed.strip_prefix("<tool_use_error>").unwrap_or(trimmed);
    let trimmed = without_open
        .strip_suffix("</tool_use_error>")
        .unwrap_or(without_open)
        .trim();
    // 先頭の有意な 1 行（空行を除く）を取得
    let first_line = trimmed
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    truncate_str(first_line, 120)
}

pub(crate) fn format_byte_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= MB {
        format!("{:.1}MB", bytes_f / MB)
    } else if bytes_f >= KB {
        format!("{:.1}KB", bytes_f / KB)
    } else {
        format!("{bytes}B")
    }
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    let mut iter = s.chars();
    let mut prefix = String::new();
    for _ in 0..max {
        match iter.next() {
            Some(ch) => prefix.push(ch),
            None => return prefix,
        }
    }
    if iter.next().is_none() {
        return prefix;
    }
    let kept: String = prefix.chars().take(max.saturating_sub(3)).collect();
    format!("{kept}...")
}

pub(crate) fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// トークン上限などのサイズを `1M` / `200K` / `64K` の単位で表現する。
/// 1000 未満や端数のあるサイズはカンマ区切りの数値にフォールバックする。
pub(crate) fn format_token_size(n: u64) -> String {
    if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 && n.is_multiple_of(1_000) {
        format!("{}K", n / 1_000)
    } else {
        format_number(n)
    }
}

/// Unix epoch ミリ秒（古い実装との互換で秒も許容）をローカル時刻の短い表記にする。
pub(crate) fn format_epoch_millis_clock(timestamp: i64) -> Option<String> {
    let seconds = if timestamp > 1_000_000_000_000 {
        timestamp / 1000
    } else {
        timestamp
    };
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

/// ミリ秒を短い秒表記に整形する（例: 14837ms → 14.8s）。
pub(crate) fn format_millis_as_seconds(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}
