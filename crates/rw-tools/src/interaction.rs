use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rw_types::{PlanArtifact, SessionId};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::registry::{
    CancellationToken, CapabilityManifest, Tool, ToolContext, ToolDescriptor, ToolError,
    ToolLimits, ToolResult, input_schema, parse_input,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskUserInput {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default = "default_true")]
    pub allow_free_text: bool,
}

const fn default_true() -> bool {
    true
}

/// Read-only control tool used by Plan mode to hand a structured artifact to
/// the engine. Core owns approval and durable state; this tool only validates
/// and returns the provider-neutral payload.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubmitPlanTool;

#[async_trait]
impl Tool for SubmitPlanTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "submit_plan".to_owned(),
            description: "Submit the complete implementation plan for explicit user approval."
                .to_owned(),
            input_schema: input_schema::<PlanArtifact>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    fn behavior(&self) -> crate::ToolBehavior {
        crate::ToolBehavior::PlanSubmission
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let artifact: PlanArtifact = parse_input(input)?;
        if artifact.title.trim().is_empty()
            || artifact.summary_md.trim().is_empty()
            || artifact.steps.is_empty()
            || artifact.steps.iter().any(|step| {
                step.description.trim().is_empty() || step.verification.trim().is_empty()
            })
        {
            return Err(ToolError::InvalidInput(
                "plan requires a title, summary, and at least one described/verifiable step"
                    .to_owned(),
            ));
        }
        let data = serde_json::to_value(&artifact)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        Ok(ToolResult::new("plan submitted for approval", data))
    }
}

/// Injected UI boundary. Headless callers can supply their own transport.
#[async_trait]
pub trait QuestionAsker: Send + Sync {
    async fn ask(
        &self,
        request: AskUserInput,
        cancellation: CancellationToken,
    ) -> Result<String, ToolError>;
}

#[derive(Clone)]
pub struct AskUserTool {
    asker: Arc<dyn QuestionAsker>,
    max_answer_bytes: usize,
}

impl AskUserTool {
    #[must_use]
    pub fn new(asker: Arc<dyn QuestionAsker>, limits: ToolLimits) -> Self {
        Self {
            asker,
            max_answer_bytes: limits.max_result_bytes,
        }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "ask_user".to_owned(),
            description: "Ask the user one focused question through the host UI boundary."
                .to_owned(),
            input_schema: input_schema::<AskUserInput>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    fn behavior(&self) -> crate::ToolBehavior {
        crate::ToolBehavior::UserInteraction
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let input: AskUserInput = parse_input(input)?;
        if input.question.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "question must not be empty".to_owned(),
            ));
        }
        if input.options.iter().any(|option| option.trim().is_empty()) {
            return Err(ToolError::InvalidInput(
                "options must not contain empty values".to_owned(),
            ));
        }
        let answer = tokio::select! {
            answer = context.question_asker().unwrap_or(&self.asker).ask(
                input,
                context.cancellation.clone(),
            ) => answer?,
            () = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
        };
        if answer.len() > self.max_answer_bytes {
            return Err(ToolError::SizeLimit {
                limit: self.max_answer_bytes,
            });
        }
        Ok(ToolResult::new(answer.clone(), json!({"answer": answer})))
    }
}

use rw_types::todo::{MAX_TODO_ITEMS, MAX_TODO_TOTAL_BYTES, TodoSnapshot};
pub use rw_types::todo::{TodoItem, TodoStatus};

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

pub struct TodoTool {
    items: RwLock<HashMap<String, Vec<TodoItem>>>,
    max_items: usize,
    max_bytes: usize,
}

impl TodoTool {
    #[must_use]
    pub fn new(limits: ToolLimits) -> Self {
        Self {
            items: RwLock::new(HashMap::new()),
            max_items: limits.max_search_results.min(MAX_TODO_ITEMS),
            max_bytes: limits.max_result_bytes.min(MAX_TODO_TOTAL_BYTES),
        }
    }

    /// Drop ephemeral todo state when a session actor closes.
    pub async fn clear_session(&self, session_id: &SessionId) {
        self.items.write().await.remove(&session_id.0);
    }
}

#[async_trait]
impl Tool for TodoTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "todo".to_owned(),
            description: "List or update the current session's structured task list.".to_owned(),
            input_schema: input_schema::<TodoInput>(),
            capabilities: CapabilityManifest::default(),
        }
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.check()?;
        let TodoInput(action) = parse_input(input)?;
        let session_key = context.session_id().map(|id| id.0.clone()).ok_or_else(|| {
            ToolError::InvalidInput("todo requires a session_id in ToolContext".to_owned())
        })?;
        let mut state = self.items.write().await;
        context.cancellation.check()?;
        let mut next = state.get(&session_key).cloned().unwrap_or_default();
        match action {
            TodoAction::List {} => {}
            TodoAction::Replace { items } => next = items,
            TodoAction::Upsert { item } => {
                if let Some(existing) = next.iter_mut().find(|existing| existing.id == item.id) {
                    *existing = item;
                } else {
                    next.push(item);
                }
            }
            TodoAction::Remove { id } => next.retain(|item| item.id != id),
            TodoAction::Clear {} => next.clear(),
        }
        validate_items(&next, self.max_items, self.max_bytes)?;
        let model_text = next
            .iter()
            .map(|item| format!("[{:?}] {}: {}", item.status, item.id, item.content))
            .collect::<Vec<_>>()
            .join("\n");
        let snapshot = TodoSnapshot {
            count: next.len(),
            items: next,
        };
        let result = ToolResult::new(
            model_text,
            serde_json::to_value(&snapshot)
                .map_err(|error| ToolError::Output(error.to_string()))?,
        );
        // Mutation is admitted only if the exact full snapshot survives the
        // central output limit. A successful todo never publishes truncated state.
        if serde_json::to_vec(&result)
            .map_err(|error| ToolError::Output(error.to_string()))?
            .len()
            > context.result_limit_bytes()
        {
            return Err(ToolError::SizeLimit {
                limit: context.result_limit_bytes(),
            });
        }
        context.cancellation.check()?;
        if snapshot.items.is_empty() {
            state.remove(&session_key);
        } else {
            state.insert(session_key, snapshot.items);
        }
        Ok(result)
    }
}

fn validate_items(items: &[TodoItem], max_items: usize, max_bytes: usize) -> Result<(), ToolError> {
    if items.len() > max_items {
        return Err(ToolError::InvalidInput(format!(
            "todo list exceeds the {max_items}-item limit"
        )));
    }
    rw_types::todo::validate_items(items)
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    let bytes = items
        .iter()
        .map(|item| item.id.len() + item.content.len())
        .sum::<usize>();
    if bytes > max_bytes {
        return Err(ToolError::SizeLimit { limit: max_bytes });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use tempfile::tempdir;

    use super::*;

    struct MockAsker;

    #[async_trait]
    impl QuestionAsker for MockAsker {
        async fn ask(
            &self,
            request: AskUserInput,
            _cancellation: CancellationToken,
        ) -> Result<String, ToolError> {
            assert_eq!(request.question, "Continue?");
            Ok("yes".to_owned())
        }
    }

    #[tokio::test]
    async fn ask_user_uses_the_injected_boundary() {
        let root = tempdir().expect("temp directory");
        let context = ToolContext::new(root.path()).expect("context");
        let result = AskUserTool::new(Arc::new(MockAsker), ToolLimits::default())
            .execute(
                &context,
                serde_json::json!({"question": "Continue?", "options": ["yes", "no"]}),
            )
            .await
            .expect("answer");
        assert_eq!(result.data["answer"], "yes");
    }

    #[tokio::test]
    async fn todo_rejects_duplicate_ids_without_mutating_state() {
        let root = tempdir().expect("temp directory");
        let context = ToolContext::new(root.path())
            .expect("context")
            .with_session_id(rw_types::SessionId("duplicate-test".to_owned()));
        let tool = TodoTool::new(ToolLimits::default());
        let error = tool
            .execute(
                &context,
                serde_json::json!({
                    "action": "replace",
                    "items": [
                        {"id": "a", "content": "one", "status": "pending"},
                        {"id": "a", "content": "two", "status": "pending"}
                    ]
                }),
            )
            .await
            .expect_err("duplicate id");
        assert!(matches!(error, ToolError::InvalidInput(_)));
        let result = tool
            .execute(&context, serde_json::json!({"action": "list"}))
            .await
            .expect("list");
        assert_eq!(result.data["count"], 0);
    }

    #[tokio::test]
    async fn todo_fails_closed_without_a_session_id() {
        let root = tempdir().expect("temp directory");
        let context = ToolContext::new(root.path()).expect("context");
        let result = TodoTool::new(ToolLimits::default())
            .execute(&context, serde_json::json!({"action": "list"}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn todo_rechecks_cancellation_after_waiting_for_the_state_lock() {
        let root = tempdir().expect("temp directory");
        let cancellation = CancellationToken::default();
        let context = ToolContext::new(root.path())
            .expect("context")
            .with_session_id(rw_types::SessionId("cancelled-todo".to_owned()))
            .with_cancellation(cancellation.clone());
        let tool = Arc::new(TodoTool::new(ToolLimits::default()));
        let lock = tool.items.write().await;
        let pending_tool = Arc::clone(&tool);
        let pending_context = context.clone();
        let pending = tokio::spawn(async move {
            pending_tool
                .execute(
                    &pending_context,
                    serde_json::json!({
                        "action": "replace",
                        "items": [{"id": "late", "content": "must not commit", "status": "pending"}]
                    }),
                )
                .await
        });
        tokio::task::yield_now().await;
        cancellation.cancel();
        drop(lock);
        assert!(matches!(
            pending.await.expect("todo task"),
            Err(ToolError::Cancelled)
        ));
        assert!(tool.items.read().await.is_empty());
    }

    #[tokio::test]
    async fn todo_rejects_missing_status_and_unrecoverable_output_before_mutating() {
        let root = tempdir().expect("workspace");
        let context = ToolContext::new(root.path())
            .expect("context")
            .with_session_id(SessionId("bounded-todo".into()));
        let tool = TodoTool::new(ToolLimits::default());
        assert!(
            tool.execute(
                &context,
                json!({"action":"upsert","item":{"id":"a","content":"task"}})
            )
            .await
            .is_err()
        );
        let tiny = context.with_result_limit(32);
        assert!(
            tool.execute(
                &tiny,
                json!({"action":"upsert","item":{"id":"a","content":"task","status":"pending"}})
            )
            .await
            .is_err()
        );
        assert!(tool.items.read().await.is_empty());
    }

    #[tokio::test]
    async fn todo_state_is_isolated_by_session() {
        let root = tempdir().expect("temp directory");
        let first = ToolContext::new(root.path())
            .expect("first context")
            .with_session_id(rw_types::SessionId("first".to_owned()));
        let second = ToolContext::new(root.path())
            .expect("second context")
            .with_session_id(rw_types::SessionId("second".to_owned()));
        let tool = TodoTool::new(ToolLimits::default());
        tool.execute(
            &first,
            serde_json::json!({
                "action": "upsert",
                "item": {"id": "a", "content": "only first", "status": "pending"}
            }),
        )
        .await
        .expect("upsert");
        let result = tool
            .execute(&second, serde_json::json!({"action": "list"}))
            .await
            .expect("second list");
        assert_eq!(result.data["count"], 0);
        tool.clear_session(&rw_types::SessionId("first".to_owned()))
            .await;
        let cleared = tool
            .execute(&first, serde_json::json!({"action": "list"}))
            .await
            .expect("cleared list");
        assert_eq!(cleared.data["count"], 0);
    }
}
