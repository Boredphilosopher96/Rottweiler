//! Owned canonical index operations; raw repair writers never invent a mode registry.
use super::super::durable_session::DurableEventSink;
use crate::journal_service::{JournalPublication, JournalReadLease};
use rw_core::{
    AgentLoopError, ExtensionStateView,
    recovery::{CanonicalHistory, CanonicalRecovery, RecoveryError},
};
use rw_ext::ModeRegistry;
use std::sync::{Arc, Mutex};

pub(super) struct CanonicalSession {
    recovery: Mutex<Option<CanonicalRecovery>>,
    inherited_journal_through: Option<rw_types::SequenceId>,
    modes: Arc<ModeRegistry>,
    reads: Arc<super::reads::ReadOperations>,
}
fn persistence(error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::Persistence(error.to_string())
}
impl CanonicalSession {
    pub(super) const fn inherited_journal_through(&self) -> Option<rw_types::SequenceId> {
        self.inherited_journal_through
    }
    async fn read<T: Send + 'static>(
        self: Arc<Self>,
        lease: JournalReadLease,
        publication: Arc<JournalPublication>,
        query: impl FnOnce(&CanonicalHistory) -> Result<T, RecoveryError> + Send + 'static,
    ) -> Result<T, AgentLoopError> {
        let reads = Arc::clone(&self.reads);
        reads
            .run(lease, move |lease| {
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
            })
            .await
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
                reads: Arc::clone(&self.reads),
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
        let admission = self.journal_service.admit_read().map_err(persistence)?;
        let inherited = self
            .reads
            .run(admission, move |_| {
                super::super::session_metadata::load_session_metadata_any(
                    &storage_root,
                    &session_id,
                )
                .map(|metadata| metadata.inherited_journal_through)
                .map_err(persistence)
            })
            .await?;
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
