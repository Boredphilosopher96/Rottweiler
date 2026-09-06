//! Prepared workspace rollback must retire before the damaged turn is repaired.
use super::{Fixture, fixture_with};
use crate::engine::{
    AgentLoopError, AgentTurnStatus, PendingEvent,
    commands::{
        SessionCommandAction, SessionCommandContext, SessionCommandOutput, WorkspaceRootController,
        WorkspaceRuntimeGeneration,
    },
    tests::fixtures::{controllers::FixedWorkspaceRootController, support::collect_turn},
};
use async_trait::async_trait;
use rw_ext::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct PrepareCommand(PathBuf);
#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for PrepareCommand {
    async fn execute(
        &self,
        _: &mut SessionCommandContext,
        _: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: String::new(),
            action: SessionCommandAction::AddWorkspaceRoot {
                path: self.0.clone(),
            },
        })
    }
}
struct PreparedRoot {
    inner: FixedWorkspaceRootController,
    entered: Notify,
    release: Notify,
    aborts: AtomicUsize,
    abort_entered: Notify,
    abort_release: Notify,
}
#[async_trait]
impl WorkspaceRootController for PreparedRoot {
    async fn append_root(
        &self,
        request: crate::WorkspaceRootRequest<'_>,
    ) -> Result<WorkspaceRuntimeGeneration, AgentLoopError> {
        let prepared = self.inner.append_root(request).await?;
        self.entered.notify_one();
        self.release.notified().await;
        Ok(prepared)
    }
    async fn prepare_commit_generation(&self, generation: u64) -> Result<(), AgentLoopError> {
        self.inner.prepare_commit_generation(generation).await
    }
    fn finalize_generation(&self, generation: u64) {
        self.inner.finalize_generation(generation);
    }
    async fn abort_generation(&self, generation: u64) -> Result<(), AgentLoopError> {
        self.aborts.fetch_add(1, Ordering::AcqRel);
        self.abort_entered.notify_one();
        self.abort_release.notified().await;
        self.inner.abort_generation(generation).await
    }
}
async fn prepared_fixture(added: PathBuf) -> (Fixture, Arc<PreparedRoot>) {
    let mut controller = None;
    let fixture = fixture_with(false, |config, peer| {
        peer.hold_fast.store(true, Ordering::Release);
        config.workspace_root = config
            .workspace_root
            .canonicalize()
            .expect("canonical workspace");
        let owner = Arc::new(PreparedRoot {
            inner: FixedWorkspaceRootController {
                extensions: None,
                roots: vec![config.workspace_root.clone(), added.clone()],
                tools: config.tools.clone(),
                permissions: config.permissions.clone(),
                committed: AtomicU64::new(0),
                aborted: AtomicU64::new(0),
                fail_commit: false,
            },
            entered: Notify::new(),
            release: Notify::new(),
            aborts: AtomicUsize::new(0),
            abort_entered: Notify::new(),
            abort_release: Notify::new(),
        });
        config.workspace_roots = owner.clone();
        let mut commands = CommandRegistry::new();
        commands
            .register(
                CommandDescriptor::new("prepare", "prepare workspace fixture"),
                PrepareCommand(added),
            )
            .expect("command");
        config.commands = Arc::new(commands);
        controller = Some(owner);
    })
    .await;
    (fixture, controller.expect("prepared root owner"))
}

#[tokio::test]
async fn prepared_workspace_abort_precedes_repair_and_fresh_admission() {
    let added = tempfile::tempdir().expect("additional workspace");
    let (mut fixture, controller) =
        prepared_fixture(added.path().canonicalize().expect("path")).await;
    fixture
        .handle
        .send_message("two parallel tools")
        .await
        .expect("turn");
    let caller = tokio::spawn({
        let handle = fixture.handle.clone();
        async move { handle.send_message("/prepare").await }
    });
    tokio::time::timeout(Duration::from_secs(2), controller.entered.notified())
        .await
        .expect("generation is physically prepared");
    fixture.peer.fast_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), fixture.peer.cancelled.notified())
        .await
        .expect("append failure cancels running peer");
    fixture.peer.release.notify_one();
    assert!(fixture.handle.snapshot().await.is_err());
    assert_eq!(controller.aborts.load(Ordering::Acquire), 0);
    assert!(
        !fixture
            .sink
            .inner
            .events
            .lock()
            .expect("source")
            .iter()
            .any(|event| matches!(event.kind, PendingEvent::TurnFinished { .. }))
    );
    controller.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), controller.abort_entered.notified())
        .await
        .expect("prepared generation enters rollback");
    assert_eq!(controller.aborts.load(Ordering::Acquire), 1);
    assert_eq!(controller.inner.aborted.load(Ordering::Acquire), 0);
    assert!(!caller.is_finished());
    assert!(
        !fixture
            .sink
            .inner
            .events
            .lock()
            .expect("source")
            .iter()
            .any(|event| matches!(event.kind, PendingEvent::TurnFinished { .. }))
    );
    controller.abort_release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), caller)
            .await
            .expect("command rollback settles")
            .expect("command owner")
            .is_err()
    );
    let repaired = collect_turn(&mut fixture.events).await;
    assert!(repaired.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    )));
    assert_eq!(controller.aborts.load(Ordering::Acquire), 1);
    assert_eq!(controller.inner.committed.load(Ordering::Acquire), 0);
    assert_eq!(controller.inner.aborted.load(Ordering::Acquire), 1);
    assert_eq!(
        fixture
            .handle
            .snapshot()
            .await
            .expect("repaired state")
            .workspace_generation,
        0
    );
    fixture
        .handle
        .send_message("fresh admission")
        .await
        .expect("new turn");
    let next = collect_turn(&mut fixture.events).await;
    assert!(next.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            turn: 2,
            status: AgentTurnStatus::Completed,
            ..
        }
    )));
    fixture.handle.close().await.expect("close");
    assert_eq!(controller.aborts.load(Ordering::Acquire), 1);
}
