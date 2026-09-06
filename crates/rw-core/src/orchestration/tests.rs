#![allow(clippy::expect_used)]

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use rw_tools::{
    CancellationToken, CapabilityManifest, SubagentEventSink, SubagentLifecycleEvent,
    SubagentProgressEvent, Tool, ToolContext, ToolDescriptor, ToolError, ToolRegistry, ToolResult,
    WorkspaceBinding, WorktreeIsolation, WorktreeLeaseRecord,
};
#[cfg(test)]
use rw_types::config::PermissionDecision;
use rw_types::{
    Cost, DiffArtifact, EngineEvent, SessionId, SessionMode, SubagentActivity, SubagentId,
    SubagentIsolation, SubagentResult, SubagentStatus, ToolCapability, Usage,
};
use serde_json::{Value, json};

use crate::{AgentLoopError, ModelDriver};

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

struct SelectedModel;

#[async_trait]
impl ModelDriver for SelectedModel {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }

    fn stream(
        &self,
        _alias: &str,
        _request: rw_providers::ProviderRequest,
        _invocation: crate::provider_admission::ProviderInvocation,
    ) -> Result<rw_providers::BoxEventStream, AgentLoopError> {
        Err(AgentLoopError::Provider(
            "selected-model fixture must not stream".to_owned(),
        ))
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        alias == "openai_codex/gpt-5.6-sol"
    }
}

#[derive(Default)]
struct RecordingSubagentSink {
    lifecycles: Mutex<Vec<SubagentLifecycleEvent>>,
}

#[async_trait]
impl SubagentEventSink for RecordingSubagentSink {
    async fn lifecycle(&self, event: SubagentLifecycleEvent) -> Result<(), ToolError> {
        self.lifecycles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
        Ok(())
    }

    async fn progress(&self, _event: SubagentProgressEvent) -> Result<(), ToolError> {
        Ok(())
    }
}

struct RejectingApprover(AtomicUsize);

#[async_trait]
impl crate::PermissionApprover for RejectingApprover {
    async fn decide(&self, _request: crate::PermissionRequest) -> rw_types::ApprovalDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        rw_types::ApprovalDecision::Deny
    }
}

#[derive(Default)]
struct FakeFactory {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
    hang_cancel: bool,
    closed_artifacts: Arc<Mutex<Vec<Option<String>>>>,
    fail_close: bool,
    launches: Arc<Mutex<Vec<SubagentRequest>>>,
}

struct FakeSession {
    session_id: SessionId,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
    history: Mutex<Vec<String>>,
    hang_cancel: bool,
    closed_artifacts: Arc<Mutex<Vec<Option<String>>>>,
    fail_close: bool,
}

struct NoopProgress;

#[async_trait]
impl SubagentProgressObserver for NoopProgress {
    async fn progress(
        &self,
        _child_sequence: Option<u64>,
        _event: Value,
    ) -> Result<(), OrchestrationError> {
        Ok(())
    }
}

#[async_trait]
impl SubagentSessionFactory for FakeFactory {
    async fn create(
        &self,
        launch: SubagentLaunch,
    ) -> Result<Arc<dyn SubagentSession>, OrchestrationError> {
        self.launches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(launch.request.clone());
        Ok(Arc::new(FakeSession {
            session_id: launch.handle.session_id,
            active: Arc::clone(&self.active),
            peak: Arc::clone(&self.peak),
            cancelled: Arc::clone(&self.cancelled),
            history: Mutex::new(Vec::new()),
            hang_cancel: self.hang_cancel,
            closed_artifacts: Arc::clone(&self.closed_artifacts),
            fail_close: self.fail_close,
        }))
    }

    async fn rebind(
        &self,
        session_id: &SessionId,
        _workspace_root: Option<&Path>,
        _worktree: Option<&WorktreeLeaseRecord>,
        _allowed_tools: Option<Arc<ToolRegistry>>,
        _policy: &SubagentRecoveryPolicy,
    ) -> Result<Option<Arc<dyn SubagentSession>>, OrchestrationError> {
        Ok(Some(Arc::new(FakeSession {
            session_id: session_id.clone(),
            active: Arc::clone(&self.active),
            peak: Arc::clone(&self.peak),
            cancelled: Arc::clone(&self.cancelled),
            history: Mutex::new(Vec::new()),
            hang_cancel: self.hang_cancel,
            closed_artifacts: Arc::clone(&self.closed_artifacts),
            fail_close: self.fail_close,
        })))
    }
}

#[async_trait]
impl SubagentSession for FakeSession {
    fn control_summary(&self) -> rw_types::family_controls::ChildControlSummary {
        rw_types::family_controls::ChildControlSummary::default()
    }
    async fn child_state(
        &self,
    ) -> Result<rw_types::session_state::SessionStateSnapshot, crate::OrchestrationError> {
        Err(crate::OrchestrationError::Session(
            "fixture has no actor controls".into(),
        ))
    }
    async fn child_controls(
        &self,
    ) -> Result<rw_types::family_controls::ChildControlsSnapshot, crate::OrchestrationError> {
        Err(crate::OrchestrationError::Session(
            "fixture has no actor controls".into(),
        ))
    }
    async fn respond_control(
        &self,
        _authority: crate::FamilyControlAuthority,
        _meta: rw_types::CommandMeta,
        _revision: rw_types::SequenceId,
        _response: rw_types::family_controls::ChildControlResponse,
    ) -> Result<rw_types::CommandOutcome, crate::OrchestrationError> {
        Err(crate::OrchestrationError::Session(
            "fixture has no actor controls".into(),
        ))
    }

    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn run_turn(
        &self,
        prompt: String,
        cancellation: CancellationToken,
        progress: Arc<dyn SubagentProgressObserver>,
    ) -> Result<SubagentTurnResult, OrchestrationError> {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
        let delay = prompt
            .strip_prefix("delay:")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(1);
        tokio::select! {
            () = cancellation.cancelled() => {
                self.active.fetch_sub(1, Ordering::AcqRel);
                return Err(OrchestrationError::Session("cancelled".to_owned()));
            }
            () = tokio::time::sleep(Duration::from_millis(delay)) => {}
        }
        progress
            .progress(Some(0), json!({"type":"text_delta","text":prompt}))
            .await?;
        let invalid_artifact = prompt == "invalid-artifact";
        let valid_artifact = prompt == "valid-artifact";
        let count = {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            history.push(prompt);
            history.len()
        };
        self.active.fetch_sub(1, Ordering::AcqRel);
        let diff_artifact = if invalid_artifact {
            let mut artifact = test_artifact();
            artifact.id = "0".repeat(64);
            Some(artifact)
        } else if valid_artifact {
            Some(test_artifact())
        } else {
            None
        };
        Ok(SubagentTurnResult {
            status: SubagentStatus::Completed,
            final_text: format!("history:{count}"),
            touched_files: Vec::new(),
            diff_artifact,
            usage: zero_usage(),
            cost: Cost::Unavailable {
                reason: "fixture".to_owned(),
            },
            turns: 1,
        })
    }

    async fn cancel(&self) -> Result<(), OrchestrationError> {
        self.cancelled.fetch_add(1, Ordering::Relaxed);
        if self.hang_cancel {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn close(
        &self,
        durable_artifact: Option<&DiffArtifact>,
    ) -> Result<(), OrchestrationError> {
        self.cancel().await?;
        if self.fail_close {
            return Err(OrchestrationError::Session(
                "fixture close failed".to_owned(),
            ));
        }
        self.closed_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(durable_artifact.map(|artifact| artifact.id.clone()));
        Ok(())
    }
}

#[derive(Default)]
struct RecordingObserver {
    events: Mutex<Vec<String>>,
    results: Mutex<Vec<SubagentResult>>,
    fail_finished: bool,
    fail_spawned: bool,
}

struct FailingMetadataStore;

#[derive(Default)]
struct FailingPromotionMetadataStore {
    retained: Mutex<Option<SubagentRecoveryRecord>>,
}

#[derive(Default)]
struct FailOnceRemoveMetadataStore {
    removes: AtomicUsize,
}

#[derive(Default)]
struct RecordingMetadataStore {
    record: Mutex<Option<SubagentRecoveryRecord>>,
    removes: AtomicUsize,
}

#[async_trait]
impl SubagentMetadataStore for FailingMetadataStore {
    async fn save(&self, _record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
        Err(OrchestrationError::Session(
            "metadata persistence failed".to_owned(),
        ))
    }

    async fn remove(
        &self,
        _parent_session_id: &SessionId,
        _subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        Ok(())
    }
}

#[async_trait]
impl SubagentMetadataStore for FailingPromotionMetadataStore {
    async fn save(&self, record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
        if record.phase == SubagentRecoveryPhase::Active {
            return Err(OrchestrationError::Session(
                "metadata promotion failed".to_owned(),
            ));
        }
        *self
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(record);
        Ok(())
    }
    async fn remove(
        &self,
        _parent_session_id: &SessionId,
        _subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        *self
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

#[async_trait]
impl SubagentMetadataStore for FailOnceRemoveMetadataStore {
    async fn save(&self, _record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
        Ok(())
    }

    async fn remove(
        &self,
        _parent_session_id: &SessionId,
        _subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        if self.removes.fetch_add(1, Ordering::AcqRel) == 0 {
            Err(OrchestrationError::Session(
                "fixture metadata remove failed".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl SubagentMetadataStore for RecordingMetadataStore {
    async fn save(&self, record: SubagentRecoveryRecord) -> Result<(), OrchestrationError> {
        *self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(record);
        Ok(())
    }

    async fn remove(
        &self,
        _parent_session_id: &SessionId,
        _subagent_id: &SubagentId,
    ) -> Result<(), OrchestrationError> {
        self.removes.fetch_add(1, Ordering::AcqRel);
        *self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

#[async_trait]
impl SubagentObserver for RecordingObserver {
    async fn spawned(
        &self,
        handle: &SubagentHandle,
        _task: &str,
    ) -> Result<(), OrchestrationError> {
        if self.fail_spawned {
            return Err(OrchestrationError::Observer(
                "spawn fixture failure".to_owned(),
            ));
        }
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("spawn:{}", handle.subagent_id.0));
        Ok(())
    }

    async fn finished(&self, result: &SubagentResult) -> Result<(), OrchestrationError> {
        if self.fail_finished {
            return Err(OrchestrationError::Observer("fixture failure".to_owned()));
        }
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("finish:{}", result.subagent_id.0));
        self.results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(result.clone());
        Ok(())
    }

    async fn progress(
        &self,
        _handle: &SubagentHandle,
        _child_sequence: Option<u64>,
        _event: Value,
    ) -> Result<(), OrchestrationError> {
        Ok(())
    }
}

fn request(task: &str) -> SubagentRequest {
    SubagentRequest {
        task: task.to_owned(),
        agent: "fixture".to_owned(),
        model: "fast".to_owned(),
        tools: Vec::new(),
        system_prompt: Some("fixture".to_owned()),
        permission_mode: SessionMode::Execute,
        max_turns: Some(4),
        isolation: SubagentIsolation::Shared,
        workspace_root: std::env::current_dir().expect("cwd"),
    }
}

fn test_artifact() -> DiffArtifact {
    let base_commit = "1".repeat(40);
    let touched_files = vec![rw_types::TouchedFile {
        path: "src/lib.rs".to_owned(),
        status: rw_types::TouchedFileStatus::Modified,
    }];
    let unified_diff = "diff --git a/src/lib.rs b/src/lib.rs\n".to_owned();
    let manifest = serde_json::to_vec(&touched_files).expect("manifest");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler.worktree-diff.v1\0");
    hasher.update(base_commit.as_bytes());
    hasher.update(b"\0");
    hasher.update(&manifest);
    hasher.update(b"\0");
    hasher.update(unified_diff.as_bytes());
    DiffArtifact {
        id: hasher.finalize().to_hex().to_string(),
        base_commit,
        touched_files,
        unified_diff,
    }
}
fn rehash_test_artifact(artifact: &mut DiffArtifact) {
    let manifest = serde_json::to_vec(&artifact.touched_files).expect("manifest");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler.worktree-diff.v1\0");
    hasher.update(artifact.base_commit.as_bytes());
    hasher.update(b"\0");
    hasher.update(&manifest);
    hasher.update(b"\0");
    hasher.update(artifact.unified_diff.as_bytes());
    artifact.id = hasher.finalize().to_hex().to_string();
}

fn recovery_record(subagent: &str, session: &str) -> SubagentRecoveryRecord {
    SubagentRecoveryRecord {
        parent_session_id: SessionId("parent".to_owned()),
        handle: SubagentHandle {
            subagent_id: SubagentId(subagent.to_owned()),
            session_id: SessionId(session.to_owned()),
        },
        task: "fixture task".to_owned(),
        agent: "fixture agent".to_owned(),
        depth: 1,
        workspace_root: std::env::current_dir().expect("cwd"),
        isolation: SubagentIsolation::Shared,
        worktree: None,
        capabilities: CapabilityManifest::default(),
        tool_names: Vec::new(),
        policy: SubagentRecoveryPolicy {
            model_alias: "fast".to_owned(),
            system_prompt: Some("fixture".to_owned()),
            permission_mode: SessionMode::Execute,
            max_turns: 4,
        },
        phase: SubagentRecoveryPhase::Active,
    }
}

fn test_event_meta(sequence: u64) -> rw_types::EventMeta {
    rw_types::EventMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        session_id: SessionId("parent".to_owned()),
        sequence_id: rw_types::SequenceId(sequence),
        emitted_at: "2026-01-01T00:00:00Z".to_owned(),
        caused_by: None,
    }
}

fn orchestrator(limits: SubagentLimits, factory: Arc<FakeFactory>) -> SubagentOrchestrator {
    SubagentOrchestrator::new(
        limits,
        factory,
        Arc::new(ToolRegistry::new()),
        Arc::new(TestArtifactSource::default()),
    )
    .expect("orchestrator")
}

struct MutatingTool;

struct FixedResultTool {
    result: ToolResult,
}

struct GatewayTool(&'static str);

#[async_trait]
impl Tool for GatewayTool {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.0.to_owned(),
            description: "MCP gateway fixture".to_owned(),
            input_schema: Value::Null,
            capabilities: CapabilityManifest::new([
                ToolCapability::Network,
                ToolCapability::Execute,
            ]),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        panic!("gateway fixture must not execute")
    }
}

#[async_trait]
impl Tool for MutatingTool {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "write".to_owned(),
            description: "fixture mutation".to_owned(),
            input_schema: Value::Null,
            capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
        }
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        panic!("restricted child must never execute mutating fixture")
    }
}

#[async_trait]
impl Tool for FixedResultTool {
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "fixed_result".to_owned(),
            description: "fixture".to_owned(),
            input_schema: Value::Null,
            capabilities: CapabilityManifest::default(),
        }
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        Ok(self.result.clone())
    }
}

mod lifecycle;
mod tools;

mod startup;

mod worktree_startup;

mod artifact_source;
use artifact_source::TestArtifactSource;

mod family_controls;
