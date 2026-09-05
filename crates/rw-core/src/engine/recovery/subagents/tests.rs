#![allow(clippy::expect_used)]
use super::SubagentLifecycleIndex;
use crate::SubagentHandle;
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage, recovery::tests::append};
use rw_store::session::journal::SegmentedJournal;
use rw_types::{Cost, DiffArtifact, SessionId, SubagentId};

fn spawn(id: &str) -> PendingEvent {
    PendingEvent::SubagentSpawned {
        subagent_id: SubagentId(id.into()),
        child_session_id: SessionId(format!("session-{id}")),
        task: "work".into(),
    }
}
fn finish(id: &str, artifact: Option<DiffArtifact>) -> PendingEvent {
    let handle = SubagentHandle {
        subagent_id: SubagentId(id.into()),
        session_id: SessionId(format!("session-{id}")),
    };
    let mut result = crate::interrupted_subagent_recovery_result(&handle);
    result.diff_artifact = artifact;
    PendingEvent::SubagentFinished {
        subagent_id: handle.subagent_id,
        result,
    }
}
fn boundary(turn: u64) -> PendingEvent {
    PendingEvent::TurnFinished {
        turn,
        status: AgentTurnStatus::Completed,
        usage: SessionUsage::default(),
        cost: Cost::Unavailable {
            reason: "fixture".into(),
        },
    }
}
fn catch_up(index: &mut SubagentLifecycleIndex, journal: &SegmentedJournal) {
    while index.advance(&journal.read_view()).expect("advance") {}
}
fn artifact(id: &str) -> DiffArtifact {
    DiffArtifact {
        id: id.into(),
        base_commit: "a".repeat(40),
        touched_files: vec![],
        unified_diff: format!("patch-{id}"),
    }
}

#[test]
fn pending_children_are_paged_without_completed_lifecycle_bodies() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut events = vec![PendingEvent::TurnStarted { turn: 1 }];
    for index in 0..300 {
        let id = format!("child-{index}");
        events.push(spawn(&id));
        events.push(finish(&id, None));
    }
    for index in 0..35 {
        events.push(spawn(&format!("pending-{index}")));
    }
    events.push(boundary(1));
    append(&mut journal, events);
    let mut index = SubagentLifecycleIndex::open(&journal.read_view()).expect("index");
    catch_up(&mut index, &journal);
    let view = index.snapshot(&journal.read_view()).expect("view");
    let first = view.pending(None, 32).expect("first");
    assert_eq!(first.len(), 32);
    let next = view
        .pending(first.last().map(|child| child.spawned), 32)
        .expect("second");
    assert_eq!(next.len(), 3);
    assert!(
        view.pending(next.last().map(|child| child.spawned), 32)
            .expect("end")
            .is_empty()
    );
    assert!(view.pending(None, 33).is_err());
    assert!(
        view.binding(&SubagentId("child-299".into()))
            .expect("binding")
            .expect("completed")
            .terminal
            .is_some()
    );
}

#[test]
fn rewinds_restore_exact_child_and_artifact_sources_but_keep_publication_proof() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            spawn("child"),
            boundary(1),
            finish("child", Some(artifact("first"))),
            PendingEvent::TurnStarted { turn: 2 },
            spawn("child"),
            finish("child", Some(artifact("second"))),
            boundary(2),
        ],
    );
    let mut index = SubagentLifecycleIndex::open(&journal.read_view()).expect("index");
    catch_up(&mut index, &journal);
    let old = index.snapshot(&journal.read_view()).expect("old view");
    let child = SubagentId("child".into());
    let second = old.binding(&child).expect("binding").expect("child");
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut index, &journal);
    let now = index.snapshot(&journal.read_view()).expect("view");
    let restored = now.binding(&child).expect("binding").expect("child");
    assert!(restored.spawned < second.spawned);
    assert!(restored.terminal.is_some());
    assert_eq!(
        now.latest_artifact(&child).expect("latest"),
        Some("first".into())
    );
    assert!(now.artifact("first").expect("first").is_some());
    assert!(now.artifact("second").expect("rewound").is_none());
    assert!(old.artifact("second").expect("captured").is_some());
    assert_eq!(
        now.published(&child, &restored.session_id)
            .expect("physical proof"),
        Some(second.spawned)
    );
    assert!(now.pending(None, 32).expect("pending").is_empty());
    drop(now);
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 3 },
            spawn("child"),
            boundary(3),
        ],
    );
    catch_up(&mut index, &journal);
    let now = index.snapshot(&journal.read_view()).expect("new branch");
    assert_eq!(now.pending(None, 32).expect("pending").len(), 1);
    assert_eq!(
        now.latest_artifact(&child).expect("latest"),
        Some("first".into())
    );
    assert!(
        now.artifact("second")
            .expect("discarded terminal")
            .is_none()
    );
}

#[test]
fn artifact_identity_cannot_be_rebound_to_different_contents() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            spawn("first"),
            finish("first", Some(artifact("same"))),
        ],
    );
    let mut index = SubagentLifecycleIndex::open(&journal.read_view()).expect("index");
    catch_up(&mut index, &journal);
    let mut different = artifact("same");
    different.unified_diff.push('x');
    append(
        &mut journal,
        vec![spawn("second"), finish("second", Some(different))],
    );
    assert!(index.advance(&journal.read_view()).is_err());
}

#[test]
fn active_child_snapshot_bounds_unicode_and_rejects_excess_associations() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut events = vec![PendingEvent::TurnStarted { turn: 1 }];
    for child in 0..256 {
        events.push(PendingEvent::SubagentSpawned {
            subagent_id: SubagentId(format!("child-{child}")),
            child_session_id: SessionId(format!("session-{child}")),
            task: "犬".repeat(400),
        });
    }
    append(&mut journal, events);
    let mut index = SubagentLifecycleIndex::open(&journal.read_view()).expect("index");
    catch_up(&mut index, &journal);
    let view = index.snapshot(&journal.read_view()).expect("view");
    let active = view.active_children().expect("bounded active set");
    assert_eq!(active.len(), 256);
    assert!(
        active
            .iter()
            .all(|child| child.task_preview == "犬".repeat(341)
                && child.task_truncated
                && child.spawned_turn == 1)
    );
    append(&mut journal, vec![spawn("excess")]);
    catch_up(&mut index, &journal);
    assert_eq!(view.active_children().expect("immutable set").len(), 256);
    assert!(
        index
            .snapshot(&journal.read_view())
            .expect("view")
            .active_children()
            .is_err()
    );
    append(&mut journal, vec![finish("child-0", None)]);
    catch_up(&mut index, &journal);
    let active = index
        .snapshot(&journal.read_view())
        .expect("view")
        .active_children()
        .expect("terminal removed");
    assert_eq!(active.len(), 256);
    assert!(active.iter().all(|child| child.subagent_id.0 != "child-0"));
}
