//! Lazy session capability: namespace I/O occurs only inside the caller's admitted worker.
use super::{JournalService, MAX_ACTIVE_JOURNALS};
use miette::{Result, miette};
use rw_mcp::PayloadSource;
use rw_store::session::{journal::JournalRoot, payloads::SessionPayloadStore};
use std::{
    io,
    sync::{Arc, Mutex},
};

pub(crate) struct SessionPayloadSource {
    root: Arc<JournalRoot>,
    session: String,
    opened: Mutex<Option<Arc<SessionPayloadStore>>>,
}
impl PayloadSource for SessionPayloadSource {
    fn open(&self) -> io::Result<Arc<SessionPayloadStore>> {
        let mut opened = self
            .opened
            .lock()
            .map_err(|_| io::Error::other("payload source poisoned"))?;
        if let Some(store) = &*opened {
            return Ok(Arc::clone(store));
        }
        let store = Arc::new(
            self.root
                .payloads(&self.session)
                .map_err(io::Error::other)?,
        );
        *opened = Some(Arc::clone(&store));
        Ok(store)
    }
}
impl JournalService {
    pub(crate) fn payload_source(&self, session: &str) -> Result<Arc<SessionPayloadSource>> {
        rw_types::SessionId::validate(session)
            .map_err(|error| miette!("invalid payload session: {error}"))?;
        let mut sources = self
            .payload_sources
            .lock()
            .map_err(|_| miette!("payload source registry poisoned"))?;
        sources.retain(|_, source| source.strong_count() > 0);
        if let Some(source) = sources.get(session).and_then(std::sync::Weak::upgrade) {
            return Ok(source);
        }
        if sources.len() >= MAX_ACTIVE_JOURNALS {
            return Err(miette!("payload session admission exhausted"));
        }
        let source = Arc::new(SessionPayloadSource {
            root: Arc::clone(&self.root),
            session: session.to_owned(),
            opened: Mutex::new(None),
        });
        sources.insert(session.to_owned(), Arc::downgrade(&source));
        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_payload_capability_is_lazy_shared_and_reopens_durable_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let service = JournalService::new(root.path())
            .map_err(|error| io::Error::other(error.to_string()))?;
        let source = service
            .payload_source("session")
            .map_err(|error| io::Error::other(error.to_string()))?;
        let other = service
            .payload_source("session")
            .map_err(|error| io::Error::other(error.to_string()))?;
        assert!(Arc::ptr_eq(&source, &other));
        assert!(!root.path().join("sessions/session/payloads").exists());
        let store = source.open()?;
        assert!(Arc::ptr_eq(&store, &other.open()?));
        let reference = store.write(b"canonical result", &|| false)?;
        drop(store);
        drop(source);
        drop(other);
        let reopened = service
            .payload_source("session")
            .map_err(|error| io::Error::other(error.to_string()))?;
        assert_eq!(
            reopened
                .open()?
                .window(&reference, 0, None, &|| false)?
                .content,
            "canonical result"
        );
        Ok(())
    }
}
