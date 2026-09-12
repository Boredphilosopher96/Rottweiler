//! JSON byte accounting and output share one concrete serializer destination.
use serde::Serialize;
use std::io::{self, Write};

enum Destination<'a> {
    Count,
    Buffer {
        bytes: &'a mut Vec<u8>,
        minimum_growth: usize,
    },
    Stream(&'a mut dyn Write),
}

/// A JSON destination whose byte ceiling is checked before every output write.
/// Buffer capacity is bounded independently of encoded length. The caller owns
/// the buffer and its admission lease through encoding and subsequent use.
pub struct JsonWriter<'a> {
    destination: Destination<'a>,
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl<'a> JsonWriter<'a> {
    /// Measure encoded bytes without allocating an output buffer.
    #[must_use]
    pub const fn count(limit: usize) -> Self {
        Self {
            destination: Destination::Count,
            written: 0,
            limit,
            exceeded: false,
        }
    }

    /// Append into an admitted buffer, growing by bounded geometric steps.
    /// A buffer already allocated to `limit` never grows while encoding.
    ///
    /// # Errors
    /// Rejects a buffer whose retained capacity already exceeds its admission.
    pub fn buffer(bytes: &'a mut Vec<u8>, limit: usize, minimum_growth: usize) -> io::Result<Self> {
        if bytes.capacity() > limit {
            return Err(io::Error::other(
                "JSON buffer exceeds its capacity admission",
            ));
        }
        Ok(Self {
            written: bytes.len(),
            destination: Destination::Buffer {
                bytes,
                minimum_growth,
            },
            limit,
            exceeded: false,
        })
    }

    /// Write to an externally owned stream with the same encoded byte ceiling.
    #[must_use]
    pub fn stream(output: &'a mut dyn Write, limit: usize) -> Self {
        Self {
            destination: Destination::Stream(output),
            written: 0,
            limit,
            exceeded: false,
        }
    }

    /// Serialize directly to the selected destination, without a JSON value tree.
    ///
    /// # Errors
    /// Preserves serialization and stream errors; rejects byte/capacity overflow.
    pub fn serialize<T: Serialize + ?Sized>(&mut self, value: &T) -> serde_json::Result<()> {
        serde_json::to_writer(self, value)
    }

    /// Number of complete bytes accepted by this destination.
    #[must_use]
    pub const fn written(&self) -> usize {
        self.written
    }

    /// Encoded byte ceiling declared by the caller.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Whether an encoded write exceeded the declared byte limit.
    #[must_use]
    pub const fn exceeded(&self) -> bool {
        self.exceeded
    }
}

impl Write for JsonWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self
            .written
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
        else {
            self.exceeded = true;
            return Err(io::Error::other("JSON encoded byte limit"));
        };
        match &mut self.destination {
            Destination::Count => {}
            Destination::Buffer {
                bytes: output,
                minimum_growth,
            } => {
                if length > output.capacity() {
                    let capacity = output
                        .capacity()
                        .max(*minimum_growth)
                        .saturating_mul(2)
                        .max(length)
                        .min(self.limit);
                    output
                        .try_reserve_exact(capacity - output.len())
                        .map_err(io::Error::other)?;
                    if output.capacity() > self.limit {
                        return Err(io::Error::other("JSON buffer capacity limit"));
                    }
                }
                output.extend_from_slice(bytes);
            }
            Destination::Stream(output) => output.write_all(bytes)?,
        }
        self.written = length;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.destination {
            Destination::Stream(output) => output.flush(),
            Destination::Count | Destination::Buffer { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
