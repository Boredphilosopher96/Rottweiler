//! Prepared source and encoded response ownership across finite CPU work.
use crate::{McpError, StructuredResponseEncoder, payload_work::Allocation};
use rw_types::{allocation::AllocationPlan, session_payload::MAX_SESSION_PAYLOAD_BYTES};
use serde_json::Value;
use std::sync::Arc;

/// An encoded result whose source/encoding allocation was admitted before construction.
/// The owner moves into the physical payload write or compact tool result.
#[derive(Debug)]
pub struct EncodedPayload {
    pub(crate) bytes: Vec<u8>,
    pub(crate) retained: Allocation,
}
impl EncodedPayload {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}
struct EncodingWork {
    encoder: Arc<dyn StructuredResponseEncoder>,
    source: AllocationPlan<Value>,
    retained: Allocation,
}
impl EncodingWork {
    fn run(mut self) -> Result<EncodedPayload, McpError> {
        let source = self.source.prepare();
        let bytes = self.encoder.encode(source.value())?;
        if bytes.len() > MAX_SESSION_PAYLOAD_BYTES || std::str::from_utf8(&bytes).is_err() {
            return Err(McpError::Encoding(
                "encoded MCP payload exceeds its UTF-8 byte contract".into(),
            ));
        }
        drop(source);
        self.retained
            .resize(bytes.capacity().saturating_add(4096))?;
        Ok(EncodedPayload {
            bytes,
            retained: self.retained,
        })
    }
}

pub(crate) async fn encode(
    encoder: Arc<dyn StructuredResponseEncoder>,
    value: Value,
) -> Result<EncodedPayload, McpError> {
    let source = AllocationPlan::new(value)
        .map_err(|_| McpError::Encoding("MCP response allocation is unsupported".into()))?;
    let bytes = source
        .bytes()
        .checked_mul(2)
        .and_then(|source_bytes| {
            encoder
                .working_bytes(source.value())
                .ok()?
                .checked_add(source_bytes)
        })
        .ok_or_else(|| McpError::Encoding("MCP encoding allocation exceeds its contract".into()))?;
    let retained = Allocation::new(bytes)?;
    let owner = EncodingWork {
        encoder,
        source,
        retained,
    };
    rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || owner.run())
        .await
        .map_err(|error| McpError::Encoding(error.to_string()))?
}
