use rat::event::Event;
use rat::{EventLog, FileEventLog};

#[test]
fn append_read_reopen_preserves_seq() {
    let dir = tempdir();
    let path = dir.join("session.log");

    {
        let mut log = FileEventLog::open(&path).unwrap();
        log.append(Event::SessionStarted {
            command: "/bin/sh".into(),
            args: vec![],
            env: vec![],
            cwd: "/tmp".into(),
            cols: 80,
            rows: 24,
        })
        .unwrap();
        log.append(Event::PtyOutput {
            data: b"hello\n".to_vec(),
        })
        .unwrap();
        log.append(Event::ClientInput {
            data: b"ls\n".to_vec(),
        })
        .unwrap();
        assert_eq!(log.latest_seq(), 3);
    }

    // Reopen: seq counter must resume past previous events.
    let mut log = FileEventLog::open(&path).unwrap();
    assert_eq!(log.latest_seq(), 3);

    let events = log.read_from(0).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].seq, 0);
    assert_eq!(events[2].seq, 2);

    log.append(Event::SessionEnded { exit_code: Some(0) }).unwrap();
    let tail = log.read_from(3).unwrap();
    assert_eq!(tail.len(), 1);
    assert!(matches!(
        tail[0].event,
        Event::SessionEnded { exit_code: Some(0) }
    ));

    std::fs::remove_dir_all(&dir).ok();
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("rat-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&base).unwrap();
    base
}
