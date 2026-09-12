use super::fixtures::checkpoints::RecordingCheckpoints;
use super::fixtures::models::ScriptedModel;
use super::fixtures::support::{collect_turn, config, next_matching, stop_script};
use crate::engine::AgentTurnStatus;
use crate::engine::pending_event::PendingEvent;
use async_trait::async_trait;
use rw_ext::{
    HookClass, HookDirective, HookDispatcher, HookEffect, HookError, HookEvent, HookFailurePolicy,
    HookHandler, HookInvocation, HookRegistration,
};
use rw_tools::ToolRegistry;
use rw_types::config::PermissionDecision;
use rw_types::{ApprovalDecision, SessionMode, ToolCapability};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

struct CompletionPolicy {
    checkpoints: Arc<RecordingCheckpoints>,
    invoked: Arc<AtomicBool>,
    block: bool,
    physical_release: Option<Arc<Notify>>,
    settling: Arc<Notify>,
}

#[async_trait]
impl HookHandler for CompletionPolicy {
    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        assert!(
            self.checkpoints
                .events
                .lock()
                .expect("checkpoints")
                .iter()
                .any(|event| event.starts_with("begin:"))
        );
        self.invoked.store(true, Ordering::SeqCst);
        if self.physical_release.is_some() {
            invocation.cancellation().cancelled().await;
        }
        if self.block {
            Ok(HookDirective::Block {
                message: "test command failed".to_owned(),
            })
        } else {
            Ok(HookDirective::Continue {})
        }
    }

    async fn settle_effects(&self) -> Result<(), HookError> {
        if self.invoked.load(Ordering::SeqCst)
            && let Some(release) = &self.physical_release
        {
            self.settling.notify_one();
            release.notified().await;
        }
        Ok(())
    }
}

fn register(hooks: &mut HookDispatcher, policy: CompletionPolicy) {
    hooks
        .register(
            HookRegistration::new("fixture.completion", HookEvent::TurnEnd, HookClass::Policy)
                .with_failure_policy(HookFailurePolicy::FailClosed)
                .with_effect(HookEffect::WorkspaceMutating)
                .with_required_capabilities(vec![ToolCapability::Execute]),
            policy,
        )
        .expect("completion policy");
}

#[tokio::test]
async fn completion_policy_is_authorized_and_checkpointed_before_invocation() {
    for allow in [false, true] {
        let root = tempfile::TempDir::new().expect("workspace");
        let checkpoints = Arc::new(RecordingCheckpoints::default());
        let invoked = Arc::new(AtomicBool::new(false));
        let mut hooks = HookDispatcher::new();
        register(
            &mut hooks,
            CompletionPolicy {
                checkpoints: checkpoints.clone(),
                invoked: invoked.clone(),
                block: true,
                physical_release: None,
                settling: Arc::new(Notify::new()),
            },
        );
        let mut cfg = config(
            root.path(),
            Arc::new(ScriptedModel::new([stop_script("done", &[])])),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Ask,
            hooks,
        );
        cfg.checkpoints = checkpoints.clone();
        let handle = crate::engine::tests::fixtures::history::spawn(cfg)
            .await
            .expect("actor");
        let mut events = handle.subscribe().expect("events");
        handle.send_message("run").await.expect("message");
        let event = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::PermissionRequested { .. })
        })
        .await;
        let PendingEvent::PermissionRequested { request, .. } = event.kind else {
            unreachable!()
        };
        assert_eq!(request.tool_name, "completion_hooks");
        assert_eq!(
            request.arguments,
            serde_json::json!({"hooks": ["fixture.completion"]})
        );
        assert!(
            request
                .capabilities
                .contains(&ToolCapability::WriteFilesystem)
        );
        assert!(request.capabilities.contains(&ToolCapability::Execute));
        assert!(!invoked.load(Ordering::SeqCst));
        assert!(checkpoints.events.lock().expect("checkpoints").is_empty());
        handle
            .approve(
                request.id,
                request.invocation_id,
                if allow {
                    ApprovalDecision::AllowOnce
                } else {
                    ApprovalDecision::Deny
                },
            )
            .await
            .expect("approval");
        let events = collect_turn(&mut events).await;
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::TurnFinished {
                status: AgentTurnStatus::Failed,
                ..
            }
        )));
        assert_eq!(invoked.load(Ordering::SeqCst), allow);
        let checkpoints = checkpoints.events.lock().expect("checkpoints");
        if allow {
            assert_eq!(checkpoints.len(), 2);
            assert!(checkpoints[0].ends_with(":OpaqueWorkspace"));
            assert!(checkpoints[1].ends_with(":Failed"));
        } else {
            assert!(checkpoints.is_empty());
        }
    }
}

#[tokio::test]
async fn read_only_modes_deny_mutating_completion_policies() {
    for mode in [SessionMode::Plan, SessionMode::Discuss] {
        let root = tempfile::TempDir::new().expect("workspace");
        let checkpoints = Arc::new(RecordingCheckpoints::default());
        let invoked = Arc::new(AtomicBool::new(false));
        let mut hooks = HookDispatcher::new();
        register(
            &mut hooks,
            CompletionPolicy {
                checkpoints: checkpoints.clone(),
                invoked: invoked.clone(),
                block: false,
                physical_release: None,
                settling: Arc::new(Notify::new()),
            },
        );
        let mut cfg = config(
            root.path(),
            Arc::new(ScriptedModel::new([stop_script("done", &[])])),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            hooks,
        );
        cfg.checkpoints = checkpoints.clone();
        cfg.recovered.mode_id = Some(crate::engine::tests::fixtures::support::wire_mode(mode));
        cfg.recovered.mode = mode;
        let handle = crate::engine::tests::fixtures::history::spawn(cfg)
            .await
            .expect("actor");
        let mut events = handle.subscribe().expect("events");
        handle.send_message("run").await.expect("message");
        let events = collect_turn(&mut events).await;
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::TurnFinished {
                status: AgentTurnStatus::Failed,
                ..
            }
        )));
        assert!(!invoked.load(Ordering::SeqCst));
        assert!(checkpoints.events.lock().expect("checkpoints").is_empty());
    }
}

#[tokio::test]
async fn failed_provider_turn_never_admits_completion_mutation() {
    let root = tempfile::TempDir::new().expect("workspace");
    let checkpoints = Arc::new(RecordingCheckpoints::default());
    let invoked = Arc::new(AtomicBool::new(false));
    let mut hooks = HookDispatcher::new();
    register(
        &mut hooks,
        CompletionPolicy {
            checkpoints: checkpoints.clone(),
            invoked: invoked.clone(),
            block: false,
            physical_release: None,
            settling: Arc::new(Notify::new()),
        },
    );
    let model = ScriptedModel::new([vec![Err(rw_providers::ProviderError::new(
        rw_providers::ProviderErrorKind::ReplayMiss,
        "fixture failure",
    ))]]);
    let mut cfg = config(
        root.path(),
        Arc::new(model),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        hooks,
    );
    cfg.checkpoints = checkpoints.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(cfg)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("events");
    handle.send_message("run").await.expect("message");
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Failed,
            ..
        }
    )));
    assert!(!invoked.load(Ordering::SeqCst));
    assert!(checkpoints.events.lock().expect("checkpoints").is_empty());
}

#[tokio::test]
async fn interrupt_retains_completion_checkpoint_until_physical_effects_settle() {
    let root = tempfile::TempDir::new().expect("workspace");
    let checkpoints = Arc::new(RecordingCheckpoints::default());
    let invoked = Arc::new(AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let settling = Arc::new(Notify::new());
    let mut hooks = HookDispatcher::new();
    register(
        &mut hooks,
        CompletionPolicy {
            checkpoints: checkpoints.clone(),
            invoked: invoked.clone(),
            block: false,
            physical_release: Some(release.clone()),
            settling: settling.clone(),
        },
    );
    let mut cfg = config(
        root.path(),
        Arc::new(ScriptedModel::new([stop_script("done", &[])])),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        hooks,
    );
    cfg.checkpoints = checkpoints.clone();
    let handle = crate::engine::tests::fixtures::history::spawn(cfg)
        .await
        .expect("actor");
    let mut events = handle.subscribe().expect("events");
    handle.send_message("run").await.expect("message");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !invoked.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("policy entered");
    handle.interrupt().await.expect("interrupt");
    tokio::time::timeout(std::time::Duration::from_secs(3), settling.notified())
        .await
        .expect("settlement started");
    assert_eq!(checkpoints.events.lock().expect("checkpoints").len(), 1);
    release.notify_one();
    let events = collect_turn(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    )));
    assert!(checkpoints.events.lock().expect("checkpoints")[1].ends_with(":Cancelled"));
}
