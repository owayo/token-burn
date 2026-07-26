//! 長時間実行ツールの `tool_progress` ハートビート表示。

use anyhow::Result;
use std::io::Write;

/// `tool_progress`: ツール名と経過時間を表示し、長時間処理が生存中だと分かるようにする。
pub(crate) fn handle_tool_progress(value: &serde_json::Value, out: &mut impl Write) -> Result<()> {
    let tool_name = value["tool_name"].as_str().unwrap_or("");
    let Some(elapsed_seconds) = value["elapsed_time_seconds"].as_u64() else {
        return Ok(());
    };
    if tool_name.is_empty() {
        return Ok(());
    }

    writeln!(
        out,
        "\x1b[2m  \u{23f1} {} running ({})\x1b[0m",
        tool_name,
        format_elapsed(elapsed_seconds)
    )?;
    Ok(())
}

fn format_elapsed(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}h {}m {}s",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
