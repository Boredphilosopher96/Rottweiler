#![cfg(test)]
use super::AgentLoopError;
use super::Arc;
use super::CheckpointStore;
use super::DurableCheckpointCoordinator;
use super::EngineEvent;
use super::MutationCheckpointCoordinator;
use super::MutationCheckpointOutcome;
use super::MutationScope;
use super::PathBuf;
use super::SessionEventLog;
use super::SessionId;
use super::checkpoint_root;
use super::checkpoint_two_edits;
use super::load_rewind_coordinator;
use super::open_checkpoint_stores;
use super::recover_rewind_transactions;
use super::tempdir;
use rw_store::checkpoint::CheckpointBlobStore;

#[test]
fn per_session_checkpoint_namespaces_isolate_pending_recovery() {
    let root = tempdir().expect("root");
    let workspace_a = root.path().join("workspace-a");
    let workspace_b = root.path().join("workspace-b");
    std::fs::create_dir_all(&workspace_a).expect("workspace a");
    std::fs::create_dir_all(&workspace_b).expect("workspace b");
    std::fs::write(workspace_a.join("file.txt"), "a-before").expect("a file");
    std::fs::write(workspace_b.join("file.txt"), "b-before").expect("b file");
    let storage = root.path().join("storage");
    let session_a = "session-a";
    let session_b = "session-b";
    let root_a = checkpoint_root(&storage, &workspace_a, session_a);
    let root_b = checkpoint_root(&storage, &workspace_b, session_b);
    assert_ne!(root_a, root_b);
    let store_a = CheckpointStore::open(
        &root_a,
        &workspace_a,
        CheckpointBlobStore::open(&storage, &workspace_a).expect("workspace a blobs"),
    )
    .expect("store a");
    let store_b = CheckpointStore::open(
        &root_b,
        &workspace_b,
        CheckpointBlobStore::open(&storage, &workspace_b).expect("workspace b blobs"),
    )
    .expect("store b");
    let pending_a = store_a
        .begin_opaque_mutation(
            session_a,
            1,
            &mut rw_store::checkpoint::CheckpointOperation::default(),
        )
        .expect("pending a");
    let pending_b = store_b
        .begin_opaque_mutation(
            session_b,
            1,
            &mut rw_store::checkpoint::CheckpointOperation::default(),
        )
        .expect("pending b");
    std::fs::write(workspace_a.join("file.txt"), "a-after").expect("mutate a");
    std::fs::write(workspace_b.join("file.txt"), "b-after").expect("mutate b");

    let recovered = store_a
        .recover_opaque_mutations(&mut rw_store::checkpoint::CheckpointOperation::default())
        .expect("recover a only");
    assert_eq!(recovered, 1);
    assert!(
        store_a
            .finish_opaque_mutation(
                &pending_a,
                &mut rw_store::checkpoint::CheckpointOperation::default()
            )
            .is_err(),
        "a marker was consumed by its recovery"
    );
    store_b
        .finish_opaque_mutation(
            &pending_b,
            &mut rw_store::checkpoint::CheckpointOperation::default(),
        )
        .expect("b marker must remain untouched");
    assert_eq!(
        std::fs::read_to_string(workspace_b.join("file.txt")).expect("b file"),
        "b-after"
    );

    checkpoint_two_edits(&store_a, session_a, &workspace_a, "a");
    checkpoint_two_edits(&store_b, session_b, &workspace_b, "b");
    let rewind_a = store_a
        .prepare_rewind(session_a, 9, "rewind-a-zero")
        .expect("stage rewind a");
    let rewind_b = store_b
        .prepare_rewind(session_b, 9, "rewind-b-zero")
        .expect("stage rewind b");
    let recovered_a = store_a.recover_rewinds().expect("recover rewind a only");
    assert_eq!(recovered_a.len(), 1);
    assert_eq!(recovered_a[0].handle, rewind_a);
    assert_eq!(
        std::fs::read_to_string(workspace_a.join("file.txt")).expect("rewound a"),
        "a-zero"
    );
    assert_eq!(
        std::fs::read_to_string(workspace_b.join("file.txt")).expect("untouched b"),
        "b-two"
    );
    store_b.apply_rewind(&rewind_b).expect("apply rewind b");
    assert_eq!(
        std::fs::read_to_string(workspace_b.join("file.txt")).expect("rewound b"),
        "b-zero"
    );
    store_a.acknowledge_rewind(&rewind_a).expect("ack rewind a");
    store_b.acknowledge_rewind(&rewind_b).expect("ack rewind b");
}

#[tokio::test]
async fn durable_coordinator_rewinds_ten_edits_to_turn_three_byte_exactly() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("state.txt"), b"turn-0\n").expect("initial state");
    let session = SessionId("session-rewind".to_owned());
    let coordinator_root = checkpoint_root(root.path(), &workspace, &session.0);
    let store = Arc::new(
        CheckpointStore::open(
            &coordinator_root,
            &workspace,
            CheckpointBlobStore::open(root.path(), &workspace).expect("workspace blobs"),
        )
        .expect("checkpoint store"),
    );
    let coordinator = DurableCheckpointCoordinator::new(coordinator_root, store);
    for turn in 1..=10_u64 {
        let checkpoint = coordinator
            .begin(
                &session,
                turn,
                &format!("edit-{turn}"),
                &MutationScope::Paths(vec![PathBuf::from("state.txt")]),
            )
            .await
            .expect("begin checkpoint");
        std::fs::write(
            workspace.join("state.txt"),
            format!("turn-{turn}\n").as_bytes(),
        )
        .expect("edit state");
        coordinator
            .finish(&checkpoint, MutationCheckpointOutcome::Completed)
            .await
            .expect("finish checkpoint");
    }
    let rewind = coordinator
        .prepare_apply_rewind(&session, 3, "rewind-test-3")
        .await
        .expect("apply rewind");
    assert_eq!(
        std::fs::read(workspace.join("state.txt")).expect("rewound bytes"),
        b"turn-3\n"
    );
    coordinator
        .acknowledge_rewind(&rewind)
        .await
        .expect("ack rewind");
}

#[tokio::test]
async fn shared_workspace_sessions_serialize_mutation_checkpoints() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("shared.txt"), "base\n").expect("fixture");
    let first_store = Arc::new(
        CheckpointStore::open(
            &root.path().join("first"),
            &workspace,
            CheckpointBlobStore::open(root.path(), &workspace).expect("shared workspace blobs"),
        )
        .expect("first store"),
    );
    let second_store = Arc::new(
        CheckpointStore::open(
            &root.path().join("second"),
            &workspace,
            CheckpointBlobStore::open(root.path(), &workspace).expect("shared workspace blobs"),
        )
        .expect("second store"),
    );
    let first = Arc::new(DurableCheckpointCoordinator::new(
        root.path().join("first"),
        first_store,
    ));
    let second = Arc::new(DurableCheckpointCoordinator::new(
        root.path().join("second"),
        second_store,
    ));
    let first_checkpoint = first
        .begin(
            &SessionId("parent".to_owned()),
            1,
            "parent-edit",
            &MutationScope::Paths(vec![PathBuf::from("shared.txt")]),
        )
        .await
        .expect("parent begins");
    let child_begin = tokio::spawn({
        let second = Arc::clone(&second);
        async move {
            second
                .begin(
                    &SessionId("child".to_owned()),
                    2,
                    "child-edit",
                    &MutationScope::Paths(vec![PathBuf::from("shared.txt")]),
                )
                .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(!child_begin.is_finished());
    first
        .finish(&first_checkpoint, MutationCheckpointOutcome::Completed)
        .await
        .expect("parent finishes");
    let child_checkpoint = tokio::time::timeout(std::time::Duration::from_secs(1), child_begin)
        .await
        .expect("child unblocks")
        .expect("child task")
        .expect("child begins");
    second
        .finish(&child_checkpoint, MutationCheckpointOutcome::Completed)
        .await
        .expect("child finishes");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn multi_root_checkpoints_restore_known_and_opaque_added_root_mutations() {
    let root = tempdir().expect("root");
    let primary = root.path().join("primary");
    let added = root.path().join("added");
    std::fs::create_dir_all(&primary).expect("primary");
    std::fs::create_dir_all(&added).expect("added");
    let primary = std::fs::canonicalize(primary).expect("canonical primary");
    let added = std::fs::canonicalize(added).expect("canonical added");
    let parent_sentinel = root.path().join("parent.txt");
    std::fs::write(&parent_sentinel, b"parent-before").expect("parent sentinel");
    let target = added.join("state.bin");
    std::fs::write(&target, b"added-before\0bytes").expect("added target");
    let session = SessionId("session-multi-root-rewind".to_owned());
    let checkpoint_root = checkpoint_root(root.path(), &primary, &session.0);
    let stores = open_checkpoint_stores(
        root.path(),
        &checkpoint_root,
        &[primary.clone(), added.clone()],
    )
    .expect("multi-root stores");
    assert!(
        open_checkpoint_stores(
            root.path(),
            &checkpoint_root,
            &[added.clone(), primary.clone()]
        )
        .is_err(),
        "persisted root order must reject reorder/replacement"
    );
    let coordinator = DurableCheckpointCoordinator::from_stores(checkpoint_root.clone(), stores);

    let known = coordinator
        .begin(
            &session,
            1,
            "known-added",
            &MutationScope::Paths(vec![PathBuf::from("@root/1/state.bin")]),
        )
        .await
        .expect("known checkpoint");
    std::fs::write(&target, b"known-after").expect("known mutation");
    coordinator
        .finish(&known, MutationCheckpointOutcome::Completed)
        .await
        .expect("finish known");
    let rewind = coordinator
        .prepare_apply_rewind(&session, 0, "rewind-known-added")
        .await
        .expect("rewind known");
    assert_eq!(
        std::fs::read(&target).expect("known restored"),
        b"added-before\0bytes"
    );
    coordinator
        .acknowledge_rewind(&rewind)
        .await
        .expect("ack known rewind");

    let sibling = coordinator
        .begin(
            &session,
            2,
            "known-added-sibling",
            &MutationScope::Paths(vec![PathBuf::from("../added/state.bin")]),
        )
        .await
        .expect("sibling checkpoint");
    std::fs::write(&target, b"sibling-after").expect("sibling mutation");
    coordinator
        .finish(&sibling, MutationCheckpointOutcome::Completed)
        .await
        .expect("finish sibling");
    let rewind = coordinator
        .prepare_apply_rewind(&session, 1, "rewind-known-sibling")
        .await
        .expect("rewind sibling");
    assert_eq!(
        std::fs::read(&target).expect("sibling restored"),
        b"added-before\0bytes"
    );
    coordinator
        .acknowledge_rewind(&rewind)
        .await
        .expect("ack sibling rewind");

    let escaped = coordinator
        .begin(
            &session,
            3,
            "parent-escape",
            &MutationScope::Paths(vec![PathBuf::from("@root/1/../parent.txt")]),
        )
        .await;
    assert!(
        escaped.is_err(),
        "checkpoint confinement must block parent escape"
    );
    assert_eq!(
        std::fs::read(&parent_sentinel).expect("parent remains"),
        b"parent-before"
    );

    let git = |arguments: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&added)
            .args(arguments)
            .status()
            .expect("git command");
        assert!(status.success(), "git command failed: {arguments:?}");
    };
    git(&["init", "--quiet"]);
    git(&["add", "state.bin"]);
    let opaque = coordinator
        .begin(&session, 4, "opaque-added", &MutationScope::OpaqueWorkspace)
        .await
        .expect("opaque checkpoint");
    std::fs::write(&target, b"opaque-after").expect("opaque mutation");
    coordinator
        .finish(&opaque, MutationCheckpointOutcome::Completed)
        .await
        .expect("finish opaque");
    let rewind = coordinator
        .prepare_apply_rewind(&session, 3, "rewind-opaque-added")
        .await
        .expect("rewind opaque");
    assert_eq!(
        std::fs::read(&target).expect("opaque restored"),
        b"added-before\0bytes"
    );
    coordinator
        .acknowledge_rewind(&rewind)
        .await
        .expect("ack opaque rewind");
    assert_eq!(
        std::fs::read(&parent_sentinel).expect("parent final"),
        b"parent-before"
    );
}

#[tokio::test]
async fn failed_multi_root_rewind_is_not_committed_by_restart_recovery() {
    let root = tempdir().expect("root");
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir_all(&first).expect("first workspace");
    std::fs::create_dir_all(&second).expect("second workspace");
    let first = std::fs::canonicalize(first).expect("canonical first");
    let second = std::fs::canonicalize(second).expect("canonical second");
    let session = SessionId("failed-multi-root-rewind".to_owned());
    let checkpoint_root = checkpoint_root(root.path(), &first, &session.0);
    let stores = open_checkpoint_stores(
        root.path(),
        &checkpoint_root,
        &[first.clone(), second.clone()],
    )
    .expect("multi-root stores");

    for (store, workspace) in stores.iter().zip([&first, &second]) {
        std::fs::write(workspace.join("state.txt"), b"before").expect("initial state");
        store
            .checkpoint_known(
                &session.0,
                1,
                [PathBuf::from("state.txt")],
                &mut rw_store::checkpoint::CheckpointOperation::default(),
            )
            .expect("checkpoint");
        std::fs::write(workspace.join("state.txt"), b"after").expect("mutated state");
    }

    let second_manifest = checkpoint_root
        .join("root-0001/checkpoints/manifests")
        .join(&session.0)
        .join("00000000000000000001.json");
    let valid_manifest = std::fs::read(&second_manifest).expect("valid second manifest");
    std::fs::write(&second_manifest, b"{}").expect("corrupt second manifest");

    let coordinator =
        DurableCheckpointCoordinator::from_stores(checkpoint_root.clone(), Arc::clone(&stores));
    assert!(
        coordinator
            .prepare_apply_rewind(&session, 0, "failed-multi-root-operation")
            .await
            .is_err(),
        "the second root must fail after the first root stages"
    );
    drop(coordinator);
    std::fs::write(second_manifest, valid_manifest).expect("repair second manifest");

    let event_root = root.path().join("event-store");
    let mut log = SessionEventLog::open(&event_root, &session.0).expect("event log");
    recover_rewind_transactions(&checkpoint_root, &stores, &mut log).expect("restart recovery");

    assert_eq!(
        std::fs::read(first.join("state.txt")).expect("first state"),
        b"after"
    );
    assert_eq!(
        std::fs::read(second.join("state.txt")).expect("second state"),
        b"after"
    );
    assert!(
        log.load::<EngineEvent>()
            .expect("events")
            .iter()
            .all(|event| !matches!(event.event, EngineEvent::ConversationRewound { .. }))
    );
}

#[tokio::test]
async fn committed_multi_root_rewind_is_completed_by_restart_recovery() {
    let root = tempdir().expect("root");
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir_all(&first).expect("first workspace");
    std::fs::create_dir_all(&second).expect("second workspace");
    let first = std::fs::canonicalize(first).expect("canonical first");
    let second = std::fs::canonicalize(second).expect("canonical second");
    let session = SessionId("committed-multi-root-rewind".to_owned());
    let checkpoint_root = checkpoint_root(root.path(), &first, &session.0);
    let stores = open_checkpoint_stores(
        root.path(),
        &checkpoint_root,
        &[first.clone(), second.clone()],
    )
    .expect("multi-root stores");

    for (store, workspace) in stores.iter().zip([&first, &second]) {
        std::fs::write(workspace.join("state.txt"), b"before").expect("initial state");
        store
            .checkpoint_known(
                &session.0,
                1,
                [PathBuf::from("state.txt")],
                &mut rw_store::checkpoint::CheckpointOperation::default(),
            )
            .expect("checkpoint");
        std::fs::write(workspace.join("state.txt"), b"after").expect("mutated state");
    }

    let coordinator =
        DurableCheckpointCoordinator::from_stores(checkpoint_root.clone(), Arc::clone(&stores));
    coordinator.fail_after_committed_rewind_decision();
    let failure = coordinator
        .prepare_apply_rewind(&session, 0, "committed-multi-root-operation")
        .await
        .expect_err("injected crash after commit decision");
    assert!(failure.to_string().contains("injected crash"));
    assert_eq!(
        std::fs::read(first.join("state.txt")).expect("first state"),
        b"after"
    );
    assert_eq!(
        std::fs::read(second.join("state.txt")).expect("second state"),
        b"after"
    );
    drop(coordinator);

    let event_root = root.path().join("event-store");
    let mut log = SessionEventLog::open(&event_root, &session.0).expect("event log");
    recover_rewind_transactions(&checkpoint_root, &stores, &mut log).expect("restart recovery");

    assert_eq!(
        std::fs::read(first.join("state.txt")).expect("first restored"),
        b"before"
    );
    assert_eq!(
        std::fs::read(second.join("state.txt")).expect("second restored"),
        b"before"
    );
    let rewind_events = log
        .load::<EngineEvent>()
        .expect("events")
        .into_iter()
        .filter(|event| matches!(event.event, EngineEvent::ConversationRewound { .. }))
        .count();
    assert_eq!(rewind_events, 1);
    assert!(
        load_rewind_coordinator(&checkpoint_root)
            .expect("coordinator state")
            .is_none()
    );
}

#[tokio::test]
async fn committed_multi_root_apply_failure_completes_in_process() {
    let root = tempdir().expect("root");
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir_all(&first).expect("first workspace");
    std::fs::create_dir_all(&second).expect("second workspace");
    let first = std::fs::canonicalize(first).expect("canonical first");
    let second = std::fs::canonicalize(second).expect("canonical second");
    let session = SessionId("retry-multi-root-rewind".to_owned());
    let checkpoint_root = checkpoint_root(root.path(), &first, &session.0);
    let stores = open_checkpoint_stores(
        root.path(),
        &checkpoint_root,
        &[first.clone(), second.clone()],
    )
    .expect("multi-root stores");

    for (store, workspace) in stores.iter().zip([&first, &second]) {
        std::fs::write(workspace.join("state.txt"), b"before").expect("initial state");
        store
            .checkpoint_known(
                &session.0,
                1,
                [PathBuf::from("state.txt")],
                &mut rw_store::checkpoint::CheckpointOperation::default(),
            )
            .expect("checkpoint");
        std::fs::write(workspace.join("state.txt"), b"after").expect("mutated state");
    }

    let coordinator =
        DurableCheckpointCoordinator::from_stores(checkpoint_root.clone(), Arc::clone(&stores));
    coordinator.fail_rewind_apply_at_root(1, false);
    let rewind = coordinator
        .prepare_apply_rewind(&session, 0, "retry-multi-root-operation")
        .await
        .expect("same-process recovery must complete the committed rewind");
    assert_eq!(
        std::fs::read(first.join("state.txt")).expect("first restored"),
        b"before"
    );
    assert_eq!(
        std::fs::read(second.join("state.txt")).expect("second restored"),
        b"before"
    );
    coordinator
        .acknowledge_rewind(&rewind)
        .await
        .expect("acknowledge recovered rewind");

    let checkpoint = coordinator
        .begin(
            &session,
            2,
            "post-rewind-checkpoint",
            &MutationScope::Paths(vec![PathBuf::from("state.txt")]),
        )
        .await
        .expect("successful recovery must leave workspace mutations available");
    coordinator
        .finish(&checkpoint, MutationCheckpointOutcome::Completed)
        .await
        .expect("finish post-rewind checkpoint");
}

#[tokio::test]
async fn repeated_committed_multi_root_apply_failure_poisons_live_mutations() {
    let root = tempdir().expect("root");
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir_all(&first).expect("first workspace");
    std::fs::create_dir_all(&second).expect("second workspace");
    let first = std::fs::canonicalize(first).expect("canonical first");
    let second = std::fs::canonicalize(second).expect("canonical second");
    let session = SessionId("poisoned-multi-root-rewind".to_owned());
    let checkpoint_root = checkpoint_root(root.path(), &first, &session.0);
    let stores = open_checkpoint_stores(
        root.path(),
        &checkpoint_root,
        &[first.clone(), second.clone()],
    )
    .expect("multi-root stores");

    for (store, workspace) in stores.iter().zip([&first, &second]) {
        std::fs::write(workspace.join("state.txt"), b"before").expect("initial state");
        store
            .checkpoint_known(
                &session.0,
                1,
                [PathBuf::from("state.txt")],
                &mut rw_store::checkpoint::CheckpointOperation::default(),
            )
            .expect("checkpoint");
        std::fs::write(workspace.join("state.txt"), b"after").expect("mutated state");
    }

    let coordinator =
        DurableCheckpointCoordinator::from_stores(checkpoint_root.clone(), Arc::clone(&stores));
    coordinator.fail_rewind_apply_at_root(1, true);
    let failure = coordinator
        .prepare_apply_rewind(&session, 0, "poisoned-multi-root-operation")
        .await
        .expect_err("repeated apply failure must remain visible");
    assert!(
        failure
            .to_string()
            .contains("immediate committed rewind recovery failed")
    );
    assert_eq!(
        std::fs::read(first.join("state.txt")).expect("first partial state"),
        b"before"
    );
    assert_eq!(
        std::fs::read(second.join("state.txt")).expect("second partial state"),
        b"after"
    );

    let blocked = coordinator
        .begin(
            &session,
            2,
            "blocked-after-rewind",
            &MutationScope::Paths(vec![PathBuf::from("state.txt")]),
        )
        .await
        .expect_err("mixed workspace state must block later mutation");
    assert!(matches!(blocked, AgentLoopError::Persistence(_)));
    assert!(
        coordinator.session_review(&session).await.is_err(),
        "mixed workspace state must not be presented as a coherent review"
    );

    let peer =
        DurableCheckpointCoordinator::from_stores(checkpoint_root.clone(), Arc::clone(&stores));
    let peer_blocked = peer
        .begin(
            &session,
            2,
            "peer-blocked-after-rewind",
            &MutationScope::Paths(vec![PathBuf::from("state.txt")]),
        )
        .await
        .expect_err("every coordinator for the workspace must observe rewind poison");
    assert!(matches!(peer_blocked, AgentLoopError::Persistence(_)));

    drop(coordinator);
    drop(peer);
    let event_root = root.path().join("event-store");
    let mut log = SessionEventLog::open(&event_root, &session.0).expect("event log");
    recover_rewind_transactions(&checkpoint_root, &stores, &mut log).expect("restart recovery");
    assert_eq!(
        std::fs::read(first.join("state.txt")).expect("first recovered"),
        b"before"
    );
    assert_eq!(
        std::fs::read(second.join("state.txt")).expect("second recovered"),
        b"before"
    );
}
