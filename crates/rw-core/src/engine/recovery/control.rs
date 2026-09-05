//! Resolve bounded live controls without materializing historical conversation.

use super::{CanonicalHistory, RecoveryError, read::SourceReader};
use crate::engine::{PendingEvent, RecoveredQuestion, RecoveredUserShell};
use rw_types::{
    ModelAlias, PermissionModeDescriptor, PlanArtifact, PlanDecision, SequenceId, StoredAttachment,
    WorkspaceRootDescriptor, config::ThinkingLevel,
};
use std::collections::VecDeque;

/// Aggregate canonical event bytes retained while resolving live control payloads.
pub const MAX_CONTROL_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

/// Durable selected model, including the exact route and reasoning configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveredModelSelection {
    pub model: ModelAlias,
    pub provider: Option<String>,
    pub thinking: ThinkingLevel,
}

/// Exact accepted or queued client message; attachment selectors remain authoritative.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveredMessage {
    pub content: String,
    pub attachments: Vec<StoredAttachment>,
}

/// Live payloads selected by the bounded recovery head. No historical IR is retained.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecoveryControlPayloads {
    pub title: Option<String>,
    pub todos: rw_types::todo::TodoSnapshot,
    pub model: Option<RecoveredModelSelection>,
    pub permission_mode: Option<PermissionModeDescriptor>,
    pub pending_plan: Option<PlanArtifact>,
    pub approved_plan: Option<PlanArtifact>,
    pub active_shell: Option<RecoveredUserShell>,
    pub workspace_roots: Vec<WorkspaceRootDescriptor>,
    pub queued_messages: Vec<(u64, RecoveredMessage)>,
    pub accepted_messages: Vec<(u64, RecoveredMessage)>,
    pub pending_questions: Vec<RecoveredQuestion>,
    /// Charged serialized bytes of the authoritative events supplying these payloads.
    pub source_bytes: u64,
}

/// Cold actor input contains live controls and the interrupted turn only.
/// Lifetime conversation and completed-turn records remain indexed.
pub struct RecoveryBootstrap {
    pub head: super::RecoveryHead,
    pub controls: RecoveryControlPayloads,
    pub interrupted: Option<super::InterruptedTurnInputs>,
}

impl CanonicalHistory {
    /// Read the bounded live input required to recover an actor.
    ///
    /// # Errors
    /// Rejects invalid selectors or control/interrupted-turn admission overflow.
    pub fn bootstrap(&self) -> Result<RecoveryBootstrap, RecoveryError> {
        Ok(RecoveryBootstrap {
            head: self.head.clone(),
            controls: self.control_payloads(MAX_CONTROL_SOURCE_BYTES)?,
            interrupted: self.interrupted_inputs()?,
        })
    }

    /// Resolve only the currently referenced control events, with aggregate admission.
    /// Each source page remains separately bounded by the journal line/page limits.
    ///
    /// # Errors
    /// Rejects stale or mismatched selectors and aggregate live-payload overflow.
    pub fn control_payloads(
        &self,
        max_source_bytes: u64,
    ) -> Result<RecoveryControlPayloads, RecoveryError> {
        if max_source_bytes > MAX_CONTROL_SOURCE_BYTES {
            return Err(RecoveryError::Limit(
                "control source limit exceeds hard bound",
            ));
        }
        let mut reader = ControlReader {
            source: SourceReader {
                source: &self.source,
                events: VecDeque::new(),
            },
            bytes: 0,
            limit: max_source_bytes,
        };
        let control = &self.head.control;
        let mut result = RecoveryControlPayloads::default();
        if let Some(sequence) = control.todos {
            let PendingEvent::TodoStateCommitted { snapshot } = reader.event(sequence)? else {
                return Err(RecoveryError::Invalid("task state source selector"));
            };
            snapshot
                .validate()
                .map_err(|_| RecoveryError::Invalid("task snapshot"))?;
            result.todos = snapshot;
        }
        reader.selection(control, &mut result)?;
        reader.workspace_and_plans(control, &mut result)?;
        for queued in &control.queued {
            let PendingEvent::MessageQueued {
                position,
                content,
                attachments,
            } = reader.event(queued.sequence)?
            else {
                return Err(RecoveryError::Invalid("queue source selector"));
            };
            if position != queued.position
                || blake3::hash(content.as_bytes()).as_bytes() != &queued.content_digest
            {
                return Err(RecoveryError::Invalid("queue source identity"));
            }
            result.queued_messages.push((
                position,
                RecoveredMessage {
                    content,
                    attachments,
                },
            ));
        }
        for accepted in &control.accepted {
            let PendingEvent::UserMessageAccepted {
                turn,
                content,
                attachments,
            } = reader.event(accepted.sequence)?
            else {
                return Err(RecoveryError::Invalid("accepted message source selector"));
            };
            if turn != accepted.agent_turn {
                return Err(RecoveryError::Invalid("accepted message source identity"));
            }
            result.accepted_messages.push((
                turn,
                RecoveredMessage {
                    content,
                    attachments,
                },
            ));
        }
        for question in &control.questions {
            let PendingEvent::QuestionAsked {
                turn,
                question_id,
                questions,
            } = reader.event(question.sequence)?
            else {
                return Err(RecoveryError::Invalid("question source selector"));
            };
            if turn != question.agent_turn || question_id.0 != question.id {
                return Err(RecoveryError::Invalid("question source identity"));
            }
            result.pending_questions.push(RecoveredQuestion {
                agent_turn: turn,
                question_id,
                questions,
            });
        }
        result.source_bytes = reader.bytes;
        Ok(result)
    }
}

struct ControlReader<'a> {
    source: SourceReader<'a>,
    bytes: u64,
    limit: u64,
}
impl ControlReader<'_> {
    fn selection(
        &mut self,
        control: &super::RecoveryControl,
        result: &mut RecoveryControlPayloads,
    ) -> Result<(), RecoveryError> {
        if let Some(sequence) = control.title {
            let PendingEvent::SessionTitleUpdated { title, .. } = self.event(sequence)? else {
                return Err(RecoveryError::Invalid("title source selector"));
            };
            result.title = Some(title);
        }
        if let Some(sequence) = control.model {
            let PendingEvent::ModelChanged {
                model,
                provider,
                thinking,
            } = self.event(sequence)?
            else {
                return Err(RecoveryError::Invalid("model source selector"));
            };
            result.model = Some(RecoveredModelSelection {
                model,
                provider,
                thinking,
            });
        }
        if let Some(sequence) = control.permission_mode {
            let PendingEvent::PermissionModeChanged { mode } = self.event(sequence)? else {
                return Err(RecoveryError::Invalid("permission source selector"));
            };
            result.permission_mode = mode;
        }
        Ok(())
    }
    fn workspace_and_plans(
        &mut self,
        control: &super::RecoveryControl,
        result: &mut RecoveryControlPayloads,
    ) -> Result<(), RecoveryError> {
        if let Some(sequence) = control.pending_plan {
            let PendingEvent::PlanSubmitted { artifact } = self.event(sequence)? else {
                return Err(RecoveryError::Invalid("pending plan source selector"));
            };
            result.pending_plan = Some(artifact);
        }
        if let Some(sequence) = control.approved_plan {
            let PendingEvent::PlanReviewed {
                artifact,
                decision: PlanDecision::Approve,
                ..
            } = self.event(sequence)?
            else {
                return Err(RecoveryError::Invalid("approved plan source selector"));
            };
            result.approved_plan = Some(artifact);
        }
        if let Some((expected, sequence)) = &control.active_shell {
            let PendingEvent::UserShellStateChanged {
                shell_id,
                command,
                active: true,
                status: None,
                captured_output: None,
            } = self.event(*sequence)?
            else {
                return Err(RecoveryError::Invalid("active shell source selector"));
            };
            if shell_id.0 != *expected {
                return Err(RecoveryError::Invalid("active shell source identity"));
            }
            result.active_shell = Some(RecoveredUserShell { shell_id, command });
        }
        if let Some(sequence) = control.workspace {
            let PendingEvent::WorkspaceRootsChanged {
                generation, roots, ..
            } = self.event(sequence)?
            else {
                return Err(RecoveryError::Invalid("workspace source selector"));
            };
            if generation != control.workspace_generation
                || roots.len() != control.workspace_root_count
            {
                return Err(RecoveryError::Invalid("workspace source identity"));
            }
            result.workspace_roots = roots;
        }
        Ok(())
    }
    fn event(&mut self, sequence: SequenceId) -> Result<PendingEvent, RecoveryError> {
        let event = self.source.event(sequence)?;
        self.bytes = self
            .bytes
            .checked_add(super::encoding::serialized_size(&event)?)
            .ok_or(RecoveryError::Limit("control source byte counter"))?;
        if self.bytes > self.limit {
            return Err(RecoveryError::Limit("live control materialization"));
        }
        crate::engine::projection::recovered_pending_event(&event)?
            .ok_or(RecoveryError::Invalid("unrecognized control source"))
    }
}
