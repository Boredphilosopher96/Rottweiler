use std::{
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use rw_tools::CancellationToken;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use super::{HookError, HookHandler};

pub(super) type Cleanup = Shared<BoxFuture<'static, Result<(), HookError>>>;

pub(super) struct HookRuntime {
    admission: Arc<Semaphore>,
    cleanup: Mutex<Option<Cleanup>>,
}

impl Default for HookRuntime {
    fn default() -> Self {
        Self {
            admission: Arc::new(Semaphore::new(1)),
            cleanup: Mutex::default(),
        }
    }
}

impl HookRuntime {
    pub(super) fn close_admission(&self) {
        self.admission.close();
    }
    #[tracing::instrument(
        target = "rw_performance",
        level = "trace",
        name = "hook.admission_wait",
        skip_all
    )]
    pub(super) async fn admit(
        self: &Arc<Self>,
        handler: Arc<dyn HookHandler>,
        deadline: Instant,
    ) -> Result<Invocation, HookError> {
        let permit = tokio::time::timeout_at(deadline, Arc::clone(&self.admission).acquire_owned())
            .await
            .map_err(|_| {
                self.close_admission();
                HookError::new(
                    "effects_unsettled",
                    "previous hook invocation has not settled before admission deadline",
                )
            })?
            .map_err(|_| HookError::new("effects_unsettled", "hook admission is closed"))?;
        Ok(Invocation {
            runtime: Arc::clone(self),
            handler,
            permit: Some(permit),
            cancellation: CancellationToken::default(),
        })
    }

    #[tracing::instrument(
        target = "rw_performance",
        level = "trace",
        name = "hook.settlement",
        skip_all
    )]
    pub(super) async fn settle(&self, deadline: Instant) -> Result<(), HookError> {
        let permit = tokio::time::timeout_at(deadline, self.admission.acquire())
            .await
            .map_err(|_| {
                HookError::new(
                    "effects_unsettled",
                    "hook invocation or cleanup remains active",
                )
            })?
            .map_err(|_| HookError::new("effects_unsettled", "hook effect settlement failed"))?;
        let cleanup = self
            .cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let result = match cleanup {
            Some(cleanup) => tokio::time::timeout_at(deadline, cleanup)
                .await
                .map_err(|_| {
                    HookError::new(
                        "effects_unsettled",
                        "hook effect settlement deadline elapsed",
                    )
                })?,
            None => Ok(()),
        };
        drop(permit);
        result
    }
}

pub(super) struct Invocation {
    runtime: Arc<HookRuntime>,
    handler: Arc<dyn HookHandler>,
    permit: Option<OwnedSemaphorePermit>,
    pub(super) cancellation: CancellationToken,
}

impl Invocation {
    pub(super) fn finish(&mut self) -> Option<Cleanup> {
        let permit = self.permit.take()?;
        let handler = Arc::clone(&self.handler);
        let admission = Arc::clone(&self.runtime.admission);
        let mut slot = self
            .runtime
            .cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let task = tokio::spawn(async move {
            let result = AssertUnwindSafe(async { handler.settle_effects().await })
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(HookError::new(
                        "effects_unsettled",
                        "hook effect settlement panicked",
                    ))
                });
            if result.is_err() {
                admission.close();
            }
            drop(permit);
            result.map_err(|error| HookError::new("effects_unsettled", error.to_string()))
        });
        let cleanup = async move {
            task.await.map_err(|error| {
                HookError::new(
                    "effects_unsettled",
                    format!("hook settlement task failed: {error}"),
                )
            })?
        }
        .boxed()
        .shared();
        *slot = Some(cleanup.clone());
        Some(cleanup)
    }
}

impl Drop for Invocation {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let _ = self.finish();
    }
}
