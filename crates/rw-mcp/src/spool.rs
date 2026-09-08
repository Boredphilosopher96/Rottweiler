//! Session-bound durable payload access; no ambient filesystem path constructor.
use crate::{
    EncodedPayload, McpError,
    payload_work::{Allocation, Jobs},
};
use async_trait::async_trait;
use rw_store::session::payloads::{
    PAYLOAD_WINDOW_WORKING_BYTES, PayloadWindow, SessionPayloadStore,
};
use rw_tools::{CancellationToken, ToolResultPayloads};
use rw_types::{McpServerId, SessionPayloadReference, session_payload::MAX_SESSION_PAYLOAD_BYTES};
use std::{io, sync::Arc};

/// The runtime binds this capability to one authenticated session's journal root.
/// Opening runs only in an admitted physical I/O worker on first actual payload use.
pub trait PayloadSource: Send + Sync {
    fn open(&self) -> io::Result<Arc<SessionPayloadStore>>;
}
impl PayloadSource for SessionPayloadStore {
    fn open(&self) -> io::Result<Arc<SessionPayloadStore>> {
        Ok(Arc::new(self.clone()))
    }
}

/// The same live secret registry used by canonical tool output, with pre-growth admission.
pub trait PayloadRedactor: Send + Sync {
    fn redact(
        &self,
        text: &str,
        max_bytes: usize,
        admit: &mut dyn FnMut(usize) -> io::Result<()>,
    ) -> io::Result<String>;
}

/// The returned window and all its source/allocation owners transfer to the native tool result.
#[derive(Debug)]
pub struct RetainedPayloadWindow {
    pub window: PayloadWindow,
    pub payloads: ToolResultPayloads,
}

#[async_trait]
pub trait OverflowSpool: Send + Sync {
    async fn write(
        &self,
        server: &McpServerId,
        operation: &str,
        bytes: EncodedPayload,
    ) -> Result<SessionPayloadReference, McpError>;
    async fn window(
        &self,
        reference: SessionPayloadReference,
        offset: usize,
        query: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<RetainedPayloadWindow, McpError>;
    async fn settle_effects(&self) -> Result<(), McpError>;
}

pub struct FilesystemSpool {
    source: Arc<dyn PayloadSource>,
    redactor: Arc<dyn PayloadRedactor>,
    jobs: Arc<Jobs>,
}
impl FilesystemSpool {
    #[must_use]
    pub fn new(source: Arc<dyn PayloadSource>, redactor: Arc<dyn PayloadRedactor>) -> Self {
        Self {
            source,
            redactor,
            jobs: Arc::new(Jobs::default()),
        }
    }
}
#[async_trait]
impl OverflowSpool for FilesystemSpool {
    async fn write(
        &self,
        _server: &McpServerId,
        _operation: &str,
        bytes: EncodedPayload,
    ) -> Result<SessionPayloadReference, McpError> {
        let _invocation = self.jobs.retain()?;
        let redactor = Arc::clone(&self.redactor);
        let redacted = self
            .jobs
            .run(
                rw_resources::ResourceClass::Cpu,
                CancellationToken::default(),
                move |cancelled| {
                    if cancelled.is_cancelled() {
                        return Err(spool_error("payload write cancelled"));
                    }
                    let original = std::str::from_utf8(&bytes.bytes)
                        .map_err(|_| spool_error("payload is not UTF-8"))?;
                    let mut scratch = Allocation::new(0)?;
                    let redacted = redactor
                        .redact(original, MAX_SESSION_PAYLOAD_BYTES, &mut |working| {
                            scratch.ensure(working)
                        })
                        .map_err(spool_error)?;
                    drop(bytes);
                    scratch.resize(redacted.capacity().saturating_add(4096))?;
                    Ok::<_, McpError>((redacted, scratch))
                },
            )
            .await??;
        let source = Arc::clone(&self.source);
        self.jobs
            .run(
                rw_resources::ResourceClass::Blocking,
                CancellationToken::default(),
                move |cancelled| {
                    let (text, _retained) = redacted;
                    source
                        .open()
                        .map_err(spool_error)?
                        .write(text.as_bytes(), &|| cancelled.is_cancelled())
                        .map_err(spool_error)
                },
            )
            .await?
    }

    async fn window(
        &self,
        reference: SessionPayloadReference,
        offset: usize,
        query: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<RetainedPayloadWindow, McpError> {
        let source = Arc::clone(&self.source);
        let retained = Allocation::new(PAYLOAD_WINDOW_WORKING_BYTES)?;
        self.jobs
            .run(
                rw_resources::ResourceClass::Blocking,
                cancellation.clone(),
                move |cancelled| {
                    let store = source.open().map_err(spool_error)?;
                    let window = store
                        .window(&reference, offset, query.as_deref(), &|| {
                            cancelled.is_cancelled() || cancellation.is_cancelled()
                        })
                        .map_err(spool_error)?;
                    let payloads = ToolResultPayloads::retained(
                        vec![reference],
                        Arc::new((retained, store, source)),
                    )
                    .map_err(spool_error)?;
                    Ok(RetainedPayloadWindow { window, payloads })
                },
            )
            .await?
    }

    async fn settle_effects(&self) -> Result<(), McpError> {
        self.jobs.settle().await;
        Ok(())
    }
}
fn spool_error(error: impl std::fmt::Display) -> McpError {
    McpError::Spool(error.to_string())
}

#[cfg(test)]
mod tests;
