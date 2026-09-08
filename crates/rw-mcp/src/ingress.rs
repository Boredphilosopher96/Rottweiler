//! Shared pre-decode MCP framing, routing and allocation accounting.
pub(crate) mod envelope;
pub(crate) mod frame;
mod header;
pub(crate) mod profile;

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}
fn json_error(error: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}
