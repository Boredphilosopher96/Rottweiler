#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery, HistoryMaterializationLimits, MAX_MATERIALIZED_HISTORY_BYTES,
    MAX_MATERIALIZED_HISTORY_TURNS, RecoveryError,
    tests::{append, catch_up, text},
};
use crate::engine::{AgentTurnStatus, PendingEvent, SessionUsage};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{Cost, Role, SequenceId};

fn finish(turn: u64) -> PendingEvent {
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
fn decoded_admission_cuts_source_pages_and_resumes_without_losing_turns() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    let expected = (0..9)
        .map(|index| text(Role::User, &format!("message {index} with escaped \"🙂")))
        .collect::<Vec<_>>();
    append(
        &mut journal,
        expected
            .iter()
            .enumerate()
            .map(|(index, turn)| PendingEvent::ConversationTurnCommitted {
                agent_turn: index as u64 + 1,
                turn: turn.clone(),
            })
            .collect(),
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let two = history.window_decoded_bytes(0..2).expect("decode charge");
    let limits = HistoryMaterializationLimits {
        max_estimated_tokens: u64::MAX,
        max_turns: MAX_MATERIALIZED_HISTORY_TURNS,
        max_serialized_bytes: MAX_MATERIALIZED_HISTORY_BYTES,
        max_decoded_bytes: two,
    };
    let mut all = Vec::new();
    let mut next = 0;
    while next < 9 {
        let page = history.conversation_page(next..9, limits).expect("page");
        assert_eq!(page.range.start, next);
        assert!(page.range.end > next);
        assert!(page.range.end - next <= 2);
        assert!(page.decoded_bytes <= two);
        for (source, ordinal) in page.sources.iter().zip(page.range.clone()) {
            assert_eq!(source.sequence, SequenceId(ordinal));
        }
        assert_eq!(page.has_more, page.range.end < 9);
        next = page.range.end;
        all.extend(page.turns);
    }
    assert_eq!(all, expected);
    let tokens = history
        .window_estimated_tokens(0..3)
        .expect("three-turn token estimate");
    let token_page = history
        .conversation_page(
            0..9,
            HistoryMaterializationLimits {
                max_estimated_tokens: tokens,
                ..HistoryMaterializationLimits::default()
            },
        )
        .expect("token-selected source window");
    assert_eq!(token_page.range, 0..3);
    assert!(token_page.has_more);

    assert!(matches!(
        history.materialize(0..9, limits),
        Err(RecoveryError::Limit(_))
    ));
    let first = history.turn_source(0).expect("first source");
    assert!(matches!(
        history.conversation_page(
            0..9,
            HistoryMaterializationLimits {
                max_decoded_bytes: first.decoded_bytes - 1,
                ..limits
            }
        ),
        Err(RecoveryError::Limit(_))
    ));
    assert!(matches!(
        history.conversation_page(
            0..9,
            HistoryMaterializationLimits {
                max_turns: 0,
                ..limits
            }
        ),
        Err(RecoveryError::Limit(_))
    ));
}

#[test]
fn captured_context_pages_keep_exact_sources_across_rewind_and_reused_ordinals() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    for turn in 1..=2 {
        append(
            &mut journal,
            vec![
                PendingEvent::TurnStarted { turn },
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: turn,
                    turn: text(Role::User, &format!("original {turn}")),
                },
                finish(turn),
            ],
        );
    }
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let captured = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    append(
        &mut journal,
        vec![
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind".into(),
                unrestorable_paths: vec![],
            },
            PendingEvent::TurnStarted { turn: 2 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: text(Role::User, "replacement"),
            },
            finish(2),
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let current = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let old_page = captured
        .conversation_page(1..2, HistoryMaterializationLimits::default())
        .expect("captured page");
    let new_page = current
        .conversation_page(1..2, HistoryMaterializationLimits::default())
        .expect("current page");
    assert_eq!(old_page.turns, vec![text(Role::User, "original 2")]);
    assert_eq!(new_page.turns, vec![text(Role::User, "replacement")]);
    assert_ne!(old_page.sources[0].sequence, new_page.sources[0].sequence);
    assert_eq!(
        current.window_decoded_bytes(0..2).expect("total"),
        current.head().conversation.decoded_bytes
    );
}

#[test]
fn output_pruning_pages_follow_captured_revisions_and_rewind() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    let tool = rw_types::Turn {
        role: Role::Tool,
        blocks: vec![rw_types::Block::ToolResult {
            id: rw_types::ToolCallId("output".into()),
            output: rw_types::ToolOutput::Text {
                text: "authoritative result".into(),
            },
            is_error: false,
        }],
        meta: rw_types::TurnMeta::default(),
    };
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: tool.clone(),
            },
            finish(1),
            PendingEvent::ToolOutputPruned {
                tool_call_id: "output".into(),
                reclaimed_tokens: 12,
            },
        ],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let captured = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("bind");
    let pruned = captured
        .conversation_page(0..1, HistoryMaterializationLimits::default())
        .expect("page");
    assert_eq!(pruned.pruned_tool_outputs.get("output"), Some(&12));
    assert_eq!(pruned.turns, vec![tool.clone()]);
    append(
        &mut journal,
        vec![PendingEvent::ConversationRewound {
            to_turn: 1,
            operation_id: "rewind-pruning".into(),
            unrestorable_paths: vec![],
        }],
    );
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let current = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("bind");
    assert!(
        current
            .conversation_page(0..1, HistoryMaterializationLimits::default())
            .expect("page")
            .pruned_tool_outputs
            .is_empty()
    );
    assert_eq!(
        captured.pruned_output("output").expect("captured"),
        Some(12)
    );
}

#[test]
fn retained_turn_window_does_not_accumulate_decoder_scratch() {
    let root = tempfile::tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let mut recovery =
        CanonicalRecovery::open(&journal.read_view(), &modes, None).expect("recovery");
    for index in 0..300 {
        append(
            &mut journal,
            vec![PendingEvent::ConversationTurnCommitted {
                agent_turn: index + 1,
                turn: text(Role::User, &format!("bounded message {index}")),
            }],
        );
    }
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    let page = history
        .conversation_page(0..300, HistoryMaterializationLimits::default())
        .expect("whole bounded window");
    assert_eq!(page.range, 0..300);
    assert!(!page.has_more);
    assert_eq!(page.turns.len(), 300);
    assert!(page.decoded_bytes < 1024 * 1024);
}
