//! One admitted decode path for canonical session metadata.
use super::{MAX_SESSION_METADATA_BYTES, SessionMetadata};
use miette::{IntoDiagnostic, Result, miette};
use rw_core::recovery::{HistoryRead, HistoryWorkingAllowance};
use rw_types::{
    Turn,
    allocation::DecodeAllocation,
    json_structure::{JsonStructureLimits, preflight_json},
};

pub(crate) fn admit_write(
    turns: &[Turn],
    other_bytes: usize,
    allowance: &mut dyn HistoryWorkingAllowance,
) -> Result<()> {
    use rw_types::allocation::PrepareAllocation;
    let source = turns
        .iter()
        .try_fold(0_usize, |bytes, turn| {
            bytes.checked_add(turn.prepared_bytes()?)
        })
        .ok_or_else(|| miette!("session metadata context allocation overflow"))?;
    let bytes = source
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(other_bytes.checked_mul(4)?))
        .and_then(|bytes| bytes.checked_add(usize::try_from(MAX_SESSION_METADATA_BYTES).ok()?))
        .and_then(|bytes| bytes.checked_add(4096))
        .ok_or_else(|| miette!("session metadata write allocation overflow"))?;
    allowance.resize(bytes).into_diagnostic()
}

impl DecodeAllocation for SessionMetadata {
    fn decode_node_bytes() -> Option<usize> {
        // All other fields are scalars, strings/paths and flat vectors thereof.
        // The Turn profile follows every nested canonical block/result variant.
        Some(Vec::<Turn>::decode_node_bytes()?.max(std::mem::size_of::<Self>()))
    }
}
struct Input {
    bytes: Vec<u8>,
    allowance: Box<dyn HistoryWorkingAllowance>,
}
pub(super) fn decode(
    bytes: Vec<u8>,
    allowance: Box<dyn HistoryWorkingAllowance>,
) -> Result<HistoryRead<SessionMetadata>> {
    Input { bytes, allowance }.decode()
}
impl Input {
    fn decode(mut self) -> Result<HistoryRead<SessionMetadata>> {
        let limit = usize::try_from(MAX_SESSION_METADATA_BYTES).into_diagnostic()?;
        let shape = preflight_json(
            &self.bytes,
            JsonStructureLimits {
                max_encoded_bytes: limit,
                max_string_bytes: limit,
                max_nodes: 65_536,
                max_depth: 64,
            },
        )
        .into_diagnostic()?;
        let bytes = shape
            .decode_bytes::<SessionMetadata>()
            .and_then(|bytes| bytes.checked_add(self.bytes.capacity().checked_mul(2)?))
            .ok_or_else(|| miette!("session metadata decode allocation overflow"))?;
        self.allowance.resize(bytes).into_diagnostic()?;
        let metadata = serde_json::from_slice(&self.bytes).into_diagnostic()?;
        Ok(HistoryRead::new(metadata, self.allowance))
    }
}
