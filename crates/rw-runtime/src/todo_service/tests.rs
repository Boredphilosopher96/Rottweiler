#![cfg(test)]
#![allow(clippy::expect_used)]
use super::read_todos;
use crate::journal_service::JournalService;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId,
    todo::{TodoReadResult, TodoSnapshot},
};
use std::sync::Arc;

#[tokio::test]
async fn active_and_offline_queries_use_acknowledged_prefix_and_bounded_catch_up() {
    let root = tempfile::tempdir().expect("root");
    let service = JournalService::new(root.path()).expect("service");
    let mut journal = SegmentedJournal::open(root.path(), "tasks").expect("journal");
    let events = (0..300)
        .map(|sequence| {
            let meta = EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: SessionId("tasks".into()),
                sequence_id: SequenceId(sequence),
                emitted_at: "2026-09-05T12:00:00Z".into(),
                caused_by: None,
            };
            EngineEvent::UserMessageAccepted {
                meta,
                agent_turn: sequence,
                content: "input".into(),
                attachments: vec![],
            }
        })
        .collect::<Vec<_>>();
    journal.append_batch(events).expect("source");
    let registration = service
        .register("tasks", journal.read_view())
        .expect("active publisher");
    let first = read_todos(Arc::clone(&service), SessionId("tasks".into()), || Ok(()))
        .await
        .expect("first read");
    assert!(matches!(
        first,
        TodoReadResult::CatchingUp {
            through: Some(SequenceId(255)),
            target: Some(SequenceId(299))
        }
    ));
    let ready = read_todos(Arc::clone(&service), SessionId("tasks".into()), || Ok(()))
        .await
        .expect("ready");
    assert!(
        matches!(ready, TodoReadResult::Ready { todos } if todos.through == Some(SequenceId(299)) && todos.snapshot == TodoSnapshot::default())
    );
    drop(registration);
    drop(journal);
    assert!(matches!(
        read_todos(service, SessionId("tasks".into()), || Ok(()))
            .await
            .expect("offline"),
        TodoReadResult::Ready { .. }
    ));
}

#[tokio::test]
async fn rejected_authorization_releases_admission_without_reading_session_data() {
    let root = tempfile::tempdir().expect("root");
    let service = JournalService::new(root.path()).expect("service");
    for _ in 0..16 {
        let error = read_todos(Arc::clone(&service), SessionId("absent".into()), || {
            Err(rw_core::HostError::Query("denied".into()))
        })
        .await
        .expect_err("denied");
        assert!(error.to_string().contains("denied"));
    }
}

#[tokio::test]
async fn task_rewind_query_reads_authoritative_commits_without_replaying_tool_arguments() {
    use rw_types::{
        Cost, TurnId, TurnStatus, Usage,
        todo::{TodoItem, TodoStatus},
    };
    let root = tempfile::tempdir().expect("root");
    let service = JournalService::new(root.path()).expect("service");
    let mut journal = SegmentedJournal::open(root.path(), "tasks").expect("source");
    let meta = |sequence| EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("tasks".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-05T12:00:00Z".into(),
        caused_by: None,
    };
    let state = |content: &str| TodoSnapshot {
        items: vec![TodoItem {
            id: "task".into(),
            content: content.into(),
            status: TodoStatus::Pending,
        }],
    };
    journal
        .append_batch([
            EngineEvent::TodoStateCommitted {
                meta: meta(0),
                snapshot: state("retained"),
            },
            EngineEvent::TurnFinished {
                meta: meta(1),
                turn_id: TurnId("1".into()),
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
            },
            EngineEvent::TodoStateCommitted {
                meta: meta(2),
                snapshot: state("discarded"),
            },
            EngineEvent::ConversationRewound {
                meta: meta(3),
                to_agent_turn: 1,
                operation_id: "rewind-task".into(),
                unrestorable_paths: vec![],
            },
        ])
        .expect("source commits");
    drop(journal);
    let TodoReadResult::Ready { todos } = read_todos(service, SessionId("tasks".into()), || Ok(()))
        .await
        .expect("query")
    else {
        panic!("small source ready")
    };
    assert_eq!(todos.through, Some(SequenceId(3)));
    assert_eq!(todos.snapshot, state("retained"));
}
