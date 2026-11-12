use super::*;
#[test]
fn atomic_snapshots_round_trip() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("snapshot.json");
    atomic_json(
        &path,
        &Snapshot {
            ready: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(read_json::<Snapshot>(&path).unwrap().ready);
}

#[test]
fn large_input_is_chunked_without_changing_byte_order() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("controls")).unwrap();
    let bytes: Vec<_> = (0..20000).map(|index| (index % 251) as u8).collect();
    send(
        directory.path(),
        &Control::Input {
            worker: 0,
            bytes: bytes.clone(),
        },
    )
    .unwrap();
    let mut files = std::fs::read_dir(directory.path().join("controls"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    files.sort();
    let mut received = Vec::new();
    for file in files {
        if let Control::Input { bytes, .. } = read_json(&file).unwrap() {
            assert!(bytes.len() <= 4096);
            received.extend(bytes);
        } else {
            panic!("expected input request");
        }
    }
    assert_eq!(received, bytes);
}

#[test]
fn pty_resize_queries_and_cell_rendering_preserve_the_terminal() {
    let temporary = tempfile::tempdir().unwrap();
    let script = temporary.path().join("worker.sh");
    std::fs::write(
        &script,
        r#"stty -echo -icanon min 1 time 0
printf '\033[6n'
IFS= read -r -d R position
printf 'reply=%s\r\n' "${position#*$'\033['}"
read -r line
stty size
printf '\033[31mleft   日本語\033[0m\r\n'
"#,
    )
    .unwrap();
    let mut worker = pty::Worker::spawn(&script).unwrap();
    worker.resize(30, 90).unwrap();
    let ready = std::time::Instant::now();
    while !worker.snapshot().lines.join("").contains("reply=") {
        assert!(ready.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    worker.write(b"continue\n").unwrap();
    let start = std::time::Instant::now();
    let mut status = None;
    while status.is_none() || !worker.drained() {
        if status.is_none() {
            status = worker.poll().unwrap();
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(status, Some(true));
    let snapshot = worker.snapshot();
    let text = snapshot
        .lines
        .iter()
        .flat_map(|line| crate::monitor::ansi::parse_lines(line.as_bytes()))
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("30 90"), "{text}");
    assert!(text.contains("left   日本語"), "{text}");
    assert!(text.contains("reply="), "{text}");
}

#[test]
fn cleanup_keeps_a_locked_native_run() {
    use fs2::FileExt;
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("20000101_000000_test");
    std::fs::create_dir_all(directory.join(".runtime")).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(".runtime/runner.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    assert!(
        crate::cleanup::cleanup_old_reports(temporary.path(), "1d")
            .unwrap()
            .deleted
            .is_empty()
    );
    // Parallel PTY tests may fork while this lock is held. Explicit unlock also releases
    // an inherited descriptor before the child reaches exec and closes it.
    FileExt::unlock(&lock).unwrap();
    drop(lock);
    assert_eq!(
        crate::cleanup::cleanup_old_reports(temporary.path(), "1d")
            .unwrap()
            .deleted,
        [directory]
    );
}
