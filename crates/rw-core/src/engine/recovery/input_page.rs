//! Input claims and materialization share the exact content-bound source checkpoint.
use super::{RecoveryError, input::materialize_claimed_event};
use rw_store::session::journal::{JournalPrefixIdentity, JournalReadView, VerifiedJournalPage};
use rw_types::{EngineEvent, input_claims::InputClaimState};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, sync::Arc};

/// Persisted input state cannot be separated from the source digest it interpreted.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputClaimCheckpoint {
    prefix: Prefix,
    claims: InputClaimState,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Prefix {
    next_sequence: u64,
    digest: [u8; 32],
}
impl From<JournalPrefixIdentity> for Prefix {
    fn from(value: JournalPrefixIdentity) -> Self {
        Self {
            next_sequence: value.next_sequence,
            digest: value.digest,
        }
    }
}
impl Default for InputClaimCheckpoint {
    fn default() -> Self {
        Self {
            prefix: JournalPrefixIdentity::empty().into(),
            claims: InputClaimState::default(),
        }
    }
}
impl InputClaimCheckpoint {
    fn identity(&self) -> JournalPrefixIdentity {
        JournalPrefixIdentity {
            next_sequence: self.prefix.next_sequence,
            digest: self.prefix.digest,
        }
    }
    fn validate(&self) -> Result<(), RecoveryError> {
        self.claims
            .validate_checkpoint(
                self.prefix.next_sequence,
                self.claims.session_id().map_or("", |id| id.0.as_str()),
            )
            .map_err(RecoveryError::Invalid)
    }
    pub(crate) fn validate_at(&self, expected: JournalPrefixIdentity) -> Result<(), RecoveryError> {
        self.validate()?;
        if self.identity() != expected {
            return Err(RecoveryError::Invalid("input checkpoint source digest"));
        }
        Ok(())
    }
    /// Encode only bounded, source-bound selector metadata.
    /// # Errors
    /// Rejects inconsistent or oversized checkpoints before publication.
    pub fn encode(&self) -> Result<Vec<u8>, RecoveryError> {
        self.validate()?;
        super::encoding::encode(
            self,
            rw_types::input_claims::MAX_INPUT_CLAIM_CHECKPOINT_BYTES,
        )
    }
    /// Decode the required checkpoint for one exact published search prefix.
    /// # Errors
    /// Rejects unknown/missing fields, structural overflow and a different source digest.
    pub fn decode(bytes: &[u8], expected: JournalPrefixIdentity) -> Result<Self, RecoveryError> {
        let shape = rw_types::json_structure::preflight_json(
            bytes,
            rw_types::json_structure::JsonStructureLimits {
                max_encoded_bytes: rw_types::input_claims::MAX_INPUT_CLAIM_CHECKPOINT_BYTES,
                // 128 identity records have six key/value pairs each, plus the envelope.
                max_nodes: 2048,
                max_string_bytes: 32 * 1024,
                max_depth: 5,
            },
        )?;
        if shape
            .decode_bytes::<Self>()
            .is_none_or(|bytes| bytes > 1024 * 1024)
        {
            return Err(RecoveryError::Limit("input checkpoint decoded allocation"));
        }
        let checkpoint: Self = serde_json::from_slice(bytes)?;
        checkpoint.validate_at(expected)?;
        Ok(checkpoint)
    }
}

/// A bounded sequential fold over events belonging to one verified journal page.
pub struct InputClaimPage<'a> {
    page: &'a VerifiedJournalPage<EngineEvent>,
    source: Arc<JournalReadView>,
    claims: InputClaimState,
    position: usize,
}
/// The exact event and source prefix whose preceding input claim was checked.
pub struct ClaimedInputEvent<'a> {
    checked: rw_types::input_claims::InputClaimChecked<'a>,
    source: Arc<JournalReadView>,
}
impl<'a> InputClaimPage<'a> {
    /// Bind a persisted claim checkpoint to the page's proven starting watermark.
    /// # Errors
    /// Rejects a checkpoint from a different or incomplete source prefix.
    pub fn new(
        page: &'a VerifiedJournalPage<EngineEvent>,
        checkpoint: InputClaimCheckpoint,
    ) -> Result<Self, RecoveryError> {
        checkpoint.validate()?;
        let first = page.page().events.first().map_or_else(
            || {
                page.page()
                    .next_cursor
                    .map_or(Some(0), |cursor| cursor.0.checked_add(1))
            },
            |event| Some(event.sequence.0),
        );
        if first != Some(checkpoint.claims.next_sequence()) {
            return Err(RecoveryError::Invalid("input page starting watermark"));
        }
        let advance = page
            .proof()
            .advance(checkpoint.identity(), page.page().next_cursor)?;
        Ok(Self {
            page,
            source: Arc::new(advance.next().clone()),
            claims: checkpoint.claims,
            position: 0,
        })
    }
    /// Check the next event, retaining source identity through later materialization.
    /// # Errors
    /// Rejects an invalid phase, repeated claim or mismatched event envelope.
    pub fn next_event(&mut self) -> Result<Option<ClaimedInputEvent<'a>>, RecoveryError> {
        let Some(envelope) = self.page.page().events.get(self.position) else {
            return Ok(None);
        };
        if envelope
            .event
            .meta()
            .is_none_or(|meta| meta.sequence_id != envelope.sequence)
        {
            return Err(RecoveryError::Invalid("input event envelope identity"));
        }
        let checked = self
            .claims
            .advance(&envelope.event)
            .map_err(RecoveryError::Invalid)?;
        self.position += 1;
        Ok(Some(ClaimedInputEvent {
            checked,
            source: Arc::clone(&self.source),
        }))
    }
    /// Capture only consumed events, for atomic publication with their derived rows.
    /// # Errors
    /// Rejects a claim watermark not covered by this verified page.
    pub fn checkpoint(&self) -> Result<InputClaimCheckpoint, RecoveryError> {
        let through = self
            .claims
            .next_sequence()
            .checked_sub(1)
            .map(rw_types::SequenceId);
        Ok(InputClaimCheckpoint {
            prefix: self
                .page
                .proof()
                .prefix_through(through)?
                .prefix_identity()
                .into(),
            claims: self.claims.clone(),
        })
    }
}
impl<'a> ClaimedInputEvent<'a> {
    /// Resolve this checked event against its descriptor-bound source page.
    /// # Errors
    /// Rejects unavailable input bodies and invalid source/text selectors.
    pub fn materialize(self) -> Result<Cow<'a, EngineEvent>, RecoveryError> {
        materialize_claimed_event(&self.source, self.checked)
    }
}

impl rw_types::allocation::DecodeAllocation for InputClaimCheckpoint {
    fn decode_node_bytes() -> Option<usize> {
        Some(
            std::mem::size_of::<Self>()
                .max(
                    <InputClaimState as rw_types::allocation::DecodeAllocation>::decode_node_bytes(
                    )?,
                )
                .max(std::mem::size_of::<Prefix>()),
        )
    }
}

#[cfg(test)]
#[path = "input_page_tests.rs"]
mod tests;
