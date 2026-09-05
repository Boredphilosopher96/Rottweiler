//! Runtime resources have one shutdown task independent of actor and caller futures.

use crate::extension_runtime::{McpSessionRuntime, PluginSessionRuntime};
use async_trait::async_trait;
use futures_util::FutureExt;
use std::{
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
        let (stop, wait) = oneshot::channel();
        let (finished, completion) = watch::channel(None);
        tokio::spawn(async move {
            let _ = wait.await;
            let work = async {
                let mut failure = None;
                if let Some(mcp) = &mcp
                    && let Err(error) = mcp.shutdown().await
                {
                    failure = Some(error.to_string());
                }
                if let Some(plugins) = &plugins
                    && let Err(error) = plugins.shutdown().await
                {
                    failure.get_or_insert_with(|| error.to_string());
                }
                failure.map_or(Ok(()), Err)
            };
            let result = match tokio::time::timeout(
                RESOURCE_PROOF_TIMEOUT,
                std::panic::AssertUnwindSafe(work).catch_unwind(),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err("session runtime cleanup panicked before proof".to_owned()),
                Err(_) => Err("session runtime cleanup proof deadline expired".to_owned()),
            };
            let failed = result.is_err();
            finished.send_replace(Some(result.map_err(Arc::from)));
            if failed {
                std::future::pending::<()>().await;
            }
            drop((mcp, plugins));
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
