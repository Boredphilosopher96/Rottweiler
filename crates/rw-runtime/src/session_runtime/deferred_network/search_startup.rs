//! The shared initialization future retains the blocking worker and its result.
use futures_util::{
    FutureExt as _,
    future::{BoxFuture, Shared},
};
use rw_tools::{ToolError, WebSearcher};
use std::sync::{Arc, Mutex};

type Result = std::result::Result<Arc<dyn WebSearcher>, Failure>;
#[derive(Clone)]
enum Failure {
    Rejected(String),
    Unsettled(String),
}
#[derive(Clone)]
pub(super) struct Startup(Shared<BoxFuture<'static, Result>>);
#[derive(Default)]
pub(super) struct SearchStartup {
    current: Mutex<Option<Startup>>,
}
impl SearchStartup {
    pub(super) fn current(&self) -> Option<Startup> {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(super) fn start(
        &self,
        build: impl FnOnce() -> std::result::Result<Arc<dyn WebSearcher>, String> + Send + 'static,
    ) -> Startup {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current
            .get_or_insert_with(|| {
                Startup(
                    async move {
                        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, build)
                            .await
                            .map_err(|error| {
                                Failure::Unsettled(format!("search startup worker failed: {error}"))
                            })?
                            .map_err(Failure::Rejected)
                    }
                    .boxed()
                    .shared(),
                )
            })
            .clone()
    }
    pub(super) async fn settle(&self) -> std::result::Result<(), ToolError> {
        let Some(startup) = self.current() else {
            return Ok(());
        };
        match startup.0.await {
            Ok(backend) => backend.settle_effects().await,
            Err(Failure::Rejected(_)) => Ok(()),
            Err(Failure::Unsettled(error)) => Err(ToolError::EffectsUnsettled(error)),
        }
    }
}
impl Startup {
    pub(super) async fn wait(self) -> std::result::Result<Arc<dyn WebSearcher>, ToolError> {
        self.0.await.map_err(|error| match error {
            Failure::Rejected(error) => ToolError::Network(error),
            Failure::Unsettled(error) => ToolError::EffectsUnsettled(error),
        })
    }
}

#[cfg(test)]
mod tests;
