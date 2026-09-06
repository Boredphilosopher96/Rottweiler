#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery,
    tests::{append, catch_up, text},
};
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{CompactionReason, Cost, ModelContextTransfer, Role};

fn model_turn(role: Role, model: &str) -> PendingEvent {
    let mut turn = text(role, "body");
    turn.meta.model = Some(model.into());
    PendingEvent::ConversationTurnCommitted {
        agent_turn: 1,
        turn,
    }
}

fn resolved(recovery: &CanonicalRecovery, journal: &SegmentedJournal) -> Option<String> {
    recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source")
        .bootstrap()
        .expect("bootstrap")
        .controls
        .resolved_model
}

#[test]
fn resolved_model_tracks_effective_source_across_clear_rewind_and_compaction() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            model_turn(Role::System, "provider/system"),
            model_turn(Role::Assistant, "provider/answer"),
            model_turn(Role::Assistant, "fast"),
            PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Completed,
                usage: SessionUsage::default(),
                cost: Cost::Unavailable {
                    reason: "fixture".into(),
                },
            },
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert_eq!(
        resolved(&recovery, &journal).as_deref(),
        Some("provider/answer")
    );
    append(
        &mut journal,
        vec![PendingEvent::ModelContextCleared {
            strategy: ModelContextTransfer::StartWithoutContext,
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert_eq!(
        resolved(&recovery, &journal).as_deref(),
        Some("provider/system")
    );
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "restore".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert_eq!(
        resolved(&recovery, &journal).as_deref(),
        Some("provider/answer")
    );
    append(
        &mut journal,
        vec![
            PendingEvent::CompactionStarted {
                reason: CompactionReason::Manual,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: text(Role::Assistant, "summary"),
            },
            PendingEvent::CompactionFinished {
                summary_turn: 2,
                reclaimed_tokens: 0,
                usage: None,
                cost: None,
            },
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert_eq!(resolved(&recovery, &journal), None);
}
