//! Field framing keeps only bounded metadata; data lines append to `RawFrame`.
use super::{METADATA_BYTES, McpError, SseParser, invalid};

#[derive(Clone, Copy)]
pub(super) enum Field {
    Data,
    Event,
    Id,
    Retry,
    Comment,
}
pub(super) struct Line {
    pub(super) field: Option<Field>,
    pub(super) skip_space: bool,
    pub(super) data_start: usize,
    prefix: [u8; 5],
    prefix_len: usize,
    pub(super) value: Vec<u8>,
}
impl Line {
    pub(super) fn new() -> Self {
        Self {
            field: None,
            skip_space: false,
            data_start: 0,
            prefix: [0; 5],
            prefix_len: 0,
            value: Vec::with_capacity(METADATA_BYTES),
        }
    }
    pub(super) fn empty(&self) -> bool {
        self.prefix_len == 0 && self.field.is_none()
    }
    pub(super) fn clear(&mut self) {
        self.field = None;
        self.skip_space = false;
        self.prefix_len = 0;
        self.value.clear();
    }
}
impl SseParser {
    pub(super) fn prefix(&mut self, byte: u8) -> Result<(), McpError> {
        if self.line.skip_space {
            self.line.skip_space = false;
            if byte == b' ' {
                return Ok(());
            }
            return self.value(&[byte]);
        }
        if byte != b':' {
            let slot = self
                .line
                .prefix
                .get_mut(self.line.prefix_len)
                .ok_or_else(invalid)?;
            *slot = byte;
            self.line.prefix_len += 1;
            return Ok(());
        }
        let field = match &self.line.prefix[..self.line.prefix_len] {
            b"data" => Field::Data,
            b"event" => Field::Event,
            b"id" => Field::Id,
            b"retry" => Field::Retry,
            b"" => Field::Comment,
            _ => return Err(invalid()),
        };
        self.line.field = Some(field);
        self.line.skip_space = true;
        if matches!(field, Field::Data) {
            self.start_data()?;
        }
        Ok(())
    }

    pub(super) fn value(&mut self, bytes: &[u8]) -> Result<(), McpError> {
        match self.line.field.ok_or_else(invalid)? {
            Field::Data => self
                .current
                .as_mut()
                .and_then(|current| current.data.as_mut())
                .ok_or_else(invalid)?
                .append(bytes),
            Field::Comment => Ok(()),
            _ => {
                if self.line.value.len().saturating_add(bytes.len()) > METADATA_BYTES {
                    return Err(invalid());
                }
                self.line.value.extend_from_slice(bytes);
                Ok(())
            }
        }
    }

    pub(super) fn metadata(&mut self, field: Field) -> Result<(), McpError> {
        if matches!(field, Field::Id) && self.line.value.contains(&0) {
            return Ok(());
        }
        if self.current.is_none() {
            self.current()?;
        }
        let current = self.current.as_mut().ok_or_else(invalid)?;
        let metadata = &mut current.metadata;
        let value = std::str::from_utf8(&self.line.value).map_err(|_| invalid())?;
        metadata.bytes = metadata
            .bytes
            .checked_add(value.len())
            .ok_or_else(invalid)?;
        if metadata.bytes > METADATA_BYTES {
            return Err(invalid());
        }
        match field {
            Field::Event if metadata.event.is_none() => metadata.event = Some(value.to_owned()),
            Field::Id if metadata.id.is_none() => metadata.id = Some(value.to_owned()),
            Field::Retry if metadata.retry.is_none() => {
                metadata.retry = Some(value.trim_ascii().parse().map_err(|_| invalid())?);
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
}
