//! Managed clients cannot bypass generation checks by caching a connection.
use super::{ExtensionGenerationId, ExtensionInvocations, InvocationLease, error};
use crate::{
    PluginConnection, PluginEndpoint, PluginEndpointMetadata, PluginProviderEventStream,
    PluginRpcClient, PluginRpcError,
};
use async_trait::async_trait;
use futures_util::Stream;
use rw_tools::{CancellationToken, ToolProgressSink};
use serde_json::Value;
use std::{
    pin::Pin,
    sync::{Arc, Weak},
    task::{Context, Poll},
};

pub(super) struct ManagedEndpoint {
    gate: Weak<ExtensionInvocations>,
    generation: ExtensionGenerationId,
    inner: Arc<dyn PluginEndpoint>,
}
impl ManagedEndpoint {
    pub(super) fn new(
        gate: Weak<ExtensionInvocations>,
        generation: ExtensionGenerationId,
        inner: Arc<dyn PluginEndpoint>,
    ) -> Self {
        Self {
            gate,
            generation,
            inner,
        }
    }
}
#[async_trait]
impl PluginEndpoint for ManagedEndpoint {
    fn metadata(&self) -> &PluginEndpointMetadata {
        self.inner.metadata()
    }
    async fn connect(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<PluginConnection, PluginRpcError> {
        let gate = upgrade(&self.gate)?;
        let lease = gate.admit(self.generation)?;
        let connection = tokio::select! {
            ()=lease.cancellation.cancelled()=>Err(error("cancelled","extension generation is retiring")),
            result=self.inner.connect(cancellation)=>result,
        }?;
        let client = wrap_client(
            self.gate.clone(),
            self.generation,
            Arc::clone(connection.client()),
        );
        Ok(connection.with_client(client))
    }
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        let proof = self.inner.settle_effects().await;
        proof?;
        check_proof(&self.gate)
    }
    async fn close(&self) -> Result<(), PluginRpcError> {
        self.inner.close().await
    }
}
struct ManagedClient {
    gate: Weak<ExtensionInvocations>,
    generation: ExtensionGenerationId,
    inner: Arc<dyn PluginRpcClient>,
}
impl ManagedClient {
    fn admit(&self) -> Result<InvocationLease, PluginRpcError> {
        upgrade(&self.gate)?.admit(self.generation)
    }
}
#[async_trait]
impl PluginRpcClient for ManagedClient {
    async fn call_command(
        &self,
        params: rw_plugin_protocol::CommandExecuteParams,
        cancellation: &CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        let lease = self.admit()?;
        tokio::select! {
            ()=cancellation.cancelled()=>Err(error("cancelled","extension command was cancelled")),
            result=self.inner.call_command(params,&lease.cancellation)=>result,
        }
    }

    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        let proof = self.inner.settle_effects().await;
        proof?;
        check_proof(&self.gate)
    }
    async fn request(&self, method: &str, params: Value) -> Result<Value, PluginRpcError> {
        let lease = self.admit()?;
        self.inner
            .request_cancellable(method, params, &lease.cancellation)
            .await
    }
    async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        let lease = self.admit()?;
        tokio::select! {
            ()=cancellation.cancelled()=>Err(error("cancelled","extension request was cancelled")),
            result=self.inner.request_cancellable(method,params,&lease.cancellation)=>result,
        }
    }
    async fn call_tool(
        &self,
        params: rw_plugin_protocol::ToolCallParams,
        cancellation: &CancellationToken,
        progress: Arc<dyn ToolProgressSink>,
    ) -> Result<Value, PluginRpcError> {
        let lease = self.admit()?;
        tokio::select! {
            ()=cancellation.cancelled()=>Err(error("cancelled","extension tool was cancelled")),
            result=self.inner.call_tool(params,&lease.cancellation,progress)=>result,
        }
    }
    async fn notify(&self, method: &str, params: Value) -> Result<(), PluginRpcError> {
        let lease = self.admit()?;
        tokio::select! {
            ()=lease.cancellation.cancelled()=>Err(error("cancelled","extension notification was cancelled")),
            result=self.inner.notify(method,params)=>result,
        }
    }
    async fn provider_stream(
        &self,
        params: Value,
    ) -> Result<PluginProviderEventStream, PluginRpcError> {
        let lease = self.admit()?;
        let inner = tokio::select! {
            ()=lease.cancellation.cancelled()=>Err(error("cancelled","extension provider was cancelled")),
            result=self.inner.provider_stream(params)=>result,
        }?;
        Ok(Box::pin(ManagedStream {
            inner: Some(inner),
            lease: Some(lease),
        }))
    }
}
struct ManagedStream {
    inner: Option<PluginProviderEventStream>,
    lease: Option<InvocationLease>,
}
impl ManagedStream {
    fn finish(&mut self) {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.inner.take())));
        if result.is_err()
            && let Some(lease) = &self.lease
        {
            lease.gate.fail(error(
                "effects_unsettled",
                "extension stream destructor panicked",
            ));
        }
        drop(self.lease.take());
    }
}
impl Stream for ManagedStream {
    type Item = Result<Value, PluginRpcError>;
    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        let result = inner.as_mut().poll_next(context);
        if matches!(result, Poll::Ready(None)) {
            this.finish();
        }
        result
    }
}
impl Drop for ManagedStream {
    fn drop(&mut self) {
        self.finish();
    }
}
fn upgrade(gate: &Weak<ExtensionInvocations>) -> Result<Arc<ExtensionInvocations>, PluginRpcError> {
    gate.upgrade()
        .ok_or_else(|| error("closed", "extension invocation coordinator is unavailable"))
}

pub(super) fn wrap_client(
    gate: Weak<ExtensionInvocations>,
    generation: ExtensionGenerationId,
    inner: Arc<dyn PluginRpcClient>,
) -> Arc<dyn PluginRpcClient> {
    Arc::new(ManagedClient {
        gate,
        generation,
        inner,
    })
}

fn check_proof(gate: &Weak<ExtensionInvocations>) -> Result<(), PluginRpcError> {
    let gate = upgrade(gate)?;
    let failure = gate.lock().failure.clone();
    failure.map_or(Ok(()), Err)
}
