//! Immutable context identities within one canonical session journal.
use crate::{ContextItemId, SequenceId};
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A block inside one authoritative conversation source event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ContextBlockId {
    pub sequence: SequenceId,
    pub block_index: u32,
}
impl ContextBlockId {
    /// Stable key for bounded in-memory projection maps.
    #[must_use]
    pub fn key(self) -> String {
        format!("{}:{}", self.sequence.0, self.block_index)
    }
    #[must_use]
    pub fn item_id(self) -> ContextItemId {
        ContextItemId(format!("tool_result:{}", self.key()))
    }
}
/// Context selection is attached to the immutable event, never a reused position.
#[must_use]
pub fn conversation_item(sequence: SequenceId) -> ContextItemId {
    ContextItemId(format!("conversation:{}", sequence.0))
}

#[cfg(test)]
mod tests {
    use super::ContextBlockId;
    #[test]
    fn block_source_requires_exact_named_identity() {
        for value in [
            serde_json::json!({"sequence":"4"}),
            serde_json::json!({"sequence":"4", "block_index":0, "tool_call_id":"alias"}),
            serde_json::json!({"sequence":4, "block_index":0}),
            serde_json::json!({"sequence":"4", "block_index":-1}),
        ] {
            assert!(serde_json::from_value::<ContextBlockId>(value).is_err());
        }
        let source: ContextBlockId =
            serde_json::from_value(serde_json::json!({"sequence":"4", "block_index":2}))
                .expect("source");
        assert_eq!(source.item_id().0, "tool_result:4:2");
    }
}
