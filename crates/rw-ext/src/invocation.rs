//! Session-wide admission and explicit retirement of one native extension generation.
//!
//! Releasing a request lease is not effect proof. Exclusive access additionally
//! requires every raw endpoint to close successfully before any generation resumes.
mod endpoint;
mod retirement;
#[cfg(test)]
mod tests;

use crate::{PluginEndpoint, PluginRpcError};
use rw_tools::CancellationToken;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{Notify, watch};

pub const MAX_EXTENSION_ENDPOINTS: usize = 64;
pub const MAX_EXTENSION_INVOCATIONS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtensionGenerationId(u64);

struct Generation {
    id: ExtensionGenerationId,
    endpoints: Vec<Arc<dyn PluginEndpoint>>,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Open,
    Retiring,
    Exclusive,
    Failed,
}
struct State {
    generation: Arc<Generation>,
    candidate: Option<Arc<Generation>>,
    phase: Phase,
    claimed: bool,
    next_invocation: u64,
    active: BTreeMap<u64, CancellationToken>,
    failure: Option<PluginRpcError>,
    retirement: Option<watch::Sender<Option<Result<(), PluginRpcError>>>>,
    retirement_deadline: Option<tokio::time::Instant>,
}

/// One owner shared by every configured and development endpoint in a session.
pub struct ExtensionInvocations {
    state: Mutex<State>,
    changed: Notify,
}
impl ExtensionInvocations {
    /// Creates an inert generation. Returned wrappers must be the only endpoints
    /// supplied to tool, hook, command, event, provider and UI adapters.
    /// # Errors
    /// Rejects duplicate plugin identities and excessive endpoint cardinality.
    pub fn new(endpoints: &[Arc<dyn PluginEndpoint>]) -> Result<Arc<Self>, PluginRpcError> {
        validate(endpoints)?;
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                generation: Arc::new(Generation {
                    id: ExtensionGenerationId(1),
                    endpoints: endpoints.to_vec(),
                }),
                candidate: None,
                phase: Phase::Open,
                claimed: false,
                next_invocation: 0,
                active: BTreeMap::new(),
                failure: None,
                retirement: None,
                retirement_deadline: None,
            }),
            changed: Notify::new(),
        }))
    }
    /// Resolves the current inert wrapper set without activating any process.
    /// # Errors
    /// Rejects access during retirement or failed proof.
    pub fn endpoints(self: &Arc<Self>) -> Result<Vec<Arc<dyn PluginEndpoint>>, PluginRpcError> {
        let state = self.lock();
        if state.phase != Phase::Open {
            return Err(unavailable(&state));
        }
        Ok(wrap(self, &state.generation))
    }
    fn admit(
        self: &Arc<Self>,
        generation: ExtensionGenerationId,
    ) -> Result<InvocationLease, PluginRpcError> {
        let mut state = self.lock();
        if state.phase != Phase::Open || state.generation.id != generation {
            return Err(unavailable(&state));
        }
        if state.active.len() >= MAX_EXTENSION_INVOCATIONS {
            return Err(error(
                "busy",
                "session extension invocation admission is saturated",
            ));
        }
        let id = state.next_invocation.checked_add(1).ok_or_else(|| {
            error(
                "exhausted",
                "extension invocation identity space is exhausted",
            )
        })?;
        state.next_invocation = id;
        let cancellation = CancellationToken::default();
        state.active.insert(id, cancellation.clone());
        Ok(InvocationLease {
            gate: Arc::clone(self),
            id,
            cancellation,
        })
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn fail(self: &Arc<Self>, failure: PluginRpcError) {
        let first = {
            let mut state = self.lock();
            if state.phase == Phase::Failed {
                false
            } else {
                state.phase = Phase::Failed;
                state.failure = Some(failure.clone());
                if let Some(retirement) = &state.retirement {
                    retirement.send_replace(Some(Err(failure)));
                }
                for cancellation in state.active.values() {
                    cancellation.cancel();
                }
                true
            }
        };
        self.changed.notify_waiters();
        if first {
            // Actual endpoints and charged native owners remain retained after a
            // failed close, panic, abandoned replacement or proof deadline.
            let retained = Arc::clone(self);
            tokio::spawn(async move {
                std::future::pending::<()>().await;
                drop(retained);
            });
        }
    }
}

struct InvocationLease {
    gate: Arc<ExtensionInvocations>,
    id: u64,
    cancellation: CancellationToken,
}
impl Drop for InvocationLease {
    fn drop(&mut self) {
        self.gate.lock().active.remove(&self.id);
        self.gate.changed.notify_waiters();
    }
}

/// Exclusive retirement proof. Dropping it never restores admission.
pub struct ExclusiveInvocationGuard {
    gate: Arc<ExtensionInvocations>,
    generation: ExtensionGenerationId,
    resumed: bool,
}
/// Inert candidate bindings retained by the exclusive owner before publication.
pub struct PreparedExtensionGeneration {
    gate: Weak<ExtensionInvocations>,
    generation: Arc<Generation>,
}
impl PreparedExtensionGeneration {
    #[must_use]
    pub fn id(&self) -> ExtensionGenerationId {
        self.generation.id
    }
    /// Returns inert candidates. Calls reject until the exact guard resumes them.
    /// # Errors
    /// Rejects use after its coordinator is gone.
    pub fn endpoints(&self) -> Result<Vec<Arc<dyn PluginEndpoint>>, PluginRpcError> {
        let gate = self
            .gate
            .upgrade()
            .ok_or_else(|| error("closed", "extension coordinator is unavailable"))?;
        Ok(wrap(&gate, &self.generation))
    }
}
impl ExclusiveInvocationGuard {
    /// Stages one complete replacement; no candidate can run before publication.
    /// # Errors
    /// Rejects duplicate identities, reused retired endpoints, or a second candidate.
    pub fn prepare(
        &self,
        endpoints: &[Arc<dyn PluginEndpoint>],
    ) -> Result<PreparedExtensionGeneration, PluginRpcError> {
        validate(endpoints)?;
        let mut state = self.gate.lock();
        if state.phase != Phase::Exclusive
            || state.generation.id != self.generation
            || state.candidate.is_some()
        {
            return Err(unavailable(&state));
        }
        if endpoints.iter().any(|candidate| {
            state
                .generation
                .endpoints
                .iter()
                .any(|retired| candidate.metadata().ui_owner() == retired.metadata().ui_owner())
        }) {
            return Err(error(
                "retired_generation",
                "replacement must own fresh endpoint generations",
            ));
        }
        let generation = Arc::new(Generation {
            id: ExtensionGenerationId(
                self.generation
                    .0
                    .checked_add(1)
                    .ok_or_else(|| error("exhausted", "extension generation space exhausted"))?,
            ),
            endpoints: endpoints.to_vec(),
        });
        state.candidate = Some(Arc::clone(&generation));
        Ok(PreparedExtensionGeneration {
            gate: Arc::downgrade(&self.gate),
            generation,
        })
    }
    /// Publishes exactly the staged generation after its host-side commit.
    /// # Errors
    /// Rejects mismatched, failed or unowned candidates.
    pub fn resume(mut self, candidate: PreparedExtensionGeneration) -> Result<(), PluginRpcError> {
        {
            let mut state = self.gate.lock();
            if state.phase != Phase::Exclusive
                || state.generation.id != self.generation
                || !candidate.gate.ptr_eq(&Arc::downgrade(&self.gate))
                || !state
                    .candidate
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &candidate.generation))
            {
                return Err(unavailable(&state));
            }
            state.generation = candidate.generation;
            state.candidate = None;
            state.phase = Phase::Open;
            state.claimed = false;
            state.retirement = None;
            state.retirement_deadline = None;
        }
        self.resumed = true;
        self.gate.changed.notify_waiters();
        Ok(())
    }
}
impl Drop for ExclusiveInvocationGuard {
    fn drop(&mut self) {
        if !self.resumed {
            let candidate = self.gate.lock().candidate.clone();
            if let Some(candidate) = candidate {
                // Closing is independently owned; failure cannot release the
                // retained candidate or reopen the old generation.
                let gate = Arc::clone(&self.gate);
                tokio::spawn(async move {
                    let _ = retirement::close_generation(&candidate).await;
                    drop(gate);
                });
                self.gate.fail(error(
                    "effects_unsettled",
                    "extension replacement was abandoned before publication",
                ));
            } else {
                self.gate.lock().claimed = false;
            }
        }
    }
}

fn wrap(
    gate: &Arc<ExtensionInvocations>,
    generation: &Arc<Generation>,
) -> Vec<Arc<dyn PluginEndpoint>> {
    generation
        .endpoints
        .iter()
        .map(|inner| {
            Arc::new(endpoint::ManagedEndpoint::new(
                Arc::downgrade(gate),
                generation.id,
                Arc::clone(inner),
            )) as Arc<dyn PluginEndpoint>
        })
        .collect()
}
fn validate(endpoints: &[Arc<dyn PluginEndpoint>]) -> Result<(), PluginRpcError> {
    if endpoints.len() > MAX_EXTENSION_ENDPOINTS {
        return Err(error("capacity", "too many session extension endpoints"));
    }
    for (index, endpoint) in endpoints.iter().enumerate() {
        if endpoints[..index]
            .iter()
            .any(|other| other.metadata().manifest().name == endpoint.metadata().manifest().name)
        {
            return Err(error(
                "duplicate_plugin",
                "session plugin identities must be unique",
            ));
        }
    }
    Ok(())
}
fn unavailable(state: &State) -> PluginRpcError {
    state.failure.clone().unwrap_or_else(|| {
        error(
            "generation_unavailable",
            "extension generation is paused, retired or unavailable",
        )
    })
}
fn error(code: &str, message: &str) -> PluginRpcError {
    PluginRpcError {
        code: code.into(),
        message: message.into(),
    }
}
