//! Per-invocation ownership survives replacement of the selected model runtime.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, Stream};
use rw_core::{AgentLoopError, ModelDriver};
use rw_providers::{BoxEventStream, ProviderError, ProviderEvent};

const MAX_INVOCATIONS: usize = 64;
type Completion = Shared<BoxFuture<'static, Result<(), Arc<str>>>>;

#[derive(Clone, Default)]
pub(super) struct ModelEffects(Arc<Mutex<Vec<Arc<Invocation>>>>);

struct Invocation {
    driver: Arc<dyn ModelDriver>,
    dropped: AtomicBool,
    completion: Completion,
}

struct Cleanup {
    invocation: Arc<Invocation>,
    effects: ModelEffects,
    runtime: tokio::runtime::Handle,
    finished: Option<tokio::sync::oneshot::Sender<Result<(), Arc<str>>>>,
}

impl ModelEffects {
    pub(super) fn stream(
        &self,
        driver: Arc<dyn ModelDriver>,
        start: impl FnOnce(&dyn ModelDriver) -> Result<BoxEventStream, AgentLoopError>,
    ) -> Result<BoxEventStream, AgentLoopError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            AgentLoopError::InvalidConfiguration(
                "model invocation requires its runtime owner".to_owned(),
            )
        })?;
        let (finished, receive) = tokio::sync::oneshot::channel();
        let completion = async move {
            receive.await.unwrap_or_else(|_| {
                Err(Arc::from(
                    "model cleanup owner exited without settlement proof",
                ))
            })
        }
        .boxed()
        .shared();
        let invocation = Arc::new(Invocation {
            driver,
            dropped: AtomicBool::new(false),
            completion,
        });
        {
            let mut active = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for previous in active
                .iter()
                .filter(|previous| previous.dropped.load(Ordering::Acquire))
            {
                if let Some(Err(message)) = previous.completion.clone().now_or_never() {
                    return Err(AgentLoopError::EffectsUnsettled(message.to_string()));
                }
            }
            if active.len() >= MAX_INVOCATIONS {
                return Err(AgentLoopError::EffectsUnsettled(
                    "model invocation admission is saturated by active or unsettled work"
                        .to_owned(),
                ));
            }
            active.push(Arc::clone(&invocation));
        }
        let cleanup = Cleanup {
            invocation,
            effects: self.clone(),
            runtime,
            finished: Some(finished),
        };
        let inner = start(cleanup.invocation.driver.as_ref())?;
        Ok(Box::pin(OwnedModelStream {
            inner: Some(inner),
            cleanup: Some(cleanup),
        }))
    }

    pub(super) async fn settle(&self) -> Result<(), AgentLoopError> {
        let pending: Vec<_> = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|invocation| invocation.dropped.load(Ordering::Acquire))
            .map(|invocation| invocation.completion.clone())
            .collect();
        for result in futures_util::future::join_all(pending).await {
            result.map_err(|error| AgentLoopError::EffectsUnsettled(error.to_string()))?;
        }
        Ok(())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.invocation.dropped.store(true, Ordering::Release);
        let Some(finished) = self.finished.take() else {
            return;
        };
        let invocation = Arc::clone(&self.invocation);
        let effects = self.effects.clone();
        self.runtime.spawn(async move {
            let result = std::panic::AssertUnwindSafe(invocation.driver.settle_effects())
                .catch_unwind()
                .await
                .map_err(|_| Arc::<str>::from("model cleanup implementation panicked"))
                .and_then(|result| result.map_err(|error| Arc::<str>::from(error.to_string())));
            if result.is_ok() {
                effects
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|known| !Arc::ptr_eq(known, &invocation));
            }
            let _ = finished.send(result);
        });
    }
}

struct OwnedModelStream {
    inner: Option<BoxEventStream>,
    cleanup: Option<Cleanup>,
}

impl OwnedModelStream {
    fn finish(&mut self) {
        let cleanup = self.cleanup.take();
        // This local owner also runs if the inner stream's destructor panics.
        drop(self.inner.take());
        drop(cleanup);
    }
}

impl Stream for OwnedModelStream {
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        let result = inner.as_mut().poll_next(cx);
        if matches!(result, Poll::Ready(None)) {
            this.finish();
        }
        result
    }
}

impl Drop for OwnedModelStream {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests;
