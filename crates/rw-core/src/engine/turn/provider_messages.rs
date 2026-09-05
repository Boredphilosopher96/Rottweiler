use crate::engine::AgentLoopError;
use crate::engine::pending_event::PendingEvent;
use crate::engine::redaction::SecretRedactor;
use crate::engine::turn::redaction::redacted_json;
use crate::engine::turn::signals::CompactionProgress;
use crate::engine::turn::signals::CompactionProgressKind;
use crate::engine::turn::signals::TurnSignal;
use crate::engine::turn::tool_requests::ToolExecution;
use rw_providers::ToolDefinition;
use rw_tools::ToolDescriptor;
use rw_tools::ToolRegistry;
use rw_types::Block;
use rw_types::EventMeta;
use rw_types::PlanArtifact;
use rw_types::SessionMode;
use rw_types::Turn;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub(super) fn send_event(signals: &mpsc::UnboundedSender<TurnSignal>, kind: PendingEvent) {
    let _ = signals.send(TurnSignal::Event(kind));
}

pub(super) fn send_compaction_progress(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    summary_turn: u64,
    attempt: u32,
    kind: CompactionProgressKind,
) {
    let _ = signals.send(TurnSignal::CompactionProgress(CompactionProgress {
        summary_turn,
        attempt,
        kind,
    }));
}

pub(super) fn flush_pending_text_delta(
    pending: &mut Option<String>,
    deadline: &mut Option<tokio::time::Instant>,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
) {
    *deadline = None;
    if let Some(text) = pending.take() {
        send_event(signals, PendingEvent::TextDelta { turn, text });
    }
}

pub(in crate::engine) async fn persist_event(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    kind: PendingEvent,
) -> Result<EventMeta, AgentLoopError> {
    let (respond, receive) = oneshot::channel();
    signals
        .send(TurnSignal::DurableEvent { kind, respond })
        .map_err(|_| AgentLoopError::Closed)?;
    receive.await.map_err(|_| AgentLoopError::Closed)?
}

pub(super) async fn persist_conversation_turn(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    agent_turn: u64,
    turn: &Turn,
) -> Result<(), AgentLoopError> {
    persist_event(
        signals,
        PendingEvent::ConversationTurnCommitted {
            agent_turn,
            turn: turn.clone(),
        },
    )
    .await
    .map(|_| ())
}

pub(in crate::engine) fn append_text(blocks: &mut Vec<Block>, delta: &str) {
    if let Some(Block::Text { text }) = blocks.last_mut() {
        text.push_str(delta);
    } else {
        blocks.push(Block::Text {
            text: delta.to_owned(),
        });
    }
}

pub(in crate::engine) fn append_thinking(
    blocks: &mut Vec<Block>,
    delta: &str,
    signature: Option<String>,
) {
    if delta.is_empty() && signature.is_none() {
        return;
    }
    if let Some(Block::Thinking {
        content,
        signature: current,
    }) = blocks.last_mut()
        && match (&signature, &*current) {
            (None | Some(_), None) => true,
            (Some(next), Some(existing)) => next == existing,
            (None, Some(_)) => false,
        }
    {
        content.push_str(delta);
        if signature.is_some() {
            *current = signature;
        }
        return;
    }
    blocks.push(Block::Thinking {
        content: delta.to_owned(),
        signature,
    });
}

pub(super) fn tool_definition(descriptor: ToolDescriptor) -> ToolDefinition {
    ToolDefinition {
        name: descriptor.name,
        description: descriptor.description,
        input_schema: descriptor.input_schema,
    }
}

pub(super) fn emit_plan_submission(
    execution: &ToolExecution,
    mode: SessionMode,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    redactor: &dyn SecretRedactor,
    tools: &ToolRegistry,
) {
    if mode != SessionMode::Plan || execution.is_error {
        return;
    }
    if let Some(arguments) = execution.call.arguments.as_ref()
        && let Ok(Some(semantics)) = tools.invocation_semantics(&execution.call.name, arguments)
        && semantics.behavior == rw_tools::ToolBehavior::PlanSubmission
        && let Ok(artifact) =
            serde_json::from_value::<PlanArtifact>(redacted_json(arguments.clone(), redactor))
    {
        send_event(signals, PendingEvent::PlanSubmitted { artifact });
    }
}
