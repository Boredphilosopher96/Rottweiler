#![allow(clippy::expect_used)]
use super::{ExtensionInvocations, MAX_EXTENSION_INVOCATIONS, endpoint};
use crate::{
    PluginConnection, PluginEndpoint, PluginEndpointMetadata, PluginProviderEventStream,
    PluginRpcClient, PluginRpcError,
};
use async_trait::async_trait;
use rw_tools::CancellationToken;
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

struct Endpoint {
    metadata: PluginEndpointMetadata,
    entered: Notify,
    release: Notify,
    completed: Notify,
    closes: AtomicUsize,
    fail: AtomicBool,
}
impl Endpoint {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            metadata: PluginEndpointMetadata::new(rw_plugin_protocol::PluginManifest {
                name: name.into(),
                version: "1.0.0".into(),
                protocol: rw_plugin_protocol::PROTOCOL_VERSION,
                capabilities: rw_plugin_protocol::PluginCapabilities::default(),
            })
            .expect("metadata"),
            entered: Notify::new(),
            release: Notify::new(),
            completed: Notify::new(),
            closes: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        })
    }
}
#[async_trait]
impl PluginEndpoint for Endpoint {
    fn metadata(&self) -> &PluginEndpointMetadata {
        &self.metadata
    }
    async fn connect(&self, _: &CancellationToken) -> Result<PluginConnection, PluginRpcError> {
        Err(super::error("approval_required", "fixture remains dormant"))
    }
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        Ok(())
    }
    async fn close(&self) -> Result<(), PluginRpcError> {
        self.closes.fetch_add(1, Ordering::AcqRel);
        self.entered.notify_one();
        self.release.notified().await;
        self.completed.notify_one();
        if self.fail.load(Ordering::Acquire) {
            Err(super::error("effects_unsettled", "seeded close failure"))
        } else {
            Ok(())
        }
    }
}
struct Client;
#[async_trait]
impl PluginRpcClient for Client {
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        Ok(())
    }
    async fn request(&self, _: &str, _: Value) -> Result<Value, PluginRpcError> {
        Ok(Value::Null)
    }
    async fn provider_stream(&self, _: Value) -> Result<PluginProviderEventStream, PluginRpcError> {
        Ok(Box::pin(futures_util::stream::pending()))
    }
}
fn raw(endpoint: &Arc<Endpoint>) -> Vec<Arc<dyn PluginEndpoint>> {
    vec![endpoint.clone()]
}
fn client(gate: &Arc<ExtensionInvocations>) -> Arc<dyn PluginRpcClient> {
    endpoint::wrap_client(
        Arc::downgrade(gate),
        gate.lock().generation.id,
        Arc::new(Client),
    )
}

#[tokio::test]
async fn dropped_waiter_keeps_retirement_owned_and_cached_clients_cannot_cross_generation() {
    let old = Endpoint::new("fixture");
    let gate = ExtensionInvocations::new(&raw(&old)).expect("coordinator");
    let cached = client(&gate);
    let waiter = tokio::spawn({
        let gate = Arc::clone(&gate);
        async move { gate.pause_and_settle().await }
    });
    old.entered.notified().await;
    assert!(
        cached
            .request("command/execute", Value::Null)
            .await
            .is_err()
    );
    waiter.abort();
    assert!(waiter.await.err().expect("waiter aborted").is_cancelled());
    assert_eq!(old.closes.load(Ordering::Acquire), 1);
    old.release.notify_one();
    let exclusive = gate
        .pause_and_settle()
        .await
        .expect("same owned retirement");
    assert_eq!(old.closes.load(Ordering::Acquire), 1);
    assert!(
        exclusive.prepare(&raw(&old)).is_err(),
        "retired endpoint cannot be republished"
    );
    let fresh = Endpoint::new("fixture");
    let prepared = exclusive.prepare(&raw(&fresh)).expect("fresh generation");
    assert!(
        prepared.endpoints().expect("inert bindings")[0]
            .connect(&CancellationToken::default())
            .await
            .err()
            .expect("candidate paused")
            .code
            .contains("generation")
    );
    exclusive.resume(prepared).expect("publish exact candidate");
    assert!(
        cached
            .request("command/execute", Value::Null)
            .await
            .is_err()
    );
    assert_eq!(
        client(&gate)
            .request("command/execute", Value::Null)
            .await
            .expect("new generation"),
        Value::Null
    );
    fresh.release.notify_one();
    drop(gate.pause_and_settle().await.expect("cleanup"));
}

#[tokio::test]
async fn endpoint_close_proof_does_not_release_live_stream_credits() {
    let endpoint = Endpoint::new("fixture");
    let gate = ExtensionInvocations::new(&raw(&endpoint)).expect("coordinator");
    let cached = client(&gate);
    let mut streams = Vec::new();
    for _ in 0..MAX_EXTENSION_INVOCATIONS {
        streams.push(
            cached
                .provider_stream(Value::Null)
                .await
                .expect("bounded stream"),
        );
    }
    assert_eq!(
        cached
            .provider_stream(Value::Null)
            .await
            .err()
            .expect("aggregate limit")
            .code,
        "busy"
    );
    let waiter = tokio::spawn({
        let gate = Arc::clone(&gate);
        async move { gate.pause_and_settle().await }
    });
    endpoint.entered.notified().await;
    endpoint.release.notify_one();
    endpoint.completed.notified().await;
    assert!(
        !waiter.is_finished(),
        "successful native close still requires admitted consumers to retire"
    );
    assert_eq!(gate.lock().active.len(), MAX_EXTENSION_INVOCATIONS);
    streams.clear();
    drop(waiter.await.expect("retirement task").expect("full proof"));
    assert!(
        cached
            .request("command/execute", Value::Null)
            .await
            .is_err(),
        "guard drop stays paused"
    );
}

#[tokio::test(start_paused = true)]
async fn retirement_deadline_is_sticky_and_does_not_drop_actual_closers() {
    let endpoint = Endpoint::new("fixture");
    let gate = ExtensionInvocations::new(&raw(&endpoint)).expect("coordinator");
    assert_eq!(
        gate.pause_and_settle().await.err().expect("deadline").code,
        "effects_unsettled"
    );
    assert_eq!(endpoint.closes.load(Ordering::Acquire), 1);
    endpoint.release.notify_one();
    endpoint.completed.notified().await;
    assert_eq!(
        gate.pause_and_settle()
            .await
            .err()
            .expect("sticky proof")
            .code,
        "effects_unsettled"
    );
    let weak = Arc::downgrade(&gate);
    drop(gate);
    assert!(
        weak.upgrade().is_some(),
        "failed generation owners remain quarantined"
    );
}

#[tokio::test]
async fn all_endpoints_close_even_when_one_proof_fails() {
    let first = Endpoint::new("first");
    let second = Endpoint::new("second");
    first.fail.store(true, Ordering::Release);
    first.release.notify_one();
    second.release.notify_one();
    let gate = ExtensionInvocations::new(&[first.clone(), second.clone()]).expect("coordinator");
    assert_eq!(
        gate.pause_and_settle()
            .await
            .err()
            .expect("failed proof")
            .code,
        "effects_unsettled"
    );
    assert_eq!(first.closes.load(Ordering::Acquire), 1);
    assert_eq!(second.closes.load(Ordering::Acquire), 1);
}
