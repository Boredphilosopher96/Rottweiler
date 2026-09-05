//! The bounded structured task list shared by tools, recovery, and clients.
mod decode;
use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use ts_rs::TS;

pub const MAX_TODO_ITEMS: usize = 128;
pub const MAX_TODO_ID_BYTES: usize = 256;
pub const MAX_TODO_CONTENT_BYTES: usize = 4_096;
pub const MAX_TODO_TOTAL_BYTES: usize = 64 * 1_024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    #[schemars(length(min = 1, max = MAX_TODO_ID_BYTES), regex(pattern = r"[^\u0009-\u000D\u0020\u0085\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000]"), extend("x-rw-max-utf8-bytes" = MAX_TODO_ID_BYTES))]
    pub id: String,
    #[schemars(length(min = 1, max = MAX_TODO_CONTENT_BYTES), regex(pattern = r"[^\u0009-\u000D\u0020\u0085\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000]"), extend("x-rw-max-utf8-bytes" = MAX_TODO_CONTENT_BYTES))]
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Clone, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
#[schemars(extend("x-rw-item-budget" = {
    "array": "items", "identity": "id", "fields": ["id", "content"],
    "maxUtf8Bytes": MAX_TODO_TOTAL_BYTES
}))]
pub struct TodoSnapshot {
    #[schemars(length(max = MAX_TODO_ITEMS))]
    pub items: Vec<TodoItem>,
}

/// Exact source prefix applied to a complete task-list read.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct TodoReadSnapshot {
    #[serde(deserialize_with = "Option::deserialize")]
    pub through: Option<crate::SequenceId>,
    pub snapshot: TodoSnapshot,
}

/// One bounded read either returns an exact snapshot or reports indexed progress.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub enum TodoReadResult {
    Ready {
        todos: TodoReadSnapshot,
    },
    CatchingUp {
        #[serde(deserialize_with = "Option::deserialize")]
        through: Option<crate::SequenceId>,
        #[serde(deserialize_with = "Option::deserialize")]
        target: Option<crate::SequenceId>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid task list: {0}")]
pub struct TodoError(pub &'static str);

impl TodoSnapshot {
    /// Validate item identity, explicit status, and aggregate retained text.
    ///
    /// # Errors
    /// Rejects duplicate/empty identities and exceeded bounds.
    pub fn validate(&self) -> Result<(), TodoError> {
        validate_items(&self.items)
    }
}

/// Validate a complete replacement before changing session state.
///
/// # Errors
/// Rejects duplicate/empty identities and exceeded item/text bounds.
pub fn validate_items(items: &[TodoItem]) -> Result<(), TodoError> {
    if items.len() > MAX_TODO_ITEMS {
        return Err(TodoError("item count exceeds limit"));
    }
    let mut ids = BTreeSet::new();
    let mut total = 0usize;
    for item in items {
        if item.id.trim().is_empty() || item.content.trim().is_empty() {
            return Err(TodoError("id and content must not be empty"));
        }
        if item.id.len() > MAX_TODO_ID_BYTES || item.content.len() > MAX_TODO_CONTENT_BYTES {
            return Err(TodoError("item text exceeds limit"));
        }
        if !ids.insert(&item.id) {
            return Err(TodoError("duplicate item identity"));
        }
        total += item.id.len() + item.content.len();
        if total > MAX_TODO_TOTAL_BYTES {
            return Err(TodoError("aggregate text exceeds limit"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX_TODO_CONTENT_BYTES, TodoItem, TodoSnapshot, TodoStatus};
    #[test]
    fn schema_carries_item_count_and_utf8_budget_without_parallel_client_constants() {
        let schema = schemars::schema_for!(TodoSnapshot);
        let value = serde_json::to_value(schema).unwrap_or_else(|error| panic!("schema: {error}"));
        assert_eq!(value["properties"]["items"]["maxItems"], 128);
        assert_eq!(value["x-rw-item-budget"]["maxUtf8Bytes"], 65_536);
        assert!(value["properties"].get("count").is_none());
    }

    #[test]
    fn snapshot_requires_explicit_status_and_bounded_content() {
        assert!(
            serde_json::from_value::<TodoItem>(serde_json::json!({"id":"a","content":"task"}))
                .is_err()
        );
        let mut snapshot = TodoSnapshot {
            items: vec![TodoItem {
                id: "a".into(),
                content: "task".into(),
                status: TodoStatus::Pending,
            }],
        };
        assert!(snapshot.validate().is_ok());
        snapshot.items[0].content = "x".repeat(MAX_TODO_CONTENT_BYTES + 1);
        assert!(snapshot.validate().is_err());
    }
}
