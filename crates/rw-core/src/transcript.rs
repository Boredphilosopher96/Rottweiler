//! Journal-owned transcript semantics, independent of terminal layout (ADR-030).

use rw_store::session::transcript_index::{
    MAX_ROW_BYTES, TranscriptIndex, TranscriptIndexError, TranscriptIndexMutation,
    TranscriptIndexRow,
};
use rw_types::transcript::{
    TRANSCRIPT_PREVIEW_BLOCKS, TRANSCRIPT_PREVIEW_TEXT_BYTES, TranscriptBodyPreview,
    TranscriptContent, TranscriptContentSelector, TranscriptContentSource,
    TranscriptConversationBlock, TranscriptPreviewFormat, TranscriptSubagentStatus,
    TranscriptToolStatus,
};
use rw_types::{Block, EngineEvent, Role, SequenceId, ToolOutput, TurnId};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use thiserror::Error;

mod projector;
pub use projector::{TranscriptProjectionProgress, TranscriptProjector};
mod content;
pub use content::{TranscriptDocument, TranscriptDocumentChunk};

/// Constant-sized state; mutable entity bindings belong to the derived index.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TranscriptProjectionState {
    pub session_id: Option<rw_types::SessionId>,
    pub next_sequence: u64,
    pub next_ordinal: u64,
    pub active_turn: Option<u64>,
}

/// An event either yields a bounded atomic update or requires a hidden rewind job.
#[derive(Debug)]
pub enum TranscriptEventProjection {
    Update {
        state: TranscriptProjectionState,
        mutations: Vec<TranscriptIndexMutation>,
    },
    Rewind {
        target_turn: u64,
        sequence: SequenceId,
    },
}

/// Invalid canonical input or an incompatible/corrupt derived projection.
#[derive(Debug, Error)]
pub enum TranscriptProjectionError {
    #[error("invalid transcript event: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Index(#[from] TranscriptIndexError),
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
}

/// Indexed lookup also permits a bounded transaction-local overlay during catch-up.
pub trait TranscriptRowLookup {
    /// Resolve a core-owned entity binding, returning no row after its removal.
    ///
    /// # Errors
    /// Fails when the projection is incomplete or storage cannot be read.
    fn bound_row(&self, binding: &str) -> Result<Option<TranscriptIndexRow>, TranscriptIndexError>;
}
impl TranscriptRowLookup for TranscriptIndex {
    fn bound_row(&self, binding: &str) -> Result<Option<TranscriptIndexRow>, TranscriptIndexError> {
        Self::bound_row(self, binding)
    }
}

/// Resolve a completed invocation in the effective transcript without scanning history.
///
/// # Errors
/// Rejects corrupt index payloads or a binding with the wrong semantic identity.
pub fn finished_tool_source(
    rows: &impl TranscriptRowLookup,
    invocation: &rw_types::ToolInvocationId,
) -> Result<Option<SequenceId>, TranscriptProjectionError> {
    let Some(row) = rows.bound_row(&entity_binding("tool", &[&invocation.0]))? else {
        return Ok(None);
    };
    match decode(&row)? {
        TranscriptContent::Tool {
            invocation_id,
            status,
            ..
        } if invocation_id == *invocation => match status {
            TranscriptToolStatus::Running {} => Ok(None),
            TranscriptToolStatus::Finished { output, .. } => Ok(Some(output.source.sequence)),
        },
        _ => Err(TranscriptProjectionError::Invalid(
            "tool source binding identity",
        )),
    }
}

/// Interpret one contiguous durable event without performing I/O mutations.
/// The caller publishes these changes and the processed raw prefix atomically.
/// A rewind must complete before its sequence can advance the published prefix.
///
/// # Errors
/// Rejects non-durable or out-of-order events, invalid lifecycles, and corrupt rows.
pub fn project_transcript_event(
    event: &EngineEvent,
    before: &TranscriptProjectionState,
    rows: &impl TranscriptRowLookup,
) -> Result<TranscriptEventProjection, TranscriptProjectionError> {
    let meta = event
        .meta()
        .ok_or(TranscriptProjectionError::Invalid("non-durable event"))?;
    if meta.sequence_id.0 != before.next_sequence {
        return Err(TranscriptProjectionError::Invalid(
            "non-contiguous sequence",
        ));
    }
    if meta.protocol_version != rw_types::PROTOCOL_VERSION
        || rw_types::SessionId::validate(&meta.session_id.0).is_err()
        || before
            .session_id
            .as_ref()
            .is_some_and(|session| session != &meta.session_id)
    {
        return Err(TranscriptProjectionError::Invalid(
            "session/protocol identity",
        ));
    }
    let mut state = before.clone();
    state.session_id = Some(meta.session_id.clone());
    state.next_sequence = before
        .next_sequence
        .checked_add(1)
        .ok_or(TranscriptProjectionError::Invalid("sequence overflow"))?;
    if let EngineEvent::ConversationRewound { to_agent_turn, .. } = event {
        return Ok(TranscriptEventProjection::Rewind {
            target_turn: *to_agent_turn,
            sequence: meta.sequence_id,
        });
    }
    if let EngineEvent::TurnStarted { turn_id, .. } = event {
        state.active_turn = Some(turn_number(turn_id)?);
    }
    let projected = project_content(event, &state, rows)?;
    let mut mutations = Vec::with_capacity(2);
    if let Some(ProjectedRow {
        prior,
        content,
        agent_turn,
        binding,
    }) = projected
    {
        let payload = serde_json::to_vec(&content)?;
        if payload.len() > MAX_ROW_BYTES {
            return Err(TranscriptProjectionError::Invalid(
                "preview exceeds encoded row bound",
            ));
        }
        let (ordinal, key, source) = if let Some(prior) = prior {
            if prior.agent_turn != agent_turn || prior.revision >= meta.sequence_id {
                return Err(TranscriptProjectionError::Invalid("invalid row revision"));
            }
            (prior.ordinal, prior.key, prior.source)
        } else {
            let ordinal = state.next_ordinal;
            state.next_ordinal = ordinal
                .checked_add(1)
                .ok_or(TranscriptProjectionError::Invalid("ordinal overflow"))?;
            (
                ordinal,
                format!("item:{}", meta.sequence_id.0),
                meta.sequence_id,
            )
        };
        mutations.push(TranscriptIndexMutation::Put(TranscriptIndexRow {
            ordinal,
            key: key.clone(),
            source,
            revision: meta.sequence_id,
            agent_turn,
            payload,
        }));
        if let Some(binding) = binding {
            mutations.push(TranscriptIndexMutation::Bind { binding, key });
        }
    }
    Ok(TranscriptEventProjection::Update { state, mutations })
}

struct ProjectedRow {
    prior: Option<TranscriptIndexRow>,
    content: TranscriptContent,
    agent_turn: Option<u64>,
    binding: Option<String>,
}

fn project_turn_summary(
    event: &EngineEvent,
) -> Result<Option<ProjectedRow>, TranscriptProjectionError> {
    let EngineEvent::TurnFinished {
        turn_id,
        status,
        usage,
        cost,
        ..
    } = event
    else {
        return Err(TranscriptProjectionError::Invalid("turn summary source"));
    };
    Ok(Some(ProjectedRow {
        prior: None,
        agent_turn: Some(turn_number(turn_id)?),
        binding: None,
        content: TranscriptContent::TurnSummary {
            turn_id: turn_id.clone(),
            status: status.clone(),
            usage: usage.clone(),
            cost: cost.clone(),
        },
    }))
}

fn project_content(
    event: &EngineEvent,
    state: &TranscriptProjectionState,
    rows: &impl TranscriptRowLookup,
) -> Result<Option<ProjectedRow>, TranscriptProjectionError> {
    match event {
        EngineEvent::TurnFinished { .. } => project_turn_summary(event),
        EngineEvent::ConversationTurnCommitted { .. } => project_conversation(event),
        EngineEvent::ToolCallStarted { .. } => project_tool_start(event, rows),
        EngineEvent::ToolCallFinished { .. } | EngineEvent::ToolDiffReady { .. } => {
            project_tool_update(event, rows)
        }
        EngineEvent::SubagentSpawned { .. } | EngineEvent::SubagentFinished { .. } => {
            project_child(event, state, rows)
        }
        EngineEvent::CommandFinished {
            meta,
            name,
            message,
            ..
        } => Ok(Some(ProjectedRow {
            prior: None,
            agent_turn: None,
            binding: None,
            content: TranscriptContent::Command {
                name: prefix(name, 128).to_owned(),
                message: PreviewBudget(2 * 1024).text(
                    message,
                    source(
                        meta.sequence_id,
                        TranscriptContentSelector::CommandMessage {},
                    ),
                ),
            },
        })),
        EngineEvent::UserShellStateChanged {
            meta,
            shell_id,
            command,
            captured_output,
            active,
            status,
        } => {
            let binding = entity_binding("shell", &[&shell_id.0]);
            let prior = rows.bound_row(&binding)?;
            let (prior_command, prior_output, prior_status) = if let Some(prior) = &prior {
                let TranscriptContent::Shell {
                    command,
                    output,
                    status,
                    ..
                } = decode(prior)?
                else {
                    return Err(TranscriptProjectionError::Invalid("shell binding kind"));
                };
                (command, output, status)
            } else {
                (None, None, None)
            };
            let command = command
                .as_ref()
                .map(|text| {
                    PreviewBudget(512).text(
                        text,
                        source(meta.sequence_id, TranscriptContentSelector::ShellCommand {}),
                    )
                })
                .or(prior_command);
            if command.is_none() {
                return Err(TranscriptProjectionError::Invalid("shell missing command"));
            }
            Ok(Some(ProjectedRow {
                prior,
                agent_turn: None,
                binding: Some(binding),
                content: TranscriptContent::Shell {
                    command,
                    output: captured_output
                        .as_ref()
                        .map(|text| {
                            PreviewBudget(2 * 1024).text(
                                text,
                                source(meta.sequence_id, TranscriptContentSelector::ShellOutput {}),
                            )
                        })
                        .or(prior_output),
                    active: *active,
                    status: status.or(prior_status),
                },
            }))
        }
        _ => Ok(None),
    }
}

fn project_conversation(
    event: &EngineEvent,
) -> Result<Option<ProjectedRow>, TranscriptProjectionError> {
    match event {
        EngineEvent::ConversationTurnCommitted {
            meta,
            agent_turn,
            turn,
        } => {
            if turn.role == Role::Tool {
                return Ok(None);
            }
            let mut budget = PreviewBudget(TRANSCRIPT_PREVIEW_TEXT_BYTES);
            let mut blocks = Vec::new();
            let mut omitted_blocks = false;
            for (index, block) in turn.blocks.iter().enumerate() {
                if matches!(block, Block::ToolCall { .. } | Block::ToolResult { .. }) {
                    continue;
                }
                if matches!(block, Block::Thinking { content, .. }
                    if !content.split("[REDACTED]").any(|part| !part.trim().is_empty()))
                {
                    continue;
                }
                if matches!(block, Block::Text { text } if text.is_empty()) {
                    continue;
                }
                if blocks.len() == TRANSCRIPT_PREVIEW_BLOCKS {
                    omitted_blocks = true;
                    break;
                }
                let index = u32::try_from(index)
                    .map_err(|_| TranscriptProjectionError::Invalid("block ordinal"))?;
                let source = source(
                    meta.sequence_id,
                    TranscriptContentSelector::ConversationBlock { index },
                );
                let projected = match block {
                    Block::Text { text } => TranscriptConversationBlock::Text {
                        body: budget.text(text, source),
                    },
                    Block::Thinking { content, .. } => TranscriptConversationBlock::Reasoning {
                        body: budget.text(content, source),
                    },
                    Block::Image { .. } => TranscriptConversationBlock::Image { source },
                    Block::Citation { .. } => TranscriptConversationBlock::Citation {
                        body: budget.json(block, source)?,
                    },
                    Block::ToolCall { .. } | Block::ToolResult { .. } => unreachable!(),
                };
                blocks.push(projected);
            }
            if blocks.is_empty() {
                return Ok(None);
            }
            Ok(Some(ProjectedRow {
                prior: None,
                agent_turn: Some(*agent_turn),
                binding: None,
                content: TranscriptContent::Conversation {
                    role: turn.role.clone(),
                    blocks,
                    omitted_blocks,
                    source: source(meta.sequence_id, TranscriptContentSelector::Conversation {}),
                },
            }))
        }
        _ => Ok(None),
    }
}

fn project_tool_start(
    event: &EngineEvent,
    rows: &impl TranscriptRowLookup,
) -> Result<Option<ProjectedRow>, TranscriptProjectionError> {
    match event {
        EngineEvent::ToolCallStarted {
            meta,
            turn_id,
            invocation_id,
            name,
            args,
            call_index,
            ..
        } => {
            if invocation_id.0.len() > 128 {
                return Err(TranscriptProjectionError::Invalid(
                    "tool invocation identifier bound",
                ));
            }
            let binding = entity_binding("tool", &[&invocation_id.0]);
            if rows.bound_row(&binding)?.is_some() {
                return Err(TranscriptProjectionError::Invalid("reused tool invocation"));
            }
            Ok(Some(ProjectedRow {
                prior: None,
                agent_turn: Some(turn_number(turn_id)?),
                binding: Some(binding),
                content: TranscriptContent::Tool {
                    invocation_id: invocation_id.clone(),
                    name: prefix(name, 128).to_owned(),
                    call_index: *call_index,
                    arguments: PreviewBudget(512).json(
                        args,
                        source(
                            meta.sequence_id,
                            TranscriptContentSelector::ToolArguments {},
                        ),
                    )?,
                    diff: None,
                    status: TranscriptToolStatus::Running {},
                },
            }))
        }
        _ => Ok(None),
    }
}

fn project_tool_update(
    event: &EngineEvent,
    rows: &impl TranscriptRowLookup,
) -> Result<Option<ProjectedRow>, TranscriptProjectionError> {
    match event {
        EngineEvent::ToolCallFinished {
            meta,
            turn_id,
            invocation_id,
            output,
            presentation,
            is_error,
            call_index,
            ..
        } => {
            let binding = entity_binding("tool", &[&invocation_id.0]);
            let prior = required_row(rows, &binding)?;
            let mut content = decode(&prior)?;
            let TranscriptContent::Tool {
                status,
                call_index: started_index,
                invocation_id: started_invocation,
                ..
            } = &mut content
            else {
                return Err(TranscriptProjectionError::Invalid("tool binding kind"));
            };
            if *started_index != *call_index
                || !matches!(status, TranscriptToolStatus::Running {})
                || started_invocation != invocation_id
            {
                return Err(TranscriptProjectionError::Invalid(
                    "tool completion identity",
                ));
            }
            let reference = source(meta.sequence_id, TranscriptContentSelector::ToolOutput {});
            let mut budget = PreviewBudget(2 * 1024);
            let output = match output {
                ToolOutput::Text { text } => budget.text(text, reference),
                _ => budget.json(output, reference)?,
            };
            *status = TranscriptToolStatus::Finished {
                is_error: *is_error,
                output,
                presentation: presentation.as_ref().map(|presentation| {
                    rw_types::transcript::TranscriptToolPresentation {
                        title: presentation.descriptor.title.clone(),
                        source: source(
                            meta.sequence_id,
                            TranscriptContentSelector::ToolPresentation {
                                invocation_id: invocation_id.clone(),
                            },
                        ),
                    }
                }),
            };
            Ok(Some(ProjectedRow {
                prior: Some(prior),
                content,
                agent_turn: Some(turn_number(turn_id)?),
                binding: None,
            }))
        }
        EngineEvent::ToolDiffReady {
            meta,
            turn_id,
            invocation_id,
            diff,
            ..
        } => {
            let prior = required_row(rows, &entity_binding("tool", &[&invocation_id.0]))?;
            let mut content = decode(&prior)?;
            let TranscriptContent::Tool {
                diff: preview,
                invocation_id: started_invocation,
                ..
            } = &mut content
            else {
                return Err(TranscriptProjectionError::Invalid("tool binding kind"));
            };
            if started_invocation != invocation_id {
                return Err(TranscriptProjectionError::Invalid("tool diff identity"));
            }
            *preview = Some(PreviewBudget(512).json(
                diff,
                source(meta.sequence_id, TranscriptContentSelector::ToolDiff {}),
            )?);
            Ok(Some(ProjectedRow {
                prior: Some(prior),
                content,
                agent_turn: Some(turn_number(turn_id)?),
                binding: None,
            }))
        }
        _ => Ok(None),
    }
}

fn project_child(
    event: &EngineEvent,
    state: &TranscriptProjectionState,
    rows: &impl TranscriptRowLookup,
) -> Result<Option<ProjectedRow>, TranscriptProjectionError> {
    match event {
        EngineEvent::SubagentSpawned {
            meta,
            subagent_id,
            child_session_id,
            task,
        } => {
            if subagent_id.0.len() > 128
                || rw_types::SessionId::validate(&child_session_id.0).is_err()
            {
                return Err(TranscriptProjectionError::Invalid("child identity"));
            }
            let binding = entity_binding("subagent", &[&subagent_id.0]);
            if rows.bound_row(&binding)?.is_some() {
                return Err(TranscriptProjectionError::Invalid("reused child identity"));
            }
            Ok(Some(ProjectedRow {
                prior: None,
                agent_turn: state.active_turn,
                binding: Some(binding),
                content: TranscriptContent::Subagent {
                    subagent_id: subagent_id.clone(),
                    session_id: child_session_id.clone(),
                    task: PreviewBudget(512).text(
                        task,
                        source(meta.sequence_id, TranscriptContentSelector::SubagentTask {}),
                    ),
                    status: TranscriptSubagentStatus::Running {},
                },
            }))
        }
        EngineEvent::SubagentFinished {
            meta,
            subagent_id,
            result,
        } => {
            let prior = required_row(rows, &entity_binding("subagent", &[&subagent_id.0]))?;
            let mut content = decode(&prior)?;
            let TranscriptContent::Subagent {
                session_id, status, ..
            } = &mut content
            else {
                return Err(TranscriptProjectionError::Invalid("child binding kind"));
            };
            if session_id != &result.session_id
                || result.subagent_id != *subagent_id
                || !matches!(status, TranscriptSubagentStatus::Running {})
            {
                return Err(TranscriptProjectionError::Invalid(
                    "child completion identity",
                ));
            }
            *status = TranscriptSubagentStatus::Finished {
                status: result.status.clone(),
                touched_file_count: u32::try_from(result.touched_files.len())
                    .map_err(|_| TranscriptProjectionError::Invalid("child file count"))?,
                diff: result
                    .diff_artifact
                    .as_ref()
                    .map(|_| source(meta.sequence_id, TranscriptContentSelector::SubagentDiff {})),
                result: PreviewBudget(2 * 1024).text(
                    &result.final_text,
                    source(
                        meta.sequence_id,
                        TranscriptContentSelector::SubagentResult {},
                    ),
                ),
            };
            let agent_turn = prior.agent_turn;
            Ok(Some(ProjectedRow {
                prior: Some(prior),
                content,
                agent_turn,
                binding: None,
            }))
        }
        _ => Ok(None),
    }
}

fn required_row(
    rows: &impl TranscriptRowLookup,
    binding: &str,
) -> Result<TranscriptIndexRow, TranscriptProjectionError> {
    rows.bound_row(binding)?
        .ok_or(TranscriptProjectionError::Invalid(
            "missing lifecycle start",
        ))
}
fn decode(row: &TranscriptIndexRow) -> Result<TranscriptContent, TranscriptProjectionError> {
    Ok(serde_json::from_slice(&row.payload)?)
}
fn turn_number(turn: &TurnId) -> Result<u64, TranscriptProjectionError> {
    turn.0
        .parse()
        .map_err(|_| TranscriptProjectionError::Invalid("invalid agent turn"))
}
fn entity_binding(kind: &str, parts: &[&str]) -> String {
    let mut hash = blake3::Hasher::new_derive_key("rottweiler transcript entity v1");
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{kind}:{}", hash.finalize().to_hex())
}
fn source(sequence: SequenceId, selector: TranscriptContentSelector) -> TranscriptContentSource {
    TranscriptContentSource { sequence, selector }
}
fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

struct PreviewBudget(usize);
impl PreviewBudget {
    fn text(&mut self, text: &str, source: TranscriptContentSource) -> TranscriptBodyPreview {
        let retained = prefix(text, self.0);
        self.0 -= retained.len();
        TranscriptBodyPreview {
            text: retained.to_owned(),
            format: TranscriptPreviewFormat::Text,
            complete: retained.len() == text.len(),
            source,
        }
    }
    fn json(
        &mut self,
        value: &impl Serialize,
        source: TranscriptContentSource,
    ) -> Result<TranscriptBodyPreview, TranscriptProjectionError> {
        let mut writer = PreviewWriter {
            bytes: Vec::with_capacity(self.0),
            remaining: self.0,
            truncated: false,
        };
        let encoded = serde_json::to_writer(&mut writer, value);
        if !writer.truncated {
            encoded?;
        }
        while std::str::from_utf8(&writer.bytes).is_err() {
            writer.bytes.pop();
        }
        self.0 -= writer.bytes.len();
        let text = String::from_utf8(writer.bytes)
            .map_err(|_| TranscriptProjectionError::Invalid("preview encoding"))?;
        Ok(TranscriptBodyPreview {
            text,
            format: TranscriptPreviewFormat::Json,
            complete: !writer.truncated,
            source,
        })
    }
}
struct PreviewWriter {
    bytes: Vec<u8>,
    remaining: usize,
    truncated: bool,
}
impl Write for PreviewWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let retained = bytes.len().min(self.remaining);
        self.bytes.extend_from_slice(&bytes[..retained]);
        self.remaining -= retained;
        if retained < bytes.len() {
            self.truncated = true;
            return Err(io::Error::other("transcript preview full"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
