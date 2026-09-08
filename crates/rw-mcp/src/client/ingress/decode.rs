//! Method-correlated MCP decoding. The transport owns encoded bytes and header
//! scratch; the required admission callback owns every typed decoder allocation.
use std::io;

use rmcp::model::{RequestId, ServerJsonRpcMessage};

use crate::ingress::envelope;
#[cfg(test)]
mod denial_tests;
mod messages;
mod profile;
#[cfg(test)]
mod tests;

/// Unsupported host requests never materialize a typed params graph.
#[derive(Debug)]
// Keep the existing inline protocol frame; boxing it adds an allocation to every
// supported response merely to make the small local denial variant symmetric.
#[allow(clippy::large_enum_variant)]
pub(crate) enum DecodedMessage {
    Protocol(ServerJsonRpcMessage),
    DeniedRequest(RequestId),
}

pub(crate) use envelope::{EnvelopeKind, EnvelopeRoute};

pub(crate) fn classify(input: &[u8]) -> io::Result<EnvelopeRoute<'_>> {
    profile::inspect(input)?;
    envelope::Envelope::read(input)?.route()
}

pub(crate) fn decode(
    input: &[u8],
    expected_method: Option<&str>,
    admit: &mut dyn FnMut(usize) -> io::Result<()>,
) -> io::Result<DecodedMessage> {
    let shape = profile::inspect(input)?;
    let envelope = envelope::Envelope::read(input)?;
    let route = envelope.route()?;
    if route.kind == EnvelopeKind::Request && route.method.as_deref() != Some("ping") {
        let id = route.id.ok_or_else(|| invalid("MCP request ID missing"))?;
        // RawFrame separately owns the original JSON and parser scratch. Only
        // the exact decoded ID, its scalar-parser overlap, and fixed denial
        // framing survive. Unsupported params receive no method-specific decode.
        let retained = id
            .encoded_bytes()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(16 * 1024))
            .ok_or_else(|| invalid("MCP denied-request allocation overflow"))?;
        admit(retained)?;
        return id.owned().map(DecodedMessage::DeniedRequest);
    }
    // Correlation is required even when the remote sends an error. Id-less
    // protocol errors have no result graph or corresponding pending request.
    if matches!(route.kind, EnvelopeKind::Response)
        || (matches!(route.kind, EnvelopeKind::Error) && route.id.is_some())
    {
        expected_method.ok_or_else(|| invalid("MCP response lacks request correlation"))?;
    }
    admit(profile::working_bytes(
        &shape,
        messages::request_graph(&envelope, &route, expected_method)?,
    )?)?;
    messages::decode(&envelope, &route, expected_method).map(DecodedMessage::Protocol)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
