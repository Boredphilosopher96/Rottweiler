#![cfg(test)]
#![allow(clippy::expect_used)]
use crate::engine::recovery::{
    CanonicalRecovery,
    tests::{append, catch_up},
};
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    ClientId, Cost, SequenceId, SessionId,
    extension_contract::{
        ExtensionDeliveryCursor, ExtensionStateMutation, ExtensionStateTransaction,
    },
};
use serde_json::json;

fn state(
    plugin: &str,
    revision: Option<u64>,
    key: &str,
    value: &str,
    ack: Option<u64>,
) -> PendingEvent {
    PendingEvent::ExtensionStateCommitted {
        plugin_id: plugin.into(),
        transaction: ExtensionStateTransaction {
            expected_revision: revision.map(SequenceId),
            mutations: vec![ExtensionStateMutation::Set {
                key: key.into(),
                value: json!(value),
            }],
            acknowledged: ack.map(|sequence| ExtensionDeliveryCursor {
                session_id: SessionId("canonical".into()),
                sequence: SequenceId(sequence),
            }),
        },
    }
}
fn terminal(turn: u64) -> PendingEvent {
    PendingEvent::TurnFinished {
        turn,
        status: AgentTurnStatus::Completed,
        usage: SessionUsage::default(),
        cost: Cost::Unavailable {
            reason: "fixture".into(),
        },
    }
}
#[test]
fn indexed_extension_state_rewinds_values_without_rewinding_delivery() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::SessionCreated {
                driver_client_id: ClientId("driver".into()),
            },
            state("plugin", None, "key", "one", Some(0)),
            terminal(1),
            state("plugin", Some(1), "key", "two", Some(2)),
            terminal(2),
        ],
    );
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let view = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    assert_eq!(
        view.extension_state("plugin")
            .expect("state")
            .snapshot
            .entries[0]
            .value,
        json!("two")
    );
    drop(view);
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    drop(recovery);
    let recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("reopen");
    let view = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let state = view.extension_state("plugin").expect("state");
    assert_eq!(state.snapshot.revision, Some(SequenceId(1)));
    assert_eq!(state.snapshot.entries[0].value, json!("one"));
    assert_eq!(
        state.snapshot.acknowledged.expect("ack").sequence,
        SequenceId(2)
    );
    assert_eq!(state.session_bytes, "key".len() + 5);
    assert_eq!(state.namespaces, 1);
}

#[test]
fn indexed_extension_state_validates_cas_before_publishing_any_rows() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![state("plugin", None, "key", "one", None)],
    );
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    append(
        &mut journal,
        vec![state("plugin", None, "other", "two", None)],
    );
    assert!(recovery.advance(&journal.read_view(), &modes).is_err());
    assert_eq!(recovery.head().expect("head").next_sequence, 1);
    let state = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source")
        .extension_state("plugin")
        .expect("state");
    assert_eq!(state.snapshot.entries.len(), 1);
    assert_eq!(state.snapshot.entries[0].key, "key");
}

#[test]
fn indexed_extension_state_fork_keeps_state_and_initializes_child_delivery() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut inherited = state("plugin", None, "key", "one", Some(0));
    if let PendingEvent::ExtensionStateCommitted { transaction, .. } = &mut inherited {
        transaction.acknowledged.as_mut().expect("ack").session_id = SessionId("parent".into());
    }
    append(
        &mut journal,
        vec![
            PendingEvent::SessionCreated {
                driver_client_id: ClientId("driver".into()),
            },
            inherited,
        ],
    );
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, Some(SequenceId(1)))
        .expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let view = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let state = view.extension_state("plugin").expect("state").snapshot;
    assert_eq!(state.revision, Some(SequenceId(1)));
    assert_eq!(state.entries[0].value, json!("one"));
    assert!(state.acknowledged.is_none());
    let start = state.delivery_start.expect("delivery boundary");
    assert_eq!(start.session_id, SessionId("canonical".into()));
    assert_eq!(start.sequence, SequenceId(1));
    drop(view);
    drop(recovery);
    assert!(CanonicalRecovery::open(&journal.read_view(), &modes, None).is_err());
}

#[test]
fn indexed_extension_state_rejects_regressed_ack_after_rewind() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::SessionCreated {
                driver_client_id: ClientId("driver".into()),
            },
            state("plugin", None, "key", "one", Some(0)),
            terminal(1),
            state("plugin", Some(1), "key", "two", Some(2)),
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
        ],
    );
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    append(
        &mut journal,
        vec![state("plugin", Some(1), "key", "three", Some(0))],
    );
    assert!(recovery.advance(&journal.read_view(), &modes).is_err());
}
