#![cfg(test)]
#![allow(clippy::expect_used)]
use super::read_todos;
use crate::journal_service::JournalService;
use rw_core::HostError;
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
    let first = read_todos(Arc::clone(&service), SessionId("tasks".into()), |_| Ok(()))
        .await
        .expect("first read");
    assert!(matches!(
        first,
        TodoReadResult::CatchingUp {
            through: Some(SequenceId(255)),
            target: Some(SequenceId(299))
        }
    ));
    let ready = read_todos(Arc::clone(&service), SessionId("tasks".into()), |_| Ok(()))
        .await
        .expect("ready");
    assert!(
        matches!(ready, TodoReadResult::Ready { todos } if todos.through == Some(SequenceId(299)) && todos.snapshot == TodoSnapshot::default())
    );
    drop(registration);
    drop(journal);
    assert!(matches!(
        read_todos(service, SessionId("tasks".into()), |_| Ok(()))
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
        let error = read_todos(Arc::clone(&service), SessionId("absent".into()), |_| {
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
    let TodoReadResult::Ready { todos } =
        read_todos(service, SessionId("tasks".into()), |_| Ok(()))
            .await
            .expect("query")
    else {
        panic!("small source ready")
    };
    assert_eq!(todos.through, Some(SequenceId(3)));
    assert_eq!(todos.snapshot, state("retained"));
}

#[tokio::test]
async fn ancestry_and_task_projection_share_one_four_transaction_allowance() {
    use crate::transcript_service::TranscriptReader;
    use rw_types::session_read::{SessionReadAncestor, SessionReadScope};
    let root = tempfile::tempdir().expect("root");
    let service = JournalService::new(root.path()).expect("journals");
    let metadata = |session: &str, sequence| EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId(session.into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-05T00:00:00Z".into(),
        caused_by: None,
    };
    let mut parent =
        rw_store::session::SessionEventLog::open(root.path(), "parent").expect("parent");
    for sequence in 0..127 {
        parent
            .append(EngineEvent::UserMessageAccepted {
                meta: metadata("parent", sequence),
                agent_turn: sequence,
                content: "body".into(),
                attachments: vec![],
            })
            .expect("parent event");
    }
    parent
        .append(EngineEvent::SubagentSpawned {
            meta: metadata("parent", 127),
            subagent_id: rw_types::SubagentId("agent".into()),
            child_session_id: SessionId("child".into()),
            task: "Read".into(),
        })
        .expect("spawn");
    let mut child = rw_store::session::SessionEventLog::open(root.path(), "child").expect("child");
    for sequence in 0..300 {
        child
            .append(EngineEvent::TodoStateCommitted {
                meta: metadata("child", sequence),
                snapshot: TodoSnapshot::default(),
            })
            .expect("task");
    }
    drop(child);
    drop(parent);
    let reader = TranscriptReader::new(service);
    let scope = SessionReadScope::Descendant {
        root_session_id: SessionId("parent".into()),
        ancestry: vec![SessionReadAncestor {
            subagent_id: rw_types::SubagentId("agent".into()),
            session_id: SessionId("child".into()),
            source_sequence: SequenceId(127),
        }],
    };
    let first = reader
        .todos(SessionId("child".into()), scope.clone())
        .await
        .expect("bounded query");
    assert!(matches!(
        first,
        TodoReadResult::CatchingUp {
            through: Some(SequenceId(127)),
            target: Some(SequenceId(299))
        }
    ));
    assert!(matches!(
        reader
            .todos(SessionId("child".into()), scope)
            .await
            .expect("resume"),
        TodoReadResult::Ready { .. }
    ));
}

#[tokio::test]
async fn concurrent_task_query_waits_for_publication_without_blocking_other_sessions() {
    use std::future::{Future as _, poll_fn};
    use std::task::Poll;

    let (_root, service) = task_query_service();
    let (ready_signal, release, publisher) = withheld_task_publisher(Arc::clone(&service)).await;
    ready_signal.await.expect("index publication held");
    let mut pending: Vec<_> = (0..crate::journal_service::MAX_PROJECTION_WAITERS)
        .map(|_| Box::pin(task_read(Arc::clone(&service))))
        .collect();
    poll_fn(|context| {
        for query in &mut pending {
            assert!(
                query.as_mut().poll(context).is_pending(),
                "same-session query waits"
            );
        }
        Poll::Ready(())
    })
    .await;
    assert!(
        task_read(Arc::clone(&service)).await.is_err(),
        "session wait queue is bounded"
    );
    drop(pending.pop());
    let mut replacement = Box::pin(task_read(Arc::clone(&service)));
    poll_fn(|context| {
        assert!(
            replacement.as_mut().poll(context).is_pending(),
            "cancelled caller returns queue credit"
        );
        Poll::Ready(())
    })
    .await;
    pending.push(replacement);
    let independent = read_todos(
        Arc::clone(&service),
        SessionId("independent".into()),
        |_| Ok(()),
    )
    .await
    .expect("independent session remains available");
    assert!(matches!(independent, TodoReadResult::Ready { .. }));
    // Queued callers consume neither journal credits nor Blocking workers.
    poll_fn(|context| {
        for query in &mut pending {
            assert!(
                query.as_mut().poll(context).is_pending(),
                "publisher still owns the index"
            );
        }
        Poll::Ready(())
    })
    .await;
    release.send(()).expect("release publication");
    publisher.join().expect("publisher settled");
    for query in pending {
        assert!(
            matches!(query.await.expect("ordered read does not report Busy"),
            TodoReadResult::Ready { todos } if todos.through == Some(SequenceId(0)))
        );
    }
}

async fn withheld_task_publisher(
    service: Arc<JournalService>,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let (notify_ready, ready) = tokio::sync::oneshot::channel();
    let (release, hold) = std::sync::mpsc::channel();
    let order = service
        .task_projection_order("same")
        .expect("projection order")
        .acquire()
        .await
        .expect("publisher order");
    let publisher = std::thread::spawn(move || {
        let _order = order;
        let lease = service.capture("same").expect("source");
        let mut projector =
            rw_core::todo_projection::TodoProjector::open(&lease.view).expect("writer");
        projector.advance(&lease.view).expect("publication");
        notify_ready.send(()).expect("reader waiting");
        hold.recv().expect("release publication");
        drop(projector);
    });
    (ready, release, publisher)
}

fn task_query_service() -> (tempfile::TempDir, Arc<JournalService>) {
    let root = tempfile::tempdir().expect("root");
    let service = JournalService::new(root.path()).expect("service");
    for session in ["same", "independent"] {
        let mut journal = SegmentedJournal::open(root.path(), session).expect("journal");
        journal
            .append_batch([EngineEvent::TodoStateCommitted {
                meta: EventMeta {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: SessionId(session.into()),
                    sequence_id: SequenceId(0),
                    emitted_at: "2026-09-05T12:00:00Z".into(),
                    caused_by: None,
                },
                snapshot: TodoSnapshot::default(),
            }])
            .expect("task state");
    }
    (root, service)
}

async fn task_read(service: Arc<JournalService>) -> Result<TodoReadResult, HostError> {
    read_todos(service, SessionId("same".into()), |_| Ok(())).await
}

#[tokio::test]
async fn cancelled_running_task_query_keeps_publication_order_until_worker_settles() {
    use std::future::{Future as _, poll_fn};
    use std::task::Poll;

    let (_root, service) = task_query_service();
    let (started, running) = tokio::sync::oneshot::channel();
    let (release, hold) = std::sync::mpsc::channel();
    let query = tokio::spawn(read_todos(
        Arc::clone(&service),
        SessionId("same".into()),
        move |_| {
            started.send(()).expect("running observer");
            hold.recv().expect("physical work release");
            Ok(())
        },
    ));
    running
        .await
        .expect("worker owns order and journal admission");
    query.abort();
    assert!(query.await.expect_err("caller cancelled").is_cancelled());
    let mut next = Box::pin(
        service
            .task_projection_order("same")
            .expect("same order after cancellation")
            .acquire(),
    );
    poll_fn(|context| {
        assert!(
            next.as_mut().poll(context).is_pending(),
            "physical owner still excludes publication"
        );
        Poll::Ready(())
    })
    .await;
    release.send(()).expect("release physical query");
    drop(
        next.await
            .expect("order returns after actual worker completion"),
    );
    assert!(matches!(
        task_read(service).await.expect("subsequent query"),
        TodoReadResult::Ready { .. }
    ));
}
