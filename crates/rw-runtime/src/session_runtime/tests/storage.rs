#![cfg(test)]
use super::AccountingAttribution;
use super::AccountingLedger;
use super::ClientId;
use super::Cost;
use super::DurableEventSink;
use super::EngineEvent;
use super::EventMeta;
use super::JournalReads;
use super::MAX_SESSION_METADATA_BYTES;
use super::PathBuf;
use super::SESSION_EVENT_VERSION;
use super::SequenceId;
use super::SessionEventLog;
use super::SessionEventSink;
use super::SessionId;
use super::SessionIndex;
use super::SessionReplayLimits;
use super::TempDir;
use super::TurnId;
use super::TurnStatus;
use super::Usage;
use super::append_checkpoint_root_generation;
use super::checkpoint_root;
use super::commit_checkpoint_root_generation;
use super::compact_title;
use super::fork_hosted_session_storage;
use super::inherited_accounting_through;
use super::initialize_private_storage_root;
use super::io;
use super::load_session_events;
use super::load_session_metadata;
use super::load_session_metadata_any_bounded;
use super::load_session_workspace_roots;
use super::open_checkpoint_stores;
use super::persist_session_metadata;
use super::preview_persisted_workspace_roots;
use super::project_accounting;
use super::project_session;
use super::restore_persisted_workspace_roots;
use super::tempdir;

#[tokio::test]
#[ignore = "manual long-session reattach benchmark"]
async fn durable_event_sink_long_gap_metrics() {
    const EVENTS: u64 = 20_000;
    const TAIL_READS: usize = 10;

    let storage = TempDir::new().expect("storage");
    let session = SessionId("durable-gap-metrics".to_owned());
    let mut log = SessionEventLog::open(storage.path(), &session.0).expect("event log");
    let events = (0..EVENTS).map(|sequence| EngineEvent::SessionCreated {
        meta: EventMeta {
            protocol_version: SESSION_EVENT_VERSION,
            session_id: session.clone(),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        },
        driver_client_id: ClientId("benchmark-driver".to_owned()),
    });
    log.append_batch(events).expect("benchmark event batch");
    let sink = DurableEventSink::new(
        log,
        storage.path().to_owned(),
        session.0.clone(),
        JournalReads::new(storage.path()).expect("journal reads"),
    )
    .expect("durable sink");

    let tail_started = std::time::Instant::now();
    for _ in 0..TAIL_READS {
        assert_eq!(
            sink.last_sequence().await.expect("durable tail"),
            Some(SequenceId(EVENTS - 1))
        );
    }
    let tail_elapsed = tail_started.elapsed();

    let gap_started = std::time::Instant::now();
    let gap = sink
        .capture_read_view()
        .expect("view")
        .read_page(
            Some(SequenceId(EVENTS - 101)),
            SessionReplayLimits::default(),
        )
        .await
        .expect("durable tail gap");
    let gap_elapsed = gap_started.elapsed();
    assert_eq!(gap.len(), 100);
    eprintln!(
        "durable_replay_metric events={EVENTS} tail_reads={TAIL_READS} tail_us={} tail_gap_us={} gap_events={}",
        tail_elapsed.as_micros(),
        gap_elapsed.as_micros(),
        gap.len()
    );
}

#[cfg(unix)]
#[test]
fn session_metadata_reads_are_bounded_descriptor_stable_and_single_link() {
    let root = tempdir().expect("metadata root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(root.path().join("sessions/metadata-bounds"))
        .expect("session directory");
    persist_session_metadata(
        root.path(),
        "metadata-bounds",
        &workspace,
        "default",
        &[],
        std::slice::from_ref(&workspace),
    )
    .expect("metadata fixture");
    let path = root.path().join("sessions/metadata-bounds/metadata.json");
    let expected_bytes = std::fs::metadata(&path).expect("metadata size").len();
    let (metadata, descriptor_bytes) = load_session_metadata_any_bounded(
        root.path(),
        "metadata-bounds",
        MAX_SESSION_METADATA_BYTES,
    )
    .expect("bounded metadata read");
    assert_eq!(metadata.session_id, "metadata-bounds");
    assert_eq!(descriptor_bytes, expected_bytes);
    assert_eq!(
        inherited_accounting_through(root.path(), "metadata-bounds").expect("accounting boundary"),
        None
    );

    let alias = root.path().join("metadata-hardlink.json");
    std::fs::hard_link(&path, &alias).expect("hard link fixture");
    assert!(
        load_session_metadata_any_bounded(
            root.path(),
            "metadata-bounds",
            MAX_SESSION_METADATA_BYTES,
        )
        .is_err()
    );
    assert!(inherited_accounting_through(root.path(), "metadata-bounds").is_err());
    std::fs::remove_file(alias).expect("remove hard link");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|file| file.set_len(MAX_SESSION_METADATA_BYTES + 1))
        .expect("oversized sparse metadata");
    assert!(
        load_session_metadata_any_bounded(
            root.path(),
            "metadata-bounds",
            MAX_SESSION_METADATA_BYTES,
        )
        .is_err()
    );
    assert!(inherited_accounting_through(root.path(), "metadata-bounds").is_err());
}

#[cfg(unix)]
#[test]
fn storage_root_creation_is_private_without_rewriting_existing_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = TempDir::new().expect("fixture");
    let fixture = fixture.path().canonicalize().expect("canonical fixture");
    let absent = fixture.join("new").join("storage");
    initialize_private_storage_root(&absent).expect("create absent storage root");
    assert_eq!(
        std::fs::symlink_metadata(&absent)
            .expect("new storage metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    crate::subagent_metadata::PrivateSubagentMetadataStore::open(&absent)
        .expect("new private storage accepted");

    let existing = fixture.join("existing-storage");
    std::fs::create_dir(&existing).expect("existing storage");
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755))
        .expect("permissive existing storage");
    let error = initialize_private_storage_root(&existing)
        .expect_err("reject permissive caller storage root");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::symlink_metadata(&existing)
            .expect("existing storage metadata")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn session_titles_are_bounded_and_single_line() {
    let title = compact_title(&format!("hello\n{}", "world ".repeat(30)));
    assert!(!title.contains('\n'));
    assert!(title.chars().count() <= 80);
}

#[test]
fn durable_generated_title_overrides_prompt_fallback_in_the_session_index() {
    let fixture = tempdir().expect("fixture");
    let storage = fixture.path().join("storage");
    initialize_private_storage_root(&storage).expect("storage");
    let session_id = "session-generated-title";
    let event_meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: SessionId(session_id.to_owned()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00Z".to_owned(),
        caused_by: None,
    };
    let events = vec![
        EngineEvent::UserMessageAccepted {
            meta: event_meta(0),
            agent_turn: 1,
            content: "please inspect everything in this repo".to_owned(),
            attachments: Vec::new(),
        },
        EngineEvent::SessionTitleUpdated {
            meta: event_meta(1),
            title: "Repository Architecture Review".to_owned(),
            usage: None,
            cost: None,
        },
    ];
    let path = fixture.path().join("projection-fixture");
    std::fs::write(&path, b"fixture").expect("event file");
    let projection = project_session(session_id, &events, &path);
    assert_eq!(projection.summary.title, "Repository Architecture Review");

    SessionIndex::open(&storage)
        .expect("index")
        .upsert(&projection)
        .expect("upsert");
    assert_eq!(
        SessionIndex::open(&storage)
            .expect("index")
            .get(session_id)
            .expect("query")
            .expect("session")
            .title,
        "Repository Architecture Review"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn fork_storage_starts_empty_review_and_skips_inherited_accounting() {
    let fixture = tempdir().expect("fixture");
    let storage = fixture.path().join("storage");
    let workspace = fixture.path().join("workspace");
    let added = fixture.path().join("added");
    let added_later = fixture.path().join("added-later");
    std::fs::create_dir(&storage).expect("storage");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&added).expect("added workspace");
    std::fs::create_dir(&added_later).expect("later added workspace");
    #[cfg(unix)]
    std::fs::set_permissions(
        &storage,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("storage permissions");
    initialize_private_storage_root(&storage).expect("private storage");
    std::fs::create_dir(storage.join("sessions")).expect("sessions directory");
    let workspace = workspace.canonicalize().expect("canonical workspace");
    let added = added.canonicalize().expect("canonical added workspace");
    let added_later = added_later
        .canonicalize()
        .expect("canonical later added workspace");
    let parent = SessionId("fork-storage-parent".to_owned());
    let child = SessionId("fork-storage-child".to_owned());
    let driver = ClientId("current-driver".to_owned());
    std::fs::create_dir(storage.join("sessions").join(&parent.0))
        .expect("parent session directory");
    persist_session_metadata(
        &storage,
        &parent.0,
        &workspace,
        "fast",
        &[],
        std::slice::from_ref(&workspace),
    )
    .expect("parent metadata");
    let parent_stores = open_checkpoint_stores(
        &checkpoint_root(&storage, &workspace, &parent.0),
        std::slice::from_ref(&workspace),
    )
    .expect("parent checkpoints");
    let parent_checkpoint_root = checkpoint_root(&storage, &workspace, &parent.0);
    append_checkpoint_root_generation(
        &parent_checkpoint_root,
        std::slice::from_ref(&workspace),
        &[workspace.clone(), added.clone()],
        1,
        2,
    )
    .expect("prepare added root");
    commit_checkpoint_root_generation(&parent_checkpoint_root, 1).expect("commit added root");
    append_checkpoint_root_generation(
        &parent_checkpoint_root,
        &[workspace.clone(), added.clone()],
        &[workspace.clone(), added.clone(), added_later.clone()],
        2,
        3,
    )
    .expect("prepare later root");
    commit_checkpoint_root_generation(&parent_checkpoint_root, 2).expect("commit later root");
    std::fs::write(workspace.join("tracked.txt"), "base\n").expect("baseline file");
    parent_stores[0]
        .checkpoint_known(
            &parent.0,
            1,
            [PathBuf::from("tracked.txt")],
            &mut rw_store::checkpoint::CheckpointOperation::default(),
        )
        .expect("parent checkpoint");
    std::fs::write(workspace.join("tracked.txt"), "parent change\n").expect("parent mutation");
    assert_eq!(
        parent_stores[0]
            .session_review(&parent.0)
            .expect("review")
            .files
            .len(),
        1
    );

    let mut log = SessionEventLog::open(&storage, &parent.0).expect("parent log");
    let meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: parent.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-07-10T12:34:56.789Z".to_owned(),
        caused_by: None,
    };
    log.append(EngineEvent::SessionCreated {
        meta: meta(0),
        driver_client_id: ClientId("historic-driver".to_owned()),
    })
    .expect("created");
    log.append(EngineEvent::TurnStarted {
        meta: meta(1),
        turn_id: TurnId("1".to_owned()),
    })
    .expect("started");
    log.append(EngineEvent::TurnFinished {
        meta: meta(2),
        turn_id: TurnId("1".to_owned()),
        status: TurnStatus::Completed,
        usage: Usage {
            input_tokens: 3,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: Cost::AiCredits {
            credits_micros: 7,
            nominal_amount_micros: None,
            currency: None,
        },
    })
    .expect("finished");
    log.append(EngineEvent::WorkspaceRootsChanged {
        meta: meta(3),
        generation: 1,
        effective_from_turn: 2,
        roots: vec![
            rw_core::WorkspaceRootDescriptor {
                index: 0,
                path: "@root/0".to_owned(),
                machine_local: false,
            },
            rw_core::WorkspaceRootDescriptor {
                index: 1,
                path: "@root/1".to_owned(),
                machine_local: false,
            },
        ],
    })
    .expect("workspace roots changed");
    log.append(EngineEvent::TurnStarted {
        meta: meta(4),
        turn_id: TurnId("2".to_owned()),
    })
    .expect("second turn started");
    log.append(EngineEvent::TurnFinished {
        meta: meta(5),
        turn_id: TurnId("2".to_owned()),
        status: TurnStatus::Completed,
        usage: Usage {
            input_tokens: 2,
            output_tokens: 3,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: Cost::AiCredits {
            credits_micros: 4,
            nominal_amount_micros: None,
            currency: None,
        },
    })
    .expect("second turn finished");
    log.append(EngineEvent::WorkspaceRootsChanged {
        meta: meta(6),
        generation: 2,
        effective_from_turn: 3,
        roots: (0..3)
            .map(|index| rw_core::WorkspaceRootDescriptor {
                index,
                path: format!("@root/{index}"),
                machine_local: false,
            })
            .collect(),
    })
    .expect("later workspace roots changed");
    drop(log);
    let parent_path = storage
        .join("sessions")
        .join(&parent.0)
        .join("journal")
        .join("active.jsonl");
    let parent_bytes = std::fs::read(&parent_path).expect("parent bytes");
    let fork_modes = rw_ext::ModeRegistry::builtins().expect("built-in modes");
    fork_hosted_session_storage(
        &JournalReads::new(&storage).expect("journal reads"),
        &storage,
        &workspace,
        &parent.0,
        &child.0,
        2,
        None,
        false,
        driver.clone(),
        None,
        &fork_modes,
    )
    .expect("fork");
    assert_eq!(
        std::fs::read(parent_path).expect("parent remains"),
        parent_bytes
    );

    let child_events =
        load_session_events(&SessionEventLog::open(&storage, &child.0).expect("child log"))
            .expect("child events");
    assert!(
        matches!(child_events.first(), Some(EngineEvent::SessionCreated {
            meta, driver_client_id,
        }) if meta.session_id == child && driver_client_id == &driver)
    );
    let inherited = inherited_accounting_through(&storage, &child.0).expect("boundary");
    assert_eq!(inherited, Some(SequenceId(5)));
    assert!(
        project_accounting(&child.0, &child_events, inherited)
            .expect("accounting")
            .is_empty()
    );
    let child_metadata =
        load_session_metadata(&storage, &child.0, &workspace).expect("child metadata");
    assert_eq!(
        child_metadata.workspace_roots,
        vec![workspace.clone(), added.clone()]
    );
    assert_eq!(child_metadata.initial_context_workspace_root_count, 1);
    assert_eq!(child_metadata.fork_at_turn, Some(2));
    assert_eq!(
        load_session_workspace_roots(
            &JournalReads::new(&storage).expect("journal reads"),
            &storage,
            &workspace,
            &parent.0
        )
        .expect("current parent roots"),
        vec![workspace.clone(), added.clone(), added_later]
    );
    let child_stores = open_checkpoint_stores(
        &checkpoint_root(&storage, &workspace, &child.0),
        &[workspace.clone(), added],
    )
    .expect("child checkpoints");
    assert!(child_stores.iter().all(|store| {
        store
            .session_review(&child.0)
            .expect("child review")
            .files
            .is_empty()
    }));
    child_stores[0]
        .checkpoint_known(
            &child.0,
            3,
            [PathBuf::from("tracked.txt")],
            &mut rw_store::checkpoint::CheckpointOperation::default(),
        )
        .expect("child checkpoint");
    std::fs::write(workspace.join("tracked.txt"), "child change\n").expect("child edit");
    assert_eq!(
        child_stores[0]
            .session_review(&child.0)
            .expect("child review")
            .files
            .len(),
        1
    );

    let invalid_child = SessionId("fork-storage-invalid-mode".to_owned());
    let parent_roots_path =
        checkpoint_root(&storage, &workspace, &parent.0).join("workspace-roots.json");
    let parent_roots_before = std::fs::read(&parent_roots_path).expect("parent roots journal");
    let mut parent_log = SessionEventLog::open(&storage, &parent.0).expect("parent log");
    parent_log
        .append(EngineEvent::ModeChanged {
            meta: meta(7),
            mode: rw_core::ModeId("removed-custom-mode".to_owned()),
            definition_fingerprint: "stale-fingerprint".to_owned(),
        })
        .expect("custom mode event");
    drop(parent_log);
    let error = fork_hosted_session_storage(
        &JournalReads::new(&storage).expect("journal reads"),
        &storage,
        &workspace,
        &parent.0,
        &invalid_child.0,
        2,
        Some(SequenceId(7)),
        true,
        driver,
        None,
        &fork_modes,
    )
    .expect_err("removed custom mode must reject fork");
    assert!(error.to_string().contains("mode projection"));
    assert!(!storage.join("sessions").join(&invalid_child.0).exists());
    assert!(!checkpoint_root(&storage, &workspace, &invalid_child.0).exists());
    assert_eq!(
        std::fs::read(parent_roots_path).expect("parent roots remain readable"),
        parent_roots_before
    );
}

#[test]
fn local_and_hosted_resume_reject_missing_nonzero_root_journal() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let workspace = workspace.canonicalize().expect("canonical workspace");
    let missing = root.path().join("missing-checkpoint-root");
    for result in [
        preview_persisted_workspace_roots(
            &missing,
            &workspace,
            std::slice::from_ref(&workspace),
            1,
        ),
        restore_persisted_workspace_roots(
            &missing,
            &workspace,
            std::slice::from_ref(&workspace),
            1,
        ),
    ] {
        let error = result.expect_err("nonzero generation requires its root journal");
        assert!(error.to_string().contains("missing its local root journal"));
    }
    assert!(
        preview_persisted_workspace_roots(
            &missing,
            &workspace,
            std::slice::from_ref(&workspace),
            0,
        )
        .expect("generation zero permits no journal")
        .is_none()
    );
}

#[test]
fn accounting_projection_keeps_main_and_compaction_attribution() {
    let meta = |sequence| EventMeta {
        protocol_version: SESSION_EVENT_VERSION,
        session_id: SessionId("accounting-session".to_owned()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-07-10T12:34:56.789Z".to_owned(),
        caused_by: None,
    };
    let usage = Usage {
        input_tokens: 11,
        output_tokens: 12,
        cache_read_tokens: 13,
        cache_write_tokens: 14,
        reasoning_tokens: 15,
    };
    let cost = Cost::AiCredits {
        credits_micros: 16,
        nominal_amount_micros: None,
        currency: None,
    };
    let entries = project_accounting(
        "accounting-session",
        &[
            EngineEvent::TurnFinished {
                meta: meta(3),
                turn_id: TurnId("1".to_owned()),
                status: TurnStatus::Completed,
                usage: usage.clone(),
                cost: cost.clone(),
            },
            EngineEvent::CompactionAttemptFinished {
                meta: meta(4),
                summary_turn_id: TurnId("compact-attempt-1".to_owned()),
                usage: usage.clone(),
                cost: cost.clone(),
            },
            EngineEvent::CompactionFinished {
                meta: meta(5),
                summary_turn_id: TurnId("compact-1".to_owned()),
                reclaimed_tokens: 20,
                usage: Some(usage.clone()),
                cost: Some(cost.clone()),
            },
        ],
        None,
    )
    .expect("accounting projection");

    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].attribution, AccountingAttribution::Main);
    assert_eq!(entries[1].attribution, AccountingAttribution::Compaction);
    assert_eq!(entries[1].sequence_id, SequenceId(4));
    assert_eq!(entries[1].usage, usage);
    assert_eq!(entries[1].cost, cost);
    assert_eq!(entries[1].utc_day.as_str(), "2026-07-10");
    assert_eq!(entries[2].attribution, AccountingAttribution::Compaction);
    assert_eq!(entries[2].sequence_id, SequenceId(5));

    let root = tempdir().expect("accounting ledger root");
    let ledger = AccountingLedger::open(root.path()).expect("accounting ledger");
    ledger.reconcile(&entries).expect("initial reconciliation");
    ledger
        .reconcile(&entries)
        .expect("idempotent reconciliation");
    let persisted = ledger.entries().expect("persisted accounting entries");
    assert_eq!(persisted.len(), 3);
    assert_eq!(persisted[1].sequence_id, SequenceId(4));
    assert_eq!(persisted[2].sequence_id, SequenceId(5));
}
