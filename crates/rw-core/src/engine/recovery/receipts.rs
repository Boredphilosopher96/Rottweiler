//! Exact attempt receipt selectors survive rewind and compaction as accounting authority.
use super::{
    CanonicalHistory, RecoveryError, RecoveryHead, projector::BatchRows, read::SourceReader,
};
use rw_store::session::{UtcTimestamp, reservations::ProviderCallReceipt};
use rw_types::{EngineEvent, EventMeta, ProviderCallActuals, ProviderCallIdentity, SequenceId};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
const NAMESPACE: u8 = 1;
#[derive(Serialize, Deserialize)]
struct ReceiptSource {
    identity: ProviderCallIdentity,
    sequence: SequenceId,
}
fn key(identity: &ProviderCallIdentity) -> Result<Vec<u8>, RecoveryError> {
    super::encoding::encode(
        &(&identity.call_id, identity.attempt),
        rw_store::session::recovery_index::MAX_RECOVERY_LOOKUP_KEY_BYTES,
    )
}
fn receipt(
    meta: &EventMeta,
    call: ProviderCallIdentity,
    actuals: ProviderCallActuals,
) -> Result<ProviderCallReceipt, RecoveryError> {
    let receipt = ProviderCallReceipt {
        identity: call,
        sequence_id: meta.sequence_id,
        accounted_at: UtcTimestamp::parse(meta.emitted_at.clone())
            .map_err(|_| RecoveryError::Invalid("provider receipt timestamp"))?,
        actuals,
    };
    receipt
        .validate()
        .map_err(|_| RecoveryError::Invalid("provider receipt"))?;
    Ok(receipt)
}
pub(super) fn index(
    head: &RecoveryHead,
    meta: &EventMeta,
    call: ProviderCallIdentity,
    actuals: ProviderCallActuals,
    rows: &mut BatchRows,
) -> Result<(), RecoveryError> {
    let receipt = receipt(meta, call, actuals)?;
    if head
        .inherited_journal_through
        .is_some_and(|through| meta.sequence_id <= through)
    {
        return Ok(());
    }
    if receipt.identity.session_id != meta.session_id {
        return Err(RecoveryError::Invalid("provider receipt source session"));
    }
    let key = key(&receipt.identity)?;
    if let Some(previous) = rows.lookup::<ReceiptSource>(NAMESPACE, &key)?
        && previous.identity != receipt.identity
    {
        return Err(RecoveryError::Invalid("provider attempt identity changed"));
    }
    rows.put_lookup(
        NAMESPACE,
        key,
        &ReceiptSource {
            identity: receipt.identity,
            sequence: meta.sequence_id,
        },
    )
}
impl CanonicalHistory {
    /// Resolve only the latest durable receipt for this exact source-owned attempt.
    /// Accounting selectors do not rewind with conversation state.
    ///
    /// # Errors
    /// Rejects foreign identities, invalid selectors, and malformed receipt actuals.
    pub fn provider_receipt(
        &self,
        identity: &ProviderCallIdentity,
    ) -> Result<Option<ProviderCallReceipt>, RecoveryError> {
        if self.head.session_id.as_ref() != Some(&identity.session_id) {
            return Err(RecoveryError::Invalid("provider query source session"));
        }
        let Some(bytes) = self.read.lookup(NAMESPACE, &key(identity)?)? else {
            return Ok(None);
        };
        let source: ReceiptSource = serde_json::from_slice(&bytes)?;
        if source.identity != *identity {
            return Err(RecoveryError::Invalid("provider attempt query identity"));
        }
        let mut reader = SourceReader {
            source: &self.source,
            events: VecDeque::new(),
        };
        let EngineEvent::ProviderCallAccounted {
            meta,
            call,
            actuals,
        } = reader.event(source.sequence)?
        else {
            return Err(RecoveryError::Invalid("provider receipt selector"));
        };
        if call != *identity
            || meta.session_id != identity.session_id
            || meta.sequence_id != source.sequence
        {
            return Err(RecoveryError::Invalid("provider receipt source identity"));
        }
        Ok(Some(receipt(&meta, call, actuals)?))
    }
}
#[cfg(test)]
mod tests;
