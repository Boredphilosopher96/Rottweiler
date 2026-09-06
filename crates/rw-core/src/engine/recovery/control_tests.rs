#![allow(clippy::expect_used)]
use super::{
    tests::{append, catch_up, text},
    *,
};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{ModelAlias, Role, config::ThinkingLevel};

#[test]
fn live_controls_do_not_materialize_historical_conversation_and_obey_byte_admission() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    append(
        &mut journal,
        vec![
            PendingEvent::SessionTitleUpdated {
                title: "Exact title".into(),
                usage: None,
                cost: None,
            },
            PendingEvent::ModelChanged {
                model: ModelAlias("coding".into()),
                provider: Some("local".into()),
                thinking: ThinkingLevel::default(),
            },
            PendingEvent::MessageQueued {
                position: 8,
                content: "queued message".into(),
                attachments: vec![],
            },
            PendingEvent::UserMessageAccepted {
                turn: 3,
                content: "accepted but not committed".into(),
                attachments: vec![],
            },
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let before = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let controls = before
        .control_payloads(MAX_CONTROL_SOURCE_BYTES)
        .expect("controls");
    assert_eq!(controls.title.as_deref(), Some("Exact title"));
    assert_eq!(
        controls.model.as_ref().expect("model").provider.as_deref(),
        Some("local")
    );
    assert_eq!(controls.queued_messages[0].0, 8);
    assert_eq!(
        controls.accepted_messages[0].1.content,
        "accepted but not committed"
    );
    assert_eq!(
        before
            .control_payloads(controls.source_bytes)
            .expect("exact bound"),
        controls
    );
    assert!(matches!(
        before.control_payloads(controls.source_bytes - 1),
        Err(RecoveryError::Limit(_))
    ));
    assert!(matches!(
        before.control_payloads(MAX_CONTROL_SOURCE_BYTES + 1),
        Err(RecoveryError::Limit(_))
    ));
    append(
        &mut journal,
        (0..256)
            .map(|_| PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: text(Role::Assistant, &"historical payload".repeat(1024)),
            })
            .collect(),
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let after = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    assert_eq!(after.head().conversation.turns, 256);
    let bootstrap = after.bootstrap().expect("bounded actor bootstrap");
    assert_eq!(bootstrap.head.conversation.turns, 256);
    let mut expected = controls.clone();
    expected.conversation.turns = 256;
    expected.conversation.has_assistant_text = true;
    assert_eq!(bootstrap.controls, expected);
    assert!(bootstrap.interrupted.is_none());

    assert_eq!(
        after
            .control_payloads(controls.source_bytes)
            .expect("same bounded controls"),
        expected
    );
    assert_eq!(
        before.head().conversation.turns,
        0,
        "earlier snapshot remains exact"
    );
}

#[test]
fn overwritten_or_removed_control_sources_are_not_recovered() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::SessionTitleUpdated {
                title: "obsolete".into(),
                usage: None,
                cost: None,
            },
            PendingEvent::MessageQueued {
                position: 1,
                content: "removed".into(),
                attachments: vec![],
            },
            PendingEvent::QueuedMessageRemoved { position: 1 },
            PendingEvent::SessionTitleUpdated {
                title: "current".into(),
                usage: None,
                cost: None,
            },
        ],
    );
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let controls = history
        .control_payloads(MAX_CONTROL_SOURCE_BYTES)
        .expect("controls");
    assert_eq!(controls.title.as_deref(), Some("current"));
    assert!(controls.queued_messages.is_empty());
}

#[test]
fn latest_budget_is_physical_state_after_conversation_rewind() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    let pending = vec![
        PendingEvent::TurnStarted { turn: 1 },
        PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: text(Role::User, "first"),
        },
        super::tests::terminal(1),
        PendingEvent::BudgetStatus {
            turn: 2,
            level: rw_types::BudgetLevel::HardCap,
            scope: rw_types::BudgetScope::Session,
            unit: rw_types::BudgetUnit::Tokens,
            current: u64::MAX,
            limit: u64::MAX - 1,
        },
        PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind-budget".into(),
            unrestorable_paths: vec![],
        },
    ];
    let events = pending
        .iter()
        .enumerate()
        .map(|(index, pending)| super::tests::event(index as u64, pending.clone()))
        .collect::<Vec<_>>();
    append(&mut journal, pending);
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let snapshot = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let budget = snapshot
        .bootstrap()
        .expect("bootstrap")
        .controls
        .latest_budget
        .expect("budget");
    assert_eq!(budget.turn_id, crate::engine::wire_turn_id(2));
    assert_eq!(budget.current, u64::MAX);
    assert_eq!(budget.limit, u64::MAX - 1);
    let projected = crate::engine::project_session_events(&events).expect("audit projection");
    assert_eq!(projected.latest_budget, Some(budget));
}

#[test]
fn completed_boundary_bootstrap_matches_rewind_without_materializing_history() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    append(
        &mut journal,
        vec![
            PendingEvent::ModeChanged {
                mode: rw_types::ModeId("plan".into()),
                definition_fingerprint: modes.get("plan").expect("mode").semantic_fingerprint(),
            },
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: text(Role::User, "first"),
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: text(Role::Assistant, &"history".repeat(100_000)),
            },
            super::tests::terminal(1),
            PendingEvent::ModeChanged {
                mode: rw_types::ModeId("execute".into()),
                definition_fingerprint: modes.get("execute").expect("mode").semantic_fingerprint(),
            },
            PendingEvent::SessionTitleUpdated {
                title: "Present title".into(),
                usage: None,
                cost: None,
            },
            PendingEvent::TurnStarted { turn: 2 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: text(Role::User, "second"),
            },
            super::tests::terminal(2),
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let expected = history
        .recovery_at_completed_turn(1)
        .expect("boundary bootstrap");
    assert_eq!(expected.head.next_sequence, history.head().next_sequence);
    assert_eq!(expected.head.accounting, history.head().accounting);
    assert_eq!(
        expected.head.control.next_turn,
        history.head().control.next_turn
    );
    assert_eq!(expected.head.conversation.turns, 2);
    assert_eq!(expected.head.control.mode, rw_types::SessionMode::Plan);
    assert_eq!(expected.controls.title.as_deref(), Some("Present title"));
    assert!(
        expected.controls.source_bytes < 4096,
        "historical bodies stay in the journal"
    );
    assert!(expected.interrupted.is_none());
    assert!(history.recovery_at_completed_turn(99).is_err());
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind-source".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let actual = recovery
        .snapshot()
        .expect("rewound snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let actual_bootstrap = actual.bootstrap().expect("rewound bootstrap");
    assert_eq!(
        actual_bootstrap.head.conversation,
        expected.head.conversation
    );
    assert_eq!(actual_bootstrap.head.control, expected.head.control);
    assert_eq!(actual_bootstrap.head.budget, expected.head.budget);
    assert_eq!(actual_bootstrap.controls, expected.controls);
    assert!(
        actual.recovery_at_completed_turn(2).is_err(),
        "removed boundary cannot be reused"
    );
}

#[test]
fn status_bootstrap_recovers_latest_sources_and_durable_clears() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    let update = |id: &str, status: &str| PendingEvent::PluginStatusChanged {
        plugin_id: id.into(),
        status: status.into(),
    };
    append(
        &mut journal,
        vec![
            update("worker", "old"),
            update("other", "ready"),
            update("worker", "working"),
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    drop(recovery);
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("reopen");
    let statuses = |recovery: &CanonicalRecovery| {
        recovery
            .snapshot()
            .expect("snapshot")
            .bind_source(&journal.read_view())
            .expect("source")
            .bootstrap()
            .expect("bootstrap")
    };
    let recovered = statuses(&recovery);
    assert_eq!(recovered.controls.plugin_statuses.len(), 2);
    let worker = recovered
        .controls
        .plugin_statuses
        .iter()
        .find(|entry| entry.plugin_id == "worker")
        .expect("worker");
    assert_eq!(worker.status, "working");
    assert_eq!(worker.source, SequenceId(2));
    assert!(recovered.retained_bytes().expect("charged") > worker.status.len());
    drop(recovered);
    append(&mut journal, vec![update("worker", "")]);
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let cleared = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source")
        .bootstrap()
        .expect("cleared");
    assert_eq!(cleared.controls.plugin_statuses.len(), 1);
    assert_eq!(cleared.controls.plugin_statuses[0].plugin_id, "other");
}
