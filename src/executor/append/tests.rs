use super::*;

#[test]
fn appended_tasks_preserve_resume_policy_and_only_hold_for_future_retries() {
    for (resume, future) in [(true, true), (true, false), (false, true)] {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("runtime");
        let target = temporary.path().join("target");
        for path in [
            directory.join("tasks"),
            directory.join("queue"),
            directory.join("markers"),
            target.clone(),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let target = target.canonicalize().unwrap();
        let now = chrono::Utc::now();
        let mut session = Session {
            monitor: serde_json::from_value(serde_json::json!({
                "agent":"test", "command":"claude", "reset":"", "total":0,
                "deadline_epoch":now.timestamp()+3600, "marker_dir":directory.join("markers"),
                "session":"test", "worker_count":1, "stop_file":directory.join("stop"),
                "report_dir":temporary.path(), "threshold":95, "ai_usage":null
            }))
            .unwrap(),
            workers: vec![],
            state_file: temporary.path().join("state.json"),
            started_at: now.fixed_offset(),
            targets: vec![],
            save_interrupted: true,
        };
        crate::runtime::atomic_json(&directory.join("session.json"), &session).unwrap();
        crate::runtime::atomic_json(
            &directory.join("append.json"),
            &Settings {
                command: vec!["claude".into()],
                env: BTreeMap::new(),
                output: OutputPipeline::ClaudeStreamJson,
                prompt: Ok("review".into()),
                overrides: BTreeMap::new(),
                resume,
                resume_prompt: Ok("continuation".into()),
                peers: vec!["test".into()],
                global: false,
                public_only: false,
            },
        )
        .unwrap();
        fs::write(directory.join("resume-prompt.txt"), "existing continuation").unwrap();
        let saved = crate::resume::ResumeEntry {
            session_id: "0633fa18-8643-40f7-9106-a35330ed247d".into(),
            interrupted_at: (now - chrono::Duration::hours(1)).fixed_offset(),
            retry_after: Some(
                (now + chrono::Duration::minutes(if future { 10 } else { -10 })).fixed_offset(),
            ),
            retry_basis_percent: Some(95.0),
            prompt_hash: crate::resume::prompt_hash("review"),
            message: None,
            log: None,
            failed_resumes: 0,
        };
        crate::resume::save_atomic(
            &crate::resume::resume_path(&session.state_file),
            "test",
            &target,
            saved,
        )
        .unwrap();
        add(&directory, &mut session, &target).unwrap();
        let script = fs::read_to_string(directory.join("tasks/task-0001.sh")).unwrap();
        assert_eq!(script.contains("--resume"), resume, "{script}");
        assert_eq!(
            crate::rate_control::pause_path_for(&session.monitor.stop_file).exists(),
            resume && future
        );
        assert_eq!(
            fs::read_to_string(directory.join("resume-prompt.txt")).unwrap(),
            "existing continuation"
        );
        assert!(directory.join("queue/pending-0001").is_file());
        assert_eq!(session.monitor.total, 1);
        assert_eq!(
            crate::runtime::read_json::<Session>(&directory.join("session.json"))
                .unwrap()
                .targets,
            vec![target]
        );
    }
}
