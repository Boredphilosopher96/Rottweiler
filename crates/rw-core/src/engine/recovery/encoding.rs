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
