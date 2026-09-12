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
    AccountingAttribution, Cost, ProviderCallActuals, ProviderCallIdentity, SequenceId, SessionId,
    TurnId, Usage,
};
fn identity() -> ProviderCallIdentity {
    ProviderCallIdentity {
        session_id: SessionId("canonical".into()),
        budget_session_id: SessionId("parent".into()),
        turn_id: TurnId("1".into()),
        attribution: AccountingAttribution::Main,
        call_id: "exact-call".into(),
        attempt: 0,
    }
}
fn actuals(amount: u64) -> ProviderCallActuals {
    ProviderCallActuals {
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        cost: Cost::Monetary {
            amount_micros: amount,
            currency: "USD".into(),
        },
    }
}
#[test]
fn exact_receipt_lookup_preserves_corrections_across_rewind_and_reopen() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("index");
    append(
        &mut journal,
        vec![
            PendingEvent::ProviderCallAccounted {
                call: identity(),
                actuals: actuals(10),
            },
            PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Completed,
                usage: SessionUsage::default(),
                cost: Cost::Unavailable {
                    reason: "summary".into(),
                },
            },
            PendingEvent::ProviderCallAccounted {
                call: identity(),
                actuals: actuals(12),
            },
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let receipt = history
        .provider_receipt(&identity())
        .expect("query")
        .expect("receipt");
    assert_eq!(receipt.sequence_id, SequenceId(2));
    assert_eq!(receipt.actuals, actuals(12));
    let mut foreign = identity();
    foreign.budget_session_id = SessionId("wrong-parent".into());
    assert!(history.provider_receipt(&foreign).is_err());
    drop(history);
    drop(recovery);
    let recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("reopen");
    assert_eq!(
        recovery
            .snapshot()
            .expect("snapshot")
            .bind_source(&journal.read_view())
            .expect("source")
            .provider_receipt(&identity())
            .expect("query"),
        Some(receipt)
    );
}
#[test]
fn changed_attempt_identity_rejects_before_advancing_derived_prefix() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![PendingEvent::ProviderCallAccounted {
            call: identity(),
            actuals: actuals(10),
        }],
    );
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("index");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let mut changed = identity();
    changed.turn_id = TurnId("2".into());
    append(
        &mut journal,
        vec![PendingEvent::ProviderCallAccounted {
            call: changed,
            actuals: actuals(12),
        }],
    );
    assert!(recovery.advance(&journal.read_view(), &modes).is_err());
    assert_eq!(recovery.head().expect("head").next_sequence, 1);
}
