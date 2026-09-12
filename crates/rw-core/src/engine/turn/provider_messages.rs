use crate::engine::AgentLoopError;
use crate::engine::pending_event::PendingEvent;
use crate::engine::redaction::SecretRedactor;
use crate::engine::turn::redaction::redacted_json;
use crate::engine::turn::signals::CompactionProgress;
use crate::engine::turn::signals::CompactionProgressKind;
use crate::engine::turn::signals::TurnSignal;
use crate::engine::turn::tool_requests::ToolExecution;
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

/// A pending text batch owns one deadline anchored to its first delta.
#[derive(Default)]
pub(super) struct PendingText {
    text: Option<String>,
    deadline: Option<tokio::time::Instant>,
}
impl PendingText {
    pub(super) fn push(&mut self, text: &str) {
        self.text.get_or_insert_with(String::new).push_str(text);
        self.deadline.get_or_insert_with(|| {
            tokio::time::Instant::now() + crate::engine::TEXT_DELTA_COALESCE_WINDOW
        });
    }

    pub(super) fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    pub(super) fn take(&mut self) -> Option<String> {
        self.deadline = None;
        self.text.take()
    }

    pub(super) fn flush(&mut self, signals: &mpsc::UnboundedSender<TurnSignal>, turn: u64) {
        if let Some(text) = self.take() {
            send_event(signals, PendingEvent::TextDelta { turn, text });
        }
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
) -> Result<rw_types::SequenceId, AgentLoopError> {
    persist_event(
        signals,
        PendingEvent::ConversationTurnCommitted {
            agent_turn,
            turn: turn.clone(),
        },
    )
    .await
    .map(|meta| meta.sequence_id)
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

#[cfg(test)]
mod coalescing_tests {
    use super::*;
    use crate::engine::TEXT_DELTA_COALESCE_WINDOW;

    #[tokio::test(start_paused = true)]
    async fn continuous_text_keeps_the_first_deadline_and_retirement_resets_it() {
        let mut pending = PendingText::default();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let started = tokio::time::Instant::now();
        pending.push("first");
        let deadline = started + TEXT_DELTA_COALESCE_WINDOW;
        assert_eq!(pending.deadline(), Some(deadline));
        tokio::time::advance(TEXT_DELTA_COALESCE_WINDOW / 2).await;
        pending.push("second");
        assert_eq!(
            pending.deadline(),
            Some(deadline),
            "later deltas cannot postpone flushing"
        );
        tokio::time::sleep_until(deadline).await;
        assert_eq!(tokio::time::Instant::now(), deadline);
        pending.flush(&sender, 7);
        assert!(
            matches!(receiver.try_recv(), Ok(TurnSignal::Event(PendingEvent::TextDelta { turn: 7, text })) if text == "firstsecond")
        );
        assert_eq!(pending.deadline(), None);
        pending.flush(&sender, 7);
        assert!(
            receiver.try_recv().is_err(),
            "retired batch cannot emit twice"
        );
        pending.push("next");
        assert_eq!(
            pending.deadline(),
            Some(deadline + TEXT_DELTA_COALESCE_WINDOW)
        );
        assert_eq!(pending.take().as_deref(), Some("next"));
        assert_eq!(
            pending.deadline(),
            None,
            "terminal transfer also retires the timer"
        );
    }
}
