use super::*;

#[test]
fn fork_copies_exact_prefix_and_parent_and_child_diverge_independently() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut parent = SessionEventLog::open(root.path(), "parent")
        .unwrap_or_else(|error| panic!("parent must open: {error}"));
    let events = (0..3)
        .map(|index| FixtureEvent {
            kind: "parent".to_owned(),
            text: format!("event-{index}"),
        })
        .collect::<Vec<_>>();
    parent
        .append_batch(events.clone())
        .unwrap_or_else(|error| panic!("parent events must append: {error}"));
    drop(parent);

    let mut child = SessionEventLog::fork(root.path(), "parent", "child", Some(SequenceId(1)))
        .unwrap_or_else(|error| panic!("child fork must succeed: {error}"));
    assert_eq!(
        child
            .load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("child prefix must load: {error}"))
            .into_iter()
            .map(|event| event.event)
            .collect::<Vec<_>>(),
        events[..2]
    );
    let parent_bytes = std::fs::read(root.path().join("sessions/parent/journal/active.jsonl"))
        .unwrap_or_else(|error| panic!("parent bytes must read: {error}"));
    let child_bytes = std::fs::read(child.path().join("active.jsonl"))
        .unwrap_or_else(|error| panic!("child bytes must read: {error}"));
    let expected_prefix_len = parent_bytes
        .split_inclusive(|byte| *byte == b'\n')
        .take(2)
        .map(<[u8]>::len)
        .sum::<usize>();
    assert_eq!(child_bytes, parent_bytes[..expected_prefix_len]);
    child
        .append(FixtureEvent {
            kind: "child".to_owned(),
            text: "diverged".to_owned(),
        })
        .unwrap_or_else(|error| panic!("child divergence must append: {error}"));
    drop(child);

    let mut parent = SessionEventLog::open(root.path(), "parent")
        .unwrap_or_else(|error| panic!("parent must reopen: {error}"));
    parent
        .append(FixtureEvent {
            kind: "parent".to_owned(),
            text: "continued".to_owned(),
        })
        .unwrap_or_else(|error| panic!("parent continuation must append: {error}"));
    drop(parent);

    let parent = SessionEventLog::load_existing::<FixtureEvent>(root.path(), "parent")
        .unwrap_or_else(|error| panic!("parent must load: {error}"));
    let child = SessionEventLog::load_existing::<FixtureEvent>(root.path(), "child")
        .unwrap_or_else(|error| panic!("child must load: {error}"));
    assert_eq!(parent.len(), 4);
    assert_eq!(child.len(), 3);
    assert_eq!(parent[2].event, events[2]);
    assert_eq!(parent[3].event.text, "continued");
    assert_eq!(child[2].event.kind, "child");
    assert!(matches!(
        SessionEventLog::fork(root.path(), "parent", "child", Some(SequenceId(1))),
        Err(SessionStoreError::ForkTargetConflict)
    ));
    assert!(matches!(
        SessionEventLog::fork(root.path(), "parent", "parent", None),
        Err(SessionStoreError::ForkIdentityConflict)
    ));
}

#[test]
fn fork_resumes_an_exact_partial_child_prefix_idempotently() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let events = (0..3)
        .map(|index| FixtureEvent {
            kind: "fixture".to_owned(),
            text: format!("event-{index}"),
        })
        .collect::<Vec<_>>();
    let mut parent = SessionEventLog::open(root.path(), "parent")
        .unwrap_or_else(|error| panic!("parent must open: {error}"));
    parent
        .append_batch(events.clone())
        .unwrap_or_else(|error| panic!("parent must append: {error}"));
    drop(parent);
    let mut partial = SessionEventLog::open(root.path(), "partial")
        .unwrap_or_else(|error| panic!("partial child must open: {error}"));
    partial
        .append(events[0].clone())
        .unwrap_or_else(|error| panic!("partial child must append: {error}"));
    drop(partial);

    let completed = SessionEventLog::fork(root.path(), "parent", "partial", Some(SequenceId(2)))
        .unwrap_or_else(|error| panic!("partial fork must recover: {error}"));
    assert_eq!(
        completed
            .load::<FixtureEvent>()
            .unwrap_or_else(|error| panic!("completed child must load: {error}"))
            .into_iter()
            .map(|event| event.event)
            .collect::<Vec<_>>(),
        events
    );
}

#[test]
fn read_view_pages_use_an_exclusive_cursor_and_reject_ahead() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir must create: {error}"));
    let mut log = SessionEventLog::open(root.path(), "suffix")
        .unwrap_or_else(|error| panic!("log must open: {error}"));
    let appended = log
        .append_batch((0..4).map(|index| FixtureEvent {
            kind: "sample".to_owned(),
            text: index.to_string(),
        }))
        .unwrap_or_else(|error| panic!("events must append: {error}"));

    assert_eq!(
        log.read_view()
            .page::<FixtureEvent>(None, SessionEventPageLimits::default())
            .map_or_else(
                |error| panic!("full suffix must load: {error}"),
                |page| page.events
            ),
        appended
    );
    assert_eq!(
        log.read_view()
            .page::<FixtureEvent>(Some(SequenceId(1)), SessionEventPageLimits::default())
            .map_or_else(
                |error| panic!("tail suffix must load: {error}"),
                |page| page.events
            ),
        appended[2..]
    );
    assert!(
        log.read_view()
            .page::<FixtureEvent>(Some(SequenceId(3)), SessionEventPageLimits::default())
            .map_or_else(
                |error| panic!("empty suffix must load: {error}"),
                |page| page.events
            )
            .is_empty()
    );
    assert!(matches!(
        log.read_view()
            .page::<FixtureEvent>(Some(SequenceId(4)), SessionEventPageLimits::default())
            .map(|page| page.events),
        Err(SessionStoreError::EventPageCursorAhead)
    ));

    let original = std::fs::read(log.path().join("active.jsonl"))
        .unwrap_or_else(|error| panic!("test log must read: {error}"));
    let mut mutated = original.clone();
    mutated[0] = b'[';
    std::fs::write(log.path().join("active.jsonl"), &mutated)
        .unwrap_or_else(|error| panic!("same-length mutation must succeed: {error}"));
    assert!(matches!(
        log.read_view()
            .page::<FixtureEvent>(Some(SequenceId(1)), SessionEventPageLimits::default())
            .map(|page| page.events),
        Err(SessionStoreError::CorruptEvent(
            "pinned journal segment checksum changed"
        ))
    ));
    std::fs::write(log.path().join("active.jsonl"), original)
        .unwrap_or_else(|error| panic!("test log restore must succeed: {error}"));

    OpenOptions::new()
        .write(true)
        .open(log.path().join("active.jsonl"))
        .and_then(|file| file.set_len(0))
        .unwrap_or_else(|error| panic!("test truncation must succeed: {error}"));
    assert!(matches!(
        log.read_view()
            .page::<FixtureEvent>(Some(SequenceId(1)), SessionEventPageLimits::default())
            .map(|page| page.events),
        Err(SessionStoreError::CorruptEvent(
            "pinned journal segment length changed"
        ))
    ));
}

#[test]
fn bounded_history_read_rejects_bytes_and_event_count_before_returning_data() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut log = SessionEventLog::open(root.path(), "bounded")
        .unwrap_or_else(|error| panic!("open log: {error}"));
    log.append(FixtureEvent {
        kind: "fixture".to_owned(),
        text: "bounded payload".to_owned(),
    })
    .unwrap_or_else(|error| panic!("append event: {error}"));
    drop(log);
    assert!(matches!(
        SessionEventLog::load_existing_bounded::<FixtureEvent>(root.path(), "bounded", 1, 10),
        Err(SessionStoreError::EventLogTooLarge { .. })
    ));
    assert!(matches!(
        SessionEventLog::load_existing_bounded::<FixtureEvent>(
            root.path(),
            "bounded",
            1024 * 1024,
            0,
        ),
        Err(SessionStoreError::EventCountTooLarge { max_events: 0 })
    ));
    let expected_bytes =
        std::fs::metadata(root.path().join("sessions/bounded/journal/active.jsonl"))
            .unwrap_or_else(|error| panic!("event metadata: {error}"))
            .len();
    let (events, descriptor_bytes) =
        SessionEventLog::load_existing_bounded_with_size::<FixtureEvent>(
            root.path(),
            "bounded",
            1024 * 1024,
            10,
        )
        .unwrap_or_else(|error| panic!("bounded descriptor read: {error}"));
    assert_eq!(events.len(), 1);
    assert_eq!(descriptor_bytes, expected_bytes);
}

#[test]
fn paged_history_streams_logs_beyond_twenty_thousand_events() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut log = SessionEventLog::open(root.path(), "paged-many")
        .unwrap_or_else(|error| panic!("open log: {error}"));
    log.append_batch((0..20_050).map(|index| FixtureEvent {
        kind: "fixture".to_owned(),
        text: format!("event-{index}"),
    }))
    .unwrap_or_else(|error| panic!("append events: {error}"));
    drop(log);

    let page = SessionEventLog::load_existing_page::<FixtureEvent>(
        root.path(),
        "paged-many",
        Some(SequenceId(19_999)),
        SessionEventPageLimits {
            max_page_events: 25,
            max_page_bytes: 1024 * 1024,
            max_scan_events: 25_000,
            max_scan_bytes: 64 * 1024 * 1024,
            max_line_bytes: 64 * 1024,
        },
    )
    .unwrap_or_else(|error| panic!("paged read: {error}"));
    assert_eq!(page.events.len(), 25);
    assert_eq!(page.events[0].sequence, SequenceId(20_000));
    assert_eq!(page.next_cursor, Some(SequenceId(20_024)));
    assert_eq!(page.total_events, 20_050);
    assert_eq!(page.tail_sequence, Some(SequenceId(20_049)));
    assert_eq!(page.events_before_page, 20_000);
    assert_eq!(page.events_after_page, 25);
    assert!(page.has_more);
}

#[test]
fn paged_history_streams_logs_beyond_eight_megabytes_with_bounded_lines() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut log = SessionEventLog::open(root.path(), "paged-large")
        .unwrap_or_else(|error| panic!("open log: {error}"));
    let payload = "x".repeat(1024 * 1024);
    log.append_batch((0..9).map(|_| FixtureEvent {
        kind: "fixture".to_owned(),
        text: payload.clone(),
    }))
    .unwrap_or_else(|error| panic!("append events: {error}"));
    drop(log);

    let limits = SessionEventPageLimits {
        max_page_events: 1,
        max_page_bytes: 2 * 1024 * 1024,
        max_line_bytes: 2 * 1024 * 1024,
        max_scan_bytes: 16 * 1024 * 1024,
        max_scan_events: 100,
    };
    let page = SessionEventLog::load_existing_page::<FixtureEvent>(
        root.path(),
        "paged-large",
        None,
        limits,
    )
    .unwrap_or_else(|error| panic!("paged read: {error}"));
    assert!(page.total_bytes > 8 * 1024 * 1024);
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.total_events, 9);
    assert_eq!(page.events_after_page, 8);
    assert!(page.has_more);

    assert!(matches!(
        SessionEventLog::load_existing_page::<FixtureEvent>(
            root.path(),
            "paged-large",
            None,
            SessionEventPageLimits {
                max_line_bytes: 1024,
                ..limits
            },
        ),
        Err(SessionStoreError::EventRecordTooLarge {
            max_line_bytes: 1024
        })
    ));
}

#[test]
fn paged_history_cursor_walk_has_exact_tail_and_truncation_metadata() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut log = SessionEventLog::open(root.path(), "paged-cursor")
        .unwrap_or_else(|error| panic!("open log: {error}"));
    log.append_batch((0..7).map(|index| FixtureEvent {
        kind: "fixture".to_owned(),
        text: format!("event-{index}"),
    }))
    .unwrap_or_else(|error| panic!("append events: {error}"));
    drop(log);
    let limits = SessionEventPageLimits {
        max_page_events: 3,
        max_page_bytes: 1024 * 1024,
        max_line_bytes: 64 * 1024,
        max_scan_bytes: 1024 * 1024,
        max_scan_events: 100,
    };
    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = SessionEventLog::load_existing_page::<FixtureEvent>(
            root.path(),
            "paged-cursor",
            cursor,
            limits,
        )
        .unwrap_or_else(|error| panic!("paged read: {error}"));
        seen.extend(page.events.iter().map(|event| event.sequence));
        cursor = page.next_cursor;
        if !page.has_more {
            assert_eq!(page.total_events, 7);
            assert_eq!(page.tail_sequence, Some(SequenceId(6)));
            assert_eq!(page.events_after_page, 0);
            break;
        }
    }
    assert_eq!(seen, (0..7).map(SequenceId).collect::<Vec<_>>());
    let tail = SessionEventLog::load_existing_page::<FixtureEvent>(
        root.path(),
        "paged-cursor",
        cursor,
        limits,
    )
    .unwrap_or_else(|error| panic!("tail read: {error}"));
    assert!(tail.events.is_empty());
    assert_eq!(tail.next_cursor, Some(SequenceId(6)));
    assert_eq!(tail.events_before_page, 7);
    assert!(!tail.has_more);
}

#[test]
fn paged_history_validates_sequences_before_and_after_the_requested_cursor() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut log = SessionEventLog::open(root.path(), "paged-corrupt")
        .unwrap_or_else(|error| panic!("open log: {error}"));
    log.append_batch((0..4).map(|index| FixtureEvent {
        kind: "fixture".to_owned(),
        text: format!("event-{index}"),
    }))
    .unwrap_or_else(|error| panic!("append events: {error}"));
    let path = log.path().join("active.jsonl");
    drop(log);
    let mut lines = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read fixture: {error}"))
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut corrupt: serde_json::Value =
        serde_json::from_str(&lines[1]).unwrap_or_else(|error| panic!("decode fixture: {error}"));
    corrupt["sequence"] = serde_json::json!("9");
    lines[1] =
        serde_json::to_string(&corrupt).unwrap_or_else(|error| panic!("encode fixture: {error}"));
    std::fs::write(&path, format!("{}\n", lines.join("\n")))
        .unwrap_or_else(|error| panic!("write corruption: {error}"));

    assert!(matches!(
        SessionEventLog::load_existing_page::<FixtureEvent>(
            root.path(),
            "paged-corrupt",
            Some(SequenceId(2)),
            SessionEventPageLimits::default(),
        ),
        Err(SessionStoreError::CorruptEvent(
            "non-contiguous event sequence"
        ))
    ));
}

#[test]
fn paged_history_rejects_concurrent_descriptor_mutation_before_returning_data() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let mut log = SessionEventLog::open(root.path(), "paged-mutated")
        .unwrap_or_else(|error| panic!("open: {error}"));
    log.append(FixtureEvent {
        kind: "fixture".to_owned(),
        text: "first".to_owned(),
    })
    .unwrap_or_else(|error| panic!("append: {error}"));
    log.append(FixtureEvent {
        kind: "fixture".to_owned(),
        text: "x".repeat(1024 * 1024),
    })
    .unwrap_or_else(|error| panic!("rotate: {error}"));
    let sealed = std::fs::read_dir(log.path())
        .unwrap_or_else(|error| panic!("directory: {error}"))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| panic!("entry: {error}"))
                .path()
        })
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                && path.file_name().is_none_or(|name| name != "active.jsonl")
        })
        .unwrap_or_else(|| panic!("sealed segment"));
    super::EVENT_READ_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            OpenOptions::new()
                .append(true)
                .open(sealed)
                .and_then(|mut file| file.write_all(b"changed"))
                .unwrap_or_else(|error| panic!("mutate opened segment: {error}"));
        }));
    });
    let result = log
        .read_view()
        .page::<FixtureEvent>(None, SessionEventPageLimits::default());
    assert!(matches!(
        result,
        Err(SessionStoreError::EventFileChangedDuringRead)
    ));
}

#[cfg(unix)]
#[test]
fn bounded_history_read_rejects_multi_link_event_files() {
    let root = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let log = SessionEventLog::open(root.path(), "linked")
        .unwrap_or_else(|error| panic!("open log: {error}"));
    let link = root.path().join("linked-copy.jsonl");
    std::fs::hard_link(log.path().join("active.jsonl"), &link)
        .unwrap_or_else(|error| panic!("hard link fixture: {error}"));
    drop(log);
    assert!(matches!(
        SessionEventLog::load_existing_bounded_with_size::<FixtureEvent>(
            root.path(),
            "linked",
            1024,
            10,
        ),
        Err(SessionStoreError::UnsafeEventFileType)
    ));
}

#[test]
#[allow(clippy::expect_used)]
fn paged_fork_resumes_partial_copy_without_a_whole_parent_batch() {
    let root = tempfile::tempdir().expect("root");
    let mut parent = SessionEventLog::open(root.path(), "paged-parent").expect("parent");
    for start in (0..5000_u64).step_by(250) {
        parent
            .append_batch((start..start + 250).map(|sequence| {
                serde_json::json!({
                    "sequence": sequence, "payload": "x".repeat(4096)
                })
            }))
            .expect("bounded source batch");
    }
    let view = parent.read_view();
    assert!(view.total_bytes() > 16 * 1024 * 1024);
    let interrupted = SessionEventLog::fork_mapped_view::<serde_json::Value, _>(
        root.path(),
        "paged-parent",
        "paged-child",
        &view,
        Some(SequenceId(4999)),
        |event| {
            if event["sequence"] == 700 {
                Err(SessionStoreError::CorruptEvent("mapping interrupted"))
            } else {
                Ok(event)
            }
        },
    );
    assert!(interrupted.is_err());
    let partial = super::journal::JournalReadView::open_existing(root.path(), "paged-child")
        .expect("partial view")
        .expect("partial child");
    assert_eq!(partial.last_sequence(), Some(SequenceId(511)));
    let child = SessionEventLog::fork_mapped_view::<serde_json::Value, _>(
        root.path(),
        "paged-parent",
        "paged-child",
        &view,
        Some(SequenceId(4999)),
        Ok,
    )
    .expect("resume matching prefix");
    assert_eq!(child.last_sequence(), Some(SequenceId(4999)));
    assert_eq!(
        child
            .read_view()
            .verify_all()
            .expect("child integrity")
            .events,
        5000
    );
}
