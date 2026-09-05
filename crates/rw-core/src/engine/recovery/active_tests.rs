#![allow(clippy::expect_used)]
use super::{tests::*, *};
use crate::engine::PendingEvent;
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{CompactionReason, EngineEvent, Role, ToolInvocationId, ToolOutput};
use tempfile::tempdir;

fn start(invocation: &str) -> PendingEvent {
    PendingEvent::ToolCallStarted {
        turn: 1,
        id: "reused-provider-id".into(),
        invocation_id: ToolInvocationId(invocation.into()),
        name: "fixture".into(),
        arguments: serde_json::json!({}),
        index: 0,
    }
}

#[test]
fn interrupted_inputs_keep_only_uncommitted_fragments_and_unresolved_host_invocations() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    let user = text(Role::User, "input");
    let answer = text(Role::Assistant, "already committed");
    let output = text(Role::Tool, "already committed tool result");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: user.clone(),
            },
            PendingEvent::TextDelta {
                turn: 1,
                text: "already committed".into(),
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: answer.clone(),
            },
            start("first"),
            PendingEvent::ToolCallFinished {
                turn: 1,
                id: "reused-provider-id".into(),
                invocation_id: ToolInvocationId("first".into()),
                output: ToolOutput::Text {
                    text: "result".into(),
                },
                is_error: false,
                index: 0,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: output.clone(),
            },
            start("second"),
        ],
    );
    append(
        &mut journal,
        (0..200)
            .map(|_| PendingEvent::ToolOutput {
                turn: 1,
                id: "reused-provider-id".into(),
                invocation_id: ToolInvocationId("second".into()),
                stream: "stdout".into(),
                chunk: "unneeded streaming output".repeat(100),
            })
            .collect(),
    );
    append(
        &mut journal,
        vec![PendingEvent::TextDelta {
            turn: 1,
            text: "uncommitted".into(),
        }],
    );
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    let head = recovery.head().expect("head");
    let active = head.control.active.expect("active");
    assert_eq!(active.assistant_parts.records, 1);
    assert_eq!(active.tool_results.records, 0);
    assert_eq!(active.tool_lifecycle.records, 3);
    let inputs = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source")
        .interrupted_inputs()
        .expect("inputs")
        .expect("active");
    assert_eq!(inputs.conversation, vec![user, answer, output]);
    assert_eq!(inputs.pending_starts.len(), 1);
    assert_eq!(
        inputs.pending_starts[0].invocation_id,
        ToolInvocationId("second".into())
    );
    assert!(
        matches!(inputs.fragments.as_slice(), [EngineEvent::TextDelta { text, .. }] if text == "uncommitted")
    );
}

#[test]
fn interrupted_inputs_follow_compaction_generation_after_reopen() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(
        &mut journal,
        (0..20)
            .map(|_| PendingEvent::ConversationTurnCommitted {
                agent_turn: 0,
                turn: text(Role::User, "old conversation"),
            })
            .collect(),
    );
    let summary = text(Role::User, "summary");
    append(
        &mut journal,
        vec![
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::TextDelta {
                turn: 1,
                text: "before compaction".into(),
            },
            PendingEvent::CompactionStarted {
                reason: CompactionReason::Manual,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: summary.clone(),
            },
            PendingEvent::CompactionFinished {
                summary_turn: 1,
                reclaimed_tokens: 0,
                usage: None,
                cost: None,
            },
            PendingEvent::TextDelta {
                turn: 1,
                text: "after compaction".into(),
            },
        ],
    );
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    drop(recovery);
    let recovery = CanonicalRecovery::open(&journal.read_view(), &modes).expect("reopen");
    let inputs = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source")
        .interrupted_inputs()
        .expect("inputs")
        .expect("active");
    assert_eq!(inputs.conversation, vec![summary]);
    assert!(
        matches!(inputs.fragments.as_slice(), [EngineEvent::TextDelta { text, .. }] if text == "after compaction")
    );
}

#[test]
fn oversized_active_materialization_is_rejected_from_admission_metadata() {
    let root = tempdir().expect("root");
    let modes = ModeRegistry::builtins().expect("modes");
    let mut journal = SegmentedJournal::open(root.path(), "canonical").expect("journal");
    append(&mut journal, vec![PendingEvent::TurnStarted { turn: 1 }]);
    for _ in 0..5 {
        append(
            &mut journal,
            vec![PendingEvent::TextDelta {
                turn: 1,
                text: "x".repeat(7 * 1024 * 1024),
            }],
        );
    }
    let mut recovery = CanonicalRecovery::open(&journal.read_view(), &modes).expect("recovery");
    catch_up(&mut recovery, &journal.read_view(), &modes);
    assert!(
        serde_json::to_vec(&recovery.head().expect("head"))
            .expect("encode")
            .len()
            < 2048
    );
    let history = recovery
        .snapshot()
        .expect("snapshot")
        .bind_source(&journal.read_view())
        .expect("source");
    assert!(matches!(
        history.interrupted_inputs(),
        Err(RecoveryError::Limit("interrupted turn materialization"))
    ));
}
