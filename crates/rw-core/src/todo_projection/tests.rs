#![cfg(test)]
#![allow(clippy::expect_used)]
use super::TodoProjector;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    Cost, EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, TurnId, TurnStatus,
    Usage,
    todo::{TodoItem, TodoSnapshot, TodoStatus},
};

fn append(journal: &mut SegmentedJournal, build: impl FnOnce(EventMeta) -> EngineEvent) {
    let meta = EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("tasks".into()),
        sequence_id: SequenceId(journal.read_view().prefix_identity().next_sequence),
        emitted_at: "2026-09-05T12:00:00Z".into(),
        caused_by: None,
    };
    journal.append_batch([build(meta)]).expect("event");
}
fn snapshot(content: &str) -> TodoSnapshot {
    TodoSnapshot {
        count: 1,
        items: vec![TodoItem {
            id: "task".into(),
            content: content.into(),
            status: TodoStatus::Pending,
        }],
    }
}
fn task(journal: &mut SegmentedJournal, content: &str) {
    append(journal, |meta| EngineEvent::TodoStateCommitted {
        meta,
        snapshot: snapshot(content),
    });
}
fn terminal(journal: &mut SegmentedJournal, turn: u64) {
    append(journal, |meta| EngineEvent::TurnFinished {
        meta,
        turn_id: TurnId(turn.to_string()),
        status: TurnStatus::Completed,
        usage: Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: Cost::Unavailable {
            reason: "fixture".into(),
        },
    });
}
fn catch_up(projector: &mut TodoProjector, journal: &SegmentedJournal) {
    for _ in 0..100 {
        if !projector
            .advance(&journal.read_view())
            .expect("bounded advance")
        {
            return;
        }
    }
    panic!("task projection failed to converge");
}
#[test]
fn snapshots_follow_rewind_and_reused_turns_without_retaining_historical_bodies() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "tasks").expect("journal");
    task(&mut journal, "first");
    terminal(&mut journal, 1);
    task(&mut journal, "removed");
    terminal(&mut journal, 2);
    let mut projector = TodoProjector::open(&journal.read_view()).expect("index");
    catch_up(&mut projector, &journal);
    assert_eq!(
        projector
            .snapshot(&journal.read_view())
            .expect("current")
            .snapshot,
        snapshot("removed")
    );
    append(&mut journal, |meta| EngineEvent::ConversationRewound {
        meta,
        to_agent_turn: 1,
        operation_id: "rewind".into(),
        unrestorable_paths: vec![],
    });
    assert!(
        projector
            .advance(&journal.read_view())
            .expect("rewind starts")
    );
    assert!(
        projector.snapshot(&journal.read_view()).is_err(),
        "half-applied rewind is hidden"
    );
    drop(projector);
    let mut projector = TodoProjector::open(&journal.read_view()).expect("resume maintenance");
    catch_up(&mut projector, &journal);
    assert_eq!(
        projector
            .snapshot(&journal.read_view())
            .expect("rewound")
            .snapshot,
        snapshot("first")
    );
    task(&mut journal, "replacement");
    terminal(&mut journal, 2);
    catch_up(&mut projector, &journal);
    assert_eq!(
        projector
            .snapshot(&journal.read_view())
            .expect("replacement")
            .snapshot,
        snapshot("replacement")
    );
    append(&mut journal, |meta| EngineEvent::ConversationRewound {
        meta,
        to_agent_turn: 1,
        operation_id: "rewind-again".into(),
        unrestorable_paths: vec![],
    });
    catch_up(&mut projector, &journal);
    assert_eq!(
        projector
            .snapshot(&journal.read_view())
            .expect("rewound again")
            .snapshot,
        snapshot("first")
    );
}
#[test]
fn catch_up_is_bounded_and_failed_or_presentation_only_tool_results_do_not_replace_state() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "tasks").expect("journal");
    task(&mut journal, "authoritative");
    for index in 0..130 {
        append(&mut journal, |meta| EngineEvent::UserMessageAccepted {
            meta,
            turn_id: TurnId(index.to_string()),
            content: "input".into(),
            attachments: vec![],
        });
    }
    let mut projector = TodoProjector::open(&journal.read_view()).expect("index");
    assert!(projector.advance(&journal.read_view()).expect("one batch"));
    assert_eq!(projector.through().expect("cursor"), Some(SequenceId(63)));
    assert!(projector.snapshot(&journal.read_view()).is_err());
    catch_up(&mut projector, &journal);
    append(&mut journal, |meta| EngineEvent::ToolCallFinished {
        meta,
        turn_id: TurnId("1".into()),
        tool_call_id: rw_types::ToolCallId("todo".into()),
        invocation_id: rw_types::ToolInvocationId("tool".into()),
        output: rw_types::ToolOutput::Text {
            text: "transformed presentation".into(),
        },
        is_error: true,
        call_index: 0,
    });
    catch_up(&mut projector, &journal);
    assert_eq!(
        projector
            .snapshot(&journal.read_view())
            .expect("state")
            .snapshot,
        snapshot("authoritative")
    );
}
#[test]
fn independent_projection_owners_do_not_share_their_writer_lock_or_rows() {
    let root = tempfile::tempdir().expect("root");
    let journal = SegmentedJournal::open(root.path(), "tasks").expect("journal");
    let _tasks = TodoProjector::open(&journal.read_view()).expect("tasks");
    let modes = rw_ext::ModeRegistry::builtins().expect("modes");
    let _conversation =
        crate::recovery::CanonicalRecovery::open(&journal.read_view(), &modes, None)
            .expect("independent canonical owner");
}
