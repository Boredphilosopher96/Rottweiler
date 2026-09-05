//! Owned canonical index operations; raw repair writers never invent a mode registry.
use super::super::durable_session::DurableEventSink;
use crate::journal_service::{JournalPublication, JournalReadLease};
use rw_core::{
    AgentLoopError, ExtensionStateView,
    recovery::{CanonicalHistory, CanonicalRecovery, RecoveryError},
};
use rw_ext::ModeRegistry;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub(super) struct CanonicalSession {
    recovery: Mutex<Option<CanonicalRecovery>>,
    inherited_journal_through: Option<rw_types::SequenceId>,
    modes: Arc<ModeRegistry>,
    jobs: Mutex<Jobs>,
    changed: Notify,
}
#[derive(Default)]
struct Jobs {
    active: usize,
    failed: bool,
}
struct Operation(Arc<CanonicalSession>);
impl Drop for Operation {
    fn drop(&mut self) {
        let mut jobs = self
            .0
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.failed |= std::thread::panicking();
        jobs.active -= 1;
        self.0.changed.notify_waiters();
    }
}
fn persistence(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(error.to_string())
}
impl CanonicalSession {
    fn admit(self: &Arc<Self>) -> Result<Operation, AgentLoopError> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| persistence("canonical job owner poisoned"))?;
        if jobs.failed {
            return Err(persistence("canonical worker proof failed"));
        }
        jobs.active += 1;
        Ok(Operation(Arc::clone(self)))
    }
    async fn read<T: Send + 'static>(
        self: Arc<Self>,
        mut lease: JournalReadLease,
        publication: Arc<JournalPublication>,
        query: impl FnOnce(&CanonicalHistory) -> Result<T, RecoveryError> + Send + 'static,
    ) -> Result<T, AgentLoopError> {
        let operation = self.admit()?;
        let (result, _lease) = tokio::task::spawn_blocking(move || {
            let result = (|| {
                let mut recovery = self
                    .recovery
                    .lock()
                    .map_err(|_| persistence("canonical recovery owner poisoned"))?;
                lease.view = publication.capture();
                if recovery.is_none() {
                    *recovery = Some(
                        CanonicalRecovery::open(
                            &lease.view,
                            &self.modes,
                            self.inherited_journal_through,
                        )
                        .map_err(persistence)?,
                    );
                }
                let recovery = recovery
                    .as_mut()
                    .ok_or_else(|| persistence("canonical owner initialization failed"))?;
                while recovery
                    .advance(&lease.view, &self.modes)
                    .map_err(persistence)?
                    .has_more
                {}
                let history = recovery
                    .snapshot()
                    .map_err(persistence)?
                    .bind_source(&lease.view)
                    .map_err(persistence)?;
                query(&history).map_err(persistence)
            })();
            drop(operation);
            (result, lease)
        })
        .await
        .map_err(|error| persistence(format!("canonical worker failed: {error}")))?;
        result
    }
    pub(super) async fn settle(&self) -> Result<(), AgentLoopError> {
        let wait = async {
            loop {
                let changed = self.changed.notified();
                {
                    let jobs = self
                        .jobs
                        .lock()
                        .map_err(|_| persistence("canonical job owner poisoned"))?;
                    if jobs.failed {
                        return Err(persistence("canonical worker proof failed"));
                    }
                    if jobs.active == 0 {
                        return Ok(());
                    }
                }
                changed.await;
            }
        };
        if let Ok(result) = tokio::time::timeout(std::time::Duration::from_secs(30), wait).await {
            return result;
        }
        self.jobs
            .lock()
            .map_err(|_| persistence("canonical job owner poisoned"))?
            .failed = true;
        Err(persistence("canonical worker effects remain unsettled"))
    }
}
impl DurableEventSink {
    pub(in crate::session_runtime) fn configure_canonical(
        &self,
        modes: Arc<ModeRegistry>,
        inherited_journal_through: Option<rw_types::SequenceId>,
    ) -> Result<(), AgentLoopError> {
        self.canonical
            .set(Arc::new(CanonicalSession {
                recovery: Mutex::new(None),
                inherited_journal_through,
                modes,
                jobs: Mutex::new(Jobs::default()),
                changed: Notify::new(),
            }))
            .map_err(|_| persistence("canonical owner is already bound"))
    }
    pub(in crate::session_runtime) async fn bind_canonical(
        self: &Arc<Self>,
        modes: Arc<ModeRegistry>,
    ) -> Result<(), AgentLoopError> {
        if self.canonical.get().is_some() {
            return Err(persistence("canonical owner is already bound"));
        }
        let storage_root = self.storage_root.clone();
        let session_id = self.session_id.clone();
        let inherited = tokio::task::spawn_blocking(move || {
            super::super::session_metadata::load_session_metadata_any(&storage_root, &session_id)
                .map(|metadata| metadata.inherited_journal_through)
                .map_err(persistence)
        })
        .await
        .map_err(|error| persistence(format!("canonical metadata read failed: {error}")))??;
        self.configure_canonical(modes, inherited)?;
        // Publish the owner before starting index I/O so close can prove its completion.
        self.read_canonical(|_| Ok(())).await
    }

    pub(in crate::session_runtime) async fn read_canonical<T: Send + 'static>(
        &self,
        query: impl FnOnce(&CanonicalHistory) -> Result<T, RecoveryError> + Send + 'static,
    ) -> Result<T, AgentLoopError> {
        let owner = self
            .canonical
            .get()
            .ok_or_else(|| persistence("canonical owner is not bound to this session"))?;
        let lease = self
            .journal_service
            .capture(&self.session_id)
            .map_err(persistence)?;
        Arc::clone(owner)
            .read(lease, Arc::clone(&self.registration.publisher), query)
            .await
    }

    pub(super) async fn read_extension_state(
        &self,
        plugin: &str,
    ) -> Result<ExtensionStateView, AgentLoopError> {
        if plugin.is_empty() || plugin.len() > 128 {
            return Err(persistence("invalid extension namespace identity"));
        }
        let plugin = plugin.to_owned();
        self.read_canonical(move |history| history.extension_state(&plugin))
            .await
    }
}

#[cfg(test)]
mod tests;
