//! 無出力が続いたときの生存表示（ハートビート）のタイミングと内容のテスト。
//! 時刻は合成した `Instant` で与え、実時間を待たない。

use super::*;

const TURN_STARTED: &str = r#"{"type":"turn.started"}"#;

fn command_started(id: &str, script: &str) -> String {
    format!(
        r#"{{"type":"item.started","item":{{"id":"{id}","type":"command_execution","command":"/bin/zsh -lc '{script}'","aggregated_output":"","exit_code":null,"status":"in_progress"}}}}"#
    )
}

fn command_completed(id: &str, script: &str) -> String {
    format!(
        r#"{{"type":"item.completed","item":{{"id":"{id}","type":"command_execution","command":"/bin/zsh -lc '{script}'","aggregated_output":"","exit_code":0,"status":"completed"}}}}"#
    )
}

#[test]
fn no_heartbeat_before_two_minutes_of_silence() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(&mut formatter, TURN_STARTED, t0);
    assert_eq!(idle(&mut formatter, t0 + secs(1)), "");
    assert_eq!(idle(&mut formatter, t0 + secs(60)), "");
    assert_eq!(idle(&mut formatter, t0 + millis(119_999)), "");
}

#[test]
fn first_heartbeat_at_two_minutes_then_every_two_minutes() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(&mut formatter, TURN_STARTED, t0);
    assert_eq!(
        idle(&mut formatter, t0 + secs(120)),
        "  \u{23f3} No codex events for 2m 00s [12:34:56]\n"
    );
    // 次は 2 分後まで出さない（毎秒の待ち明けでログを埋めない）。
    assert_eq!(idle(&mut formatter, t0 + secs(121)), "");
    assert_eq!(idle(&mut formatter, t0 + secs(239)), "");
    assert_eq!(
        idle(&mut formatter, t0 + secs(240)),
        "  \u{23f3} No codex events for 4m 00s [12:34:56]\n"
    );
    assert_eq!(idle(&mut formatter, t0 + secs(300)), "");
    assert_eq!(
        idle(&mut formatter, t0 + secs(390)),
        "  \u{23f3} No codex events for 6m 30s [12:34:56]\n"
    );
}

#[test]
fn a_late_idle_tick_still_produces_one_heartbeat() {
    // 待ち明けが遅れて届いても、溜まった分をまとめて出したりはしない。
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(&mut formatter, TURN_STARTED, t0);
    assert_eq!(
        idle(&mut formatter, t0 + secs(400)),
        "  \u{23f3} No codex events for 6m 40s [12:34:56]\n"
    );
    assert_eq!(idle(&mut formatter, t0 + secs(401)), "");
    assert_eq!(
        idle(&mut formatter, t0 + secs(520)),
        "  \u{23f3} No codex events for 8m 40s [12:34:56]\n"
    );
}

#[test]
fn a_new_event_resets_the_heartbeat() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(&mut formatter, TURN_STARTED, t0);
    assert!(!idle(&mut formatter, t0 + secs(120)).is_empty());

    // 新しい行が届いたら、次はその行から 2 分後まで出さない。
    feed(
        &mut formatter,
        r#"{"type":"item.completed","item":{"id":"r1","type":"reasoning","text":"thinking"}}"#,
        t0 + secs(130),
    );
    assert_eq!(idle(&mut formatter, t0 + secs(240)), "");
    assert_eq!(idle(&mut formatter, t0 + secs(249)), "");
    assert_eq!(
        idle(&mut formatter, t0 + secs(250)),
        "  \u{23f3} No codex events for 2m 00s [12:34:56]\n"
    );
}

#[test]
fn non_json_and_blank_lines_also_reset_the_heartbeat() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(&mut formatter, TURN_STARTED, t0);
    feed(
        &mut formatter,
        "Reading additional input from stdin...",
        t0 + secs(100),
    );
    assert_eq!(idle(&mut formatter, t0 + secs(219)), "");
    // 表示するものが無い空行でも、届いた以上は沈黙ではない。
    assert_eq!(feed(&mut formatter, "", t0 + secs(219)), "");
    assert_eq!(idle(&mut formatter, t0 + secs(338)), "");
    assert_eq!(
        idle(&mut formatter, t0 + secs(339)),
        "  \u{23f3} No codex events for 2m 00s [12:34:56]\n"
    );
}

#[test]
fn heartbeat_before_any_event_counts_from_the_start() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    assert_eq!(idle(&mut formatter, t0 + secs(119)), "");
    assert_eq!(
        idle(&mut formatter, t0 + secs(120)),
        "  \u{23f3} No codex events for 2m 00s [12:34:56]\n"
    );
}

#[test]
fn heartbeat_mentions_the_longest_running_command() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(&mut formatter, &command_started("c1", "cargo test"), t0);
    feed(
        &mut formatter,
        &command_started("c2", "sleep 600"),
        t0 + secs(10),
    );
    feed(
        &mut formatter,
        &command_started("c3", "npm ci"),
        t0 + secs(20),
    );
    assert_eq!(
        idle(&mut formatter, t0 + secs(140)),
        "  \u{23f3} No codex events for 2m 00s [12:34:56] \u{2014} running: $ cargo test (2m 20s) and 2 more\n"
    );
}

#[test]
fn heartbeat_with_a_single_running_command_has_no_more_suffix() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(
        &mut formatter,
        &command_started("c1", "cargo build --release"),
        t0,
    );
    assert_eq!(
        idle(&mut formatter, t0 + secs(3_725)),
        "  \u{23f3} No codex events for 1h 02m [12:34:56] \u{2014} running: $ cargo build --release (1h 02m)\n"
    );
}

#[test]
fn heartbeat_truncates_the_running_command() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    let script = format!("echo {}", "y".repeat(200));
    feed(&mut formatter, &command_started("c1", &script), t0);
    let shown = idle(&mut formatter, t0 + secs(120));
    let command = shown
        .split_once("running: $ ")
        .and_then(|(_, rest)| rest.strip_suffix(" (2m 00s)\n"))
        .expect("running command");
    assert_eq!(command.chars().count(), 60);
    assert!(command.ends_with("..."));
}

#[test]
fn completed_commands_are_no_longer_mentioned() {
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(&mut formatter, &command_started("c1", "cargo test"), t0);
    feed(
        &mut formatter,
        &command_completed("c1", "cargo test"),
        t0 + secs(5),
    );
    assert_eq!(
        idle(&mut formatter, t0 + secs(125)),
        "  \u{23f3} No codex events for 2m 00s [12:34:56]\n"
    );
}

#[test]
fn turn_end_forgets_commands_that_never_completed() {
    // 完了イベントを伴わずにターンが終わった（中断された）コマンドを、以後のハートビートで
    // 「実行中」と表示し続けない。
    let t0 = Instant::now();
    let mut formatter = new_formatter(t0);
    feed(&mut formatter, &command_started("c1", "cargo test"), t0);
    feed(
        &mut formatter,
        r#"{"type":"turn.failed","error":{"message":"stream disconnected"}}"#,
        t0 + secs(5),
    );
    assert_eq!(
        idle(&mut formatter, t0 + secs(125)),
        "  \u{23f3} No codex events for 2m 00s [12:34:56]\n"
    );
}
