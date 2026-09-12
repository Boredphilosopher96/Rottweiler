//! Native adapters retain invoked host scopes through request-future destruction.
use futures_util::FutureExt;
use rw_tools::{ToolEffectGrant, ToolEffectHost, ToolError, ToolResult};
use serde_json::Value;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Exact approved declaration paired with the host's outer invocation scope.
pub struct PluginToolEffects {
    host: Arc<dyn ToolEffectHost>,
    grant: ToolEffectGrant,
}
impl PluginToolEffects {
    pub(crate) async fn call(&self, name: &str, input: Value) -> Result<ToolResult, ToolError> {
        self.host.call(&self.grant, name, input).await
    }
}
struct Entry {
    effects: Arc<PluginToolEffects>,
    abandoned: AtomicBool,
    failed: AtomicBool,
}
#[derive(Default)]
pub(crate) struct ToolEffectsOwner {
    entries: Mutex<BTreeMap<u64, Arc<Entry>>>,
    next: AtomicU64,
    failed: AtomicBool,
}
impl Drop for ToolEffectsOwner {
    fn drop(&mut self) {
        // Destruction is not proof. Retain exact host/backend authority on an
        // unproven exit even when the adapter itself loses its last owner.
        let entries = self
            .entries
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, entry) in std::mem::take(entries) {
            std::mem::forget(entry);
        }
    }
}
pub(crate) struct ToolEffectsCall {
    owner: Arc<ToolEffectsOwner>,
    id: u64,
    entry: Arc<Entry>,
    finished: bool,
}
impl ToolEffectsOwner {
    pub(crate) fn begin(
        self: &Arc<Self>,
        host: Arc<dyn ToolEffectHost>,
        grant: ToolEffectGrant,
    ) -> Result<ToolEffectsCall, ToolError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| unsettled("host effect ownership poisoned"))?;
        if self.failed.load(Ordering::Acquire)
            || entries.len() >= usize::from(rw_plugin_protocol::MAX_IN_FLIGHT_REQUESTS)
        {
            return Err(unsettled("host effect ownership is closed or exhausted"));
        }
        let id = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| unsettled("host effect identity exhausted"))?;
        let entry = Arc::new(Entry {
            effects: Arc::new(PluginToolEffects { host, grant }),
            abandoned: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        entries.insert(id, entry.clone());
        Ok(ToolEffectsCall {
            owner: self.clone(),
            id,
            entry,
            finished: false,
        })
    }
    pub(crate) async fn settle(&self) -> Result<(), ToolError> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| unsettled("host effect ownership poisoned"))?
            .iter()
            .filter(|(_, entry)| entry.abandoned.load(Ordering::Acquire))
            .map(|(id, entry)| (*id, entry.clone()))
            .collect::<Vec<_>>();
        let mut failure = None;
        for (id, entry) in entries {
            match prove(&entry).await {
                Ok(()) => {
                    self.entries
                        .lock()
                        .map_err(|_| unsettled("host effect ownership poisoned"))?
                        .remove(&id);
                }
                Err(error) => {
                    self.failed.store(true, Ordering::Release);
                    failure.get_or_insert(error);
                }
            }
        }
        if self.failed.load(Ordering::Acquire) {
            return Err(failure.unwrap_or_else(|| unsettled("host effect proof failed")));
        }
        Ok(())
    }
}
impl ToolEffectsCall {
    pub(crate) fn effects(&self) -> Arc<PluginToolEffects> {
        self.entry.effects.clone()
    }
    pub(crate) async fn finish(mut self) -> Result<(), ToolError> {
        if let Err(error) = prove(&self.entry).await {
            self.owner.failed.store(true, Ordering::Release);
            return Err(error);
        }
        self.owner
            .entries
            .lock()
            .map_err(|_| unsettled("host effect ownership poisoned"))?
            .remove(&self.id);
        self.finished = true;
        Ok(())
    }
}
impl Drop for ToolEffectsCall {
    fn drop(&mut self) {
        if !self.finished {
            self.entry.abandoned.store(true, Ordering::Release);
        }
    }
}
fn unsettled(message: &str) -> ToolError {
    ToolError::EffectsUnsettled(message.into())
}

async fn prove(entry: &Entry) -> Result<(), ToolError> {
    if entry.failed.load(Ordering::Acquire) {
        return Err(unsettled("host effect proof failed"));
    }
    let proof = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        AssertUnwindSafe(entry.effects.host.close_and_settle()).catch_unwind(),
    )
    .await;
    let result = match proof {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(unsettled("host effect proof panicked")),
        Err(_) => Err(unsettled("host effect proof deadline expired")),
    };
    if result.is_err() {
        entry.failed.store(true, Ordering::Release);
    }
    result
}

#[cfg(test)]
#[path = "tool_effects/tests.rs"]
mod tests;
