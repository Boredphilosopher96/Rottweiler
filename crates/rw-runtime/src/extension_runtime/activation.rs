//! One owned activation and retirement operation per immutable plugin generation.
mod budget;
mod recipe;

use async_trait::async_trait;
pub(crate) use budget::PluginActivationBudget;
use budget::{ACTIVATION_DEADLINE, ActivationLease};
use futures_util::FutureExt as _;
use recipe::ActivationResources;
pub(super) use recipe::{ActivationApproval, ActivationRecipe};
use rw_ext::{
    PluginConnection, PluginEndpoint, PluginEndpointMetadata, PluginHost, PluginRpcError,
};
use rw_tools::CancellationToken;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

const PROOF_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone)]
enum Phase {
    Dormant,
    Starting {
        deadline: Instant,
    },
    Ready(Arc<PluginHost>),
    Closing,
    Closed {
        request: PluginRpcError,
        proof: Result<(), PluginRpcError>,
    },
}

struct Generation {
    recipe: ActivationRecipe,
    phase: Mutex<Phase>,
    resources: Mutex<ActivationResources>,
    changed: Notify,
    cancellation: CancellationToken,
}

pub(super) struct DormantPluginEndpoint {
    generation: Arc<Generation>,
}

impl DormantPluginEndpoint {
    pub(super) fn new(recipe: ActivationRecipe) -> Self {
        Self {
            generation: Arc::new(Generation {
                recipe,
                phase: Mutex::new(Phase::Dormant),
                resources: Mutex::new(ActivationResources::default()),
                changed: Notify::new(),
                cancellation: CancellationToken::default(),
            }),
        }
    }
}

impl Drop for DormantPluginEndpoint {
    fn drop(&mut self) {
        self.generation.begin_close();
    }
}

struct ActivationWaiter {
    generation: Arc<Generation>,
    armed: bool,
}
impl Drop for ActivationWaiter {
    fn drop(&mut self) {
        if self.armed {
            self.generation.begin_close();
        }
    }
}

#[async_trait]
impl PluginEndpoint for DormantPluginEndpoint {
    fn metadata(&self) -> &PluginEndpointMetadata {
        &self.generation.recipe.metadata
    }

    async fn connect(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<PluginConnection, PluginRpcError> {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let _waiter_slot = self.generation.recipe.budget.waiter()?;
        self.generation.begin_activation()?;
        let mut waiter = ActivationWaiter {
            generation: Arc::clone(&self.generation),
            armed: true,
        };
        loop {
            let changed = self.generation.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let phase = self.generation.snapshot();
            let deadline = match phase {
                Phase::Ready(host) => {
                    waiter.armed = false;
                    return Ok(PluginConnection::from_host(&host));
                }
                Phase::Closed { request, proof } => {
                    waiter.armed = false;
                    return Err(proof.err().unwrap_or(request));
                }
                Phase::Starting { deadline } => deadline,
                Phase::Closing => {
                    let proof = self.generation.wait_closed().await;
                    waiter.armed = false;
                    return Err(proof.err().unwrap_or_else(cancelled));
                }
                Phase::Dormant => return Err(error("closed", "plugin activation did not start")),
            };
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    self.generation.begin_close();
                    let proof = self.generation.wait_closed().await;
                    waiter.armed = false;
                    return Err(proof.err().unwrap_or_else(cancelled));
                }
                () = tokio::time::sleep_until(deadline) => {
                    self.generation.begin_close();
                    let proof = self.generation.wait_closed().await;
                    waiter.armed = false;
                    return Err(proof.err().unwrap_or_else(|| error("timeout", "plugin activation deadline expired")));
                }
                () = &mut changed => {}
            }
        }
    }

    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        match self.generation.snapshot() {
            Phase::Dormant => Ok(()),
            Phase::Ready(host) => host.client().settle_effects().await,
            Phase::Starting { .. } | Phase::Closing => {
                self.generation.begin_close();
                self.generation.wait_closed().await
            }
            Phase::Closed { proof, .. } => proof,
        }
    }

    async fn close(&self) -> Result<(), PluginRpcError> {
        self.generation.begin_close();
        self.generation.wait_closed().await
    }
}

impl Generation {
    fn snapshot(&self) -> Phase {
        self.phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn begin_activation(self: &Arc<Self>) -> Result<(), PluginRpcError> {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*phase, Phase::Dormant) {
            return Ok(());
        }
        let lease = self.recipe.budget.admit()?;
        let deadline = Instant::now() + ACTIVATION_DEADLINE;
        self.resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lease = Some(lease);
        *phase = Phase::Starting { deadline };
        let owner = OperationOwner {
            generation: Arc::clone(self),
            armed: true,
        };
        drop(phase);
        spawn_owned(owner.activate(deadline));
        Ok(())
    }

    fn begin_close(self: &Arc<Self>) {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cancellation.cancel();
        match *phase {
            Phase::Dormant => {
                *phase = Phase::Closed {
                    request: cancelled(),
                    proof: Ok(()),
                };
                self.changed.notify_waiters();
            }
            Phase::Ready(_) => {
                *phase = Phase::Closing;
                let owner = OperationOwner {
                    generation: Arc::clone(self),
                    armed: true,
                };
                drop(phase);
                spawn_owned(owner.retire(cancelled()));
            }
            Phase::Starting { .. } | Phase::Closing | Phase::Closed { .. } => {}
        }
    }

    async fn wait_closed(&self) -> Result<(), PluginRpcError> {
        let deadline = Instant::now() + PROOF_DEADLINE;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Phase::Closed { proof, .. } = self.snapshot() {
                return proof;
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    let failure = unsettled("plugin activation retirement proof deadline expired; owner remains charged");
                    self.resources.lock().unwrap_or_else(std::sync::PoisonError::into_inner).failure.get_or_insert_with(|| failure.clone());
                    return Err(failure);
                },
                () = &mut changed => {}
            }
        }
    }

    fn finish(&self, request: PluginRpcError, proof: Result<(), PluginRpcError>) {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Phase::Closed { request, proof };
        self.changed.notify_waiters();
    }
}

struct OperationOwner {
    generation: Arc<Generation>,
    armed: bool,
}
impl OperationOwner {
    async fn activate(mut self, deadline: Instant) {
        let generation = Arc::clone(&self.generation);
        let activation = recipe::activate(&generation, deadline);
        tokio::pin!(activation);
        let result = std::panic::AssertUnwindSafe(async {
            tokio::select! {
                biased;
                result = &mut activation => result,
                () = generation.cancellation.cancelled() => activation.await,
                () = tokio::time::sleep_until(deadline) => {
                    generation.cancellation.cancel();
                    activation.await
                }
            }
        })
        .catch_unwind()
        .await;
        let result = result.unwrap_or_else(|_| {
            let failure = unsettled("plugin activation panicked");
            generation
                .resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .failure = Some(failure.clone());
            Err(failure)
        });
        if let Ok(host) = result.as_ref() {
            let mut phase = generation
                .phase
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !generation.cancellation.is_cancelled() {
                generation
                    .resources
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .publish();
                *phase = Phase::Ready(Arc::clone(host));
                self.armed = false;
                tracing::debug!(plugin = %generation.recipe.config.name, elapsed_ms = (Instant::now() - (deadline - ACTIVATION_DEADLINE)).as_secs_f64() * 1000.0, "plugin activation ready");
                generation.changed.notify_waiters();
                return;
            }
        }
        self.retire(result.err().unwrap_or_else(cancelled)).await;
    }

    async fn retire(mut self, request: PluginRpcError) {
        let started = Instant::now();
        let proof = recipe::retire(&self.generation).await;
        tracing::debug!(plugin = %self.generation.recipe.config.name, request_error = %request.code, settled = proof.is_ok(), elapsed_ms = started.elapsed().as_secs_f64() * 1000.0, "plugin activation retired");
        self.generation.finish(request, proof.clone());
        if proof.is_err() {
            // Failed physical proof keeps the actual owners and their permits.
            std::mem::forget(Arc::clone(&self.generation));
        }
        self.armed = false;
    }
}
impl Drop for OperationOwner {
    fn drop(&mut self) {
        if self.armed {
            self.generation.cancellation.cancel();
            let failure =
                unsettled("plugin activation owner was dropped before proving settlement");
            let mut resources = self
                .generation
                .resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let proof = if resources.effects_started {
                std::mem::forget(Arc::clone(&self.generation));
                Err(failure.clone())
            } else {
                resources.settled();
                Ok(())
            };
            drop(resources);
            self.generation.finish(failure, proof);
        }
    }
}

fn error(code: &str, message: &str) -> PluginRpcError {
    PluginRpcError {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}
fn cancelled() -> PluginRpcError {
    error("cancelled", "plugin generation is closed")
}
fn unsettled(message: &str) -> PluginRpcError {
    error("effects_unsettled", message)
}

fn spawn_owned(future: impl std::future::Future<Output = ()> + Send + 'static) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(future);
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "macos"))]
mod native_tests;

#[cfg(test)]
mod hook_grants_tests;
