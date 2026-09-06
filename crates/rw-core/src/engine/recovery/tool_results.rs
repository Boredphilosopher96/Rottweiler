//! Ordered source references retain the same logical admission as an embedded tool turn.
use super::{
    RecoveryError,
    input::{EventSource, read_source},
};
use rw_store::session::journal::{MAX_JOURNAL_APPEND_BYTES, MAX_JOURNAL_DECODE_BYTES};
use rw_types::{
    Block, EngineEvent, EventMeta, Role, Turn, TurnMeta, allocation::PrepareAllocation,
    conversation_input::ToolResultReference,
};
use serde::Serialize;

pub(super) fn resolve(
    source: EventSource<'_>,
    meta: &EventMeta,
    agent_turn: u64,
    results: &[ToolResultReference],
    logical: &rw_types::tool_result_admission::ToolResultAdmission,
) -> Result<Turn, RecoveryError> {
    validate_admission(meta, agent_turn, logical)?;
    if results.is_empty() || results.len() > rw_types::tool_admission::MAX_PENDING_TOOL_INVOCATIONS
    {
        return Err(RecoveryError::Limit("tool result reference count"));
    }
    if results.iter().enumerate().any(|(index, result)| {
        results[..index]
            .iter()
            .any(|prior| prior.invocation_id == result.invocation_id)
    }) {
        return Err(RecoveryError::Invalid("duplicate tool result invocation"));
    }
    let mut turn = Turn {
        role: Role::Tool,
        blocks: Vec::with_capacity(results.len()),
        meta: TurnMeta::default(),
    };
    let mut encoded = 0_u64;
    let mut retained = turn
        .prepared_bytes()
        .ok_or(RecoveryError::Limit("tool result allocation"))?;
    for result in results {
        let EngineEvent::ToolCallFinished {
            turn_id,
            tool_call_id,
            invocation_id,
            output,
            is_error,
            ..
        } = read_source(source, result.finished_source, meta)?
        else {
            return Err(RecoveryError::Invalid(
                "tool result reference is not a completion",
            ));
        };
        if turn_id.0 != agent_turn.to_string() || invocation_id != result.invocation_id {
            return Err(RecoveryError::Invalid("tool result completion identity"));
        }
        let block = Block::ToolResult {
            id: tool_call_id,
            output,
            is_error,
        };
        encoded = encoded
            .checked_add(super::encoding::serialized_size(&block)?)
            .ok_or(RecoveryError::Limit("logical tool result encoding"))?;
        retained = retained
            .checked_add(
                block
                    .prepared_bytes()
                    .ok_or(RecoveryError::Limit("tool result allocation"))?,
            )
            .ok_or(RecoveryError::Limit("logical tool result allocation"))?;
        // A reference cannot multiply individually legal sources into an oversized IR.
        // The current source decoder owns at most 64 MiB beside this retained prefix.
        if encoded > MAX_JOURNAL_APPEND_BYTES as u64
            || retained > super::MAX_MATERIALIZED_HISTORY_DECODE_BYTES as usize
        {
            return Err(RecoveryError::Limit("logical tool result admission"));
        }
        turn.blocks.push(block);
    }
    if rw_types::tool_result_admission::ToolResultAdmission::measure(&turn)? != *logical {
        return Err(RecoveryError::Invalid(
            "tool result logical admission differs from its sources",
        ));
    }
    Ok(turn)
}

/// Check actual event-envelope overhead before the selector is published.
/// Body profiling happens in the turn worker; this checks only bounded metadata.
pub(in crate::engine) fn validate_admission(
    meta: &EventMeta,
    agent_turn: u64,
    logical: &rw_types::tool_result_admission::ToolResultAdmission,
) -> Result<(), RecoveryError> {
    use rw_types::json_structure::{JsonStructure, JsonStructureLimits, preflight_json};
    let event = LogicalCommit::ConversationTurnCommitted {
        meta,
        agent_turn: agent_turn.to_string(),
        turn: (),
    };
    let envelope = rw_store::session::EventEnvelope {
        schema_version: rw_store::session::SESSION_EVENT_SCHEMA_VERSION,
        sequence: meta.sequence_id,
        event,
    };
    let bytes = super::encoding::encode(&envelope, MAX_JOURNAL_APPEND_BYTES)?;
    let header = preflight_json(
        &bytes,
        JsonStructureLimits {
            max_encoded_bytes: MAX_JOURNAL_APPEND_BYTES,
            max_nodes: 65_536,
            max_string_bytes: MAX_JOURNAL_APPEND_BYTES,
            max_depth: 64,
        },
    )?;
    let encoded = (bytes.len() as u64)
        .checked_sub(4)
        .and_then(|bytes| bytes.checked_add(logical.encoded_bytes))
        .and_then(|bytes| bytes.checked_add(1));
    let nodes = header
        .nodes
        .checked_sub(1)
        .and_then(|nodes| nodes.checked_add(logical.nodes as usize));
    let strings = usize::try_from(logical.string_bytes)
        .ok()
        .and_then(|bytes| header.string_bytes.checked_add(bytes));
    let shape = nodes
        .zip(strings)
        .map(|(nodes, string_bytes)| JsonStructure {
            nodes,
            string_bytes,
            depth: header.depth.max(logical.depth as usize + 2),
            ..JsonStructure::default()
        })
        .ok_or(RecoveryError::Limit(
            "tool result logical admission overflow",
        ))?;
    if encoded.is_none_or(|bytes| bytes > MAX_JOURNAL_APPEND_BYTES as u64)
        || shape.nodes > 65_536
        || shape.string_bytes > MAX_JOURNAL_APPEND_BYTES
        || shape.depth > 64
        || shape
            .decode_bytes::<EngineEvent>()
            .is_none_or(|bytes| bytes > MAX_JOURNAL_DECODE_BYTES)
    {
        return Err(RecoveryError::Limit(
            "logical tool result journal admission",
        ));
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LogicalCommit<'a> {
    ConversationTurnCommitted {
        meta: &'a EventMeta,
        agent_turn: String,
        turn: (),
    },
}
