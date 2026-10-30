//! モニターペインの描画。
//!
//! ratatui は前回の画面との差分だけを書き換えるので、毎秒描き直してもちらつかない。
//! その代わり、ratatui が数える文字幅（unicode-width）と端末が実際に使うマス数が
//! 食い違うと、差分の書き込み位置がずれて表示が崩れる。⏱ ⏸ ⚠ ⏹ のように幅の扱いが
//! 端末ごとに揺れる絵文字（East Asian Width が N）は使わず、常に 2 マスの絵文字だけを使う。

use std::cmp::Reverse;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::controller::{Controller, Finish, Notice, Phase};
use super::disk::{DiskState, Row, Status, Volume};
use super::load::{LoadHistory, WINDOW, pressure_from_code};
use super::manifest::Manifest;
use super::recovery;
use super::system::Pressure;

/// 値に添えるラベル（ai-usage の LABEL と同じ番号）。
const LABEL: Color = Color::Indexed(245);
/// 値の無いところや補足（ai-usage の MUTED）。
const MUTED: Color = Color::Indexed(242);
/// 4 段階の状態の色（ai-usage の Level と同じ番号）。
const NORMAL: Color = Color::Indexed(35);
const CAUTION: Color = Color::Indexed(178);
const WARNING: Color = Color::Indexed(208);
const CRITICAL: Color = Color::Indexed(196);
/// CPU のグラフの色。量を表す 1 系列なので 1 色に固定する（状態の色と混ぜない）。
const CPU_SERIES: Color = Color::Indexed(39);
/// メモリのグラフの色（逼迫していない区間）。逼迫した区間だけ状態の色にする。
const MEMORY_SERIES: Color = Color::Indexed(141);

/// グラフの文字（下から 1/8 刻み）。
const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// 「  CPU   」のようなラベルの欄の幅。
const LABEL_FIELD: usize = 8;
/// グラフと値の間。
const GAP: usize = 2;
/// グラフをこれより短くしてまで値の詳細を出さない幅。
const COMFORT_SPARK: usize = 20;
const MAX_SPARK: usize = 120;
/// ディスクのバーの幅（ai-usage の TUI と同じ考え方）。
const MIN_BAR: usize = 6;
const COMFORT_BAR: usize = 16;
const MAX_BAR: usize = 40;
/// ボリューム名の欄の最大幅。
const MAX_NAME: usize = 16;
/// ディスクの使用率の段階の境界（ai-usage と同じ）。満杯に近い状態で使われるため、
/// 利用枠より高めに置く。
const DISK_LEVELS: [f64; 3] = [80.0, 90.0, 95.0];
/// 進捗バーの幅（ペインが狭いときは MIN_PROGRESS_BAR まで縮める）。
const PROGRESS_BAR: usize = 20;
const MIN_PROGRESS_BAR: usize = 10;

pub struct View<'a> {
    pub manifest: &'a Manifest,
    pub controller: &'a Controller,
    /// ai-usage の statusline。まだ取れていなければ空。
    pub usage: &'a [Line<'static>],
    pub load: &'a LoadHistory,
    pub disk: &'a DiskState,
    /// モニター自身の失敗（stop file を書けなかった等）。畳まずに必ず出す。
    pub alerts: &'a [String],
    /// 一時停止中の回復の確認（手動リセット等による早期解除）の様子。
    pub recovery: &'a recovery::Status,
    /// 履歴の時刻の基準（単調時計）。
    pub now: Instant,
}

pub fn draw(frame: &mut Frame, view: &View) {
    let area = frame.area();
    let lines = layout(view, usize::from(area.width), usize::from(area.height));
    frame.render_widget(Paragraph::new(lines), area);
}

/// 画面の行を組み立てる。
///
/// 高さが足りないときは、次の順に行を割り当てる。最下行の進捗、モニター自身の失敗の警告
/// （stop file を書けなかった等。ワーカーが止まらないことを知らせる）、通知（Ctrl-C の案内を
/// 含む）、エラー（少なくとも 1 件と残りの件数）、本文（見出し・使用量・負荷）。本文は末尾から
/// 削る。進捗の前の空行は、これらをすべて割り当てて余りがあるときだけ置く。
pub fn layout(view: &View, width: usize, height: usize) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let progress = Line::from(progress_line(view.controller, width));
    let mut room = height - 1;

    let mut alerts: Vec<Line<'static>> = view
        .alerts
        .iter()
        .flat_map(|alert| wrap(&format!(" ⛔ {alert}"), width))
        .map(|text| Line::styled(text, Style::new().fg(CRITICAL).add_modifier(Modifier::BOLD)))
        .collect();
    alerts.truncate(room);
    room -= alerts.len();

    let mut notices: Vec<Line<'static>> = notice_texts(view)
        .into_iter()
        .flat_map(|(text, style)| {
            wrap(&text, width)
                .into_iter()
                .map(move |line| Line::styled(line, style))
        })
        .collect();
    notices.truncate(room);
    room -= notices.len();

    let mut body = header(view.manifest);
    if !view.usage.is_empty() {
        body.push(Line::default());
        body.push(Line::from(" AI Usage:"));
        body.extend(view.usage.iter().cloned());
    }
    body.extend(system(view, width));

    // エラーは本文の後ろに置く。本文を出し切った残りを使い、足りなくても 1 件目と残りの
    // 件数は出せるよう、本文から最大 3 行を譲らせる。
    let errors = view.controller.errors();
    let error_budget = if errors.is_empty() {
        0
    } else {
        room.saturating_sub(body.len()).max(3.min(room))
    };
    let errors = error_lines(errors, error_budget);
    room -= errors.len();
    body.truncate(room);
    room -= body.len();

    // 進捗の前の空行。ほかの行をすべて割り当てて余りがあるときだけ置く。先に場所を取ると、
    // 低いペインでエラーの要約や本文の行を押し出す。
    let gap = usize::from(room > 0);

    let mut lines = body;
    lines.extend(errors);
    lines.extend(alerts);
    lines.extend(notices);
    if gap == 1 {
        lines.push(Line::default());
    }
    lines.push(progress);
    lines
}

/// エラーの一覧を `budget` 行に収める。収まらなければ先頭から出し、残りは件数にまとめる。
fn error_lines(errors: &[String], budget: usize) -> Vec<Line<'static>> {
    if errors.is_empty() || budget == 0 {
        return Vec::new();
    }
    let summary = |text: String| Line::styled(text, Style::new().fg(CRITICAL));
    // 本文との間の空行は、エラーを削らずに置けるときだけ置く。
    let (mut lines, budget) = if budget > errors.len().min(2) {
        (vec![Line::default()], budget - 1)
    } else {
        (Vec::new(), budget)
    };
    if errors.len() <= budget {
        lines.extend(
            errors
                .iter()
                .map(|error| Line::from(format!(" ❌ {error}"))),
        );
    } else if budget == 1 {
        lines.push(summary(format!(" ❌ {} errors", errors.len())));
    } else {
        let shown = budget - 1;
        lines.extend(
            errors[..shown]
                .iter()
                .map(|error| Line::from(format!(" ❌ {error}"))),
        );
        lines.push(summary(format!(
            " ❌ +{} more errors",
            errors.len() - shown
        )));
    }
    lines
}

fn header(manifest: &Manifest) -> Vec<Line<'static>> {
    let rule = "━".repeat(24);
    let field = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!(" {label:<9}"), Style::new().fg(LABEL)),
            Span::raw(value),
        ])
    };
    vec![
        Line::from(rule.clone()),
        Line::styled(
            " 🔥 token-burn 🔥",
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Line::from(rule),
        Line::default(),
        field("Agent:", manifest.agent.clone()),
        field("Command:", manifest.command.clone()),
        field("Reset:", manifest.reset.clone()),
        field("Tasks:", manifest.total.to_string()),
        field("Workers:", manifest.worker_count.to_string()),
        field("Logs:", manifest.report_dir.display().to_string()),
    ]
}

/// CPU・メモリ・スワップ・ディスク。
fn system(view: &View, width: usize) -> Vec<Line<'static>> {
    let load = view.load;
    let latest = load.latest.as_ref();
    let mut lines = vec![Line::default()];
    let mut title = vec![
        Span::raw(" System"),
        Span::styled(
            format!(" · last {}m", WINDOW.as_secs() / 60),
            Style::new().fg(LABEL),
        ),
    ];
    if let Some(cpus) = latest.and_then(|sample| sample.logical_cpus) {
        title.push(Span::styled(
            format!(" · {cpus} CPUs"),
            Style::new().fg(LABEL),
        ));
    }
    lines.push(Line::from(title));

    let cpu = cpu_values(load.recent_cpu(view.now), latest.and_then(|s| s.load));
    let memory = memory_values(
        load.recent_memory(view.now),
        latest.and_then(|s| s.memory.as_ref()),
    );
    // CPU とメモリのグラフは同じ幅にして、同じ列が同じ時刻を指すようにする。
    let (level, spark) = (0..cpu.len())
        .map(|level| {
            let value = width_of(&cpu[level]).max(width_of(&memory[level]));
            (level, width.saturating_sub(LABEL_FIELD + GAP + value))
        })
        .find(|&(_, spark)| spark >= COMFORT_SPARK)
        .unwrap_or_else(|| {
            let last = cpu.len() - 1;
            let value = width_of(&cpu[last]).max(width_of(&memory[last]));
            (last, width.saturating_sub(LABEL_FIELD + GAP + value))
        });
    let spark = spark.min(MAX_SPARK);

    let cpu_columns = load.cpu.columns_mean(view.now, spark);
    lines.push(series_line(
        "CPU",
        sparkline(&cpu_columns, |_| CPU_SERIES),
        cpu[level].clone(),
        spark,
    ));
    let memory_columns = load.memory.columns_mean(view.now, spark);
    let pressure_columns = load.pressure.columns_max(view.now, spark);
    lines.push(series_line(
        "Mem",
        sparkline(&memory_columns, |index| {
            match pressure_columns
                .get(index)
                .copied()
                .flatten()
                .map(pressure_from_code)
            {
                Some(Pressure::Critical) => CRITICAL,
                Some(Pressure::Warning) => CAUTION,
                _ => MEMORY_SERIES,
            }
        }),
        memory[level].clone(),
        spark,
    ));
    if let Some(line) = swap_line(latest) {
        lines.push(line);
    }
    lines.extend(disk_lines(view.disk, width, view.now));
    lines
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// 「  CPU   <グラフ>  <値>」。グラフが描けないほど狭いときは値だけにする。
fn series_line(
    label: &str,
    spark: Vec<Span<'static>>,
    value: Vec<Span<'static>>,
    spark_width: usize,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("  {label:<width$}", width = LABEL_FIELD - 2),
        Style::new().fg(LABEL),
    )];
    if spark_width > 0 {
        spans.extend(spark);
        spans.push(Span::raw(" ".repeat(GAP)));
    }
    spans.extend(value);
    Line::from(spans)
}

/// 0〜100 の値の列を 1 行のグラフにする。データの無い列は空白（記録開始前・欠測）。
/// 0 でも最下段の ▁ を出し、「値が 0」と「値が無い」を見分けられるようにする。
pub fn sparkline(values: &[Option<f64>], color: impl Fn(usize) -> Color) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_color: Option<Color> = None;
    for (index, value) in values.iter().enumerate() {
        let (glyph, cell_color) = match value {
            Some(value) => (bar_glyph(*value), Some(color(index))),
            None => (' ', None),
        };
        if cell_color != run_color && !run.is_empty() {
            spans.push(styled_run(std::mem::take(&mut run), run_color));
        }
        run_color = cell_color;
        run.push(glyph);
    }
    if !run.is_empty() {
        spans.push(styled_run(run, run_color));
    }
    spans
}

fn styled_run(text: String, color: Option<Color>) -> Span<'static> {
    match color {
        Some(color) => Span::styled(text, Style::new().fg(color)),
        None => Span::raw(text),
    }
}

fn bar_glyph(value: f64) -> char {
    let value = if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let level = ((value / 100.0) * BARS.len() as f64).ceil() as usize;
    BARS[level.clamp(1, BARS.len()) - 1]
}

/// CPU の値の表示（詳しい順）。
fn cpu_values(percent: Option<f64>, load: Option<[f64; 3]>) -> Vec<Vec<Span<'static>>> {
    let percent = percent_span(percent, None);
    let mut variants = Vec::new();
    if let Some([one, five, fifteen]) = load {
        variants.push(vec![
            percent.clone(),
            Span::styled("  load ", Style::new().fg(LABEL)),
            Span::raw(format!(
                "{} {} {}",
                format_load(one),
                format_load(five),
                format_load(fifteen)
            )),
        ]);
        variants.push(vec![
            percent.clone(),
            Span::styled("  load ", Style::new().fg(LABEL)),
            Span::raw(format_load(one)),
        ]);
    } else {
        variants.push(vec![percent.clone()]);
        variants.push(vec![percent.clone()]);
    }
    variants.push(vec![percent]);
    variants
}

fn format_load(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// メモリの値の表示（詳しい順）。macOS では逼迫度が最も重要なので、狭くても残す。
fn memory_values(
    percent: Option<f64>,
    memory: Option<&super::system::Memory>,
) -> Vec<Vec<Span<'static>>> {
    let pressure = memory.map(|memory| memory.pressure);
    let percent_span = percent_span(percent, None);
    let sizes =
        memory.map(|memory| Span::raw(format_gib_pair(memory.used_bytes, memory.total_bytes)));
    let pressure_spans = pressure.and_then(pressure_spans);
    let mut full = vec![percent_span.clone()];
    if let Some(sizes) = sizes.clone() {
        full.push(Span::raw("  "));
        full.push(sizes);
    }
    if let Some(spans) = pressure_spans.clone() {
        full.push(Span::styled(" · ", Style::new().fg(LABEL)));
        full.extend(spans);
    }
    let mut short = vec![percent_span.clone()];
    match (pressure_spans, sizes) {
        (Some(spans), _) => {
            short.push(Span::raw("  "));
            short.extend(spans);
        }
        (None, Some(sizes)) => {
            short.push(Span::raw("  "));
            short.push(sizes);
        }
        (None, None) => {}
    }
    vec![full, short, vec![percent_span]]
}

/// 逼迫度。状態は色だけで伝えず、必ず文字で出す。
fn pressure_spans(pressure: Pressure) -> Option<Vec<Span<'static>>> {
    let (text, style) = match pressure {
        Pressure::Unknown => return None,
        Pressure::Normal => ("normal", Style::new().fg(NORMAL)),
        Pressure::Warning => (
            "WARNING",
            Style::new().fg(CAUTION).add_modifier(Modifier::BOLD),
        ),
        Pressure::Critical => (
            "CRITICAL",
            Style::new().fg(CRITICAL).add_modifier(Modifier::BOLD),
        ),
    };
    Some(vec![
        Span::styled("pressure ", Style::new().fg(LABEL)),
        Span::styled(text, style),
    ])
}

fn percent_span(percent: Option<f64>, color: Option<Color>) -> Span<'static> {
    let text = match percent.filter(|value| value.is_finite()) {
        Some(value) => format!("{:>3.0}%", value.clamp(0.0, 100.0)),
        None => " --%".to_string(),
    };
    match color {
        Some(color) => Span::styled(text, Style::new().fg(color)),
        None => Span::raw(text),
    }
}

fn swap_line(latest: Option<&super::system::Sample>) -> Option<Line<'static>> {
    let latest = latest?;
    let compressed = latest
        .memory
        .as_ref()
        .and_then(|memory| memory.compressed_bytes);
    let mut spans = vec![Span::styled(
        format!("  {:<width$}", "Swap", width = LABEL_FIELD - 2),
        Style::new().fg(LABEL),
    )];
    match &latest.swap {
        Some(swap) if swap.total_bytes == 0 => {
            spans.push(Span::styled("off", Style::new().fg(MUTED)));
        }
        Some(swap) => {
            spans.push(Span::raw(format_gib_pair(
                swap.used_bytes,
                swap.total_bytes,
            )));
            if let Some(rate) = swap.out_bytes_per_sec {
                spans.push(Span::styled(" · out ", Style::new().fg(LABEL)));
                spans.push(Span::raw(format_rate(rate)));
            }
        }
        None if compressed.is_none() => return None,
        None => spans.push(Span::styled("--", Style::new().fg(MUTED))),
    }
    if let Some(compressed) = compressed {
        spans.push(Span::styled(" · compressed ", Style::new().fg(LABEL)));
        spans.push(Span::raw(format_gib(compressed)));
    }
    Some(Line::from(spans))
}

/// メモリの量（2 進の単位）。macOS のアクティビティモニタと同じく GiB で数える。
fn format_gib(bytes: u64) -> String {
    format!("{} GiB", gib(bytes))
}

/// 「使用量/総量 GiB」。単位は 1 回だけ書く。
fn format_gib_pair(used: u64, total: u64) -> String {
    format!("{}/{} GiB", gib(used), gib(total))
}

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 30) as f64)
}

/// スワップアウトの速さ（2 進の単位）。
fn format_rate(bytes_per_sec: f64) -> String {
    let bytes = if bytes_per_sec.is_finite() {
        bytes_per_sec.max(0.0)
    } else {
        0.0
    };
    const UNITS: [&str; 4] = ["KiB/s", "MiB/s", "GiB/s", "TiB/s"];
    if bytes < 1024.0 {
        return format!("{bytes:.0} B/s");
    }
    let mut value = bytes / 1024.0;
    let mut unit = 0;
    while value >= 1023.95 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn disk_lines(disk: &DiskState, width: usize, now: Instant) -> Vec<Line<'static>> {
    let label = |first: bool| {
        Span::styled(
            format!(
                "  {:<width$}",
                if first { "Disk" } else { "" },
                width = LABEL_FIELD - 2
            ),
            Style::new().fg(LABEL),
        )
    };
    if disk.rows.is_empty() {
        return vec![Line::from(vec![
            label(true),
            Span::styled("reading…", Style::new().fg(MUTED)),
        ])];
    }
    let names = disk
        .rows
        .iter()
        .map(|row| row_name(row).width())
        .max()
        .unwrap_or(0)
        .min(MAX_NAME);
    let budget = width.saturating_sub(LABEL_FIELD + names + 1);
    // 行どうしを比べられるよう、バーの幅は最も説明の長い行に合わせて揃える。
    let bar = disk
        .rows
        .iter()
        .filter_map(|row| preferred_bar(row, budget, now))
        .min()
        .unwrap_or(MIN_BAR);
    disk.rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = vec![
                label(index == 0),
                Span::styled(name_field(row, names), Style::new().fg(LABEL)),
            ];
            spans.extend(row_spans(row, budget, bar, now));
            Line::from(spans)
        })
        .collect()
}

fn row_name(row: &Row) -> String {
    row.last
        .as_ref()
        .map_or_else(|| row.target.clone(), |(volume, _)| volume.name.clone())
}

fn name_field(row: &Row, width: usize) -> String {
    let name = truncate_display(&row_name(row), width);
    let pad = width.saturating_sub(name.width());
    format!("{name}{} ", " ".repeat(pad))
}

fn truncate_display(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        let next = format!("{out}{c}…");
        if next.width() > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// 最も詳しい説明を出せるバーの幅。値のない行は None。
fn preferred_bar(row: &Row, budget: usize, now: Instant) -> Option<usize> {
    let (volume, _) = row.last.as_ref()?;
    let needs: Vec<usize> = details(volume, stale_suffix(row, now))
        .iter()
        .map(|detail| width_of(detail))
        .collect();
    let budget = budget.saturating_sub(percent_label(volume).width() + 1);
    // バーを COMFORT_BAR 以上に保てる中で最も詳しい説明を選ぶ。どれも入らない幅では
    // バーを MIN_BAR まで縮めて説明を残し、それも無理ならバーと使用率だけを出す。
    let chosen = needs
        .iter()
        .copied()
        .filter(|&need| need > 0)
        .find(|&need| budget >= need + COMFORT_BAR)
        .or_else(|| needs.iter().copied().find(|&need| budget >= need + MIN_BAR));
    Some(chosen.map_or_else(
        || budget.clamp(1, MAX_BAR),
        |need| (budget - need).min(MAX_BAR),
    ))
}

fn row_spans(row: &Row, budget: usize, bar: usize, now: Instant) -> Vec<Span<'static>> {
    let Some((volume, _)) = row.last.as_ref() else {
        let (text, color) = match &row.status {
            Status::NotMounted => ("unavailable: not mounted".to_string(), CRITICAL),
            Status::NotLocal => ("unavailable: not a local volume".to_string(), CRITICAL),
            Status::Timeout => ("no response".to_string(), CAUTION),
            Status::Error(error) => (format!("unavailable: {error}"), CRITICAL),
            Status::Ok => ("reading…".to_string(), MUTED),
        };
        return vec![Span::styled(text, Style::new().fg(color))];
    };
    let percent = percent_label(volume);
    let room = budget.saturating_sub(percent.width() + 1);
    let bar = bar.min(room).max(1);
    let detail = details(volume, stale_suffix(row, now))
        .into_iter()
        .find(|detail| room >= width_of(detail) + bar)
        .unwrap_or_default();
    // 使用中の区分と使用率の数字は使用率の段階の色にし、パージ可能分と空きは段階によらず
    // 灰色にする（ai-usage の TUI と同じ）。
    let level = disk_level(volume.used_percent());
    let [used, purgeable, free] = bar_cells(volume, bar);
    let mut spans = Vec::new();
    for (cells, symbol, color) in [
        (used, "█", level),
        (purgeable, "▒", LABEL),
        (free, "░", MUTED),
    ] {
        if cells > 0 {
            spans.push(Span::styled(symbol.repeat(cells), Style::new().fg(color)));
        }
    }
    let mut percent_style = Style::new().fg(level);
    if level == WARNING || level == CRITICAL {
        percent_style = percent_style.add_modifier(Modifier::BOLD);
    }
    spans.push(Span::styled(format!(" {percent}"), percent_style));
    spans.extend(detail);
    spans
}

fn disk_level(used_percent: f64) -> Color {
    if used_percent >= DISK_LEVELS[2] {
        CRITICAL
    } else if used_percent >= DISK_LEVELS[1] {
        WARNING
    } else if used_percent >= DISK_LEVELS[0] {
        CAUTION
    } else {
        NORMAL
    }
}

/// 古い値を出しているときの経過時間（`· 3m ago`）。
fn stale_suffix(row: &Row, now: Instant) -> Option<String> {
    let (_, read_at) = row.last.as_ref()?;
    row.is_stale()
        .then(|| format!("{} ago", age(now.saturating_duration_since(*read_at))))
}

/// バーの後ろに置く説明（詳しい順。最後は経過時間だけか空）。
fn details(volume: &Volume, stale: Option<String>) -> Vec<Vec<Span<'static>>> {
    let available = format_bytes(volume.available);
    let total = format_bytes(volume.total);
    let mut texts = Vec::new();
    if let Some(purgeable) = volume.purgeable.filter(|&purgeable| purgeable > 0) {
        let purgeable = format_bytes(purgeable);
        texts.push(format!(
            "{available} available of {total} · {} free · {purgeable} purgeable",
            format_bytes(volume.free)
        ));
        // 狭いときは、今すぐ書ける空きを残す（パージ可能分は available − free で分かる）。
        texts.push(format!(
            "{available} available · {} free",
            format_bytes(volume.free)
        ));
    }
    texts.push(format!("{available} available of {total}"));
    texts.push(format!("{available} available"));
    texts.push(String::new());
    texts
        .into_iter()
        .map(|text| {
            let mut spans = Vec::new();
            if !text.is_empty() {
                spans.push(Span::styled(format!("  {text}"), Style::new().fg(LABEL)));
            }
            if let Some(stale) = &stale {
                let separator = if text.is_empty() { "  " } else { " · " };
                spans.push(Span::styled(
                    format!("{separator}{stale}"),
                    Style::new().fg(MUTED),
                ));
            }
            spans
        })
        .collect()
}

/// 使用・パージ可能・空きのセル数。合計は必ず `width` になる（ai-usage の TUI と同じ配分）。
fn bar_cells(volume: &Volume, width: usize) -> [usize; 3] {
    let parts = [
        volume.used(),
        volume.purgeable.unwrap_or(0),
        volume.free.min(volume.total),
    ];
    let total: u128 = parts.iter().map(|&part| u128::from(part)).sum();
    if total == 0 || width == 0 {
        return [0, 0, width];
    }
    let width_wide = width as u128;
    let mut cells = parts.map(|part| (u128::from(part) * width_wide / total) as usize);
    let mut remainders = parts.map(|part| u128::from(part) * width_wide % total);
    // 切り捨てで余ったセルを、端数の大きい区分から 1 つずつ配る。
    for _ in cells.iter().sum::<usize>()..width {
        let index = (0..3)
            .max_by_key(|&index| (remainders[index], Reverse(index)))
            .expect("three parts");
        cells[index] += 1;
        remainders[index] = 0;
    }
    // 少しでも使っている・空いているディスクが、空・満杯に見えないようにする。
    if parts.iter().filter(|&&part| part > 0).count() <= width {
        for index in [0, 2] {
            if parts[index] > 0 && cells[index] == 0 {
                let donor = (0..3)
                    .max_by_key(|&other| cells[other])
                    .expect("three parts");
                if cells[donor] > 1 {
                    cells[donor] -= 1;
                    cells[index] += 1;
                }
            }
        }
    }
    cells
}

/// 使用率。丸めで空きや使用が消えて見えないよう、0% と 100% は本当に空・満杯のときだけ出す。
fn percent_label(volume: &Volume) -> String {
    let used = volume.used();
    let mut shown = volume.used_percent().round() as u64;
    if used > 0 {
        shown = shown.max(1);
    }
    if used < volume.total {
        shown = shown.min(99);
    }
    format!("{shown:>3}%")
}

/// Finder と同じ 10 進の単位（1 GB = 1,000,000,000 バイト）。
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn age(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        "<1m".into()
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// 通知（または終了時の集計）と、その文字の装飾。ペインが狭ければ単語の区切りで折り返して
/// 出す（ai-usage の行のように切り捨てると、Ctrl-C の案内が読めなくなる）。
fn notice_texts(view: &View) -> Vec<(String, Style)> {
    let controller = view.controller;
    if let Some(finish) = controller.finished() {
        return vec![
            (finish_line(controller, finish), Style::new()),
            (
                format!(" 📁 Logs: {}", view.manifest.report_dir.display()),
                Style::new(),
            ),
        ];
    }
    let Some(notice) = controller.notice() else {
        return Vec::new();
    };
    let mut line = notice_line(notice, controller.now());
    let mut detail = None;
    if matches!(notice, Notice::Paused { .. }) {
        let (suffix, note) = recovery_notes(controller, view.recovery);
        line.push_str(suffix);
        detail = note;
    }
    let mut texts = vec![(line, Style::new())];
    // 確認の様子は通知の補足なので、ラベルと同じ控えめな色にする。
    texts.extend(detail.map(|text| (text, Style::new().fg(LABEL))));
    texts
}

/// 一時停止中に、回復を確かめていることを伝える補足（通知の末尾に足す）。
const EARLY_RESUME: &str = ", or earlier if the window is reset";

/// 一時停止中の回復の確認の様子。通知の末尾に足す補足と、その下に添える行を返す。
///
/// 回復を確かめていない構成（ai-usage 連携が無い・ai-usage が古い）と、確かめても解除
/// できない一時停止では、早めに再開するとは案内しない（案内どおりにならないと、手動
/// リセットしたのに待ち続ける理由が分からなくなる）。解除できない一時停止には理由を添える。
fn recovery_notes(
    controller: &Controller,
    recovery: &recovery::Status,
) -> (&'static str, Option<String>) {
    let Some(pause) = controller.pause().filter(|_| recovery.active) else {
        return ("", None);
    };
    if let Some(reason) = pause.not_liftable {
        return ("", Some(format!("    early resume unavailable: {reason}")));
    }
    // 直近の確認の結果は、同じ一時停止についてのものだけ添える。
    let last = recovery
        .last
        .as_ref()
        .filter(|check| check.since == pause.since)
        .map(|check| {
            format!(
                "    last checked {}: {}",
                format_clock(check.at, controller.now()),
                check.detail
            )
        });
    (EARLY_RESUME, last)
}

/// 空白の位置で `width` に収まるように折り返す。続きの行は 3 桁下げる（先頭の絵文字の後ろに
/// 揃える）。1 語が幅を超える場合はその語だけはみ出させる（描画で切れる）。
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.width() <= width {
        return vec![text.to_string()];
    }
    let indent = "    ";
    // 先頭の空白（行頭の字下げ）は 1 行目に残す。単語に分けると空の単語になって消える。
    let body = text.trim_start_matches(' ');
    let mut lines = Vec::new();
    let mut current = text[..text.len() - body.len()].to_string();
    for word in body.split(' ') {
        let candidate = if current.trim().is_empty() {
            format!("{current}{word}")
        } else {
            format!("{current} {word}")
        };
        if candidate.width() > width && !current.trim().is_empty() {
            lines.push(current);
            current = format!("{indent}{word}");
        } else {
            current = candidate;
        }
    }
    lines.push(current);
    lines
}

pub fn notice_line(notice: &Notice, now: i64) -> String {
    match notice {
        // 原因は決め付けず、pause file の理由（成立している条件だけを書いたもの）を出す。
        // 閾値を下回っていても拒否されることがあり（42% で拒否された例がある）、前回の実行の
        // 中断記録から保留することもある。理由の無い旧形式では枠名だけを出す。
        Notice::Paused {
            window,
            reason,
            resume_at,
        } => {
            let at = format_clock(*resume_at, now);
            if reason.is_empty() {
                format!(" 💤 Paused: {window} window — resuming at {at}")
            } else {
                format!(" 💤 Paused: {reason} — resuming at {at}")
            }
        }
        Notice::ResumedEarly {
            window,
            reason,
            lifted_at,
        } => {
            let at = format_clock(*lifted_at, now);
            if reason.is_empty() {
                format!(" ✅ Resumed early: {window} window recovered ({at})")
            } else {
                format!(" ✅ Resumed early: {window} window recovered ({reason}, {at})")
            }
        }
        Notice::Interrupted => {
            " ⏳ Waiting for current tasks to finish... (Ctrl-C again to force kill)".to_string()
        }
        Notice::Deadline => {
            " ⌛ DEADLINE REACHED — waiting for current tasks (Ctrl-C to force kill)".to_string()
        }
        Notice::UsageLimit => {
            " ⛔ Usage/rate limit reached — waiting for current tasks to finish".to_string()
        }
    }
}

/// 終了時の集計。tmux から戻った後に呼び出し元の端末へ出す集計（`execute_plan_tmux`）と
/// 同じ文言にするため、どちらも [`summary_text`] を使う。
pub fn finish_line(controller: &Controller, finish: Finish) -> String {
    let counts = controller.counts();
    // 終了の理由はコントローラーが決める（処理済みの数だけでは、全ワーカーの終了と
    // 全タスクの完了が同時に起きたときにどちらを出すかが決まらない）。
    let total = match finish {
        Finish::Completed => counts.processed().min(controller.total()),
        Finish::Stopped => controller.total(),
    };
    format!(
        " {}",
        summary_text(counts.done, counts.failed, counts.retry, total)
    )
}

/// 実行の集計の文言。処理済みが total に届かなければ停止、失敗かリトライがあれば警告、
/// それ以外は完了。絵文字は幅が常に 2 マスのものだけを使う（モジュール先頭の説明を参照）。
pub fn summary_text(done: usize, failed: usize, retry: usize, total: usize) -> String {
    let processed = done + failed + retry;
    if processed < total {
        format!("🛑 Stopped: {processed}/{total} processed (fail:{failed} retry:{retry})")
    } else if failed > 0 || retry > 0 {
        format!("❗ Completed: {done} succeeded / {failed} failed / {retry} retry")
    } else {
        format!("✅ All {done}/{total} tasks completed")
    }
}

/// 最下行の進捗。件数と失敗の数を先に確保し、バーは余った幅に [`MIN_PROGRESS_BAR`]〜
/// [`PROGRESS_BAR`] マスで置く。収まらなければ、バーを省き、時刻を短い形にし、最後に件数の
/// 括弧を省く。
pub fn progress_line(controller: &Controller, width: usize) -> String {
    let counts = controller.counts();
    let percent = controller.percent();
    let (head, short_head) = match controller.phase() {
        Phase::Countdown(remaining) => (
            format!(
                " ⏰ {}d {:02}h {:02}m {:02}s",
                remaining / 86_400,
                (remaining % 86_400) / 3_600,
                (remaining % 3_600) / 60,
                remaining % 60
            ),
            format!(
                " ⏰ {}d{:02}h{:02}m",
                remaining / 86_400,
                (remaining % 86_400) / 3_600,
                (remaining % 3_600) / 60
            ),
        ),
        Phase::Resuming(remaining) => (
            format!(
                " 💤 resuming in {:02}h {:02}m {:02}s",
                remaining / 3_600,
                (remaining % 3_600) / 60,
                remaining % 60
            ),
            format!(
                " 💤 {:02}h{:02}m",
                remaining / 3_600,
                (remaining % 3_600) / 60
            ),
        ),
        Phase::Stopping => (" ⏳ Stopping...".to_string(), " ⏳".to_string()),
    };
    let tail = format!(
        " {}/{} ({percent}%, fail:{} retry:{})",
        counts.processed(),
        controller.total(),
        counts.failed,
        counts.retry
    );
    let short_tail = format!(
        " {}/{} fail:{} retry:{}",
        counts.processed(),
        controller.total(),
        counts.failed,
        counts.retry
    );
    let with_bar = |head: &str, tail: &str| {
        // 「head  [bar]tail」。バーの外の幅は head + 括弧と空白 4 + tail。
        let room = width.checked_sub(head.width() + 4 + tail.width())?;
        (room >= MIN_PROGRESS_BAR).then(|| {
            let bar = room.min(PROGRESS_BAR);
            let filled = percent.min(100) * bar / 100;
            format!(
                "{head}  [{}{}]{tail}",
                "█".repeat(filled),
                "░".repeat(bar - filled)
            )
        })
    };
    let without_bar = |head: &str, tail: &str| {
        let line = format!("{head} {tail}");
        (line.width() <= width).then_some(line)
    };
    with_bar(&head, &tail)
        .or_else(|| without_bar(&head, &tail))
        .or_else(|| with_bar(&short_head, &tail))
        .or_else(|| without_bar(&short_head, &tail))
        .or_else(|| without_bar(&short_head, &short_tail))
        // それでも収まらなければ、最も短い形を出す（描画で右端が切れる）。
        .unwrap_or_else(|| format!("{short_head} {short_tail}"))
}

/// Unix 秒をローカル時刻の HH:MM へ。当日でなければ MM/DD HH:MM（待機が翌日まで続くのか
/// 今日中に明けるのかを読めるようにする）。
pub fn format_clock(timestamp: i64, now: i64) -> String {
    let (Some(at), Some(today)) = (
        Local.timestamp_opt(timestamp, 0).single(),
        Local.timestamp_opt(now, 0).single(),
    ) else {
        return timestamp.to_string();
    };
    if at.date_naive() == today.date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%m/%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::controller::{Counts, Observation, Pause};
    use super::super::disk::{Reading, Source, Update};
    use super::super::system::{Memory, Sample, Swap};
    use super::*;

    const NOW: i64 = 1_800_000_000;
    const GB: u64 = 1_000_000_000;
    const GIB: u64 = 1 << 30;

    fn manifest() -> Manifest {
        Manifest {
            agent: "claude".into(),
            command: "claude -p".into(),
            reset: "2026/10/27 09:00".into(),
            total: 10,
            deadline_epoch: NOW + 3 * 86_400 + 3_661,
            marker_dir: PathBuf::from("/tmp/markers"),
            session: "token-burn".into(),
            worker_count: 2,
            stop_file: PathBuf::from("/tmp/stop"),
            report_dir: PathBuf::from("/tmp/report"),
            threshold: 95,
            ai_usage: None,
        }
    }

    fn controller(counts: Counts) -> Controller {
        let manifest = manifest();
        let mut controller = Controller::new(
            manifest.total,
            manifest.worker_count,
            manifest.deadline_epoch,
            NOW,
        );
        controller.observe(Observation {
            now: NOW,
            counts,
            ..Observation::default()
        });
        controller
    }

    /// 5 分ぶんの一定の負荷と、最新のサンプル。
    fn busy_load(now: Instant, pressure: Pressure) -> LoadHistory {
        let mut load = LoadHistory::default();
        let start = now - WINDOW;
        for second in 1..=WINDOW.as_secs() {
            let at = start + Duration::from_secs(second);
            load.record(Sample {
                at,
                cpu_percent: Some(75.0),
                load: Some([12.3, 10.0, 8.3]),
                logical_cpus: Some(8),
                memory: Some(Memory {
                    total_bytes: 24 * GIB,
                    used_bytes: 18 * GIB,
                    compressed_bytes: Some(5 * GIB),
                    pressure,
                }),
                swap: Some(Swap {
                    total_bytes: 7 * GIB,
                    used_bytes: 6 * GIB,
                    out_bytes_per_sec: Some(1.5 * 1024.0 * 1024.0),
                }),
            });
        }
        load
    }

    fn startup_disk(now: Instant) -> DiskState {
        let mut disk = DiskState::default();
        disk.apply(
            Update::Readings {
                source: Source::AiUsage,
                readings: vec![Reading {
                    target: "/".into(),
                    status: Status::Ok,
                    volume: Some(Volume {
                        name: "Macintosh HD".into(),
                        total: 494 * GB,
                        free: 13 * GB,
                        available: 70 * GB,
                        purgeable: Some(57 * GB),
                    }),
                }],
            },
            now,
        );
        disk
    }

    fn render(view: &View, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, view)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let mut line = String::new();
                let mut x = 0;
                while x < width {
                    let cell = &buffer[(x, y)];
                    line.push_str(cell.symbol());
                    x += cell.symbol().width().max(1) as u16;
                }
                line.trim_end().to_string()
            })
            .collect()
    }

    fn find<'a>(lines: &'a [String], prefix: &str) -> &'a str {
        lines
            .iter()
            .find(|line| line.trim_start().starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starts with {prefix:?}:\n{}", lines.join("\n")))
    }

    #[test]
    fn renders_the_load_graphs_and_the_disk_at_common_pane_widths() {
        let now = Instant::now();
        let manifest = manifest();
        let controller = controller(Counts {
            done: 3,
            failed: 1,
            retry: 0,
            workers_done: 0,
        });
        let load = busy_load(now, Pressure::Normal);
        let disk = startup_disk(now);
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &[],
            recovery: &recovery::Status::default(),
            now,
        };
        for width in [60u16, 70, 90] {
            let lines = render(&view, width, 30);
            let text = lines.join("\n");
            assert!(text.contains("System · last 5m · 8 CPUs"), "{text}");
            let cpu = find(&lines, "CPU");
            // 一定の 75% は 6/8 段の ▆ で、幅いっぱいに描かれる。
            assert!(cpu.contains("▆▆▆▆▆▆▆▆▆▆"), "{cpu}");
            assert!(cpu.contains(" 75%"), "{cpu}");
            let memory = find(&lines, "Mem");
            assert!(memory.contains(" 75%"), "{memory}");
            assert!(memory.contains("pressure normal"), "{memory}");
            // CPU とメモリのグラフは同じ列から始まり、同じ幅を持つ。
            let start = |line: &str| line.find('▆').unwrap();
            assert_eq!(start(cpu), start(memory), "{text}");
            assert_eq!(
                cpu.matches('▆').count(),
                memory.matches('▆').count(),
                "{text}"
            );
            let disk = find(&lines, "Disk");
            assert!(disk.contains("Macintosh HD"), "{disk}");
            assert!(disk.contains("86%"), "{disk}");
            assert!(disk.contains("70.0 GB available"), "{disk}");
            for line in &lines {
                assert!(line.width() <= usize::from(width), "{line}");
            }
            // 最下行は進捗。狭いペインでもバーを縮めて件数と失敗の数を残す。
            let progress = lines.iter().rev().find(|line| !line.is_empty()).unwrap();
            assert!(progress.ends_with("] 4/10 (40%, fail:1 retry:0)"), "{text}");
        }
        let wide = render(&view, 90, 30);
        assert!(find(&wide, "CPU").contains("load 12.3 10.0 8.3"));
        assert!(find(&wide, "Mem").contains("18.0/24.0 GiB · pressure normal"));
        assert!(find(&wide, "Swap").contains("6.0/7.0 GiB · out 1.5 MiB/s · compressed 5.0 GiB"));
        assert!(find(&wide, "Disk").contains("70.0 GB available · 13.0 GB free"));
        let widest = render(&view, 120, 30);
        assert!(
            find(&widest, "Disk")
                .contains("70.0 GB available of 494.0 GB · 13.0 GB free · 57.0 GB purgeable")
        );
    }

    #[test]
    fn graphs_start_empty_and_fill_from_the_right() {
        // 記録開始から間もないときは、左側（記録開始前）を空白にして 0% と区別する。
        let now = Instant::now();
        let manifest = manifest();
        let controller = controller(Counts::default());
        let mut load = LoadHistory::default();
        for second in 0..60 {
            load.record(Sample {
                at: now - Duration::from_secs(59 - second),
                cpu_percent: Some(0.0),
                load: None,
                logical_cpus: None,
                memory: None,
                swap: None,
            });
        }
        let disk = DiskState::default();
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &[],
            recovery: &recovery::Status::default(),
            now,
        };
        let lines = render(&view, 70, 30);
        let cpu = find(&lines, "CPU");
        assert!(cpu.contains('▁'), "{cpu}");
        let first_bar = cpu.find('▁').unwrap();
        let label_end = cpu.find("CPU").unwrap() + 3;
        assert!(
            first_bar > label_end + 20,
            "the left part must stay empty: {cpu:?}"
        );
        assert!(find(&lines, "Mem").contains("--%"));
        assert!(find(&lines, "Disk").contains("reading…"));
    }

    #[test]
    fn memory_pressure_is_written_out_and_colors_the_graph() {
        let now = Instant::now();
        let manifest = manifest();
        let controller = controller(Counts::default());
        let load = busy_load(now, Pressure::Critical);
        let disk = startup_disk(now);
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &[],
            recovery: &recovery::Status::default(),
            now,
        };
        let lines = layout(&view, 90, 30);
        let memory = lines
            .iter()
            .find(|line| {
                line.spans
                    .first()
                    .is_some_and(|span| span.content.contains("Mem"))
            })
            .unwrap();
        assert!(
            memory
                .spans
                .iter()
                .any(|span| span.content.contains('▆') && span.style.fg == Some(CRITICAL))
        );
        assert!(
            memory.spans.iter().any(|span| span.content == "CRITICAL"
                && span.style.add_modifier.contains(Modifier::BOLD))
        );
        let cpu = lines
            .iter()
            .find(|line| {
                line.spans
                    .first()
                    .is_some_and(|span| span.content.contains("CPU"))
            })
            .unwrap();
        assert!(
            cpu.spans
                .iter()
                .filter(|span| span.content.contains('▆'))
                .all(|span| span.style.fg == Some(CPU_SERIES))
        );
    }

    #[test]
    fn short_panes_keep_the_progress_line_and_fold_the_errors() {
        let now = Instant::now();
        let manifest = manifest();
        let mut controller = controller(Counts::default());
        controller.observe(Observation {
            now: NOW,
            errors: (1..=20).map(|n| format!("task {n} failed")).collect(),
            ..Observation::default()
        });
        let load = LoadHistory::default();
        let disk = DiskState::default();
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &[],
            recovery: &recovery::Status::default(),
            now,
        };
        let lines = render(&view, 70, 24);
        let text = lines.join("\n");
        assert!(text.contains("❌ task 1 failed"), "{text}");
        assert!(text.contains("more errors"), "{text}");
        assert!(
            lines.last().unwrap().contains("⏰ 3d 01h 01m 01s"),
            "{text}"
        );
        // 極端に低いペインでも進捗は残る。
        let tiny = render(&view, 70, 3);
        assert!(tiny.last().unwrap().contains("0/10"), "{tiny:?}");
    }

    #[test]
    fn short_panes_keep_alerts_notices_and_progress_before_the_body() {
        // stop file を書けなかった警告と Ctrl-C の案内は、本文より先に行を割り当てる。
        let now = Instant::now();
        let manifest = manifest();
        let mut controller = controller(Counts::default());
        controller.interrupt();
        let load = busy_load(now, Pressure::Normal);
        let disk = startup_disk(now);
        let alerts = vec!["failed to write the stop file /tmp/stop: No space left".to_string()];
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &alerts,
            recovery: &recovery::Status::default(),
            now,
        };
        let lines = render(&view, 40, 10);
        let text = lines.join("\n");
        assert!(text.contains("⛔ failed to write the stop"), "{text}");
        assert!(text.contains("Waiting for current tasks"), "{text}");
        assert!(text.contains("kill)"), "{text}");
        // 折り返しても、1 行目の字下げは残る。
        assert!(
            lines.iter().any(|line| line.starts_with(" ⛔ failed")),
            "{text}"
        );
        assert!(lines.last().unwrap().contains("0/10"), "{text}");
        // 2 行しか無ければ、警告と進捗だけを出す。
        let tiny = render(&view, 40, 2);
        assert!(tiny[0].contains('⛔'), "{tiny:?}");
        assert!(tiny[1].contains("0/10"), "{tiny:?}");
    }

    #[test]
    fn notices_and_progress_follow_the_run_state() {
        let manifest = manifest();
        let mut controller = controller(Counts::default());
        controller.observe(Observation {
            now: NOW,
            pause: Some(Pause::new(NOW + 3_725, "five_hour")),
            ..Observation::default()
        });
        assert_eq!(
            progress_line(&controller, 200),
            " 💤 resuming in 01h 02m 05s  [░░░░░░░░░░░░░░░░░░░░] 0/10 (0%, fail:0 retry:0)"
        );
        // 理由の無い一時停止では、原因を推測せず枠名だけを出す。
        assert!(
            notice_line(controller.notice().unwrap(), NOW)
                .starts_with(" 💤 Paused: five_hour window — resuming at ")
        );
        controller.interrupt();
        assert!(progress_line(&controller, 200).starts_with(" ⏳ Stopping...  ["));
        assert_eq!(
            notice_line(controller.notice().unwrap(), NOW),
            " ⏳ Waiting for current tasks to finish... (Ctrl-C again to force kill)"
        );

        let mut done = controller_with(
            manifest.total,
            Counts {
                done: 8,
                failed: 1,
                retry: 1,
                workers_done: 0,
            },
        );
        assert_eq!(
            finish_line(&done, Finish::Completed),
            " ❗ Completed: 8 succeeded / 1 failed / 1 retry"
        );
        done = controller_with(
            manifest.total,
            Counts {
                done: 10,
                ..Counts::default()
            },
        );
        assert_eq!(
            finish_line(&done, Finish::Completed),
            " ✅ All 10/10 tasks completed"
        );
        let stopped = controller_with(
            manifest.total,
            Counts {
                done: 4,
                failed: 1,
                retry: 0,
                workers_done: 2,
            },
        );
        assert_eq!(
            finish_line(&stopped, Finish::Stopped),
            " 🛑 Stopped: 5/10 processed (fail:1 retry:0)"
        );
    }

    /// 一時停止中の通知の行（折り返さない幅で）。
    fn paused_notices(pause: Pause, recovery: &recovery::Status) -> Vec<String> {
        let manifest = manifest();
        let mut controller = controller(Counts::default());
        controller.observe(Observation {
            now: NOW,
            pause: Some(pause),
            ..Observation::default()
        });
        let load = LoadHistory::default();
        let disk = DiskState::default();
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &[],
            recovery,
            now: Instant::now(),
        };
        notice_texts(&view)
            .into_iter()
            .map(|(text, _)| text)
            .collect()
    }

    #[test]
    fn a_pause_says_it_may_resume_early_only_while_recovery_is_checked() {
        let resume = format_clock(NOW + 3_600, NOW);
        let pause = Pause {
            reason: "request rejected (five_hour); five_hour at 100%".into(),
            since: NOW - 60,
            ..Pause::new(NOW + 3_600, "five_hour")
        };
        let paused = format!(
            " 💤 Paused: request rejected (five_hour); five_hour at 100% — resuming at {resume}"
        );
        // ai-usage 連携が無い（または ai-usage が古い）構成では、早めの再開を案内しない。
        assert_eq!(
            paused_notices(pause.clone(), &recovery::Status::default()),
            [paused.as_str()]
        );
        // 確かめている構成では案内し、同じ一時停止についての直近の確認を添える。
        let mut status = recovery::Status::new(true);
        status.last = Some(recovery::Check {
            at: NOW,
            since: NOW - 60,
            detail: "five_hour at 100% (was 100% when paused)".into(),
        });
        assert_eq!(
            paused_notices(pause.clone(), &status),
            [
                format!("{paused}, or earlier if the window is reset"),
                format!(
                    "    last checked {}: five_hour at 100% (was 100% when paused)",
                    format_clock(NOW, NOW)
                ),
            ]
        );
        // 別の一時停止についての古い確認は添えない。
        status.last.as_mut().unwrap().since = NOW - 600;
        assert_eq!(paused_notices(pause.clone(), &status).len(), 1);
        // 確かめても解除できない一時停止では案内せず、理由を出す。
        let unliftable = Pause {
            not_liftable: Some("the pause has no recorded usage"),
            ..pause
        };
        assert_eq!(
            paused_notices(unliftable, &status),
            [
                paused,
                "    early resume unavailable: the pause has no recorded usage".to_string(),
            ]
        );
    }

    #[test]
    fn an_early_resume_is_announced_with_the_reason_and_time() {
        let notice = Notice::ResumedEarly {
            window: "five_hour".into(),
            reason: "five_hour dropped from 100% to 0%".into(),
            lifted_at: NOW,
        };
        assert_eq!(
            notice_line(&notice, NOW),
            format!(
                " ✅ Resumed early: five_hour window recovered (five_hour dropped from 100% to 0%, {})",
                format_clock(NOW, NOW)
            )
        );
        let without_reason = Notice::ResumedEarly {
            window: "five_hour".into(),
            reason: String::new(),
            lifted_at: NOW,
        };
        assert_eq!(
            notice_line(&without_reason, NOW),
            format!(
                " ✅ Resumed early: five_hour window recovered ({})",
                format_clock(NOW, NOW)
            )
        );
    }

    #[test]
    fn an_old_ai_usage_is_reported_as_an_alert_in_narrow_panes() {
        let now = Instant::now();
        let manifest = manifest();
        let controller = controller(Counts::default());
        let load = LoadHistory::default();
        let disk = DiskState::default();
        let alerts = vec![recovery::UNSUPPORTED_ALERT.to_string()];
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &alerts,
            recovery: &recovery::Status::default(),
            now,
        };
        let lines = render(&view, 40, 12);
        // 折り返しの字下げを畳んで、文言が欠けずに出ていることを確かめる。
        let text = lines
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("⛔ ai-usage does not support"), "{text}");
        assert!(text.contains("(update ai-usage)"), "{text}");
        for line in &lines {
            assert!(line.width() <= 40, "{line}");
        }
    }

    #[test]
    fn progress_bar_keeps_its_width_at_the_boundaries_and_shrinks_in_narrow_panes() {
        for (done, filled) in [(0, 0), (5, 10), (10, 20)] {
            let controller = controller_with(
                10,
                Counts {
                    done,
                    ..Counts::default()
                },
            );
            let line = progress_line(&controller, 200);
            let bar: String = line
                .chars()
                .skip_while(|c| *c != '[')
                .skip(1)
                .take_while(|c| *c != ']')
                .collect();
            assert_eq!(bar.chars().count(), 20, "{line}");
            assert_eq!(bar.matches('█').count(), filled, "{line}");
        }
        let controller = controller_with(
            10,
            Counts {
                done: 5,
                ..Counts::default()
            },
        );
        // 幅 60 ではバーを縮めて、件数と失敗の数を残す。
        let narrow = progress_line(&controller, 60);
        assert_eq!(narrow.width(), 60, "{narrow}");
        assert!(narrow.ends_with("] 5/10 (50%, fail:0 retry:0)"), "{narrow}");
        let cells = narrow.matches(['█', '░']).count();
        assert!(
            (MIN_PROGRESS_BAR..PROGRESS_BAR).contains(&cells),
            "{narrow}"
        );
        // バーを最小の幅でも置けなければ省き、件数を削らない。
        let no_bar = progress_line(&controller, 50);
        assert_eq!(no_bar, " ⏰ 0d 00h 01m 40s  5/10 (50%, fail:0 retry:0)");
        // さらに狭ければ時刻を短い形にし、最後に件数の括弧を省く。
        let compact = progress_line(&controller, 40);
        assert_eq!(compact, " ⏰ 0d00h01m  5/10 (50%, fail:0 retry:0)");
        assert!(compact.width() <= 40, "{compact}");
        assert_eq!(
            progress_line(&controller, 20),
            " ⏰ 0d00h01m  5/10 fail:0 retry:0"
        );
    }

    #[test]
    fn notices_wrap_at_spaces_in_narrow_panes() {
        let text = " ⌛ DEADLINE REACHED — waiting for current tasks (Ctrl-C to force kill)";
        let lines = wrap(text, 40);
        assert!(lines.len() > 1, "{lines:?}");
        assert!(lines.iter().all(|line| line.width() <= 40), "{lines:?}");
        assert!(lines[0].starts_with(" ⌛ DEADLINE"), "{lines:?}");
        assert!(lines[1].starts_with("    "), "{lines:?}");
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
        assert_eq!(wrap("short", 40), ["short"]);
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn the_gap_before_the_progress_line_never_takes_a_row_from_the_errors() {
        // 進捗・通知の後に 1 行しか残らないときは、その行でエラーの要約を出す
        // （割り当て順: 進捗 → 警告 → 通知 → エラー → 本文。空行はこの順に無い）。
        let now = Instant::now();
        let manifest = manifest();
        let mut controller = controller(Counts::default());
        controller.observe(Observation {
            now: NOW,
            errors: (1..=5).map(|n| format!("task {n} failed")).collect(),
            ..Observation::default()
        });
        controller.interrupt();
        let load = LoadHistory::default();
        let disk = DiskState::default();
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &[],
            recovery: &recovery::Status::default(),
            now,
        };
        let tiny = texts(&layout(&view, 120, 3));
        assert_eq!(tiny.len(), 3, "{tiny:?}");
        assert_eq!(tiny[0], " ❌ 5 errors", "{tiny:?}");
        assert!(tiny[1].contains("Waiting for current tasks"), "{tiny:?}");
        assert!(tiny[2].contains("0/10"), "{tiny:?}");

        // 空行は、ほかの行をすべて割り当てて余りがあるときだけ置く。全部を出すのに 1 行
        // 足りない高さでは、空行を省いてエラーも本文も削らない。
        let full = texts(&layout(&view, 120, 200));
        let gap = full.len() - 2;
        assert_eq!(full[gap], "", "{full:?}");
        let mut without_gap = full.clone();
        without_gap.remove(gap);
        assert_eq!(texts(&layout(&view, 120, full.len() - 1)), without_gap);
        assert_eq!(texts(&layout(&view, 120, full.len())), full);
    }

    /// pause file を書いて読み直し、一時停止の通知の行を返す（折り返さない幅で）。
    fn notices_for_pause_file(state: &crate::rate_control::PauseState) -> Vec<String> {
        let tmp = tempfile::TempDir::new().unwrap();
        let stop = tmp.path().join("stop");
        let pause_file = crate::rate_control::pause_path_for(&stop);
        crate::rate_control::write_pause(&pause_file, state).unwrap();
        let observation = super::super::controller::read_observation(
            &tmp.path().join("markers"),
            &stop,
            &pause_file,
            NOW,
        );
        let manifest = manifest();
        let mut controller = controller(Counts::default());
        controller.observe(observation);
        let load = LoadHistory::default();
        let disk = DiskState::default();
        let view = View {
            manifest: &manifest,
            controller: &controller,
            usage: &[],
            load: &load,
            disk: &disk,
            alerts: &[],
            recovery: &recovery::Status::default(),
            now: Instant::now(),
        };
        notice_texts(&view)
            .into_iter()
            .map(|(text, _)| text)
            .collect()
    }

    #[test]
    fn a_pause_notice_states_the_condition_recorded_in_the_pause_file() {
        // 通知は pause file の理由（成立している条件だけを書いたもの）を出す。42% で拒否された
        // 一時停止や、前回の実行の中断記録から保留した一時停止を「閾値超過」と出さない。
        use crate::rate_control::{PauseCause, PauseSource, PauseState};
        let resume = format_clock(NOW + 3_600, NOW);
        let cases = [
            (
                PauseState::new(
                    NOW + 3_600,
                    "five_hour",
                    "request rejected (five_hour); five_hour at 42%",
                )
                .observed(PauseSource::Api, PauseCause::Rejected, NOW - 60)
                .with_basis(42.0, Some(NOW + 3_570)),
                "request rejected (five_hour); five_hour at 42%",
            ),
            (
                PauseState::new(
                    NOW + 3_600,
                    "five_hour",
                    "a resumed session was rate limited (five_hour) until 2027-01-15 09:00",
                )
                .observed(PauseSource::Saved, PauseCause::Rejected, NOW - 3_600)
                .with_basis(100.0, Some(NOW + 3_570)),
                "a resumed session was rate limited (five_hour) until 2027-01-15 09:00",
            ),
            (
                PauseState::new(NOW + 3_600, "five_hour", "five_hour 96% >= threshold 95%")
                    .observed(PauseSource::Api, PauseCause::Threshold, NOW - 60)
                    .with_basis(96.0, Some(NOW + 3_570)),
                "five_hour 96% >= threshold 95%",
            ),
            // 改行・エスケープシーケンス・制御文字は 1 行に畳んで除く。
            (
                PauseState::new(
                    NOW + 3_600,
                    "five_hour",
                    "usage-gate:\r\n\x1b[31mfive_hour 96%\x1b[0m >= threshold 95%",
                ),
                "usage-gate: five_hour 96% >= threshold 95%",
            ),
            // 理由の無い（旧形式の）一時停止では、原因を推測せず枠名だけを出す。
            (
                PauseState::new(NOW + 3_600, "five_hour", ""),
                "five_hour window",
            ),
        ];
        for (state, shown) in cases {
            assert_eq!(
                notices_for_pause_file(&state).first().map(String::as_str),
                Some(format!(" 💤 Paused: {shown} — resuming at {resume}").as_str())
            );
        }
    }

    fn controller_with(total: usize, counts: Counts) -> Controller {
        let mut controller = Controller::new(total, 2, NOW + 100, NOW);
        controller.observe(Observation {
            now: NOW,
            counts,
            ..Observation::default()
        });
        controller
    }

    #[test]
    fn format_clock_adds_the_date_only_on_another_day() {
        let now = Local::now();
        assert_eq!(
            format_clock(now.timestamp(), now.timestamp()),
            now.format("%H:%M").to_string()
        );
        let tomorrow = now + chrono::Duration::days(1);
        assert_eq!(
            format_clock(tomorrow.timestamp(), now.timestamp()),
            tomorrow.format("%m/%d %H:%M").to_string()
        );
    }

    #[test]
    fn sparkline_levels_and_gaps() {
        let spans = sparkline(
            &[
                None,
                Some(0.0),
                Some(12.5),
                Some(13.0),
                Some(100.0),
                Some(f64::NAN),
            ],
            |_| CPU_SERIES,
        );
        let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, " ▁▁▂█▁");
        assert_eq!(spans[0].style.fg, None);
        assert_eq!(spans[1].style.fg, Some(CPU_SERIES));
    }

    #[test]
    fn disk_rows_show_stale_values_and_unavailable_volumes() {
        let t0 = Instant::now();
        let mut disk = startup_disk(t0);
        disk.apply(
            Update::Readings {
                source: Source::AiUsage,
                readings: vec![
                    Reading {
                        target: "/".into(),
                        status: Status::Timeout,
                        volume: None,
                    },
                    Reading {
                        target: "/Volumes/Archive".into(),
                        status: Status::NotMounted,
                        volume: None,
                    },
                ],
            },
            t0 + Duration::from_secs(200),
        );
        let lines: Vec<String> = disk_lines(&disk, 90, t0 + Duration::from_secs(200))
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert!(lines[0].contains("3m ago"), "{lines:?}");
        assert!(lines[1].contains("unavailable: not mounted"), "{lines:?}");
        assert!(
            lines[1].starts_with("        /Volumes/Archive"),
            "{lines:?}"
        );
    }

    #[test]
    fn bar_cells_fill_the_width_and_keep_small_parts_visible() {
        let volume = Volume {
            name: "X".into(),
            total: 1_000,
            free: 1,
            available: 1,
            purgeable: None,
        };
        let cells = bar_cells(&volume, 10);
        assert_eq!(cells.iter().sum::<usize>(), 10);
        assert_eq!(cells[2], 1, "{cells:?}");
        assert_eq!(percent_label(&volume), " 99%");
        let empty = Volume {
            name: "Y".into(),
            total: 1_000,
            free: 1_000,
            available: 1_000,
            purgeable: Some(0),
        };
        assert_eq!(percent_label(&empty), "  0%");
    }

    #[test]
    fn rates_and_sizes_use_binary_units_for_memory() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(512.0), "512 B/s");
        assert_eq!(format_rate(1536.0), "1.5 KiB/s");
        assert_eq!(format_rate(f64::NAN), "0 B/s");
        assert_eq!(format_gib(24 * GIB), "24.0 GiB");
        assert_eq!(format_bytes(70 * GB), "70.0 GB");
        assert_eq!(format_bytes(999_960_000_000), "1.0 TB");
    }
}
