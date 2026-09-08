//! Encoded ingress storage is admitted before capacity grows.
use crate::{McpError, payload_work::Allocation};

pub(crate) const READ_SCRATCH_BYTES: usize = 16 * 1024;
pub(crate) const STDIO_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const HTTP_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Includes encoded storage and geometric escaped-string parser scratch.
/// The typed decoder adds its own checked working requirement before decoding.
pub(crate) struct RawFrame {
    pub(crate) bytes: Vec<u8>,
    pub(crate) retained: Allocation,
    pub(crate) parser_scratch: Allocation,
    _read_scratch: Allocation,
    limit: usize,
}

impl RawFrame {
    pub(crate) fn new(limit: usize) -> Result<Self, McpError> {
        if limit == 0 || limit > HTTP_BODY_BYTES {
            return Err(invalid_frame());
        }
        Ok(Self {
            bytes: Vec::new(),
            retained: Allocation::new(0)?,
            parser_scratch: Allocation::new(0)?,
            _read_scratch: Allocation::new(READ_SCRATCH_BYTES)?,
            limit,
        })
    }

    pub(crate) fn append(&mut self, input: &[u8]) -> Result<(), McpError> {
        let length = self
            .bytes
            .len()
            .checked_add(input.len())
            .ok_or_else(invalid_frame)?;
        if length > self.limit {
            return Err(invalid_frame());
        }
        if length > self.bytes.capacity() {
            let charge = length.checked_mul(2).ok_or_else(invalid_frame)?;
            self.retained.resize(length)?;
            self.parser_scratch.resize(charge)?;
            // Exact reserve avoids an unadmitted geometric capacity expansion.
            self.bytes
                .try_reserve_exact(length - self.bytes.len())
                .map_err(|_| invalid_frame())?;
        }
        self.bytes.extend_from_slice(input);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn parser_working_bytes(&self) -> Result<usize, McpError> {
        self.bytes
            .capacity()
            .checked_mul(3)
            .ok_or_else(invalid_frame)
    }
}

fn invalid_frame() -> McpError {
    McpError::Protocol("MCP encoded frame admission exceeded".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_growth_keeps_the_previous_frame_and_capacity() -> Result<(), McpError> {
        let mut frame = RawFrame::new(17)?;
        frame.append(b"existing")?;
        let capacity = frame.bytes.capacity();
        assert!(frame.append(b"too large to fit").is_err());
        assert_eq!(frame.bytes, b"existing");
        assert_eq!(frame.bytes.capacity(), capacity);
        frame.append(b"012345678")?;
        assert_eq!(frame.bytes.len(), 17);
        assert_eq!(frame.parser_working_bytes()?, 51);
        Ok(())
    }
}
