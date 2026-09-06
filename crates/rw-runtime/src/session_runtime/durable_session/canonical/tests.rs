#![cfg(test)]
#![allow(clippy::expect_used)]
use super::super::DurableEventSink;
use crate::journal_service::JournalService;
use rw_core::{SessionEventSink, commit_session_events};
use rw_ext::ModeRegistry;
use rw_store::session::SessionEventLog;
use rw_types::{
    EngineEvent, EventMeta, SequenceId, SessionId,
    extension_contract::{ExtensionStateMutation, ExtensionStateTransaction},
};
use std::{sync::Arc, time::Duration};

fn sink(root: &std::path::Path) -> Arc<DurableEventSink> {
    let sink = DurableEventSink::new(
        SessionEventLog::open(root, "state").expect("log"),
        root.to_owned(),
        "state".into(),
        JournalService::new(root).expect("journals"),
    )
    .expect("sink");
    sink.configure_canonical(Arc::new(ModeRegistry::builtins().expect("modes")), None)
        .expect("canonical binding");
    sink
}
fn state_event(sequence: u64) -> EngineEvent {
    EngineEvent::ExtensionStateCommitted {
        meta: EventMeta {
            protocol_version: rw_core::SESSION_EVENT_VERSION,
            session_id: SessionId("state".into()),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-09-05T00:00:00Z".into(),
            caused_by: None,
        },
        plugin_id: "plugin".into(),
        transaction: ExtensionStateTransaction {
            expected_revision: sequence.checked_sub(1).map(SequenceId),
            mutations: vec![ExtensionStateMutation::Set {
                key: "key".into(),
                value: serde_json::json!(sequence),
            }],
            acknowledged: None,
        },
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_runtime_query_tracks_acknowledged_commits_and_reopens() {
    let root = tempfile::tempdir().expect("root");
    let current = sink(root.path());
    commit_session_events(Arc::clone(&current), vec![state_event(0)])
        .await
        .expect("commit");
    let first = current.extension_state("plugin").await.expect("state");
    assert_eq!(first.snapshot.revision, Some(SequenceId(0)));
    commit_session_events(Arc::clone(&current), vec![state_event(1)])
        .await
        .expect("commit");
    let second = current.extension_state("plugin").await.expect("state");
    assert_eq!(second.snapshot.entries[0].value, serde_json::json!(1));
    assert_eq!(first.snapshot.entries[0].value, serde_json::json!(0));
    current.settle_effects().await.expect("settled");
    drop(current);
    let reopened = sink(root.path());
    assert_eq!(
        reopened
            .extension_state("plugin")
            .await
            .expect("reopened")
            .snapshot
            .revision,
        Some(SequenceId(1))
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_canonical_query_retains_worker_until_actual_completion() {
    let root = tempfile::tempdir().expect("root");
    let current = sink(root.path());
    commit_session_events(Arc::clone(&current), vec![state_event(0)])
        .await
        .expect("commit");
    let owner = Arc::clone(current.canonical.get().expect("owner"));
    let (ready, locked) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let lock_thread = std::thread::spawn({
        let owner = Arc::clone(&owner);
        move || {
            let _lock = owner.recovery.lock().expect("lock");
            ready.send(()).expect("ready");
            wait.recv_timeout(Duration::from_secs(5)).expect("release");
        }
    });
    locked.await.expect("locked");
    let query = tokio::spawn({
        let current = Arc::clone(&current);
        async move { current.extension_state("plugin").await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if current.reads.active() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker admitted");
    query.abort();
    assert!(query.await.expect_err("cancelled").is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), current.settle_effects())
            .await
            .is_err()
    );
    release.send(()).expect("release");
    lock_thread.join().expect("locker");
    current
        .settle_effects()
        .await
        .expect("actual worker settled");
    assert_eq!(
        current
            .extension_state("plugin")
            .await
            .expect("state")
            .snapshot
            .revision,
        Some(SequenceId(0))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_bootstrap_shares_committed_prefix_without_mutating_prior_results() {
    let root = tempfile::tempdir().expect("root");
    let current = sink(root.path());
    commit_session_events(Arc::clone(&current), vec![state_event(0)])
        .await
        .expect("commit");
    let bootstrap = current
        .read_canonical(rw_core::recovery::CanonicalHistory::bootstrap)
        .await
        .expect("bootstrap");
    assert_eq!(bootstrap.head.next_sequence, 1);
    assert!(bootstrap.interrupted.is_none());
    commit_session_events(Arc::clone(&current), vec![state_event(1)])
        .await
        .expect("next commit");
    assert_eq!(
        current
            .read_canonical(|history| Ok(history.head().next_sequence))
            .await
            .expect("head"),
        2
    );
    assert_eq!(
        bootstrap.head.next_sequence, 1,
        "returned bootstrap remains exact after the next commit"
    );
}

#[tokio::test]
async fn source_rewind_query_validates_exact_published_prefix() {
    let root = tempfile::tempdir().expect("root");
    let current = sink(root.path());
    let events = completed_events(1..=2, 0);
    commit_session_events(Arc::clone(&current), events)
        .await
        .expect("commit");
    assert_eq!(
        current
            .source_rewind_target(
                SequenceId(5),
                SequenceId(4),
                2,
                rw_types::RewindSourcePosition::Before
            )
            .await
            .expect("before"),
        1
    );
    assert_eq!(
        current
            .source_rewind_target(
                SequenceId(5),
                SequenceId(4),
                2,
                rw_types::RewindSourcePosition::Through
            )
            .await
            .expect("through"),
        2
    );
    assert!(
        current
            .source_rewind_target(
                SequenceId(4),
                SequenceId(4),
                2,
                rw_types::RewindSourcePosition::Through
            )
            .await
            .is_err()
    );
}

fn completed_events(turns: std::ops::RangeInclusive<u64>, first_sequence: u64) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    let meta = |sequence| EventMeta {
        protocol_version: rw_core::SESSION_EVENT_VERSION,
        session_id: SessionId("state".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-05T00:00:00Z".into(),
        caused_by: None,
    };
    for turn in turns {
        let start = first_sequence + events.len() as u64;
        events.extend([
            EngineEvent::TurnStarted {
                meta: meta(start),
                turn_id: rw_types::TurnId(turn.to_string()),
            },
            EngineEvent::UserMessageAccepted {
                meta: meta(start + 1),
                agent_turn: turn,
                content: "user".into(),
                attachments: vec![],
            },
            EngineEvent::ConversationInputCommitted {
                meta: meta(start + 2),
                agent_turn: turn,
                accepted_source: SequenceId(start + 1),
                selection: rw_types::conversation_input::InputSelection::Accepted {},
            },
            EngineEvent::TurnFinished {
                meta: meta(start + 3),
                turn_id: rw_types::TurnId(turn.to_string()),
                status: rw_types::TurnStatus::Completed,
                usage: rw_core::SessionUsage::default().into(),
                cost: rw_types::Cost::Unavailable {
                    reason: "fixture".into(),
                },
            },
        ]);
    }
    events
}

#[tokio::test]
async fn completed_turn_query_uses_effective_source_after_rewind_and_reopen() {
    let root = tempfile::tempdir().expect("root");
    let current = sink(root.path());
    commit_session_events(Arc::clone(&current), completed_events(1..=2, 0))
        .await
        .expect("completed turns");
    assert_eq!(
        current.completed_turn(2).await.expect("second"),
        Some(rw_core::CompletedTurn {
            sequence_id: SequenceId(7),
            completed_turns: 2,
        })
    );
    commit_session_events(
        Arc::clone(&current),
        vec![EngineEvent::ConversationRewound {
            meta: EventMeta {
                protocol_version: rw_core::SESSION_EVENT_VERSION,
                session_id: SessionId("state".into()),
                sequence_id: SequenceId(8),
                emitted_at: "2026-09-05T00:00:00Z".into(),
                caused_by: None,
            },
            to_agent_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    )
    .await
    .expect("rewind");
    assert_eq!(current.completed_turn(2).await.expect("removed"), None);
    assert_eq!(
        current.completed_turn(1).await.expect("first"),
        Some(rw_core::CompletedTurn {
            sequence_id: SequenceId(3),
            completed_turns: 1,
        })
    );
    commit_session_events(Arc::clone(&current), completed_events(2..=2, 9))
        .await
        .expect("replacement");
    current.settle_effects().await.expect("settle");
    drop(current);
    let reopened = sink(root.path());
    assert_eq!(
        reopened
            .completed_turn(2)
            .await
            .expect("replacement source"),
        Some(rw_core::CompletedTurn {
            sequence_id: SequenceId(12),
            completed_turns: 2,
        })
    );
}

#[tokio::test]
async fn captured_semantic_history_preserves_pages_and_charges_its_read_lifetime() {
    use rw_core::recovery::{HistoryMaterializationLimits, SessionHistory};
    let root = tempfile::tempdir().expect("root");
    let current = sink(root.path());
    let conversation = |sequence, text: &str| {
        crate::session_runtime::test_history::input_events(
            EventMeta {
                protocol_version: rw_core::SESSION_EVENT_VERSION,
                session_id: SessionId("state".into()),
                sequence_id: SequenceId(sequence),
                emitted_at: "2026-09-05T00:00:00.000Z".into(),
                caused_by: None,
            },
            1,
            text.into(),
        )
    };
    commit_session_events(Arc::clone(&current), conversation(0, "first").to_vec())
        .await
        .expect("commit");
    let captured = current.capture_history().await.expect("history");
    commit_session_events(Arc::clone(&current), conversation(2, "second").to_vec())
        .await
        .expect("append after capture");
    assert_eq!(captured.through(), Some(SequenceId(1)));
    assert_eq!(captured.conversation().turns, 1);
    let page = captured
        .conversation_page(0..1, HistoryMaterializationLimits::default())
        .await
        .expect("captured page");
    assert_eq!(page.sources[0].sequence, SequenceId(1));
    assert!(!page.has_more);
    assert!(
        captured
            .conversation_page(0..2, HistoryMaterializationLimits::default())
            .await
            .is_err()
    );
    // Completed results retain byte ownership without reserving an active read slot.
    let mut pages = vec![page];
    let mut bootstraps = Vec::new();
    for _ in 1..32 {
        pages.push(
            captured
                .conversation_page(0..1, HistoryMaterializationLimits::default())
                .await
                .expect("small delivered page"),
        );
        bootstraps.push(
            captured
                .bootstrap()
                .await
                .expect("small retained bootstrap"),
        );
    }
    drop(captured);
    let captured = current.capture_history().await.expect("released pages");
    let mut views = vec![captured];
    for _ in 1..8 {
        let view = current.capture_history().await.expect("admitted view");
        assert_eq!(view.conversation().turns, 2);
        views.push(view);
    }
    assert!(current.capture_history().await.is_err());
    views.pop();
    current
        .capture_history()
        .await
        .expect("released view readmission");
    drop(views);
    assert_eq!(pages[0].sources[0].sequence, SequenceId(1));
    assert_eq!(bootstraps[0].head.next_sequence, 2);
    drop((pages, bootstraps));
    current.settle_effects().await.expect("read jobs settled");
}
