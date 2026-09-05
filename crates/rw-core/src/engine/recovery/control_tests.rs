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
    assert_eq!(bootstrap.controls, controls);
    assert!(bootstrap.interrupted.is_none());

    assert_eq!(
        after
            .control_payloads(controls.source_bytes)
            .expect("same bounded controls"),
        controls
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
