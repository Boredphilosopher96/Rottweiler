//! Streaming SSE framing with one admitted event and no decoded-message queue.
use super::frame::RawFrame;
use crate::{McpError, payload_work::Allocation};
use std::sync::Arc;

mod line;
#[cfg(test)]
mod tests;
use line::{Field, Line};

const METADATA_BYTES: usize = 16 * 1024;

/// Keeping this Arc also owns a Last-Event-ID used by the HTTP reconnect owner.
/// That owner separately enforces the HTTP header length contract.
pub(super) struct SseMetadata {
    pub(super) event: Option<String>,
    pub(super) id: Option<String>,
    pub(super) retry: Option<u64>,
    bytes: usize,
    _retained: Allocation,
}
impl SseMetadata {
    fn new() -> Result<Self, McpError> {
        let retained = Allocation::new(2 * METADATA_BYTES)?;
        Ok(Self {
            event: None,
            id: None,
            retry: None,
            bytes: 0,
            _retained: retained,
        })
    }
}

pub(super) struct SseEvent {
    pub(super) data: Option<RawFrame>,
    pub(super) metadata: Arc<SseMetadata>,
}
struct Current {
    data: Option<RawFrame>,
    metadata: SseMetadata,
}

pub(super) struct SseParser {
    line: Line,
    current: Option<Current>,
    limit: usize,
    wire_bytes: usize,
    skip_lf: bool,
    bom: [u8; 3],
    bom_len: usize,
    bom_done: bool,
    failed: bool,
    _scratch: Allocation,
}
impl SseParser {
    pub(super) fn new(event_limit: usize) -> Result<Self, McpError> {
        if event_limit == 0 || event_limit > super::frame::HTTP_BODY_BYTES {
            return Err(invalid());
        }
        let scratch = Allocation::new(METADATA_BYTES)?;
        Ok(Self {
            line: Line::new(),
            current: None,
            limit: event_limit,
            wire_bytes: 0,
            skip_lf: false,
            bom: [0; 3],
            bom_len: 0,
            bom_done: false,
            failed: false,
            _scratch: scratch,
        })
    }

    /// Stops at the first dispatch, leaving the caller's remaining chunk borrowed.
    pub(super) fn push(&mut self, input: &[u8]) -> Result<(usize, Option<SseEvent>), McpError> {
        if self.failed {
            return Err(invalid());
        }
        let result = self.push_inner(input);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(&mut self, input: &[u8]) -> Result<(usize, Option<SseEvent>), McpError> {
        let mut position = 0;
        while position < input.len() {
            if !self.bom_done {
                self.begin(input[position])?;
                position += 1;
                continue;
            }
            let byte = input[position];
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    position += 1;
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.charge_wire(1)?;
                self.skip_lf = byte == b'\r';
                position += 1;
                if let Some(event) = self.end_line()? {
                    return Ok((position, Some(event)));
                }
            } else if self.line.field.is_some() && !self.line.skip_space {
                let remaining = &input[position..];
                let length = remaining
                    .iter()
                    .position(|byte| matches!(byte, b'\r' | b'\n'))
                    .unwrap_or(remaining.len());
                self.charge_wire(length)?;
                self.value(&remaining[..length])?;
                position += length;
            } else {
                self.charge_wire(1)?;
                self.prefix(byte)?;
                position += 1;
            }
        }
        Ok((position, None))
    }

    fn begin(&mut self, byte: u8) -> Result<(), McpError> {
        const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];
        self.bom[self.bom_len] = byte;
        self.bom_len += 1;
        if self.bom[..self.bom_len] == BOM[..self.bom_len] {
            self.bom_done = self.bom_len == BOM.len();
            return Ok(());
        }
        self.bom_done = true;
        // A mismatched prefix can contain at most three bytes and cannot contain
        // a complete data/metadata field. No event can dispatch during this replay.
        for index in 0..self.bom_len {
            let byte = self.bom[index];
            self.charge_wire(1)?;
            if matches!(byte, b'\r' | b'\n') {
                self.skip_lf = byte == b'\r';
                let _ = self.end_line()?;
            } else {
                self.prefix(byte)?;
            }
        }
        Ok(())
    }

    fn charge_wire(&mut self, bytes: usize) -> Result<(), McpError> {
        self.wire_bytes = self.wire_bytes.checked_add(bytes).ok_or_else(invalid)?;
        // Data has its exact RawFrame ceiling. This separate framing allowance
        // also bounds ignored comments/field overhead before the next blank line.
        if self.wire_bytes > self.limit + 4 * METADATA_BYTES {
            return Err(invalid());
        }
        Ok(())
    }

    fn current(&mut self) -> Result<&mut Current, McpError> {
        if self.current.is_none() {
            self.current = Some(Current {
                data: None,
                metadata: SseMetadata::new()?,
            });
        }
        self.current.as_mut().ok_or_else(invalid)
    }

    fn start_data(&mut self) -> Result<(), McpError> {
        let limit = self.limit;
        let current = self.current()?;
        if let Some(data) = &mut current.data {
            data.append(b"\n")?;
        } else {
            current.data = Some(RawFrame::new(limit)?);
        }
        self.line.data_start = self
            .current
            .as_ref()
            .and_then(|current| current.data.as_ref())
            .map_or(0, |data| data.bytes.len());
        Ok(())
    }

    fn end_line(&mut self) -> Result<Option<SseEvent>, McpError> {
        if self.line.empty() {
            self.wire_bytes = 0;
            return Ok(self.current.take().map(|current| SseEvent {
                data: current.data,
                metadata: Arc::new(current.metadata),
            }));
        }
        match self.line.field.ok_or_else(invalid)? {
            Field::Data => {
                let data = self
                    .current
                    .as_ref()
                    .and_then(|current| current.data.as_ref())
                    .ok_or_else(invalid)?;
                std::str::from_utf8(&data.bytes[self.line.data_start..]).map_err(|_| invalid())?;
            }
            Field::Comment => {}
            field => self.metadata(field)?,
        }
        self.line.clear();
        Ok(None)
    }

    /// EOF never dispatches a partial event, including a complete last data line
    /// without the terminating blank line (the pinned SSE stream contract).
    pub(super) fn finish(self) {
        drop(self);
    }
}
fn invalid() -> McpError {
    McpError::Protocol("MCP SSE framing or metadata admission failed".into())
}
