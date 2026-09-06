#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery,
    tests::{append, catch_up, text},
};
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{Cost, Role};

fn billed_turn(id: u64, used: &str) -> Vec<SourceEvent> {
    vec![
        SourceEvent::event(PendingEvent::TurnStarted { turn: id }),
        SourceEvent::Input {
            agent_turn: id,
            turn: text(Role::User, "request"),
        },
        SourceEvent::event(PendingEvent::TurnFinished {
            turn: id,
            status: AgentTurnStatus::Completed,
            usage: SessionUsage {
                input_tokens: 7,
                output_tokens: 11,
                ..SessionUsage::default()
            },
            cost: Cost::SubscriptionQuota {
                used: Some(used.into()),
                unit: Some("requests".into()),
            },
        }),
    ]
}

#[test]
fn indexed_accounting_survives_rewind_reopen_and_reused_turn_number() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("index");
    append_script(&mut journal, billed_turn(1, "9007199254740993.1"));
    append_script(&mut journal, billed_turn(2, "0.2"));
    catch_up(&mut recovery, &journal.read_view(), &modes);
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert_eq!(recovery.head().expect("head").control.completed_turns, 1);
    assert_eq!(recovery.head().expect("head").accounting.entries, 2);
    drop(recovery);
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("reopen");
    append_script(&mut journal, billed_turn(2, "0.7"));
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let state = &recovery.head().expect("head").accounting;
    assert_eq!(state.entries, 3);
    assert_eq!(state.usage.input_tokens, 21);
    assert_eq!(state.usage.output_tokens, 33);
    assert_eq!(
        state.subscription_quota().expect("quota").used,
        "9007199254740994"
    );
}

use super::test_source::{SourceEvent, append_script};
