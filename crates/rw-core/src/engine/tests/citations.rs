#![cfg(test)]
use super::fixtures::support::{collect_turn, config};
use crate::engine::{
    AgentLoopError, AgentTurnStatus, ModelDriver, PendingEvent, SessionActor,
    builtin_hook_dispatcher,
};
use async_trait::async_trait;
use rw_providers::{BoxEventStream, ProviderEvent, ProviderRequest};
use rw_tools::ToolRegistry;
use rw_types::{citation_admission::MAX_TURN_CITATIONS, config::PermissionDecision};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;

#[derive(Default)]
struct CitationModel {
    settling: Notify,
    release: Notify,
    released: AtomicBool,
}
#[async_trait]
impl ModelDriver for CitationModel {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        self.settling.notify_one();
        while !self.released.load(Ordering::Acquire) {
            let notified = self.release.notified();
            if self.released.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
        Ok(())
    }
    fn stream(
        &self,
        _: &str,
        _: ProviderRequest,
        _: crate::provider_admission::ProviderInvocation,
    ) -> Result<BoxEventStream, AgentLoopError> {
        Ok(Box::pin(futures_util::stream::iter(
            (0..=MAX_TURN_CITATIONS).map(|index| {
                Ok(ProviderEvent::Citation {
                    uri: format!("https://example.test/{index}"),
                    title: None,
                    start_index: None,
                    end_index: None,
                })
            }),
        )))
    }
}

#[tokio::test]
async fn citation_overflow_stops_announcements_and_waits_for_provider_effects() {
    let root = tempfile::tempdir().expect("root");
    let model = Arc::new(CitationModel::default());
    let actor = SessionActor::spawn(config(
        root.path(),
        model.clone(),
        Arc::new(ToolRegistry::new()),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut subscription = actor.subscribe().expect("subscription");
    actor.send_message("cite sources").await.expect("message");
    tokio::time::timeout(std::time::Duration::from_secs(3), model.settling.notified())
        .await
        .expect("provider settlement started");
    assert!(actor.snapshot().await.expect("responsive snapshot").running);
    model.released.store(true, Ordering::Release);
    model.release.notify_waiters();
    let events = collect_turn(&mut subscription).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, PendingEvent::CitationDelta { .. }))
            .count(),
        MAX_TURN_CITATIONS
    );
    assert!(events.iter().any(|event| matches!(&event.kind, PendingEvent::Error { message } if message.contains("citation") && message.contains("admission"))));
    assert!(events.iter().any(|event| matches!(
        event.kind,
        PendingEvent::TurnFinished {
            status: AgentTurnStatus::Failed,
            ..
        }
    )));
    actor.close().await.expect("settlement");
}
