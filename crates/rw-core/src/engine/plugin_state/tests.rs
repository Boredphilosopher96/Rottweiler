#![allow(clippy::expect_used)]

use super::*;
use rw_types::extension_contract::{
    ExtensionDeliveryCursor, ExtensionStateEntry, ExtensionStateSnapshot,
};
use serde_json::json;

fn view() -> ExtensionStateView {
    ExtensionStateView {
        snapshot: ExtensionStateSnapshot {
            revision: Some(SequenceId(10)),
            entries: vec![ExtensionStateEntry {
                key: "task".to_owned(),
                value: json!(true),
            }],
            acknowledged: None,
            delivery_start: None,
        },
        session_bytes: 8,
        namespaces: 1,
    }
}

fn transaction() -> ExtensionStateTransaction {
    ExtensionStateTransaction {
        expected_revision: Some(SequenceId(10)),
        mutations: vec![ExtensionStateMutation::Set {
            key: "task".to_owned(),
            value: json!(false),
        }],
        acknowledged: None,
    }
}

#[test]
fn replacement_charges_only_the_new_value_and_checks_cas() {
    let mut view = view();
    view.session_bytes = MAX_SESSION_EXTENSION_STATE_BYTES - 1;
    let mut transaction = transaction();
    assert_eq!(
        validate_update(
            &view,
            &transaction,
            &SessionId("session".to_owned()),
            Some(SequenceId(11))
        )
        .expect("replacement fits exact aggregate bound"),
        9
    );
    transaction.expected_revision = None;
    assert!(
        validate_update(
            &view,
            &transaction,
            &SessionId("session".to_owned()),
            Some(SequenceId(11))
        )
        .is_err()
    );
    transaction.expected_revision = view.snapshot.revision;
    view.session_bytes += 1;
    assert!(
        validate_update(
            &view,
            &transaction,
            &SessionId("session".to_owned()),
            Some(SequenceId(11))
        )
        .is_err()
    );
}

#[test]
fn acknowledgement_cannot_cross_sessions_or_move_backwards_or_ahead() {
    let mut view = view();
    view.snapshot.delivery_start = Some(ExtensionDeliveryCursor {
        session_id: SessionId("child".to_owned()),
        sequence: SequenceId(8),
    });
    let mut transaction = transaction();
    transaction.mutations.clear();
    for (session, sequence) in [("parent", 9), ("child", 8), ("child", 12)] {
        transaction.acknowledged = Some(ExtensionDeliveryCursor {
            session_id: SessionId(session.to_owned()),
            sequence: SequenceId(sequence),
        });
        assert!(
            validate_update(
                &view,
                &transaction,
                &SessionId("child".to_owned()),
                Some(SequenceId(11))
            )
            .is_err()
        );
    }
    transaction.acknowledged = Some(ExtensionDeliveryCursor {
        session_id: SessionId("child".to_owned()),
        sequence: SequenceId(9),
    });
    assert!(
        validate_update(
            &view,
            &transaction,
            &SessionId("child".to_owned()),
            Some(SequenceId(11))
        )
        .is_ok()
    );
}

#[test]
fn deleting_a_key_releases_its_state_charge() {
    let view = view();
    let mut transaction = transaction();
    transaction.mutations = vec![ExtensionStateMutation::Delete {
        key: "task".to_owned(),
    }];
    assert_eq!(
        validate_update(
            &view,
            &transaction,
            &SessionId("session".to_owned()),
            Some(SequenceId(11))
        )
        .expect("deletion"),
        0
    );
}
