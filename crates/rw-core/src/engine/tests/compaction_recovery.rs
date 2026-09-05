#![cfg(test)]

use crate::engine::AgentTurnStatus;
use crate::engine::SessionUsage;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::project_session_events;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::observe_event;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::text_turn;
use crate::engine::tests::fixtures::support::wire_event;
use crate::engine::unavailable_cost;
use rw_tools::ToolRegistry;
use rw_types::CompactionReason;
use rw_types::Role;
use rw_types::config::PermissionDecision;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[test]
fn compaction_projection_is_atomic_across_crash_boundaries() {
    let old = text_turn(Role::User, "old history");
    let summary = rw_context::summary_turn("summary");
    let unfinished = vec![
        wire_event(
            0,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: old.clone(),
            },
        ),
        wire_event(
            1,
            PendingEvent::CompactionStarted {
                reason: CompactionReason::Manual,
            },
        ),
        wire_event(
            2,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: summary.clone(),
            },
        ),
    ];
    let recovered = project_session_events(&unfinished).expect("unfinished compaction projects");
    assert_eq!(recovered.conversation, vec![old.clone()]);
    assert!(recovered.interrupted_compaction);

    let mut finished = unfinished.clone();
    finished.push(wire_event(
        3,
        PendingEvent::CompactionFinished {
            summary_turn: 2,
            reclaimed_tokens: 100,
            usage: None,
            cost: None,
        },
    ));
    let recovered = project_session_events(&finished).expect("finished compaction projects");
    assert_eq!(recovered.conversation, vec![summary]);
    assert!(!recovered.interrupted_compaction);

    let later = text_turn(Role::User, "later after recovery");
    let mut aborted = unfinished;
    aborted.push(wire_event(
        3,
        PendingEvent::Error {
            message: "interrupted compaction was aborted during recovery".to_owned(),
        },
    ));
    aborted.push(wire_event(
        4,
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 3,
            turn: later.clone(),
        },
    ));
    aborted.push(wire_event(
        5,
        PendingEvent::TurnFinished {
            turn: 3,
            status: AgentTurnStatus::Completed,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
        },
    ));
    let first_resume = project_session_events(&aborted).expect("first resume");
    let second_resume = project_session_events(&aborted).expect("second resume");
    assert_eq!(first_resume.conversation, vec![old.clone(), later.clone()]);
    assert_eq!(second_resume.conversation, vec![old, later]);
    assert!(!second_resume.interrupted_compaction);
}

#[test]
fn projector_rewind_before_multiple_compactions_restores_original_history() {
    let original_user = text_turn(Role::User, "original request");
    let original_assistant = text_turn(Role::Assistant, "original answer");
    let first_summary = rw_context::summary_turn("first summary");
    let later_user = text_turn(Role::User, "later request");
    let second_summary = rw_context::summary_turn("second summary");
    let kinds = vec![
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: original_user.clone(),
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: original_assistant.clone(),
        },
        PendingEvent::TurnFinished {
            turn: 1,
            status: AgentTurnStatus::Completed,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
        },
        PendingEvent::CompactionStarted {
            reason: CompactionReason::Manual,
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 2,
            turn: first_summary,
        },
        PendingEvent::CompactionFinished {
            summary_turn: 2,
            reclaimed_tokens: 100,
            usage: None,
            cost: None,
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 2,
            turn: later_user,
        },
        PendingEvent::TurnFinished {
            turn: 2,
            status: AgentTurnStatus::Completed,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
        },
        PendingEvent::CompactionStarted {
            reason: CompactionReason::Automatic,
        },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 3,
            turn: second_summary,
        },
        PendingEvent::CompactionFinished {
            summary_turn: 3,
            reclaimed_tokens: 100,
            usage: None,
            cost: None,
        },
        PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind-before-first-compaction".to_owned(),
            unrestorable_paths: Vec::new(),
        },
    ];
    let events = kinds
        .into_iter()
        .enumerate()
        .map(|(sequence, kind)| {
            wire_event(u64::try_from(sequence).expect("fixture sequence"), kind)
        })
        .collect::<Vec<_>>();

    let recovered = project_session_events(&events).expect("project rewind after compactions");
    assert_eq!(
        recovered.conversation,
        vec![original_user, original_assistant]
    );
    assert_eq!(recovered.turn_ends, BTreeMap::from([(1, 2)]));
    assert_eq!(recovered.completed_turns, 1);
}

#[tokio::test]
async fn actor_durably_aborts_interrupted_compaction_before_accepting_new_work() {
    for reason in [CompactionReason::Manual, CompactionReason::Automatic] {
        let root = TempDir::new().expect("tempdir");
        let old = text_turn(Role::User, format!("old history for {reason:?}"));
        let unfinished_summary = rw_context::summary_turn("must stay uncommitted");
        let durable_prefix = vec![
            wire_event(
                0,
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: 1,
                    turn: old.clone(),
                },
            ),
            wire_event(1, PendingEvent::CompactionStarted { reason }),
            wire_event(
                2,
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: 2,
                    turn: unfinished_summary,
                },
            ),
        ];
        let recovered = project_session_events(&durable_prefix).expect("recover prefix");
        assert!(recovered.interrupted_compaction);
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(
                durable_prefix
                    .into_iter()
                    .map(|event| observe_event(event).expect("durable prefix event"))
                    .collect(),
            ),
            batch_sizes: Mutex::new(Vec::new()),
            tail_floor: Mutex::new(None),
        });
        let model = Arc::new(ScriptedModel::new([stop_script("new answer", &[])]));
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        actor_config.recovered = recovered;

        let handle = crate::engine::tests::fixtures::history::spawn(actor_config)
            .await
            .expect("actor");
        timeout(Duration::from_secs(3), async {
            loop {
                let abort_persisted = sink.events.lock().expect("sink lock").iter().any(|event| {
                    matches!(
                        &event.kind,
                        PendingEvent::Error { message }
                            if message == "interrupted compaction was aborted during recovery"
                    )
                });
                if abort_persisted {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recovery abort must be persisted before commands are accepted");

        let mut subscription = handle.subscribe().expect("subscription");
        handle
            .send_message("later after recovery")
            .await
            .expect("later turn");
        collect_turn(&mut subscription).await;

        let durable_log = sink
            .events
            .lock()
            .expect("sink lock")
            .iter()
            .map(|event| event.wire.clone())
            .collect::<Vec<_>>();
        let first = project_session_events(&durable_log).expect("first reconstruction");
        let second = project_session_events(&durable_log).expect("second reconstruction");
        let mut assistant = text_turn(Role::Assistant, "new answer");
        assistant.meta.model = Some("fixture-model".to_owned());
        let expected = vec![
            old,
            text_turn(Role::User, "later after recovery"),
            assistant,
        ];
        assert_eq!(first.conversation, expected);
        assert_eq!(second.conversation, expected);
        assert!(!first.interrupted_compaction);
        assert!(!second.interrupted_compaction);
    }
}
