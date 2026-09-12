//! One immutable, delivery-scoped source per plugin. Outstanding readers retain
//! the exact byte allocation and its admission after new reads are revoked.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rw_ext::PluginRpcError;
use rw_types::extension_contract::ExtensionDeliveryCursor;
use rw_types::extension_events::{
    ExtensionEventChunk, ExtensionEventRead, MAX_EXTENSION_EVENT_CHUNK_BYTES,
    MAX_EXTENSION_EVENT_SOURCE_BYTES,
};
use std::sync::{Arc, Mutex};
use tokio::sync::OwnedSemaphorePermit;

pub(crate) struct PluginEventSource {
    pub(crate) cursor: ExtensionDeliveryCursor,
    bytes: Vec<u8>,
    _retained: OwnedSemaphorePermit,
}
impl PluginEventSource {
    pub(crate) fn new(
        cursor: ExtensionDeliveryCursor,
        bytes: Vec<u8>,
        retained: OwnedSemaphorePermit,
    ) -> Result<Self, PluginRpcError> {
        if bytes.is_empty()
            || bytes.len() > MAX_EXTENSION_EVENT_SOURCE_BYTES
            || retained.num_permits() < bytes.capacity()
        {
            return Err(error("invalid delivery source allocation"));
        }
        Ok(Self {
            cursor,
            bytes,
            _retained: retained,
        })
    }
}

#[derive(Default)]
pub(crate) struct PluginEventSources {
    source: Mutex<Option<Arc<PluginEventSource>>>,
}
impl PluginEventSources {
    pub(crate) fn install(
        self: &Arc<Self>,
        source: Arc<PluginEventSource>,
    ) -> Result<PluginEventSourceLease, PluginRpcError> {
        let mut active = self
            .source
            .lock()
            .map_err(|_| error("delivery source owner poisoned"))?;
        if active.is_some() {
            return Err(error("delivery source is already active"));
        }
        *active = Some(Arc::clone(&source));
        Ok(PluginEventSourceLease {
            registry: Arc::clone(self),
            source,
        })
    }
    pub(crate) fn read(
        &self,
        request: &ExtensionEventRead,
    ) -> Result<ExtensionEventChunk, PluginRpcError> {
        if request.max_bytes == 0 || request.max_bytes > MAX_EXTENSION_EVENT_CHUNK_BYTES {
            return Err(error("delivery read size"));
        }
        let source = self
            .source
            .lock()
            .map_err(|_| error("delivery source owner poisoned"))?
            .clone()
            .ok_or_else(|| error("delivery source is not active"))?;
        if request.cursor != source.cursor {
            return Err(error("delivery cursor differs from active source"));
        }
        let start = usize::try_from(request.offset).map_err(|_| error("delivery read offset"))?;
        let size = usize::try_from(request.max_bytes).map_err(|_| error("delivery read size"))?;
        if start >= source.bytes.len() {
            return Err(error("delivery read offset outside source"));
        }
        let end = start.saturating_add(size).min(source.bytes.len());
        Ok(ExtensionEventChunk {
            cursor: source.cursor.clone(),
            offset: request.offset,
            data_base64: STANDARD.encode(&source.bytes[start..end]),
            next_offset: if end == source.bytes.len() {
                None
            } else {
                Some(u32::try_from(end).map_err(|_| error("delivery read offset"))?)
            },
        })
    }
}

pub(crate) struct PluginEventSourceLease {
    registry: Arc<PluginEventSources>,
    source: Arc<PluginEventSource>,
}
impl Drop for PluginEventSourceLease {
    fn drop(&mut self) {
        if let Ok(mut active) = self.registry.source.lock()
            && active
                .as_ref()
                .is_some_and(|source| Arc::ptr_eq(source, &self.source))
        {
            active.take();
        }
    }
}
fn error(message: &str) -> PluginRpcError {
    PluginRpcError {
        code: "invalid_event_source".into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginEventSource, PluginEventSources};
    use rw_types::{
        SequenceId, SessionId, extension_contract::ExtensionDeliveryCursor,
        extension_events::ExtensionEventRead,
    };
    use std::sync::Arc;
    #[tokio::test]
    async fn source_identity_and_allocation_survive_revocation_until_last_owner() {
        let cursor = ExtensionDeliveryCursor {
            session_id: SessionId("session".into()),
            sequence: SequenceId(4),
        };
        let bytes = b"abcdef".to_vec();
        let budget = Arc::new(tokio::sync::Semaphore::new(bytes.capacity()));
        let permit = budget
            .clone()
            .acquire_many_owned(6)
            .await
            .unwrap_or_else(|_| panic!("permit"));
        let source = Arc::new(
            PluginEventSource::new(cursor.clone(), bytes, permit)
                .unwrap_or_else(|_| panic!("source")),
        );
        let registry = Arc::new(PluginEventSources::default());
        let lease = registry
            .install(source.clone())
            .unwrap_or_else(|_| panic!("lease"));
        let request = ExtensionEventRead {
            cursor: cursor.clone(),
            offset: 0,
            max_bytes: 3,
        };
        let chunk = registry.read(&request).unwrap_or_else(|_| panic!("chunk"));
        assert_eq!(chunk.data_base64, "YWJj");
        assert_eq!(chunk.next_offset, Some(3));
        assert!(
            registry
                .read(&ExtensionEventRead {
                    cursor: ExtensionDeliveryCursor {
                        sequence: SequenceId(5),
                        ..cursor
                    },
                    ..request.clone()
                })
                .is_err()
        );
        drop(lease);
        assert!(registry.read(&request).is_err());
        assert_eq!(budget.available_permits(), 0);
        drop(source);
        assert_eq!(budget.available_permits(), 6);
    }
}
