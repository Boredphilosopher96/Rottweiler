#![allow(clippy::expect_used)]
use super::*;
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    Block, ClientId, CompactionReason, Cost, EngineEvent, EventMeta, PROTOCOL_VERSION, Role,
    SequenceId, SessionId, Turn, TurnMeta,
};
use tempfile::tempdir;

pub(super) fn text(role: Role, body: &str) -> Turn {
    Turn {
        role,
        blocks: vec![Block::Text { text: body.into() }],
        meta: TurnMeta::default(),
    }
}
pub(super) fn event(sequence: u64, pending: PendingEvent) -> EngineEvent {
    pending.stamp(EventMeta {
        protocol_version: PROTOCOL_VERSION,
        session_id: SessionId("canonical".into()),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-04T00:00:00.000Z".into(),
        caused_by: None,
    })
}
pub(super) fn terminal(turn: u64) -> PendingEvent {
    PendingEvent::TurnFinished {
        turn,
        status: AgentTurnStatus::Completed,
        usage: SessionUsage::default(),
        cost: Cost::Unavailable {
            reason: "fixture".into(),
        },
    }
}
pub(super) fn catch_up(
    recovery: &mut CanonicalRecovery,
    source: &rw_store::session::journal::JournalReadView,
    modes: &ModeRegistry,
) {
    for _ in 0..10_000 {
        if !recovery.advance(source, modes).expect("advance").has_more {
            return;
        }
    }
    panic!("projection did not converge");
}
pub(super) fn append(journal: &mut SegmentedJournal, pending: Vec<PendingEvent>) {
    let first = journal.read_view().prefix_identity().next_sequence;
    journal
        .append_batch(
            pending
                .into_iter()
                .enumerate()
                .map(|(index, kind)| event(first + index as u64, kind)),
        )
        .expect("append");
}

#[test]
fn exact_window_admission_and_snapshot_are_independent_of_later_appends() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    let user = text(Role::User, "first");
    let answer = text(Role::Assistant, "answer\n\"🙂");
    append(
        &mut journal,
        vec![
            PendingEvent::SessionCreated {
                driver_client_id: ClientId("driver".into()),
            },
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: user.clone(),
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: answer.clone(),
            },
            terminal(1),
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let bytes = history.window_bytes(0..2).expect("bytes");
    assert_eq!(
        history
            .materialize(
                0..2,
                HistoryMaterializationLimits {
                    max_turns: 2,
                    max_serialized_bytes: bytes,
                    max_decoded_bytes: MAX_MATERIALIZED_HISTORY_DECODE_BYTES,
                }
            )
            .expect("exact"),
        vec![user, answer.clone()]
    );
    assert!(matches!(
        history.materialize(
            0..2,
            HistoryMaterializationLimits {
                max_turns: 2,
                max_serialized_bytes: bytes - 1,
                max_decoded_bytes: MAX_MATERIALIZED_HISTORY_DECODE_BYTES,
            }
        ),
        Err(RecoveryError::Limit(_))
    ));
    assert!(matches!(
        history.materialize(
            0..2,
            HistoryMaterializationLimits {
                max_turns: 1,
                max_serialized_bytes: bytes,
                max_decoded_bytes: MAX_MATERIALIZED_HISTORY_DECODE_BYTES,
            }
        ),
        Err(RecoveryError::Limit(_))
    ));
    assert_eq!(
        history
            .materialize(1..2, HistoryMaterializationLimits::default())
            .expect("window"),
        vec![answer]
    );
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 2 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: text(Role::User, "later"),
            },
            terminal(2),
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert_eq!(history.head().conversation.turns, 2);
    assert_eq!(recovery.head().expect("head").conversation.turns, 3);
    assert_eq!(history.window_bytes(0..2).expect("old bytes"), bytes);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Exercise compaction, interrupted rewind and branch append in one durable lifecycle."
)]
fn compaction_generation_and_rewind_restore_exact_canonical_turns_across_reopen() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: text(Role::User, "original"),
            },
            terminal(1),
            PendingEvent::CompactionStarted {
                reason: CompactionReason::Manual,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: text(Role::User, "summary"),
            },
            PendingEvent::CompactionFinished {
                summary_turn: 2,
                reclaimed_tokens: 0,
                usage: None,
                cost: None,
            },
        ],
    );
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("open");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert_ne!(recovery.head().expect("head").conversation.generation, 0);
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 3 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 3,
                turn: text(Role::Assistant, "future"),
            },
            terminal(3),
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
        ],
    );
    assert!(
        recovery
            .advance(&journal.read_view(), &modes)
            .expect("start rewind")
            .maintenance
    );
    assert!(matches!(
        recovery.snapshot(),
        Err(RecoveryError::Maintenance)
    ));
    for _ in 0..10 {
        drop(recovery);
        recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None)
            .expect("reopen maintenance");
        if !recovery
            .advance(&journal.read_view(), &modes)
            .expect("maintain")
            .has_more
        {
            break;
        }
    }
    let history = recovery
        .snapshot()
        .expect("published")
        .bind_source(&journal.read_view())
        .expect("bind");
    assert_eq!(history.head().control.completed_turns, 1);
    assert_eq!(
        history
            .materialize(0..1, HistoryMaterializationLimits::default())
            .expect("old generation"),
        vec![text(Role::User, "original")]
    );
    drop(history);
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 4 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 4,
                turn: text(Role::Assistant, "new branch"),
            },
            terminal(4),
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("new snapshot")
        .bind_source(&journal.read_view())
        .expect("bind");
    assert_eq!(
        history
            .materialize(0..2, HistoryMaterializationLimits::default())
            .expect("new branch"),
        vec![
            text(Role::User, "original"),
            text(Role::Assistant, "new branch")
        ]
    );
    assert!(
        recovery
            .index
            .read()
            .expect("read")
            .get(projector::key(state::BOUNDARIES, 0, 3))
            .expect("removed boundary")
            .is_none()
    );
}

#[test]
fn model_clear_retains_only_system_rows_and_resumes_bounded_batches() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let pending = (0..200)
        .map(|index| PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: text(
                if index % 50 == 0 {
                    Role::System
                } else {
                    Role::User
                },
                &index.to_string(),
            ),
        })
        .collect();
    append(&mut journal, pending);
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("open");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    append(
        &mut journal,
        vec![PendingEvent::ModelContextCleared {
            strategy: rw_types::ModelContextTransfer::StartWithoutContext,
        }],
    );
    assert!(
        recovery
            .advance(&journal.read_view(), &modes)
            .expect("clear")
            .maintenance
    );
    for _ in 0..10 {
        drop(recovery);
        recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("reopen");
        if !recovery
            .advance(&journal.read_view(), &modes)
            .expect("batch")
            .has_more
        {
            break;
        }
    }
    let history = recovery
        .snapshot()
        .expect("published")
        .bind_source(&journal.read_view())
        .expect("bind");
    assert_eq!(
        history
            .materialize(0..4, HistoryMaterializationLimits::default())
            .expect("systems"),
        [0, 50, 100, 150]
            .iter()
            .map(|index| text(Role::System, &index.to_string()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn recovery_head_stays_small_when_canonical_payload_history_grows() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("open");
    for turn in 1..=200 {
        append(
            &mut journal,
            vec![
                PendingEvent::TurnStarted { turn },
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: turn,
                    turn: text(Role::User, &"historical".repeat(1000)),
                },
                terminal(turn),
            ],
        );
    }
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let head = recovery.head().expect("head");
    assert_eq!(head.control.completed_turns, 200);
    assert_eq!(head.conversation.turns, 200);
    assert!(serde_json::to_vec(&head).expect("serialize").len() < 2048);
    assert!(head.conversation.serialized_bytes > 2_000_000);
    drop(recovery);
    let recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("cold open");
    assert_eq!(recovery.head().expect("cold head"), head);
}

#[test]
fn large_source_record_does_not_inflate_metadata_or_prevent_exact_materialization() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let turn = text(Role::User, &"large source".repeat(200_000));
    append(
        &mut journal,
        vec![PendingEvent::ConversationTurnCommitted {
            agent_turn: 1,
            turn: turn.clone(),
        }],
    );
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("open");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert!(
        recovery
            .index
            .head()
            .expect("metadata head")
            .checkpoint
            .len()
            < 2048
    );
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("bind");
    assert_eq!(
        history
            .materialize(0..1, HistoryMaterializationLimits::default())
            .expect("large source"),
        vec![turn]
    );
    assert!(matches!(
        history.materialize(
            0..1,
            HistoryMaterializationLimits {
                max_turns: usize::MAX,
                max_serialized_bytes: u64::MAX,
                max_decoded_bytes: MAX_MATERIALIZED_HISTORY_DECODE_BYTES,
            }
        ),
        Err(RecoveryError::Limit(_))
    ));
}

#[test]
fn changed_mode_definition_rejects_entire_batch_without_advancing_head() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("open");
    append(
        &mut journal,
        vec![
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: text(Role::User, "must not publish"),
            },
            PendingEvent::ModeChanged {
                mode: rw_types::ModeId("plan".into()),
                definition_fingerprint: "changed".into(),
            },
        ],
    );
    assert!(matches!(
        recovery.advance(&journal.read_view(), &modes),
        Err(RecoveryError::Projection(
            crate::engine::SessionProjectionError::ModeDefinitionChanged(_)
        ))
    ));
    assert_eq!(recovery.head().expect("head").next_sequence, 0);
    assert_eq!(recovery.head().expect("head").conversation.turns, 0);
    assert!(
        recovery
            .index
            .read()
            .expect("read")
            .get(projector::key(state::CONVERSATION, 0, 0))
            .expect("row")
            .is_none()
    );
}
