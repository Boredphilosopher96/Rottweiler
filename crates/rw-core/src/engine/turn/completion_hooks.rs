use super::hooks::{
    dispatch_hook, dispatch_hook_effect, mark_unsettled, permission_hook_override,
    report_hook_failures,
};
use super::signals::TurnSignal;
use super::tool_requests::RedactingApprover;
use crate::engine::mutation_checkpoints::MutationCheckpointOutcome;
use crate::engine::session::SessionActorConfig;
use crate::engine::{AgentLoopError, AgentTurnStatus};
use crate::{PermissionApprover, PermissionOutcome, PermissionRequest};
use rw_ext::{HookDispatchResult, HookEffect, HookEvent};
use rw_tools::{CancellationToken, MutationScope};
use rw_types::hook_contract::{HookInput, HookPermissionInput, HookTurnInput};
use rw_types::{SessionMode, ToolCapability, ToolInvocationId};
use tokio::sync::mpsc;

/// Owns permission admission, workspace capture, and physical completion of
/// turn-end policies. The turn retains this owner through cancellation.
pub(super) struct CompletionHooks<'a> {
    pub(super) config: &'a SessionActorConfig,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) signals: &'a mpsc::UnboundedSender<TurnSignal>,
    pub(super) approver: &'a dyn PermissionApprover,
    pub(super) mode: SessionMode,
}

impl CompletionHooks<'_> {
    pub(super) async fn dispatch(
        &self,
        turn: u64,
        status: AgentTurnStatus,
    ) -> Result<HookDispatchResult, AgentLoopError> {
        let input = HookInput::TurnEnd(HookTurnInput {
            turn,
            status: status.into(),
        });
        if status != AgentTurnStatus::Completed {
            return dispatch_hook_effect(
                &self.config.hooks,
                input,
                HookEffect::ReadOnly,
                self.cancellation,
                self.signals,
            )
            .await;
        }
        let mut ids = Vec::new();
        let mut capabilities = vec![ToolCapability::WriteFilesystem];
        for registration in self
            .config
            .hooks
            .registrations(HookEvent::TurnEnd)
            .filter(|registration| registration.effect() == HookEffect::WorkspaceMutating)
        {
            ids.push(registration.id());
            for capability in registration.required_capabilities() {
                if !capabilities.contains(capability) {
                    capabilities.push(capability.clone());
                }
            }
        }
        if ids.is_empty() {
            return dispatch_hook(&self.config.hooks, input, self.cancellation, self.signals).await;
        }
        let operation = format!("turn-{turn}:completion-hooks");
        self.authorize(&operation, &ids, capabilities).await?;
        if self.cancellation.is_cancelled() {
            return Err(AgentLoopError::Extension(
                "completion hooks cancelled".to_owned(),
            ));
        }
        if self
            .config
            .tools
            .session_activity(&self.config.session_id)
            .is_some()
        {
            return Err(AgentLoopError::Extension(
                "completion hook workspace mutation is blocked while a background shell process is running".to_owned(),
            ));
        }
        let begin = self
            .config
            .checkpoints
            .begin(
                &self.config.session_id,
                turn,
                &operation,
                &MutationScope::OpaqueWorkspace,
            )
            .await;
        let checkpoint = match begin {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                self.settle_checkpoints().await?;
                return Err(error);
            }
        };
        let result =
            dispatch_hook(&self.config.hooks, input, self.cancellation, self.signals).await;
        if matches!(&result, Err(AgentLoopError::EffectsUnsettled(_))) {
            // A workspace outcome cannot be finalized without physical hook settlement.
            return result;
        }
        let outcome = if self.cancellation.is_cancelled() {
            MutationCheckpointOutcome::Cancelled
        } else if result.as_ref().is_ok_and(HookDispatchResult::completed) {
            MutationCheckpointOutcome::Completed
        } else {
            MutationCheckpointOutcome::Failed
        };
        let finished = self.config.checkpoints.finish(&checkpoint, outcome).await;
        let settled = self.settle_checkpoints().await;
        if let Err(error) = finished {
            mark_unsettled(self.signals, self.cancellation, error.to_string());
            return Err(AgentLoopError::EffectsUnsettled(error.to_string()));
        }
        settled?;
        result
    }

    async fn settle_checkpoints(&self) -> Result<(), AgentLoopError> {
        self.config
            .checkpoints
            .settle_effects()
            .await
            .map_err(|error| {
                mark_unsettled(self.signals, self.cancellation, error.to_string());
                AgentLoopError::EffectsUnsettled(error.to_string())
            })
    }

    async fn authorize(
        &self,
        operation: &str,
        ids: &[&str],
        capabilities: Vec<ToolCapability>,
    ) -> Result<(), AgentLoopError> {
        let request = PermissionRequest {
            id: operation.to_owned(),
            invocation_id: ToolInvocationId(operation.to_owned()),
            tool_name: "completion_hooks".to_owned(),
            arguments: serde_json::json!({"hooks": ids}),
            capabilities,
            approval_diff: None,
        };
        let policy = dispatch_hook(
            &self.config.hooks,
            HookInput::PermissionCheck(HookPermissionInput {
                id: request.id.clone(),
                name: request.tool_name.clone(),
                arguments: request.arguments.clone(),
                capabilities: request.capabilities.clone(),
            }),
            self.cancellation,
            self.signals,
        )
        .await?;
        report_hook_failures(
            HookEvent::PermissionCheck,
            policy.failures(),
            self.signals,
            self.config.secret_redactor.as_ref(),
        );
        let approver = RedactingApprover {
            inner: self.approver,
            redactor: self.config.secret_redactor.as_ref(),
        };
        match self
            .config
            .permissions
            .authorize_in_mode(
                request,
                &approver,
                permission_hook_override(policy.status(), policy.permission()),
                self.mode,
            )
            .await
        {
            PermissionOutcome::Allowed => Ok(()),
            PermissionOutcome::Denied | PermissionOutcome::RememberedApprovalUnavailable => Err(
                AgentLoopError::Extension("permission denied for completion hooks".to_owned()),
            ),
        }
    }
}
