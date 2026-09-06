//! A durable append failure cannot close a turn whose other tools are still running.
use super::fixtures::{
    models::ScriptedModel,
    sinks::FailNextBatchSink,
    support::{collect_turn, config, descriptor, stop_script, tool_script},
};
use crate::engine::{AgentTurnStatus, PendingEvent, builtin_hook_dispatcher};
use async_trait::async_trait;
use rw_tools::{Tool, ToolContext, ToolDescriptor, ToolError, ToolRegistry, ToolResult};
use rw_types::config::PermissionDecision;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct Peer {
    entered: Notify,
    cancelled: Notify,
    release: Notify,
    settled: AtomicBool,
    fail_settlement: bool,
}
struct RecoveryTool {
    blocked: bool,
    peer: Arc<Peer>,
}
#[async_trait]
impl Tool for RecoveryTool {
    fn descriptor(&self) -> ToolDescriptor {
        descriptor(if self.blocked { "blocked" } else { "fast" })
    }
    async fn execute(&self, context: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        if self.blocked {
            self.peer.entered.notify_one();
            context.cancellation.cancelled().await;
            self.peer.cancelled.notify_one();
        } else {
            self.peer.entered.notified().await;
        }
        Ok(ToolResult::new("physical tool result", Value::Null))
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        if self.blocked && !self.peer.settled.load(Ordering::Acquire) {
            self.peer.release.notified().await;
            self.peer.settled.store(true, Ordering::Release);
        }
        if self.blocked && self.peer.fail_settlement {
            Err(ToolError::EffectsUnsettled(
                "fixture physical proof failed".into(),
            ))
        } else {
            Ok(())
        }
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    peer: Arc<Peer>,
    sink: Arc<FailNextBatchSink>,
    handle: crate::engine::SessionHandle,
    events: crate::engine::SessionSubscription,
}

async fn fixture(fail_settlement: bool) -> Fixture {
    let root = tempfile::tempdir().expect("workspace");
    let peer = Arc::new(Peer {
        fail_settlement,
        ..Peer::default()
    });
    let mut tools = ToolRegistry::new();
    for blocked in [false, true] {
        tools
            .register(Arc::new(RecoveryTool {
                blocked,
                peer: peer.clone(),
            }))
            .expect("tool");
    }
    let sink = Arc::new(FailNextBatchSink::default());
    sink.fail_tool_finished.store(true, Ordering::Release);
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[
                ("fast-call", "fast", json!({})),
                ("blocked-call", "blocked", json!({})),
            ],
            &[],
        ),
        stop_script("new turn after physical repair", &[]),
    ]));
    let mut configuration = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    configuration.event_sink = sink.clone();
    let handle = super::fixtures::history::spawn(configuration)
        .await
        .expect("actor");
    let events = handle.subscribe().expect("subscription");
    Fixture {
        _root: root,
        peer,
        sink,
        handle,
        events,
    }
}

async fn assert_deferred(fixture: &Fixture) {
    let Fixture {
        peer, sink, handle, ..
    } = fixture;
    handle
        .send_message("two parallel tools")
        .await
        .expect("turn");
    tokio::time::timeout(Duration::from_secs(2), peer.cancelled.notified())
        .await
        .expect("failed append cancels peer");
    // This request crosses the actor mailbox after the failed append and must
    // not observe a repaired projection while the physical peer remains owned.
    assert!(
        handle.snapshot().await.is_err(),
        "damaged projection is unavailable until repair"
    );
    assert!(!peer.settled.load(Ordering::Acquire));
    assert!(
        !sink
            .inner
            .events
            .lock()
            .expect("source")
            .iter()
            .any(|event| matches!(event.kind, PendingEvent::TurnFinished { .. }))
    );
}

#[tokio::test]
async fn failed_finished_append_waits_for_other_tools_before_repairing_turn() {
    let mut fixture = fixture(false).await;
    assert_deferred(&fixture).await;
    let Fixture {
        peer,
        handle,
        events,
        ..
    } = &mut fixture;
    peer.release.notify_one();
    let repaired = collect_turn(events).await;
    assert!(peer.settled.load(Ordering::Acquire));
    assert!(repaired.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    )));
    assert_eq!(
        repaired
            .iter()
            .filter(|event| matches!(
                event.kind,
                PendingEvent::ToolCallFinished { is_error: true, .. }
            ))
            .count(),
        2
    );
    handle
        .send_message("continue after repair")
        .await
        .expect("fresh turn");
    let next = collect_turn(events).await;
    assert!(next.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            turn: 2,
            status: AgentTurnStatus::Completed,
            ..
        }
    )));
    handle.close().await.expect("settled close");
}

#[tokio::test]
async fn failed_physical_tool_settlement_prevents_deferred_journal_repair() {
    let fixture = fixture(true).await;
    assert_deferred(&fixture).await;
    fixture.peer.release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), fixture.handle.close())
            .await
            .expect("failed proof is bounded")
            .is_err()
    );
    assert!(fixture.peer.settled.load(Ordering::Acquire));
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
}

#[tokio::test]
async fn close_during_deferred_repair_keeps_the_physical_peer_owned() {
    use futures_util::FutureExt;
    let mut fixture = fixture(false).await;
    assert_deferred(&fixture).await;
    let mut closing = Box::pin(fixture.handle.close());
    assert!(closing.as_mut().now_or_never().is_none());
    assert!(!fixture.peer.settled.load(Ordering::Acquire));
    fixture.peer.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), closing)
        .await
        .expect("close settlement")
        .expect("closed");
    assert!(fixture.peer.settled.load(Ordering::Acquire));
    let repaired = collect_turn(&mut fixture.events).await;
    assert!(repaired.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Interrupted,
            ..
        }
    )));
}

#[tokio::test]
async fn failed_repair_append_keeps_reconstructed_state_unavailable() {
    let fixture = fixture(false).await;
    assert_deferred(&fixture).await;
    fixture
        .sink
        .fail_interrupted_finish
        .store(true, Ordering::Release);
    fixture.peer.release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), fixture.handle.close())
            .await
            .expect("failed repair is bounded")
            .is_err()
    );
    assert!(
        !fixture.sink.fail_interrupted_finish.load(Ordering::Acquire),
        "repair append attempted"
    );
    assert!(
        fixture
            .handle
            .send_message("must not run after failed repair")
            .await
            .is_err()
    );
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
}
