use async_trait::async_trait;
use rw_core::{SessionCommandAction, SessionCommandContext, SessionCommandOutput};
use rw_ext::{CommandExecutionError, CommandHandler, CommandInvocation};
use rw_store::workflow::WorkflowRunStore;
use rw_types::workflow::{WorkflowRunId, WorkflowRunState, WorkflowTaskOutcome, WorkflowTaskState};
use std::{fmt::Write as _, path::PathBuf};

pub(super) struct WorkflowStatusCommand {
    pub(super) storage_root: PathBuf,
}

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for WorkflowStatusCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let run_id = WorkflowRunId::parse(invocation.arguments().trim().to_owned())
            .map_err(|error| CommandExecutionError::new("invalid_workflow_run", error))?;
        let root = self.storage_root.clone();
        let parent = context.session_id().clone();
        let state = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            WorkflowRunStore::snapshot(&root, &run_id, &parent)
        })
        .await
        .map_err(|error| CommandExecutionError::new("workflow_status", error.to_string()))?
        .map_err(|error| CommandExecutionError::new("workflow_status", error.to_string()))?;
        Ok(SessionCommandOutput {
            message: summary(&state),
            action: SessionCommandAction::None,
        })
    }
}

fn summary(state: &WorkflowRunState) -> String {
    let mut text = format!(
        "workflow `{}` run {}",
        state.workflow,
        state.run_id.as_str()
    );
    for (name, task) in &state.tasks {
        let status = match task {
            WorkflowTaskState::Pending => "pending",
            WorkflowTaskState::Started { .. } => "started; completion not yet recorded",
            WorkflowTaskState::Settled {
                outcome: WorkflowTaskOutcome::Completed { .. },
            } => "completed",
            WorkflowTaskState::Settled {
                outcome: WorkflowTaskOutcome::Failed { .. },
            } => "failed",
            WorkflowTaskState::Settled {
                outcome: WorkflowTaskOutcome::Skipped,
            } => "skipped",
        };
        let _ = write!(&mut text, "\n{name}: {status}");
    }
    text
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use rw_ext::{CommandDescriptor, CommandRegistry};
    use rw_types::workflow::TaskId;
    #[tokio::test]
    async fn registered_status_reads_an_active_writer_and_enforces_parent_identity() {
        let root = tempfile::tempdir().expect("root");
        let mut context = SessionCommandContext::default();
        let run_id = WorkflowRunId::parse("a".repeat(32)).expect("id");
        let mut state = WorkflowRunState {
            run_id: run_id.clone(),
            parent_session_id: context.session_id().clone(),
            workflow: "build".to_owned(),
            definition_digest: "b".repeat(64),
            tasks: [("plan".to_owned(), WorkflowTaskState::Pending)].into(),
        };
        let mut writer = WorkflowRunStore::open(root.path(), state.clone()).expect("writer");
        writer
            .claim(&[TaskId {
                run_id: run_id.clone(),
                step_id: "plan".to_owned(),
            }])
            .expect("claim");
        let mut registry = CommandRegistry::new();
        registry
            .register(
                CommandDescriptor::new("workflow-status", "status"),
                WorkflowStatusCommand {
                    storage_root: root.path().to_owned(),
                },
            )
            .expect("registration");
        let output = registry
            .dispatch_line(
                &mut context,
                &format!("/workflow-status {}", run_id.as_str()),
            )
            .await
            .expect("status");
        assert!(
            output
                .message
                .contains("plan: started; completion not yet recorded")
        );
        assert_eq!(output.action, SessionCommandAction::None);
        state.run_id = WorkflowRunId::parse("c".repeat(32)).expect("foreign id");
        state.parent_session_id = rw_types::SessionId("foreign".to_owned());
        let _foreign = WorkflowRunStore::open(root.path(), state.clone()).expect("foreign writer");
        assert!(
            registry
                .dispatch_line(
                    &mut context,
                    &format!("/workflow-status {}", state.run_id.as_str())
                )
                .await
                .is_err()
        );
    }
}
