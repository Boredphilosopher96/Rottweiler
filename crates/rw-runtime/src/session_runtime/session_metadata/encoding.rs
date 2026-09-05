use super::{MAX_SESSION_METADATA_BYTES, SessionMetadata};
use miette::{IntoDiagnostic as _, Result};
use std::io::{self, Write};

pub(crate) fn encode(metadata: &SessionMetadata) -> Result<Vec<u8>> {
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, metadata).into_diagnostic()?;
    let mut output = Output {
        bytes: Vec::with_capacity(counter.0),
        limit: counter.0,
    };
    serde_json::to_writer(&mut output, metadata).into_diagnostic()?;
    Ok(output.bytes)
}

struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > MAX_SESSION_METADATA_BYTES - self.0 as u64 {
            return Err(io::Error::other("session metadata exceeds its byte limit"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Output {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::Error::other(
                "session metadata exceeded its measured size",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
