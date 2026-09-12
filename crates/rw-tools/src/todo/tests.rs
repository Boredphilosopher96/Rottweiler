#![cfg(test)]
#![allow(clippy::expect_used)]
use super::{
    TodoAction, TodoAdmission, TodoSnapshot, TodoStateStore, TodoTool, prepare_todo_update,
};
use crate::{CancellationToken, Tool, ToolContext, ToolError, ToolLimits, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default)]
struct FixtureStore(Mutex<TodoSnapshot>);
#[async_trait]
impl TodoStateStore for FixtureStore {
    async fn transact(
        &self,
        action: TodoAction,
        admission: TodoAdmission,
        cancellation: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let mut state = self.0.lock().await;
        cancellation.check()?;
        let (next, result, _) = prepare_todo_update(&state, action, admission)?;
        *state = next;
        Ok(result)
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }
}
#[tokio::test]
async fn task_binding_is_required_and_clones_do_not_share_session_state() {
    let workspace = tempfile::tempdir().expect("workspace");
    let context = ToolContext::new(workspace.path()).expect("context");
    let tool = TodoTool::new(ToolLimits::default());
    assert!(
        tool.execute(&context, json!({"action":"list"}))
            .await
            .is_err()
    );
    let first = context
        .clone()
        .with_todo_store(Arc::new(FixtureStore::default()));
    let second = context.with_todo_store(Arc::new(FixtureStore::default()));
    tool.execute(
        &first,
        json!({"action":"upsert","item":{"id":"a","content":"first","status":"pending"}}),
    )
    .await
    .expect("first state");
    assert!(
        tool.execute(&second, json!({"action":"list"}))
            .await
            .expect("second state")
            .data["items"]
            .as_array()
            .expect("items")
            .is_empty()
    );
}
#[tokio::test]
async fn malformed_or_unrepresentable_mutation_leaves_authoritative_state_unchanged() {
    let workspace = tempfile::tempdir().expect("workspace");
    let store = Arc::new(FixtureStore::default());
    let context = ToolContext::new(workspace.path())
        .expect("context")
        .with_todo_store(store.clone());
    let tool = TodoTool::new(ToolLimits::default());
    for input in [
        json!({"action":"upsert","item":{"id":"a","content":"missing status"}}),
        json!({"action":"replace","items":[{"id":"a","content":"one","status":"pending"},{"id":"a","content":"two","status":"pending"}]}),
    ] {
        assert!(tool.execute(&context, input).await.is_err());
    }
    assert!(
        tool.execute(
            &context.with_result_limit(32),
            json!({"action":"upsert","item":{"id":"a","content":"too large","status":"pending"}})
        )
        .await
        .is_err()
    );
    assert!(store.0.lock().await.items.is_empty());
}
#[tokio::test]
async fn cancellation_before_store_admission_does_not_mutate() {
    let workspace = tempfile::tempdir().expect("workspace");
    let store = Arc::new(FixtureStore::default());
    let cancellation = CancellationToken::default();
    let context = ToolContext::new(workspace.path())
        .expect("context")
        .with_todo_store(store.clone())
        .with_cancellation(cancellation.clone());
    let guard = store.0.lock().await;
    let tool = Arc::new(TodoTool::new(ToolLimits::default()));
    let task = tokio::spawn(async move { tool.execute(&context, json!({"action":"clear"})).await });
    tokio::task::yield_now().await;
    cancellation.cancel();
    drop(guard);
    assert!(matches!(
        task.await.expect("join"),
        Err(ToolError::Cancelled)
    ));
    assert!(store.0.lock().await.items.is_empty());
}
