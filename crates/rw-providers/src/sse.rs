use crate::{ProviderError, ProviderErrorKind};

const MAX_PENDING_SSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub(crate) struct SseDecoder {
    pending: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, ProviderError> {
        if self.pending.len().saturating_add(bytes.len()) > MAX_PENDING_SSE_BYTES {
            return Err(ProviderError::new(
                ProviderErrorKind::Protocol,
                "provider SSE frame exceeded the 8 MiB safety limit",
            ));
        }
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some((end, delimiter_len)) = find_event_boundary(&self.pending) {
            let frame = self.pending.drain(..end).collect::<Vec<_>>();
            self.pending.drain(..delimiter_len);
            if let Some(event) = parse_frame(&frame)? {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<SseEvent>, ProviderError> {
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        let frame = std::mem::take(&mut self.pending);
        Ok(parse_frame(&frame)?.into_iter().collect())
    }
}

fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes.windows(2).position(|window| window == b"\n\n");
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(left), Some(right)) if left <= right => Some((left, 2)),
        (Some(_) | None, Some(right)) => Some((right, 4)),
        (Some(left), None) => Some((left, 2)),
        (None, None) => None,
    }
}

fn parse_frame(frame: &[u8]) -> Result<Option<SseEvent>, ProviderError> {
    let text = std::str::from_utf8(frame).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Protocol,
            format!("provider SSE contained invalid UTF-8: {error}"),
        )
    })?;
    let mut event = None;
    let mut data = Vec::new();
    for line in text.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseEvent {
        event,
        data: data.join("\n"),
    }))
}

#[cfg(test)]
mod tests {
    use super::SseDecoder;

    #[test]
    fn decodes_chunked_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"event: content\r\ndata: {\"a\":").is_ok());
        let events = decoder
            .push(b"1}\r\ndata: tail\r\n\r\n")
            .unwrap_or_else(|error| panic!("SSE must parse: {error}"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("content"));
        assert_eq!(events[0].data, "{\"a\":1}\ntail");
    }
}
