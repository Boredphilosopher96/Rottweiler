#![cfg(test)]
#![allow(clippy::expect_used)]
use super::read_todos;
use crate::journal_service::JournalService;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, TurnId,
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
                turn_id: TurnId(sequence.to_string()),
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
