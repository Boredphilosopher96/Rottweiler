//! Task mutations are serialized and durably acknowledged by the session owner.
use crate::invocation_effects::{InvocationEffect, InvocationEffects};
use crate::registry::{input_schema, parse_input};
use crate::{
    CancellationToken, CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError,
    ToolLimits, ToolResult,
};
use async_trait::async_trait;
use rw_types::todo::{MAX_TODO_ITEMS, MAX_TODO_TOTAL_BYTES, TodoSnapshot};
pub use rw_types::todo::{TodoItem, TodoStatus};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TodoAction {
    List {},
    Replace { items: Vec<TodoItem> },
    Upsert { item: TodoItem },
    Remove { id: String },
    Clear {},
}
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TodoInput(pub TodoAction);

#[derive(Clone, Copy, Debug)]
pub struct TodoAdmission {
    pub max_items: usize,
    pub max_state_bytes: usize,
    pub max_result_bytes: usize,
}

/// Bound to one session invocation; successful mutation replies prove durability.
#[async_trait]
pub trait TodoStateStore: Send + Sync {
    async fn transact(
        &self,
        action: TodoAction,
        admission: TodoAdmission,
        cancellation: CancellationToken,
    ) -> Result<ToolResult, ToolError>;
    async fn settle_effects(&self) -> Result<(), ToolError>;
}

struct TodoEffect(Arc<dyn TodoStateStore>);
#[async_trait]
impl InvocationEffect for TodoEffect {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        self.0.settle_effects().await
    }
}

pub struct TodoTool {
    max_items: usize,
    max_bytes: usize,
    operations: Arc<InvocationEffects>,
}
impl TodoTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            max_items: limits.max_search_results.min(MAX_TODO_ITEMS),
            max_bytes: limits.max_result_bytes.min(MAX_TODO_TOTAL_BYTES),
            operations: Arc::new(InvocationEffects::default()),
        }
    }
}
#[async_trait]
impl Tool for TodoTool {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        self.operations.settle().await
    }
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "todo".into(),
            description: "List or update the current session's structured task list.".into(),
            input_schema: input_schema::<TodoInput>(),
            capabilities: CapabilityManifest::default(),
        }
    }
    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let TodoInput(action) = parse_input(input)?;
        let store = context.todo_store().ok_or_else(|| {
            ToolError::InvalidInput("todo requires an authoritative session binding".into())
        })?;
        let operation = self.operations.begin(
            Arc::new(TodoEffect(Arc::clone(store))),
            context.cancellation.clone(),
        )?;
        let result = store
            .transact(
                action,
                TodoAdmission {
                    max_items: self.max_items,
                    max_state_bytes: self.max_bytes,
                    max_result_bytes: context.result_limit_bytes(),
                },
                context.cancellation.clone(),
            )
            .await;
        operation.finish().await?;
        result
    }
}

/// Validate complete next state and its result before the actor admits a mutation.
///
/// # Errors
/// Rejects malformed tasks or state/result allocations outside invocation limits.
pub fn prepare_todo_update(
    current: &TodoSnapshot,
    action: TodoAction,
    admission: TodoAdmission,
) -> Result<(TodoSnapshot, ToolResult, bool), ToolError> {
    let mut next = current.clone();
    match action {
        TodoAction::List {} => {}
        TodoAction::Replace { items } => next.items = items,
        TodoAction::Upsert { item } => {
            if let Some(existing) = next
                .items
                .iter_mut()
                .find(|existing| existing.id == item.id)
            {
                *existing = item;
            } else {
                next.items.push(item);
            }
        }
        TodoAction::Remove { id } => next.items.retain(|item| item.id != id),
        TodoAction::Clear {} => next.items.clear(),
    }
    next.validate()
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    if next.items.len() > admission.max_items.min(MAX_TODO_ITEMS) {
        return Err(ToolError::InvalidInput(
            "task item admission exceeded".into(),
        ));
    }
    if next
        .items
        .iter()
        .map(|item| item.id.len() + item.content.len())
        .sum::<usize>()
        > admission.max_state_bytes.min(MAX_TODO_TOTAL_BYTES)
    {
        return Err(ToolError::SizeLimit {
            limit: admission.max_state_bytes.min(MAX_TODO_TOTAL_BYTES),
        });
    }
    let text = next
        .items
        .iter()
        .map(|item| format!("[{:?}] {}: {}", item.status, item.id, item.content))
        .collect::<Vec<_>>()
        .join("\n");
    let result = ToolResult::new(
        text,
        serde_json::to_value(&next).map_err(|error| ToolError::Output(error.to_string()))?,
    );
    if serde_json::to_vec(&result)
        .map_err(|error| ToolError::Output(error.to_string()))?
        .len()
        > admission.max_result_bytes
    {
        return Err(ToolError::SizeLimit {
            limit: admission.max_result_bytes,
        });
    }
    let changed = &next != current;
    Ok((next, result, changed))
}

#[cfg(test)]
mod tests;
