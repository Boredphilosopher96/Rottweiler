//! Invoke a connection under a bounded owner, including result encoding and spooling.
use super::{McpManager, operations};
use crate::{McpClient, McpError, ServerState};
use rw_types::McpServerId;
use std::{future::Future, sync::Arc};
use tokio::sync::oneshot;

impl McpManager {
    pub async fn settle_effects(&self) -> Result<(), McpError> {
        let deadline = tokio::time::Instant::now() + self.inner.limits.shutdown_timeout;
        self.inner
            .operations
            .settle(self.inner.limits.shutdown_timeout)
            .await?;
        let transitions = self
            .inner
            .servers
            .read()
            .await
            .values()
            .filter_map(|entry| entry.transition.as_ref())
            .filter(|transition| {
                transition.cancelled()
                    || matches!(
                        transition.result(),
                        Some(Err(McpError::EffectsUnsettled { .. }))
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        for transition in transitions {
            let result = tokio::time::timeout_at(deadline, transition.completed()).await;
            if matches!(result, Err(_) | Ok(Err(McpError::EffectsUnsettled { .. }))) {
                return Err(operations::unsettled(&McpServerId::from_static(
                    "connection",
                )));
            }
        }
        Ok(())
    }

    pub(super) async fn invoke<T: Send + 'static>(
        &self,
        server: &McpServerId,
        client: Arc<dyn McpClient>,
        future: impl Future<Output = Result<T, McpError>> + Send + 'static,
    ) -> Result<T, McpError> {
        let (owner, mut caller, mut cancelled) = self.inner.operations.admit(server)?;
        let manager = self.clone();
        let id = server.clone();
        let (reply, response) = oneshot::channel();
        // `owner` exists before spawn, so even an unpolled or panicking task fails closed.
        tokio::spawn(async move {
            tokio::pin!(future);
            let deadline = tokio::time::sleep(manager.inner.limits.request_timeout);
            tokio::pin!(deadline);
            let mut reply = Some(reply);
            let result = tokio::select! {
                biased;
                _ = cancelled.changed() => {
                    owner.cancel();
                    if let Some(reply) = reply.take() { let _ = reply.send(Err(operations::unsettled(&id))); }
                    None
                },
                () = &mut deadline => {
                    owner.cancel();
                    if let Some(reply) = reply.take() { let _ = reply.send(Err(operations::unsettled(&id))); }
                    None
                }
                result = &mut future => Some(result),
            };
            if let Some(result) = result {
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
                owner.finish(true);
                return;
            }
            owner.cancel();
            // Cancellation cannot drop an invocation that still owns host or process effects.
            let _ = future.await;
            let proven = manager
                .retire_invocation_connection(&id, &client)
                .await
                .is_ok();
            if let Some(reply) = reply {
                let _ = reply.send(Err(operations::unsettled(&id)));
            }
            owner.finish(proven);
        });
        let result = response.await.map_err(|_| operations::unsettled(server));
        caller.disarm();
        result?
    }

    async fn retire_invocation_connection(
        &self,
        server: &McpServerId,
        client: &Arc<dyn McpClient>,
    ) -> Result<(), McpError> {
        {
            let mut servers = self.inner.servers.write().await;
            if let Some(entry) = servers.get_mut(server)
                && entry
                    .client
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, client))
            {
                entry.client = None;
                entry.state = ServerState::Failed {
                    message: "MCP connection retired after an interrupted invocation".to_owned(),
                };
            }
        }
        client.close(self.inner.limits.shutdown_timeout).await
    }
}
