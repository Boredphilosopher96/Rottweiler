//! The bounded structured task list shared by tools, recovery, and clients.
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(
    Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation,
)]
#[serde(deny_unknown_fields)]
pub struct TodoSnapshot {
    pub items: Vec<TodoItem>,
    pub count: usize,
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
    /// Rejects count mismatch, duplicate/empty identities, and exceeded bounds.
    pub fn validate(&self) -> Result<(), TodoError> {
        if self.count != self.items.len() {
            return Err(TodoError("item count mismatch"));
        }
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
    fn snapshot_requires_explicit_status_and_matching_count() {
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
            count: 1,
        };
        assert!(snapshot.validate().is_ok());
        snapshot.count = 0;
        assert!(snapshot.validate().is_err());
        snapshot.count = 1;
        snapshot.items[0].content = "x".repeat(MAX_TODO_CONTENT_BYTES + 1);
        assert!(snapshot.validate().is_err());
    }
}
