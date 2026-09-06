#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    CanonicalRecovery, InterruptedTurnRecovery,
    tests::{append, catch_up},
};
use crate::engine::{PendingEvent, SessionRecoveredState, project_session_events};
use rw_ext::ModeRegistry;
use rw_store::session::journal::SegmentedJournal;
use rw_types::{
    Block, EventMeta, PROTOCOL_VERSION, Role, SequenceId, SessionId, ToolCallId, ToolInvocationId,
    ToolOutput, Turn, TurnMeta,
};

fn source_start() -> PendingEvent {
    PendingEvent::ToolCallStarted {
        turn: 1,
        id: "call".into(),
        invocation_id: ToolInvocationId("host-owned".into()),
        name: "fixture".into(),
        arguments: serde_json::json!({}),
        index: 3,
    }
}
fn source_finish() -> PendingEvent {
    PendingEvent::ToolCallFinished {
        turn: 1,
        id: "call".into(),
        invocation_id: ToolInvocationId("host-owned".into()),
        output: ToolOutput::Text {
            text: "actual result".into(),
        },
        is_error: false,
        index: 3,
        presentation: None,
    }
}
fn provider_call() -> PendingEvent {
    PendingEvent::ConversationTurnCommitted {
        agent_turn: 1,
        turn: Turn {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall {
                id: ToolCallId("call".into()),
                name: "fixture".into(),
                args: serde_json::json!({}),
            }],
            meta: TurnMeta::default(),
        },
    }
}
fn recover(pending: Vec<PendingEvent>) -> (SessionRecoveredState, InterruptedTurnRecovery) {
    let events = pending
        .iter()
        .enumerate()
        .map(|(index, event)| {
            event.clone().stamp(EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: SessionId("canonical".into()),
                sequence_id: SequenceId(index as u64),
                emitted_at: "2026-09-04T00:00:00.000Z".into(),
                caused_by: None,
            })
        })
        .collect::<Vec<_>>();
    let projected = project_session_events(&events).expect("audit projection");
    let directory = tempfile::tempdir().expect("directory");
    let mut journal = SegmentedJournal::open(directory.path(), "canonical").expect("journal");
    append(&mut journal, pending);
    let modes = ModeRegistry::builtins().expect("modes");
    let source = journal.read_view();
    let mut index = CanonicalRecovery::open(&source, &modes, None).expect("index");
    catch_up(&mut index, &source, &modes);
    let bootstrap = index
        .snapshot()
        .expect("snapshot")
        .bind_source(&source)
        .expect("source")
        .bootstrap()
        .expect("bootstrap");
    (projected, bootstrap.interrupted.expect("interrupted"))
}
#[test]
fn completed_tool_only_effect_does_not_invent_a_model_result_on_restart() {
    let (audit, canonical) = recover(vec![
        PendingEvent::TurnStarted { turn: 1 },
        source_start(),
        source_finish(),
    ]);
    assert!(audit.conversation.is_empty());
    assert!(audit.interrupted_tool_turn.is_none());
    assert!(audit.interrupted_tool_repairs.is_empty());
    assert!(canonical.tool_turn.is_none());
    assert!(canonical.tools.is_empty());
}
#[test]
fn interrupted_tool_only_effect_preserves_its_host_identity_without_model_ir() {
    let (audit, canonical) = recover(vec![PendingEvent::TurnStarted { turn: 1 }, source_start()]);
    assert!(audit.conversation.is_empty());
    assert!(canonical.tool_turn.is_none());
    assert_eq!(canonical.tools, audit.interrupted_tool_repairs);
    assert_eq!(canonical.tools.len(), 1);
    assert_eq!(
        canonical.tools[0].invocation_id,
        ToolInvocationId("host-owned".into())
    );
    assert_eq!(canonical.tools[0].call_index, 3);
    assert!(canonical.tools[0].missing_start.is_none());
}
#[test]
fn committed_provider_call_recovers_its_completed_tool_result() {
    let (audit, canonical) = recover(vec![
        PendingEvent::TurnStarted { turn: 1 },
        provider_call(),
        source_start(),
        source_finish(),
    ]);
    assert_eq!(
        canonical.tool_turn.as_ref().map(|value| &value.turn),
        audit.interrupted_tool_turn.as_ref()
    );
    assert!(canonical.tools.is_empty());
    let turn = canonical.tool_turn.expect("provider result").turn;
    assert!(
        matches!(turn.blocks.as_slice(), [Block::ToolResult { is_error: false, output: ToolOutput::Text { text }, .. }] if text == "actual result")
    );
}
#[test]
fn committed_unstarted_provider_call_gets_the_same_repair_from_both_readers() {
    let (audit, canonical) = recover(vec![PendingEvent::TurnStarted { turn: 1 }, provider_call()]);
    assert_eq!(
        canonical.tool_turn.as_ref().map(|value| &value.turn),
        audit.interrupted_tool_turn.as_ref()
    );
    assert_eq!(canonical.tools, audit.interrupted_tool_repairs);
    assert_eq!(canonical.tools.len(), 1);
    assert_eq!(
        canonical.tools[0].invocation_id,
        ToolInvocationId("turn-1:repair-0".into())
    );
    assert!(canonical.tools[0].missing_start.is_some());
}
