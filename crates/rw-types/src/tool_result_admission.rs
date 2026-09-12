//! Measured logical provider IR accompanying a source-only tool-result commit.
use crate::{
    Block, Role, Turn, TurnMeta,
    json_encoding::JsonWriter,
    json_structure::{JsonStructureLimits, preflight_json},
};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Maximum encoded bytes of the expanded logical tool IR, before envelope overhead.
pub const MAX_TOOL_RESULT_IR_BYTES: usize = 16 * 1024 * 1024;
/// Aggregate retained selector metadata; result bodies are owned by their source records.
pub const MAX_TOOL_RESULT_REFERENCE_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ToolResultAdmission {
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub encoded_bytes: u64,
    pub nodes: u32,
    #[serde(with = "crate::protocol::decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub string_bytes: u64,
    pub depth: u32,
}
impl ToolResultAdmission {
    /// Measure without retaining an encoded copy beyond this call.
    /// # Errors
    /// Rejects invalid Tool IR or encoding/structural admission overflow.
    pub fn measure(turn: &Turn) -> Result<Self, serde_json::Error> {
        use serde::de::Error as _;
        if turn.role != Role::Tool
            || turn.meta != TurnMeta::default()
            || turn.blocks.is_empty()
            || turn.blocks.len() > crate::tool_admission::MAX_PENDING_TOOL_INVOCATIONS
            || turn
                .blocks
                .iter()
                .any(|block| !matches!(block, Block::ToolResult { .. }))
        {
            return Err(serde_json::Error::custom(
                "tool result IR requires only ordered result blocks",
            ));
        }
        Self::measure_shape(turn)
    }

    /// Combine singleton result profiles without re-encoding their bodies.
    /// # Errors
    /// Rejects an empty/oversized batch, invalid component profile, or arithmetic overflow.
    pub fn combine(parts: &[Self]) -> Result<Self, serde_json::Error> {
        use serde::de::Error as _;
        if parts.is_empty() || parts.len() > crate::tool_admission::MAX_PENDING_TOOL_INVOCATIONS {
            return Err(serde_json::Error::custom("tool result profile count"));
        }
        let empty = Self::measure_shape(&Turn {
            role: Role::Tool,
            blocks: Vec::new(),
            meta: TurnMeta::default(),
        })?;
        let mut combined = empty.clone();
        for part in parts {
            combined.encoded_bytes = part
                .encoded_bytes
                .checked_sub(empty.encoded_bytes)
                .and_then(|n| combined.encoded_bytes.checked_add(n))
                .ok_or_else(|| serde_json::Error::custom("tool result encoding overflow"))?;
            combined.nodes = part
                .nodes
                .checked_sub(empty.nodes)
                .and_then(|n| combined.nodes.checked_add(n))
                .ok_or_else(|| serde_json::Error::custom("tool result structure overflow"))?;
            combined.string_bytes = part
                .string_bytes
                .checked_sub(empty.string_bytes)
                .and_then(|n| combined.string_bytes.checked_add(n))
                .ok_or_else(|| serde_json::Error::custom("tool result string overflow"))?;
            combined.depth = combined.depth.max(part.depth);
        }
        combined.encoded_bytes = combined
            .encoded_bytes
            .checked_add(parts.len() as u64 - 1)
            .ok_or_else(|| serde_json::Error::custom("tool result separator overflow"))?;
        Ok(combined)
    }

    fn measure_shape(turn: &Turn) -> Result<Self, serde_json::Error> {
        use serde::de::Error as _;
        let mut bytes = Vec::new();
        JsonWriter::buffer(&mut bytes, MAX_TOOL_RESULT_IR_BYTES, 0)
            .map_err(serde_json::Error::io)?
            .serialize(turn)?;
        let shape = preflight_json(
            &bytes,
            JsonStructureLimits {
                max_encoded_bytes: MAX_TOOL_RESULT_IR_BYTES,
                max_nodes: 65_536,
                max_string_bytes: MAX_TOOL_RESULT_IR_BYTES,
                max_depth: 62,
            },
        )?;
        Ok(Self {
            encoded_bytes: bytes.len() as u64,
            nodes: u32::try_from(shape.nodes).map_err(serde_json::Error::custom)?,
            string_bytes: shape.string_bytes as u64,
            depth: u32::try_from(shape.depth).map_err(serde_json::Error::custom)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    #[test]
    fn singleton_profiles_combine_to_exact_full_body_shape() {
        let mut whole = Turn {
            role: Role::Tool,
            blocks: Vec::new(),
            meta: TurnMeta::default(),
        };
        let mut profiles = Vec::new();
        for (index, text) in ["plain", "escaped\n\0\"é", "nested"]
            .into_iter()
            .enumerate()
        {
            let block = Block::ToolResult {
                id: crate::ToolCallId(format!("call-{index}")),
                output: crate::ToolOutput::Structured {
                    value: serde_json::json!({"a":[text,{"b":true}]}),
                },
                is_error: index == 1,
            };
            profiles.push(
                ToolResultAdmission::measure(&Turn {
                    role: Role::Tool,
                    blocks: vec![block.clone()],
                    meta: TurnMeta::default(),
                })
                .expect("singleton"),
            );
            whole.blocks.push(block);
            assert_eq!(
                ToolResultAdmission::combine(&profiles).expect("combined"),
                ToolResultAdmission::measure(&whole).expect("whole")
            );
        }
    }
}
