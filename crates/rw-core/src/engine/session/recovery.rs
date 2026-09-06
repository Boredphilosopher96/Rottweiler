use super::SessionActorRecovery;
use crate::engine::AgentLoopError;
use crate::engine::AgentTurnStatus;
use crate::engine::SessionUsage;
use crate::engine::pending_event::PendingEvent;
use crate::engine::projection::InterruptedToolRepair;
use crate::engine::session::config::SessionActorConfig;
use crate::engine::session::state::ActorState;
use crate::engine::turn::emit;
use crate::engine::turn::emit_batch;
use crate::engine::unavailable_cost;
use crate::engine::wire_turn_id;
use rw_types::AccountingAttribution;
use rw_types::ApprovalDecision;
use rw_types::TurnAccounting;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

pub(in crate::engine) fn interrupted_tool_recovery_events(
    repair: &InterruptedToolRepair,
) -> Vec<PendingEvent> {
    let mut events = Vec::with_capacity(2);
    if let Some(start) = &repair.missing_start {
        events.push(PendingEvent::ToolCallStarted {
            turn: repair.agent_turn,
            id: repair.tool_call_id.0.clone(),
            invocation_id: repair.invocation_id.clone(),
            name: start.name.clone(),
            arguments: start.arguments.clone(),
            index: repair.call_index,
        });
    }
    events.push(PendingEvent::ToolCallFinished {
        presentation: None,
        turn: repair.agent_turn,
        id: repair.tool_call_id.0.clone(),
        invocation_id: repair.invocation_id.clone(),
        output: repair.output.clone(),
        is_error: true,
        index: repair.call_index,
    });
    events
}

/// A repair batch binds newly generated completion references at its actual append prefix.
pub(super) struct InterruptedRecoveryEvents {
    turn: Option<u64>,
    events: Vec<PendingEvent>,
    tool_turn: Option<crate::engine::recovery::RecoveredToolTurn>,
    completed: Vec<(
        rw_types::ToolCallId,
        rw_types::conversation_input::ToolResultReference,
    )>,
}
pub(super) fn interrupted_turn_recovery_events(
    recovered: &mut SessionActorRecovery,
) -> InterruptedRecoveryEvents {
    let mut events = recovered
        .interrupted_tool_repairs
        .iter()
        .flat_map(interrupted_tool_recovery_events)
        .collect::<Vec<_>>();
    events.extend(recovered.accepted_messages.iter().filter_map(|message| {
        let accepted = message.accepted.as_ref()?;
        (Some(accepted.claimed_turn) == recovered.interrupted_turn && !accepted.retained).then_some(
            PendingEvent::UserMessageRetained {
                accepted_source: accepted.sequence,
            },
        )
    }));
    if let (Some(turn), Some(assistant)) = (
        recovered.interrupted_turn,
        recovered.interrupted_assistant_turn.take(),
    ) {
        events.push(PendingEvent::ConversationTurnCommitted {
            agent_turn: turn,
            turn: assistant,
        });
    }
    InterruptedRecoveryEvents {
        turn: recovered.interrupted_turn,
        events,
        tool_turn: recovered.interrupted_tool_turn.take(),
        completed: std::mem::take(&mut recovered.interrupted_completed_results),
    }
}
impl InterruptedRecoveryEvents {
    pub(super) fn into_events(mut self, first: u64) -> Result<Vec<PendingEvent>, AgentLoopError> {
        use rw_types::conversation_input::ToolResultReference;
        let Some(turn) = self.turn else {
            return Ok(Vec::new());
        };
        for (index, event) in self.events.iter().enumerate() {
            if let PendingEvent::ToolCallFinished {
                id, invocation_id, ..
            } = event
            {
                let source = first
                    .checked_add(index as u64)
                    .ok_or_else(|| invalid_repair("repair sequence overflow"))?;
                self.completed.push((
                    rw_types::ToolCallId(id.clone()),
                    ToolResultReference {
                        invocation_id: invocation_id.clone(),
                        finished_source: rw_types::SequenceId(source),
                    },
                ));
            }
        }
        if let Some(crate::engine::recovery::RecoveredToolTurn {
            turn: tool_turn,
            logical,
        }) = self.tool_turn
        {
            let mut results = Vec::with_capacity(tool_turn.blocks.len());
            for block in tool_turn.blocks {
                let rw_types::Block::ToolResult { id, .. } = block else {
                    return Err(invalid_repair("repair result block"));
                };
                let index = self
                    .completed
                    .iter()
                    .position(|(candidate, _)| *candidate == id)
                    .ok_or_else(|| {
                        invalid_repair("repair result has no authoritative completion")
                    })?;
                results.push(self.completed.remove(index).1);
            }
            self.events
                .push(PendingEvent::ConversationToolResultsCommitted {
                    agent_turn: turn,
                    results,
                    logical,
                });
        }
        self.events.push(PendingEvent::TurnFinished {
            turn,
            status: AgentTurnStatus::Interrupted,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
        });
        Ok(self.events)
    }
}
fn invalid_repair(message: &str) -> AgentLoopError {
    AgentLoopError::Persistence(message.into())
}

/// Rebuilds all mutable actor state from the authoritative journal after an
/// append error. A sink's default batch implementation may have committed a
/// prefix before returning an error, so retaining any in-memory mutations is
/// unsafe. The interrupted turn is durably closed before the actor accepts
/// more work.
pub(in crate::engine) async fn recover_actor_from_journal(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &crate::engine::live_events::LiveEvents,
    active_turn: &Arc<AtomicU64>,
) -> Result<(), AgentLoopError> {
    if let Some(running) = &state.running {
        running.cancellation.cancel();
        state.control.finish(running.id);
    }
    active_turn.store(0, Ordering::Release);
    for (_, pending) in std::mem::take(&mut state.pending_approvals) {
        let _ = pending.respond.send(ApprovalDecision::Deny);
    }
    for (_, pending) in std::mem::take(&mut state.pending_questions) {
        let _ = pending.respond.send(Err(rw_tools::ToolError::Cancelled));
    }

    let suspended = state.suspended_inputs.is_some();
    let mut recovered = SessionActorRecovery::from_bootstrap(
        config.history.capture_history().await?.bootstrap().await?,
    )?;
    let client_roles = std::mem::take(&mut state.client_roles);
    let tasks = state.tasks.clone();
    let control = Arc::clone(&state.control);
    control.commit_driver(recovered.driver_client_id.clone());
    let interrupted_compaction = recovered.interrupted_compaction;
    let interrupted_turn = recovered.interrupted_turn;
    let recovery_events = interrupted_turn_recovery_events(&mut recovered);
    let suspended_inputs = (suspended || !recovered.accepted_messages.is_empty())
        .then(|| std::mem::take(&mut recovered.accepted_messages));
    *state = ActorState::recover(
        config.session_id.clone(),
        Arc::clone(&config.event_clock),
        &config.model_alias,
        config.thinking,
        &config.modes,
        recovered,
        control,
    );
    state.suspended_inputs = suspended_inputs;
    state.tasks = tasks;
    state.client_roles = client_roles;

    if interrupted_compaction {
        emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::Error {
                message: "interrupted compaction was aborted during recovery".to_owned(),
            },
        )
        .await?;
    }
    if let Some(turn) = interrupted_turn {
        let recovery_events =
            recovery_events.into_events(state.sequence.map_or(0, |sequence| sequence + 1))?;
        emit_batch(state, events, &config.event_sink, recovery_events).await?;
        state.accounting.record(&TurnAccounting {
            turn_id: wire_turn_id(turn),
            attribution: AccountingAttribution::Main,
            usage: SessionUsage::default().into(),
            cost: unavailable_cost(),
        });
        state.completed_turns = state.completed_turns.saturating_add(1);
    }
    Ok(())
}
