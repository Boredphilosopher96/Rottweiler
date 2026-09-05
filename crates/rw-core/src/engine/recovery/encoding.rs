use super::RecoveryError;
use serde::Serialize;
use std::io::{self, Write};

pub(super) fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, RecoveryError> {
    let mut output = BoundedBytes {
        bytes: Vec::new(),
        limit,
        overflow: false,
    };
    let result = serde_json::to_writer(&mut output, value);
    if output.overflow {
        return Err(RecoveryError::Limit("serialized recovery metadata"));
    }
    result?;
    Ok(output.bytes)
}
struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
    overflow: bool,
}
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.overflow = true;
            return Err(io::Error::other("bounded metadata capacity"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn serialized_size(value: &impl Serialize) -> Result<u64, RecoveryError> {
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}
struct Counter(u64);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Source-derived decode allowance is persisted with each IR selector. This scratch
/// buffer is bounded independently of the raw reader and retained context window.
pub(super) fn turn_decode_bytes(turn: &rw_types::Turn) -> Result<u64, RecoveryError> {
    let limit = rw_store::session::SessionEventPageLimits::default().max_line_bytes;
    let bytes = encode(turn, limit)?;
    let shape = rw_types::json_structure::preflight_json(
        &bytes,
        rw_types::json_structure::JsonStructureLimits {
            max_encoded_bytes: limit,
            max_nodes: 65_536,
            max_string_bytes: limit,
            max_depth: 64,
        },
    )?;
    shape
        .decode_bytes::<rw_types::Turn>()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(RecoveryError::Limit("conversation decoded allocation"))
}
