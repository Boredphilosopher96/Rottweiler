//! Connection creation owns initialization and native cleanup across caller loss.
use super::{RmcpClient, ingress::Ingress};
use crate::{McpClient, McpError};
use rmcp::{ServiceExt as _, service::RoleClient, transport::Transport};
use rw_tools::ProtocolProcessHandle;
use rw_types::McpServerId;
use std::{sync::Arc, time::Duration};
use tokio::sync::oneshot;

struct Starting {
    child: Option<Box<dyn ProtocolProcessHandle>>,
    ingress: Arc<Ingress>,
    armed: bool,
}
impl Starting {
    async fn run<T: Transport<RoleClient> + 'static>(
        mut self,
        server: McpServerId,
        transport: T,
        mut result: oneshot::Sender<Result<Arc<dyn McpClient>, McpError>>,
    ) {
        let router = self.ingress.router.clone();
        let initialization = {
            let future = router.serve(transport);
            tokio::pin!(future);
            tokio::select! {
                () = result.closed() => None,
                initialized = &mut future => Some(initialized),
            }
        }; // The entire cancelled/error initialize future is destroyed here.
        let response = match initialization {
            Some(Ok(service)) => {
                let child = self.child.take();
                let client =
                    RmcpClient::new(server, service, child, Arc::clone(&self.ingress)).await;
                self.armed = false;
                let client: Arc<dyn McpClient> = Arc::new(client);
                if result.is_closed() {
                    let _ = client.close(Duration::from_secs(3)).await;
                    return;
                }
                Ok(client)
            }
            Some(Err(error)) => {
                // Initialize errors can contain the complete decoded result.
                // Destroy them while the outer ingress initialization guard lives.
                drop(error);
                let early_exit = if let Some(child) = &mut self.child {
                    child
                        .observe_exit(Duration::from_millis(50))
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                match self.cleanup().await {
                    Err(_) => Err(McpError::EffectsUnsettled {
                        server,
                        message: "MCP initialization failed without native process settlement"
                            .into(),
                    }),
                    Ok(()) => Err(early_exit.map_or_else(super::protocol_failure, |status| {
                        McpError::Protocol(format!(
                            "MCP process exited before protocol initialization ({status})"
                        ))
                    })),
                }
            }
            None => {
                let _ = self.cleanup().await;
                return;
            }
        };
        if let Err(Ok(client)) = result.send(response) {
            let _ = client.close(Duration::from_secs(3)).await;
        }
    }

    async fn cleanup(&mut self) -> Result<(), McpError> {
        self.ingress.close();
        self.ingress.jobs.settle().await;
        let result = if let Some(child) = self.child.take() {
            super::closure::retire_process(child, Duration::from_secs(3))
                .await
                .map_err(|_| super::protocol_failure())
        } else {
            Ok(())
        };
        self.armed = false;
        result
    }
}
impl Drop for Starting {
    fn drop(&mut self) {
        if self.armed {
            // Runtime loss is not a proof that transport/CPU/native work settled.
            // Preserve the actual remaining authority, including initialization.
            let owner = (self.child.take(), Arc::clone(&self.ingress));
            let _ = Box::leak(Box::new(owner));
        }
    }
}

pub(super) async fn start<T: Transport<RoleClient> + 'static>(
    server: McpServerId,
    transport: T,
    ingress: Arc<Ingress>,
    child: Option<Box<dyn ProtocolProcessHandle>>,
) -> Result<Arc<dyn McpClient>, McpError> {
    let (result, wait) = oneshot::channel();
    let owner = Starting {
        child,
        ingress,
        armed: true,
    };
    tokio::spawn(owner.run(server, transport, result));
    wait.await.map_err(|_| super::protocol_failure())?
}
