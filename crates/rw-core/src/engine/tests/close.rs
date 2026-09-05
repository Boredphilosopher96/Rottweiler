#![cfg(test)]

use crate::engine::AgentLoopError;
use crate::engine::model::ModelDriver;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::SessionHandle;
use crate::engine::tests::fixtures::checkpoints::RecordingCheckpoints;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::sinks::RecordingSink;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use async_trait::async_trait;
use futures_util::stream;
use rw_ext::HookDirective;
use rw_ext::HookDispatcher;
use rw_ext::HookEffect;
use rw_ext::HookError;
use rw_ext::HookEvent;
use rw_ext::HookHandler;
use rw_ext::HookInvocation;
use rw_ext::HookRegistration;
use rw_providers::BoxEventStream;
use rw_providers::ProviderRequest;
use rw_tools::CapabilityManifest;
use rw_tools::Tool;
use rw_tools::ToolContext;
use rw_tools::ToolDescriptor;
use rw_tools::ToolError;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::SessionId;
use rw_types::SubagentId;
use rw_types::ToolCapability;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct CleanupTool {
    entered: Notify,
    release: Notify,
    completed: AtomicBool,
    callback: Mutex<Option<SessionHandle>>,
}

#[async_trait]
impl Tool for CleanupTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "cleanup_probe".to_owned(),
            description: "owned cleanup fixture".to_owned(),
            input_schema: json!({"type":"object"}),
            capabilities: CapabilityManifest::new(Vec::new()),
        }
    }
    async fn execute(&self, _: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::new("done", Value::Null))
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        Ok(())
    }
    async fn end_session(&self, _: &SessionId) -> Result<(), ToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        let callback = self.callback.lock().expect("callback").take();
        if let Some(handle) = callback {
            handle
                .record_subagent_spawned(
                    SubagentId("cleanup-child".to_owned()),
                    SessionId("cleanup-child-session".to_owned()),
                    "closing child acknowledgement".to_owned(),
                )
                .await
                .map_err(|error| ToolError::EffectsUnsettled(error.to_string()))?;
        }
        self.completed.store(true, Ordering::Release);
        Ok(())
    }
}

#[tokio::test]
async fn dropped_close_caller_does_not_release_cleanup_or_block_internal_acknowledgements() {
    let root = tempfile::tempdir().expect("root");
    let tool = Arc::new(CleanupTool::default());
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("registered");
    let handle = crate::engine::tests::fixtures::history::spawn(config(
        root.path(),
        Arc::new(ScriptedModel::new(Vec::new())),
        Arc::new(tools),
        PermissionDecision::Allow,
        HookDispatcher::new(),
    ))
    .await
    .expect("actor");
    *tool.callback.lock().expect("callback") = Some(handle.clone());
    let closing = tokio::spawn({
        let handle = handle.clone();
        async move { handle.close().await }
    });
    tool.entered.notified().await;
    assert!(!closing.is_finished());
    closing.abort();
    assert!(closing.await.expect_err("caller aborted").is_cancelled());
    assert!(!tool.completed.load(Ordering::Acquire));
    tool.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), handle.close())
        .await
        .expect("cleanup ack")
        .expect("settled");
    assert!(tool.completed.load(Ordering::Acquire));
    assert!(handle.send_message("late mutation").await.is_err());
    handle.close().await.expect("idempotent closed proof");
}

struct FailedModel {
    entered: Notify,
    panic: bool,
}
#[async_trait]
impl ModelDriver for FailedModel {
    fn stream(
        &self,
        _: &str,
        _: ProviderRequest,
        _: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.entered.notify_one();
        Ok(Box::pin(stream::iter(stop_script("response", &[]))))
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        assert!(!self.panic, "fixture model settlement panic");
        Err(AgentLoopError::EffectsUnsettled(
            "fixture model effects remain owned".to_owned(),
        ))
    }
}

#[tokio::test]
async fn failed_or_panicked_model_cleanup_has_no_terminal_and_retains_owner_after_close() {
    for panic in [false, true] {
        let root = tempfile::tempdir().expect("root");
        let model = Arc::new(FailedModel {
            entered: Notify::new(),
            panic,
        });
        let weak = Arc::downgrade(&model);
        let sink = Arc::new(RecordingSink::default());
        let mut configuration = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            HookDispatcher::new(),
        );
        configuration.event_sink = sink.clone();
        let handle = crate::engine::tests::fixtures::history::spawn(configuration)
            .await
            .expect("actor");
        handle
            .send_message("trigger invocation")
            .await
            .expect("turn admitted");
        model.entered.notified().await;
        let proof = tokio::time::timeout(Duration::from_secs(1), handle.close())
            .await
            .expect("failed proof is bounded");
        assert!(matches!(proof, Err(AgentLoopError::EffectsUnsettled(_))));
        assert!(
            sink.events
                .lock()
                .expect("events")
                .iter()
                .all(|event| !matches!(event.kind, PendingEvent::TurnFinished { .. }))
        );
        drop(model);
        drop(handle);
        tokio::task::yield_now().await;
        assert!(weak.upgrade().is_some());
    }
}

struct ResourcesProbe {
    entered: Notify,
    release: Notify,
    fail: bool,
}
#[async_trait]
impl crate::SessionResources for ResourcesProbe {
    fn bind_session(&self, _binding: crate::PluginSessionBinding) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        self.entered.notify_one();
        self.release.notified().await;
        if self.fail {
            Err(AgentLoopError::EffectsUnsettled(
                "resource owner failed".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn actor_acknowledges_runtime_resources_only_after_success_and_retains_failure() {
    for fail in [false, true] {
        let root = tempfile::tempdir().expect("root");
        let resources = Arc::new(ResourcesProbe {
            entered: Notify::new(),
            release: Notify::new(),
            fail,
        });
        let weak = Arc::downgrade(&resources);
        let mut configuration = config(
            root.path(),
            Arc::new(ScriptedModel::new(Vec::new())),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            HookDispatcher::new(),
        );
        configuration.resources = resources.clone();
        let handle = crate::engine::tests::fixtures::history::spawn(configuration)
            .await
            .expect("actor");
        let closing = tokio::spawn({
            let handle = handle.clone();
            async move { handle.close().await }
        });
        resources.entered.notified().await;
        assert!(!closing.is_finished());
        resources.release.notify_one();
        let proof = closing.await.expect("close task");
        assert_eq!(proof.is_err(), fail);
        drop(resources);
        drop(handle);
        tokio::task::yield_now().await;
        assert_eq!(weak.upgrade().is_some(), fail);
    }
}

struct FailedHookProof {
    invoked: AtomicBool,
    entered: Notify,
}
#[async_trait]
impl HookHandler for FailedHookProof {
    async fn invoke(&self, _: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        self.invoked.store(true, Ordering::Release);
        self.entered.notify_one();
        Ok(HookDirective::Continue {})
    }
    async fn settle_effects(&self) -> Result<(), HookError> {
        if self.invoked.load(Ordering::Acquire) {
            Err(HookError::new(
                "effects_unsettled",
                "fixture hook effects remain owned",
            ))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn failed_hook_proof_never_finishes_tool_or_checkpoint() {
    for (event, effect) in [
        (HookEvent::PreTool, HookEffect::ReadOnly),
        (HookEvent::PreTool, HookEffect::WorkspaceMutating),
        (HookEvent::PostTool, HookEffect::WorkspaceMutating),
    ] {
        let root = tempfile::tempdir().expect("root");
        let hook = Arc::new(FailedHookProof {
            invoked: AtomicBool::new(false),
            entered: Notify::new(),
        });
        let mut hooks = HookDispatcher::new();
        hooks
            .register_shared(
                HookRegistration::new(
                    "failed-proof",
                    event,
                    rw_types::hook_contract::HookClass::Policy,
                )
                .with_effect(effect),
                hook.clone(),
            )
            .expect("hook");
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(StubTool::new(
                "probe",
                vec![ToolCapability::ReadFilesystem],
                StubOutcome::Success(ToolResult::new("done", Value::Null)),
            )))
            .expect("tool");
        let checkpoints = Arc::new(RecordingCheckpoints::default());
        let sink = Arc::new(RecordingSink::default());
        let mut configuration = config(
            root.path(),
            Arc::new(ScriptedModel::new([tool_script(
                &[("call", "probe", json!({}))],
                &[],
            )])),
            Arc::new(tools),
            PermissionDecision::Allow,
            hooks,
        );
        configuration.checkpoints = checkpoints.clone();
        configuration.event_sink = sink.clone();
        let handle = crate::engine::tests::fixtures::history::spawn(configuration)
            .await
            .expect("actor");
        handle
            .send_message("run hook proof")
            .await
            .expect("admitted");
        hook.entered.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), handle.close())
                .await
                .expect("bounded proof")
                .is_err()
        );
        assert!(
            checkpoints
                .events
                .lock()
                .expect("checkpoints")
                .iter()
                .all(|event| !event.starts_with("finish:")),
            "unproven hook cannot finalize its checkpoint"
        );
        assert!(
            sink.events
                .lock()
                .expect("events")
                .iter()
                .all(|event| !matches!(
                    event.kind,
                    PendingEvent::ToolCallFinished { .. } | PendingEvent::TurnFinished { .. }
                )),
            "unproven hook cannot publish tool or turn completion"
        );
    }
}

struct ClosingJournal {
    inner: Arc<RecordingSink>,
    entered: Notify,
    release: Notify,
    waited: AtomicBool,
    fail: bool,
}
#[async_trait]
impl crate::SessionEventSink for ClosingJournal {
    async fn completed_turn(
        &self,
        turn: u64,
    ) -> Result<Option<crate::CompletedTurn>, AgentLoopError> {
        self.inner.completed_turn(turn).await
    }

    async fn todo_state(&self) -> Result<rw_types::todo::TodoSnapshot, crate::AgentLoopError> {
        self.inner.todo_state().await
    }

    async fn source_rewind_target(
        &self,
        expected_through: rw_types::SequenceId,
        source: rw_types::SequenceId,
        turn: u64,
        position: rw_types::RewindSourcePosition,
    ) -> std::result::Result<u64, AgentLoopError> {
        self.inner
            .source_rewind_target(expected_through, source, turn, position)
            .await
    }

    async fn extension_state(
        &self,
        plugin_id: &str,
    ) -> Result<crate::ExtensionStateView, AgentLoopError> {
        self.inner.extension_state(plugin_id).await
    }
    async fn reserve(
        &self,
        plan: &crate::EventBatchPlan,
    ) -> Result<crate::EventBatchReservation, AgentLoopError> {
        self.inner.reserve(plan).await
    }
    async fn commit(
        self: Arc<Self>,
        batch: Arc<crate::AdmittedEventBatch>,
    ) -> Result<Arc<crate::AdmittedEventBatch>, AgentLoopError> {
        Arc::clone(&self.inner).commit(batch).await
    }
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        if !self.waited.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self.fail {
            Err(AgentLoopError::EffectsUnsettled(
                "journal proof failed".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
    fn capture_read_view(&self) -> Result<Arc<dyn crate::SessionEventReadView>, AgentLoopError> {
        self.inner.capture_read_view()
    }
}

#[tokio::test]
async fn actor_close_waits_for_journal_ownership_and_propagates_failed_proof() {
    for fail in [false, true] {
        let root = tempfile::tempdir().expect("root");
        let sink = Arc::new(ClosingJournal {
            inner: Arc::new(RecordingSink::default()),
            entered: Notify::new(),
            release: Notify::new(),
            waited: AtomicBool::new(false),
            fail,
        });
        let mut configuration = config(
            root.path(),
            Arc::new(ScriptedModel::new(Vec::new())),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            HookDispatcher::new(),
        );
        configuration.event_sink = sink.clone();
        let handle = crate::engine::tests::fixtures::history::spawn(configuration)
            .await
            .expect("actor");
        let closing = tokio::spawn(async move { handle.close().await });
        tokio::time::timeout(Duration::from_secs(1), sink.entered.notified())
            .await
            .expect("journal barrier entered");
        assert!(!closing.is_finished());
        sink.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(1), closing)
            .await
            .expect("proof deadline")
            .expect("close task");
        assert_eq!(result.is_err(), fail);
    }
}

struct RejectBinding;
#[async_trait]
impl crate::SessionResources for RejectBinding {
    fn bind_session(&self, _binding: crate::PluginSessionBinding) -> Result<(), AgentLoopError> {
        Err(AgentLoopError::InvalidConfiguration(
            "session resource binding rejected".into(),
        ))
    }
    async fn shutdown(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

#[tokio::test]
async fn failed_resource_binding_prevents_actor_startup() {
    let root = tempfile::tempdir().expect("root");
    let sink = Arc::new(RecordingSink::default());
    let mut configuration = config(
        root.path(),
        Arc::new(ScriptedModel::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        HookDispatcher::new(),
    );
    configuration.event_sink = sink.clone();
    configuration.resources = Arc::new(RejectBinding);
    let error = match SessionActor::spawn(configuration) {
        Ok(_) => panic!("unbound actor must not start"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("session resource binding rejected")
    );
    tokio::task::yield_now().await;
    assert!(sink.events.lock().expect("events").is_empty());
}
