//! One owned activation and retirement operation per immutable plugin generation.
mod recipe;

use super::PluginRuntimeBudget;
use super::budget::{ACTIVATION_DEADLINE, ActivationLease};
use async_trait::async_trait;
use futures_util::FutureExt as _;
use recipe::ActivationResources;
pub(super) use recipe::{ActivationApproval, ActivationRecipe};
use rw_ext::{
    PluginConnection, PluginEndpoint, PluginEndpointMetadata, PluginHost, PluginRpcError,
};
use rw_tools::CancellationToken;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

const PROOF_DEADLINE: Duration = rw_ext::PLUGIN_ACTIVATION_SETTLEMENT_TIMEOUT;

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
    fn is_ready(&self) -> bool {
        matches!(self.generation.snapshot(), Phase::Ready(_))
    }
    fn metadata(&self) -> &PluginEndpointMetadata {
        &self.generation.recipe.metadata
    }

    async fn connect(
        &self,
        activation: &rw_ext::PluginActivation,
    ) -> Result<PluginConnection, PluginRpcError> {
        if activation.is_cancelled() {
            return Err(cancelled());
        }
        let _waiter_slot = self.generation.recipe.budget.waiter()?;
        self.generation.begin_activation(activation.deadline())?;
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
                Phase::Starting { deadline } => deadline.min(activation.deadline()),
                Phase::Closing => {
                    let proof = self.generation.wait_closed().await;
                    waiter.armed = false;
                    return Err(proof.err().unwrap_or_else(cancelled));
                }
                Phase::Dormant => return Err(error("closed", "plugin activation did not start")),
            };
            tokio::select! {
                biased;
                () = activation.cancellation().cancelled() => {
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

    fn begin_activation(self: &Arc<Self>, deadline: Instant) -> Result<(), PluginRpcError> {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*phase, Phase::Dormant) {
            return Ok(());
        }
        let lease = self.recipe.budget.admit()?;
        let started = Instant::now();
        let deadline = deadline.min(started + ACTIVATION_DEADLINE);
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
        spawn_owned(owner.activate(deadline, started));
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

enum ActivationOutcome<T> {
    Completed(T),
    Revoked {
        completed: T,
        request: PluginRpcError,
    },
}

async fn await_activation<T>(
    cancellation: &CancellationToken,
    deadline: Instant,
    activation: impl Future<Output = T>,
) -> ActivationOutcome<T> {
    tokio::pin!(activation);
    tokio::select! {
        biased;
        () = tokio::time::sleep_until(deadline) => {
            cancellation.cancel();
            ActivationOutcome::Revoked {
                completed: activation.await,
                request: timed_out(),
            }
        }
        () = cancellation.cancelled() => ActivationOutcome::Revoked {
            completed: activation.await,
            request: cancelled(),
        },
        completed = &mut activation => ActivationOutcome::Completed(completed),
    }
}

impl OperationOwner {
    async fn activate(mut self, deadline: Instant, started: Instant) {
        let generation = Arc::clone(&self.generation);
        let result = std::panic::AssertUnwindSafe(async {
            await_activation(
                &generation.cancellation,
                deadline,
                recipe::activate(&generation, deadline),
            )
            .await
        })
        .catch_unwind()
        .await;
        let outcome = result.unwrap_or_else(|_| {
            let failure = unsettled("plugin activation panicked");
            generation
                .resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .failure = Some(failure.clone());
            ActivationOutcome::Completed(Err(failure))
        });
        let (result, mut request) = match outcome {
            ActivationOutcome::Completed(result) => (result, None),
            ActivationOutcome::Revoked { completed, request } => (completed, Some(request)),
        };
        if request.is_none()
            && let Ok(host) = result.as_ref()
        {
            let mut phase = generation
                .phase
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if Instant::now() >= deadline {
                generation.cancellation.cancel();
                request = Some(timed_out());
            } else if generation.cancellation.is_cancelled() {
                request = Some(cancelled());
            } else {
                generation
                    .resources
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .publish();
                *phase = Phase::Ready(Arc::clone(host));
                self.armed = false;
                tracing::debug!(plugin = %generation.recipe.config.name, elapsed_ms = started.elapsed().as_secs_f64() * 1000.0, "plugin activation ready");
                generation.changed.notify_waiters();
                return;
            }
        }
        self.retire(request.unwrap_or_else(|| result.err().unwrap_or_else(cancelled)))
            .await;
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
fn timed_out() -> PluginRpcError {
    error("timeout", "plugin activation deadline expired")
}
pub(super) fn unsettled(message: &str) -> PluginRpcError {
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
