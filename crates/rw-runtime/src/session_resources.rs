//! Runtime resources have one shutdown task independent of actor and caller futures.

use crate::extension_runtime::{McpSessionRuntime, PluginSessionRuntime};
use async_trait::async_trait;
use futures_util::FutureExt;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{oneshot, watch};

const RESOURCE_PROOF_TIMEOUT: Duration = Duration::from_secs(30);
type Proof = Result<(), Arc<str>>;

pub(crate) struct RuntimeSessionResources {
    stop: Mutex<Option<oneshot::Sender<()>>>,
    completion: watch::Receiver<Option<Proof>>,
}

impl RuntimeSessionResources {
    pub(crate) fn new(
        mcp: Option<Arc<McpSessionRuntime>>,
        plugins: Option<Arc<PluginSessionRuntime>>,
    ) -> Arc<Self> {
        let retained = (mcp.clone(), plugins.clone());
        Self::start(
            retained,
            settle_both(
                async move {
                    if let Some(mcp) = mcp {
                        mcp.shutdown().await
                    } else {
                        Ok(())
                    }
                },
                async move {
                    if let Some(plugins) = plugins {
                        plugins.shutdown().await
                    } else {
                        Ok(())
                    }
                },
            ),
            RESOURCE_PROOF_TIMEOUT,
        )
    }

    fn start(
        owners: impl Send + 'static,
        work: impl Future<Output = Proof> + Send + 'static,
        timeout: Duration,
    ) -> Arc<Self> {
        let (stop, wait) = oneshot::channel();
        let (finished, completion) = watch::channel(None);
        tokio::spawn(async move {
            let _ = wait.await;
            let mut work = Box::pin(std::panic::AssertUnwindSafe(work).catch_unwind());
            let (result, timed_out) = match tokio::time::timeout(timeout, &mut work).await {
                Ok(Ok(result)) => (result, false),
                Ok(Err(_)) => (
                    Err(Arc::from("session runtime cleanup panicked before proof")),
                    false,
                ),
                Err(_) => (
                    Err(Arc::from("session runtime cleanup proof deadline expired")),
                    true,
                ),
            };
            let failed = result.is_err();
            finished.send_replace(Some(result));
            if timed_out {
                // The future may now own clients removed from a service registry.
                // Continue polling that exact future rather than dropping its owners.
                let _ = work.await;
            }
            if failed {
                std::future::pending::<()>().await;
            }
            drop(owners);
        });
        Arc::new(Self {
            stop: Mutex::new(Some(stop)),
            completion,
        })
    }
    fn request(&self) {
        if let Some(stop) = self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = stop.send(());
        }
    }
}

#[async_trait]
impl rw_core::SessionResources for RuntimeSessionResources {
    async fn shutdown(&self) -> Result<(), rw_core::AgentLoopError> {
        let mut completion = self.completion.clone();
        self.request();
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result.map_err(|message| {
                    rw_core::AgentLoopError::EffectsUnsettled(message.to_string())
                });
            }
            completion.changed().await.map_err(|_| {
                rw_core::AgentLoopError::EffectsUnsettled(
                    "runtime resource owner exited without shutdown proof".to_owned(),
                )
            })?;
        }
    }
}
impl Drop for RuntimeSessionResources {
    fn drop(&mut self) {
        self.request();
    }
}

pub(crate) struct SessionResourcePair(pub(crate) [Arc<RuntimeSessionResources>; 2]);
#[async_trait]
impl rw_core::SessionResources for SessionResourcePair {
    async fn shutdown(&self) -> Result<(), rw_core::AgentLoopError> {
        for owner in &self.0 {
            owner.request();
        }
        let (first, second) = tokio::join!(
            rw_core::SessionResources::shutdown(self.0[0].as_ref()),
            rw_core::SessionResources::shutdown(self.0[1].as_ref())
        );
        first.and(second)
    }
}

async fn settle_both(
    mcp: impl Future<Output = miette::Result<()>>,
    plugins: impl Future<Output = miette::Result<()>>,
) -> Proof {
    let (mcp, plugins) = tokio::join!(settle_one("MCP", mcp), settle_one("plugin", plugins));
    mcp.and(plugins)
}

async fn settle_one(kind: &str, work: impl Future<Output = miette::Result<()>>) -> Proof {
    std::panic::AssertUnwindSafe(work)
        .catch_unwind()
        .await
        .map_err(|_| Arc::<str>::from(format!("{kind} cleanup panicked before proof")))?
        .map_err(|error| Arc::from(error.to_string()))
}

#[cfg(test)]
mod tests;
