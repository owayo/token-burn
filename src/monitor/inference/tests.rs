use super::*;
use std::io::Write;

const SESSION: &str = "018cc251-f400-7000-8000-000000000001";

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    monitor: Monitor,
}

impl Fixture {
    fn new(provider: Provider) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("config");
        let reports = temporary.path().join("reports");
        let markers = temporary.path().join("markers");
        for path in [&root, &reports, &markers] {
            fs::create_dir_all(path).unwrap();
        }
        let monitor = Monitor::new(
            Configuration {
                provider: Some(provider),
                settings: Settings {
                    model: Some("requested-model".into()),
                    effort: None,
                },
                session_roots: vec![root.clone()],
                started_at: Some(chrono::Utc::now()),
            },
            reports,
            markers,
        );
        Self {
            _temporary: temporary,
            root,
            monitor,
        }
    }

    fn transcript(&self, provider: Provider) -> PathBuf {
        let path = match provider {
            Provider::Claude => self
                .root
                .join("projects/project")
                .join(format!("{SESSION}.jsonl")),
            // UUID は 2024-01-01 UTC。CLI のローカル日付が翌日でも見つかる。
            Provider::Codex => self
                .root
                .join("sessions/2024/01/02")
                .join(format!("rollout-example-{SESSION}.jsonl")),
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    fn start(&self, provider: Provider) {
        let value = match provider {
            Provider::Claude => {
                serde_json::json!({"type":"system","subtype":"init","session_id":SESSION,"model":"session-model"})
            }
            Provider::Codex => serde_json::json!({"type":"thread.started","thread_id":SESSION}),
        };
        fs::write(
            self.monitor.reports.join("0001_project.jsonl"),
            format!("{value}\n"),
        )
        .unwrap();
        fs::write(self.monitor.markers.join("worker-task-0"), "0001").unwrap();
    }
}

#[test]
fn claude_uses_the_matching_session_and_tracks_per_turn_effort() {
    let mut fixture = Fixture::new(Provider::Claude);
    fixture.start(Provider::Claude);
    let path = fixture.transcript(Provider::Claude);
    fs::write(&path, concat!(
        "{\"type\":\"assistant\",\"message\":{\"model\":\"actual-model\"},\"effort\":\"max\"}\n",
        "{\"type\":\"assistant\",\"isSidechain\":true,\"message\":{\"model\":\"subagent-model\"},\"effort\":\"low\"}\n",
        "{\"type\":\"assistant\",\"parent_tool_use_id\":\"tool-1\",\"message\":{\"model\":\"another-subagent\"}}\n"
    )).unwrap();
    let settings = fixture.monitor.poll();
    assert_eq!(settings.model.as_deref(), Some("actual-model"));
    assert_eq!(settings.effort.as_deref(), Some("max"));
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    write!(file, "{{\"type\":\"assistant\",\"message\":{{\"model\":\"actual-model\"}},\"effort\":\"max\",\"perTurnEffort\":\"high\"}}").unwrap();
    assert_eq!(fixture.monitor.poll().effort.as_deref(), Some("max"));
    writeln!(file).unwrap();
    assert_eq!(fixture.monitor.poll().effort.as_deref(), Some("high"));
}

#[test]
fn codex_rollout_overrides_launch_settings_and_follows_model_changes() {
    let mut fixture = Fixture::new(Provider::Codex);
    fixture.start(Provider::Codex);
    let path = fixture.transcript(Provider::Codex);
    fs::write(&path, "{\"type\":\"turn_context\",\"payload\":{\"model\":\"actual-model\",\"effort\":\"xhigh\"}}\n").unwrap();
    let settings = fixture.monitor.poll();
    assert_eq!(settings.model.as_deref(), Some("actual-model"));
    assert_eq!(settings.effort.as_deref(), Some("xhigh"));
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{{\"type\":\"turn_context\",\"payload\":{{\"model\":\"next-model\",\"effort\":\"high\"}}}}"
    )
    .unwrap();
    let settings = fixture.monitor.poll();
    assert_eq!(settings.model.as_deref(), Some("next-model"));
    assert_eq!(settings.effort.as_deref(), Some("high"));
}

#[test]
fn resumed_sessions_do_not_report_the_previous_runs_effort() {
    let mut fixture = Fixture::new(Provider::Claude);
    fixture.start(Provider::Claude);
    let path = fixture.transcript(Provider::Claude);
    fs::write(&path, "{\"type\":\"assistant\",\"timestamp\":\"2000-01-01T00:00:00Z\",\"message\":{\"model\":\"old-model\"},\"effort\":\"low\"}\n").unwrap();
    let settings = fixture.monitor.poll();
    assert_eq!(settings.model.as_deref(), Some("session-model"));
    assert_eq!(settings.effort, None);
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", serde_json::json!({"type":"assistant","timestamp":chrono::Utc::now().to_rfc3339(),"message":{"model":"current-model"},"effort":"max"})).unwrap();
    assert_eq!(fixture.monitor.poll().effort.as_deref(), Some("max"));
}

#[test]
fn duplicate_session_records_and_invalid_ids_are_not_guessed() {
    let fixture = Fixture::new(Provider::Claude);
    let path = fixture.transcript(Provider::Claude);
    fs::write(&path, "{}").unwrap();
    let other = fixture.root.join("projects/other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join(format!("{SESSION}.jsonl")), "{}").unwrap();
    assert_eq!(
        find_transcript(&[fixture.root], Provider::Claude, SESSION),
        None
    );
    assert!(!valid_session_id("../../other-session"));
    assert!(!valid_session_id("not-a-session"));
}

#[test]
fn only_active_tasks_contribute_and_fresh_logs_replace_failed_resumes() {
    let mut fixture = Fixture::new(Provider::Claude);
    fixture.start(Provider::Claude);
    fs::write(
        fixture.monitor.reports.join("0002_other.jsonl"),
        "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"other-model\"}\n",
    )
    .unwrap();
    assert_eq!(
        fixture.monitor.poll().model.as_deref(),
        Some("session-model")
    );
    fs::write(
        fixture.monitor.reports.join("0001_project.fresh.jsonl"),
        "{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"fresh-model\"}\n",
    )
    .unwrap();
    assert_eq!(fixture.monitor.poll().model.as_deref(), Some("fresh-model"));
    fs::write(fixture.monitor.markers.join("worker-task-1"), "0002").unwrap();
    assert_eq!(
        fixture.monitor.poll().model.as_deref(),
        Some("fresh-model, other-model")
    );
}

#[test]
fn json_tail_is_bounded_and_recovers_after_an_oversized_line_and_truncation() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("large.jsonl");
    let mut data = vec![b'x'; 5 * 1024 * 1024];
    data.extend_from_slice(b"\n{\"type\":\"turn_context\"}\n");
    fs::write(&path, data).unwrap();
    let mut tail = JsonTail::default();
    for _ in 0..5 {
        assert!(tail.read(&path).is_empty());
    }
    assert_eq!(
        tail.read(&path),
        vec![serde_json::json!({"type":"turn_context"})]
    );
    fs::write(&path, "{\"type\":\"new\"}\n").unwrap();
    assert_eq!(tail.read(&path), vec![serde_json::json!({"type":"new"})]);
}
