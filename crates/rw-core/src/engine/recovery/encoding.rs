use super::RecoveryError;
use rw_types::json_encoding::JsonWriter;
use serde::Serialize;

pub(super) fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, RecoveryError> {
    let mut bytes = Vec::new();
    let mut output = JsonWriter::buffer(&mut bytes, limit, 0).map_err(serde_json::Error::io)?;
    let result = output.serialize(value);
    if output.exceeded() {
        return Err(RecoveryError::Limit("serialized recovery metadata"));
    }
    result?;
    Ok(bytes)
}

pub(super) fn serialized_size(value: &impl Serialize) -> Result<u64, RecoveryError> {
    let mut output = JsonWriter::count(usize::MAX);
    output.serialize(value)?;
    u64::try_from(output.written())
        .map_err(|_| RecoveryError::Limit("serialized recovery metadata"))
}

/// The fold and source reader decode the same canonical event. Persist the
/// normalized retained allocation of that turn, independently of decoder scratch.
/// `SourceReader` rechecks the prepared allocation before transferring the turn.
pub(super) fn turn_decode_bytes(turn: &rw_types::Turn) -> Result<u64, RecoveryError> {
    use rw_types::allocation::PrepareAllocation;
    turn.prepared_bytes()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(RecoveryError::Limit("retained conversation allocation"))
}

pub(super) fn decode_bytes<T: Serialize + rw_types::allocation::DecodeAllocation>(
    value: &T,
) -> Result<u64, RecoveryError> {
    let limit = rw_store::session::SessionEventPageLimits::default().max_line_bytes;
    let bytes = encode(value, limit)?;
    let shape = rw_types::json_structure::preflight_json(
        &bytes,
        rw_types::json_structure::JsonStructureLimits {
            max_encoded_bytes: limit,
            max_nodes: 65_536,
            max_string_bytes: limit,
            max_depth: 64,
        },
    )?;
    shape
        .decode_bytes::<T>()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(RecoveryError::Limit("conversation decoded allocation"))
}
