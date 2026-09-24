use super::native_model_generations::NativeModelGenerations;
use super::workspace_roots::RuntimeWorkspaceRootController;
use async_trait::async_trait;
use miette::Result;
use rw_core::AgentLoopError;
use rw_core::HostError;
use rw_core::HostSubagentService;
use rw_core::PermissionGate;
use rw_core::SessionActorConfig;
use rw_core::SubagentOrchestrator;
use rw_core::SubagentSessionFactory;
use rw_tools::CancellationToken;
use rw_tools::ToolRegistry;
use rw_tools::WorktreeLeaseRecord;
use rw_types::SessionId;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) struct ChildActorTemplate {
    pub(super) budget_session_id: SessionId,
    pub(super) provider_admission: Arc<dyn rw_core::provider_admission::ProviderAdmission>,
    pub(super) storage_root: PathBuf,
    pub(super) model: std::sync::Weak<NativeModelGenerations>,
    pub(super) permissions: Arc<PermissionGate>,
    pub(super) secret_redactor: Arc<dyn rw_core::SecretRedactor>,
    pub(super) lease_runtime: Arc<RuntimeWorkspaceRootController>,
    pub(super) max_turns: usize,
}

pub(super) struct RuntimeSubagentSessionFactory {
    pub(super) shared: Arc<dyn SubagentSessionFactory>,
    pub(super) isolated: Option<Arc<dyn SubagentSessionFactory>>,
    pub(super) isolation_error: String,
}

#[async_trait]
impl SubagentSessionFactory for RuntimeSubagentSessionFactory {
    async fn create(
        &self,
        launch: rw_core::SubagentLaunch,
    ) -> std::result::Result<Arc<dyn rw_core::SubagentSession>, rw_core::OrchestrationError> {
        if launch.request.isolation == rw_types::SubagentIsolation::Shared {
            return self.shared.create(launch).await;
        }
        let isolated = self.isolated.as_ref().ok_or_else(|| {
            rw_core::OrchestrationError::InvalidRequest(format!(
                "worktree isolation is unavailable for this workspace: {}",
                self.isolation_error
            ))
        })?;
        isolated.create(launch).await
    }

    async fn rebind(
        &self,
        session_id: &SessionId,
        workspace_root: Option<&Path>,
        worktree: Option<&WorktreeLeaseRecord>,
        allowed_tools: Option<Arc<ToolRegistry>>,
        policy: &rw_core::SubagentRecoveryPolicy,
    ) -> std::result::Result<Option<Arc<dyn rw_core::SubagentSession>>, rw_core::OrchestrationError>
    {
        if worktree.is_none() {
            return self
                .shared
                .rebind(session_id, workspace_root, worktree, allowed_tools, policy)
                .await;
        }
        let isolated = self.isolated.as_ref().ok_or_else(|| {
            rw_core::OrchestrationError::InvalidRequest(format!(
                "persisted worktree cannot rebind: {}",
                self.isolation_error
            ))
        })?;
        isolated
            .rebind(session_id, workspace_root, worktree, allowed_tools, policy)
            .await
    }
}

impl ChildActorTemplate {
    pub(super) async fn recovery_controls(
        &self,
        session: &SessionId,
        workspace: &Path,
    ) -> std::result::Result<rw_core::DormantChildControls, AgentLoopError> {
        self.lease_runtime
            .dormant_controls(session, workspace)
            .await
    }

    pub(super) async fn config(
        &self,
        launch: &rw_core::SubagentLaunch,
    ) -> std::result::Result<SessionActorConfig, AgentLoopError> {
        self.lease_runtime
            .child_config(
                &self.storage_root,
                &self.budget_session_id,
                &launch.handle.session_id,
                &launch.workspace_root,
                &launch.request.model,
                NativeModelGenerations::capture_child(
                    &self.model,
                    &launch.workspace_root,
                    &launch.request.model,
                )?,
                Arc::clone(&self.secret_redactor),
                self.permissions.as_ref(),
                self.max_turns,
                Arc::clone(&self.provider_admission),
            )
            .await
    }

    pub(super) async fn rebind_config(
        &self,
        session_id: &SessionId,
        workspace_root: &Path,
        policy: &rw_core::SubagentRecoveryPolicy,
    ) -> std::result::Result<SessionActorConfig, AgentLoopError> {
        self.lease_runtime
            .child_config(
                &self.storage_root,
                &self.budget_session_id,
                session_id,
                workspace_root,
                &policy.model_alias,
                NativeModelGenerations::capture_child(
                    &self.model,
                    workspace_root,
                    &policy.model_alias,
                )?,
                Arc::clone(&self.secret_redactor),
                self.permissions.as_ref(),
                self.max_turns,
                Arc::clone(&self.provider_admission),
            )
            .await
    }
}

pub(super) struct HostedSubagentController {
    pub(super) journals: Arc<crate::journal_service::JournalService>,
    pub(super) parent: rw_core::SessionHandle,
    pub(super) orchestrator: SubagentOrchestrator,
}

impl HostedSubagentController {
    pub(super) fn ensure_parent(&self, parent_session_id: &SessionId) -> Result<(), HostError> {
        if self.parent.session_id() == parent_session_id {
            Ok(())
        } else {
            Err(HostError::Protocol(
                "child-agent parent session does not match this controller".to_owned(),
            ))
        }
    }
}

#[async_trait]
impl HostSubagentService for HostedSubagentController {
    async fn family_controls(
        &self,
        root: &SessionId,
    ) -> Result<rw_types::family_controls::FamilyControlsSnapshot, HostError> {
        self.ensure_parent(root)?;
        self.orchestrator
            .family_controls(root)
            .map_err(|error| HostError::Protocol(error.to_string()))
    }
    async fn child_read_scope(
        &self,
        root: &SessionId,
        target: &rw_types::family_controls::ChildControlTarget,
    ) -> Result<rw_types::family_controls::ChildReadScopeResult, HostError> {
        self.ensure_parent(root)?;
        self.orchestrator
            .control_child(root, target)
            .map_err(|error| HostError::Protocol(error.to_string()))?;
        let result = super::family_read_scope::resolve(
            Arc::clone(&self.journals),
            root.clone(),
            target.clone(),
        )
        .await?;
        self.orchestrator
            .control_child(root, target)
            .map_err(|error| HostError::Protocol(error.to_string()))?;
        Ok(result)
    }
    async fn child_state(
        &self,
        root: &SessionId,
        target: &rw_types::family_controls::ChildControlTarget,
    ) -> Result<rw_types::session_state::SessionStateSnapshot, HostError> {
        self.ensure_parent(root)?;
        let child = self
            .orchestrator
            .control_child(root, target)
            .map_err(|error| HostError::Protocol(error.to_string()))?;
        child
            .child_state()
            .await
            .map_err(|error| HostError::Protocol(error.to_string()))
    }
    async fn child_controls(
        &self,
        root: &SessionId,
        target: &rw_types::family_controls::ChildControlTarget,
    ) -> Result<rw_types::family_controls::ChildControlsSnapshot, HostError> {
        self.ensure_parent(root)?;
        let child = self
            .orchestrator
            .control_child(root, target)
            .map_err(|error| HostError::Protocol(error.to_string()))?;
        child
            .child_controls()
            .await
            .map_err(|error| HostError::Protocol(error.to_string()))
    }
    async fn respond_control(
        &self,
        root: &SessionId,
        target: &rw_types::family_controls::ChildControlTarget,
        authority: rw_core::FamilyControlAuthority,
        meta: rw_types::CommandMeta,
        revision: rw_types::SequenceId,
        response: rw_types::family_controls::ChildControlResponse,
    ) -> Result<rw_types::CommandOutcome, HostError> {
        self.ensure_parent(root)?;
        if authority.root_session_id() != root {
            return Err(HostError::Protocol(
                "family control authority root mismatch".into(),
            ));
        }
        let child = self
            .orchestrator
            .control_child(root, target)
            .map_err(|error| HostError::Protocol(error.to_string()))?;
        child
            .respond_control(authority, meta, revision, response)
            .await
            .map_err(|error| HostError::Protocol(error.to_string()))
    }

    async fn list(
        &self,
        parent_session_id: &SessionId,
    ) -> Result<Vec<rw_core::SubagentDescriptor>, HostError> {
        self.ensure_parent(parent_session_id)?;
        Ok(self.orchestrator.list_for_parent(parent_session_id))
    }

    async fn continue_child(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &rw_core::SubagentId,
        content: String,
    ) -> Result<(), HostError> {
        self.ensure_parent(parent_session_id)?;
        // A user-continued child reports like a background child: its result
        // is delivered once and wakes the parent if it is idle.
        let observer = self
            .orchestrator
            .background_observer(self.parent.background_subagent_event_sink());
        self.orchestrator
            .follow_up(
                parent_session_id,
                subagent_id,
                content,
                observer,
                CancellationToken::default(),
            )
            .await
            .map(|_| ())
            .map_err(|error| HostError::Protocol(error.to_string()))
    }

    async fn interrupt(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &rw_core::SubagentId,
    ) -> Result<(), HostError> {
        self.ensure_parent(parent_session_id)?;
        self.orchestrator
            .cancel(parent_session_id, subagent_id)
            .await
            .map_err(|error| HostError::Protocol(error.to_string()))
    }

    async fn move_to_background(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &rw_core::SubagentId,
    ) -> Result<(), HostError> {
        self.ensure_parent(parent_session_id)?;
        self.orchestrator
            .move_to_background(parent_session_id, subagent_id)
            .map_err(|error| HostError::Protocol(error.to_string()))
    }

    async fn close(
        &self,
        parent_session_id: &SessionId,
        subagent_id: &rw_core::SubagentId,
    ) -> Result<(), HostError> {
        self.ensure_parent(parent_session_id)?;
        self.orchestrator
            .close(parent_session_id, subagent_id)
            .await
            .map_err(|error| HostError::Protocol(error.to_string()))
    }
}
