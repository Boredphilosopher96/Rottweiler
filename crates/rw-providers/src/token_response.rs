use std::time::Duration;

use futures_util::StreamExt;
use serde::de::DeserializeOwned;

use crate::{ProviderError, ProviderErrorKind};

pub(crate) const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_TOKEN_LIFETIME: Duration = Duration::from_hours(90 * 24);

/// Reads a small OAuth/device-flow response without permitting an unbounded body.
pub(crate) async fn read_json<T: DeserializeOwned>(
    response: reqwest::Response,
    invalid_message: &'static str,
) -> Result<T, ProviderError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "token endpoint response could not be read",
            )
        })?;
        append_chunk(&mut body, &chunk)?;
    }
    serde_json::from_slice(&body)
        .map_err(|_| ProviderError::new(ProviderErrorKind::Protocol, invalid_message))
}

fn append_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ProviderError> {
    if body.len().saturating_add(chunk.len()) > MAX_TOKEN_RESPONSE_BYTES {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "token endpoint response exceeded the 64 KiB safety limit",
        ));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

/// Validates a wire-provided lifetime before it is used in deadline arithmetic.
pub(crate) fn expiry_duration(expires_in: u64) -> Result<Duration, ProviderError> {
    let duration = Duration::from_secs(expires_in);
    if duration.is_zero() || duration > MAX_TOKEN_LIFETIME {
        return Err(ProviderError::new(
            ProviderErrorKind::Protocol,
            "token endpoint returned an invalid expiration",
        ));
    }
    Ok(duration)
}

pub(crate) fn checked_deadline(
    now: tokio::time::Instant,
    expires_in: u64,
) -> Result<tokio::time::Instant, ProviderError> {
    now.checked_add(expiry_duration(expires_in)?)
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                "token endpoint returned an expiration outside the supported range",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{MAX_TOKEN_RESPONSE_BYTES, append_chunk, expiry_duration};

    #[test]
    fn expiry_rejects_zero_and_excessive_values() {
        assert!(expiry_duration(0).is_err());
        assert!(expiry_duration(90 * 24 * 60 * 60 + 1).is_err());
        assert!(expiry_duration(3600).is_ok());
    }

    #[test]
    fn bounded_reader_rejects_a_body_larger_than_64_kib() {
        let mut body = vec![0; MAX_TOKEN_RESPONSE_BYTES];
        assert!(append_chunk(&mut body, b"x").is_err());
    }
}
