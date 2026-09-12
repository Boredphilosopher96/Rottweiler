//! Repair materialized interrupted inputs without retaining completed conversation.
use super::{InterruptedTurnInputs, RecoveryError};
use crate::engine::{
    PendingEvent,
    projection::{InterruptedToolRepair, repair::repair_tools},
    turn::{append_text, append_thinking},
};
use rw_types::{Block, Role, Turn, TurnMeta};

/// The only new records required to close interrupted work. Historical bodies
/// remain in their original source; tool-only effects do not become model IR.
#[derive(Clone, Debug, PartialEq)]
pub struct InterruptedTurnRecovery {
    pub turn: u64,
    pub tools: Vec<InterruptedToolRepair>,
    pub tool_turn: Option<RecoveredToolTurn>,
    pub completed_results: Vec<(
        rw_types::ToolCallId,
        rw_types::conversation_input::ToolResultReference,
    )>,
    pub assistant_turn: Option<Turn>,
}

/// Repaired provider IR is profiled under the source worker's allocation owner.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveredToolTurn {
    pub turn: Turn,
    pub logical: rw_types::tool_result_admission::ToolResultAdmission,
}

impl InterruptedTurnInputs {
    /// Resolve canonical fragments and terminal repairs through the same rules
    /// used by audit replay. Consuming inputs releases active historical bodies.
    ///
    /// # Errors
    /// Rejects a fragment with a different turn or an unsupported fragment kind.
    pub fn repair(self) -> Result<InterruptedTurnRecovery, RecoveryError> {
        let mut assistant = Vec::new();
        let mut completed = Vec::new();
        let mut completed_results = Vec::new();
        for event in self.fragments {
            if let rw_types::EngineEvent::ToolCallFinished {
                meta,
                tool_call_id,
                invocation_id,
                ..
            } = &event
            {
                completed_results.push((
                    tool_call_id.clone(),
                    rw_types::conversation_input::ToolResultReference {
                        invocation_id: invocation_id.clone(),
                        finished_source: meta.sequence_id,
                    },
                ));
            }
            let pending = crate::engine::projection::recovered_pending_event(&event)?
                .ok_or(RecoveryError::Invalid("interrupted fragment kind"))?;
            match pending {
                PendingEvent::TextDelta { turn, text } if turn == self.turn => {
                    append_text(&mut assistant, &text);
                }
                PendingEvent::ThinkingDelta {
                    turn,
                    content,
                    signature,
                } if turn == self.turn => {
                    append_thinking(&mut assistant, &content, signature);
                }
                PendingEvent::CitationDelta { turn, uri, title } if turn == self.turn => {
                    assistant.push(Block::Citation {
                        uri,
                        title,
                        excerpt: None,
                    });
                }
                PendingEvent::ToolCallFinished {
                    turn,
                    id,
                    output,
                    is_error,
                    ..
                } if turn == self.turn => {
                    completed.push(Block::ToolResult {
                        id: rw_types::ToolCallId(id),
                        output,
                        is_error,
                    });
                }
                _ => return Err(RecoveryError::Invalid("interrupted fragment identity")),
            }
        }
        let repair = repair_tools(
            self.turn,
            self.conversation.iter(),
            self.pending_starts,
            completed,
        );
        let tool_turn = repair
            .tool_turn
            .map(|turn| {
                let logical = rw_types::tool_result_admission::ToolResultAdmission::measure(&turn)?;
                Ok::<_, RecoveryError>(RecoveredToolTurn { turn, logical })
            })
            .transpose()?;
        Ok(InterruptedTurnRecovery {
            turn: self.turn,
            tools: repair.tools,
            tool_turn,
            completed_results,
            assistant_turn: (!assistant.is_empty()).then_some(Turn {
                role: Role::Assistant,
                blocks: assistant,
                meta: TurnMeta::default(),
            }),
        })
    }
}
