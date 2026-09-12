//! Exact bounded SEP-2243 base64 sentinel encoding.
use super::{McpError, invalid};
use base64::{Engine as _, prelude::BASE64_STANDARD};
const PREFIX: &str = "=?base64?";
const SUFFIX: &str = "?=";

fn wrapped(value: &str) -> bool {
    let bytes = value.as_bytes();
    matches!(bytes.first(), Some(b' ' | b'\t'))
        || matches!(bytes.last(), Some(b' ' | b'\t'))
        || bytes.iter().any(|byte| !(0x20..=0x7e).contains(byte))
        || (value.starts_with(PREFIX) && value.ends_with(SUFFIX))
}
pub(super) fn encoded_len(value: &str) -> Result<usize, McpError> {
    if !wrapped(value) {
        return Ok(value.len());
    }
    value
        .len()
        .checked_add(2)
        .map(|n| n / 3)
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| n.checked_add(PREFIX.len() + SUFFIX.len()))
        .ok_or_else(invalid)
}
pub(super) fn encode(value: &str) -> Result<String, McpError> {
    if !wrapped(value) {
        return Ok(value.to_owned());
    }
    let mut output = String::with_capacity(encoded_len(value)?);
    output.push_str(PREFIX);
    BASE64_STANDARD.encode_string(value, &mut output);
    output.push_str(SUFFIX);
    Ok(output)
}
