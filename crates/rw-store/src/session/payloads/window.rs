//! Streaming UTF-8 windows and literal line queries with fixed-size scan state.
use super::{
    check_cancelled,
    format::{CHUNK_BYTES, PayloadReader, corrupt},
};
use rw_types::session_payload::{MAX_PAYLOAD_QUERY_BYTES, MAX_PAYLOAD_WINDOW_BYTES};
use serde::Serialize;
use std::io;

/// A bounded slice of a durable payload. Offsets are UTF-8 byte offsets in the original body.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct PayloadWindow {
    pub content: String,
    /// Exclusive raw cursor, or the byte after the last consumed matching line for queries.
    pub next_offset: usize,
    pub has_more: bool,
    /// A matching line exceeded this window; raw reads can retrieve its remaining bytes.
    pub line_truncated: bool,
}

pub(super) fn read(
    reader: &mut PayloadReader,
    offset: usize,
    query: Option<&str>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<PayloadWindow> {
    if offset > reader.manifest.bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload cursor exceeds body",
        ));
    }
    check_cancelled(cancelled)?;
    if offset < reader.manifest.bytes
        && reader.chunk(offset / CHUNK_BYTES)?[offset % CHUNK_BYTES] & 0xc0 == 0x80
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload cursor splits UTF-8",
        ));
    }
    match query {
        None => raw(reader, offset, cancelled),
        Some(query) => query_lines(reader, offset, query, cancelled),
    }
}

fn raw(
    reader: &mut PayloadReader,
    offset: usize,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<PayloadWindow> {
    let end = reader
        .manifest
        .bytes
        .min(offset.saturating_add(MAX_PAYLOAD_WINDOW_BYTES));
    let mut bytes = Vec::with_capacity(end - offset);
    copy_range(reader, offset, end, &mut bytes, cancelled)?;
    trim_utf8(&mut bytes)?;
    let next_offset = offset + bytes.len();
    Ok(PayloadWindow {
        content: String::from_utf8(bytes).map_err(|_| corrupt("invalid payload UTF-8"))?,
        next_offset,
        has_more: next_offset < reader.manifest.bytes,
        line_truncated: false,
    })
}

struct Matcher<'a> {
    needle: &'a [u8],
    prefixes: Vec<usize>,
    matched: usize,
    found: bool,
}
impl<'a> Matcher<'a> {
    fn new(query: &'a str) -> io::Result<Self> {
        if query.is_empty() || query.len() > MAX_PAYLOAD_QUERY_BYTES || query.contains(['\r', '\n'])
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "payload query must be a nonempty literal line substring of at most 512 bytes",
            ));
        }
        let needle = query.as_bytes();
        let mut prefixes = vec![0; needle.len()];
        let mut length = 0;
        for index in 1..needle.len() {
            while length > 0 && needle[index] != needle[length] {
                length = prefixes[length - 1];
            }
            if needle[index] == needle[length] {
                length += 1;
            }
            prefixes[index] = length;
        }
        Ok(Self {
            needle,
            prefixes,
            matched: 0,
            found: false,
        })
    }
    fn feed(&mut self, bytes: &[u8]) {
        if self.found {
            return;
        }
        for byte in bytes {
            while self.matched > 0 && *byte != self.needle[self.matched] {
                self.matched = self.prefixes[self.matched - 1];
            }
            if *byte == self.needle[self.matched] {
                self.matched += 1;
            }
            if self.matched == self.needle.len() {
                self.found = true;
                return;
            }
        }
    }
    fn reset(&mut self) {
        self.matched = 0;
        self.found = false;
    }
}

fn query_lines(
    reader: &mut PayloadReader,
    offset: usize,
    query: &str,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<PayloadWindow> {
    let mut matcher = Matcher::new(query)?;
    let total = reader.manifest.bytes;
    let mut output = Vec::with_capacity(MAX_PAYLOAD_WINDOW_BYTES);
    let mut position = offset;
    let mut line_start = offset;
    while position < total {
        check_cancelled(cancelled)?;
        let bytes = &reader.chunk(position / CHUNK_BYTES)?[position % CHUNK_BYTES..];
        let newline = memchr::memchr(b'\n', bytes);
        let length = newline.map_or(bytes.len(), |index| index + 1);
        matcher.feed(&bytes[..newline.unwrap_or(length)]);
        position += length;
        if newline.is_none() && position < total {
            continue;
        }
        let mut end = position - usize::from(newline.is_some());
        if end > line_start
            && reader.chunk((end - 1) / CHUNK_BYTES)?[(end - 1) % CHUNK_BYTES] == b'\r'
        {
            end -= 1;
        }
        if matcher.found {
            let separator = usize::from(!output.is_empty());
            let available = MAX_PAYLOAD_WINDOW_BYTES - output.len();
            if available <= separator {
                return finish(output, line_start, total, false);
            }
            if separator != 0 {
                output.push(b'\n');
            }
            let accepted_end = end.min(line_start + available - separator);
            copy_range(reader, line_start, accepted_end, &mut output, cancelled)?;
            trim_utf8(&mut output)?;
            if accepted_end < end {
                return finish(output, position, total, true);
            }
            if output.len() == MAX_PAYLOAD_WINDOW_BYTES {
                return finish(output, position, total, false);
            }
        }
        line_start = position;
        matcher.reset();
    }
    finish(output, position, total, false)
}

fn finish(
    bytes: Vec<u8>,
    next_offset: usize,
    total: usize,
    line_truncated: bool,
) -> io::Result<PayloadWindow> {
    Ok(PayloadWindow {
        content: String::from_utf8(bytes).map_err(|_| corrupt("invalid payload UTF-8"))?,
        next_offset,
        has_more: next_offset < total,
        line_truncated,
    })
}

fn copy_range(
    reader: &mut PayloadReader,
    mut start: usize,
    end: usize,
    output: &mut Vec<u8>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<()> {
    while start < end {
        check_cancelled(cancelled)?;
        let chunk = reader.chunk(start / CHUNK_BYTES)?;
        let within = start % CHUNK_BYTES;
        let count = (end - start).min(chunk.len() - within);
        // The caller determines the admitted end before this allocation or copy.
        if output.len().saturating_add(count) > MAX_PAYLOAD_WINDOW_BYTES {
            return Err(corrupt("payload window budget exceeded"));
        }
        output.extend_from_slice(&chunk[within..within + count]);
        start += count;
    }
    Ok(())
}
fn trim_utf8(bytes: &mut Vec<u8>) -> io::Result<()> {
    if let Err(error) = std::str::from_utf8(bytes) {
        if error.error_len().is_some() {
            return Err(corrupt("invalid payload UTF-8"));
        }
        bytes.truncate(error.valid_up_to());
    }
    Ok(())
}
