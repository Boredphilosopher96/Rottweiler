//! Method-correlated MCP decoding. The transport owns encoded bytes and header
//! scratch; the required admission callback owns every typed decoder allocation.
use std::io;

use rmcp::model::ServerJsonRpcMessage;

use crate::ingress::envelope;
mod messages;
mod profile;
#[cfg(test)]
mod tests;

pub(crate) use envelope::{EnvelopeKind, EnvelopeRoute};

pub(crate) fn classify(input: &[u8]) -> io::Result<EnvelopeRoute<'_>> {
    profile::inspect(input)?;
    envelope::Envelope::read(input)?.route()
}

pub(crate) fn decode(
    input: &[u8],
    expected_method: Option<&str>,
    admit: &mut dyn FnMut(usize) -> io::Result<()>,
) -> io::Result<ServerJsonRpcMessage> {
    let shape = profile::inspect(input)?;
    let envelope = envelope::Envelope::read(input)?;
    let route = envelope.route()?;
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
    messages::decode(&envelope, &route, expected_method)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
