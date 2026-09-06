use super::{RecoveryError, RecoveryHead, encoding::encode};
use rw_ext::ModeRegistry;
use rw_store::session::{
    SessionEventPageLimits,
    journal::{JournalPrefixIdentity, JournalReadView},
    recovery_index::{
        MAX_RECOVERY_BATCH_BYTES, MAX_RECOVERY_BATCH_ROWS, MAX_RECOVERY_HEAD_BYTES,
        MAX_RECOVERY_ROW_BYTES, RecoveryIndex, RecoveryKey, RecoveryLookup, RecoveryMutation,
        RecoveryReadView, RecoveryRow,
    },
};
use rw_types::{EngineEvent, SequenceId};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

pub(super) const VERSION: u32 = 21;
const EVENTS_PER_BATCH: usize = 64;

/// One cancellable, durable canonical projection step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveryProgress {
    pub applied_next_sequence: u64,
    pub interpreted_events: usize,
    pub maintenance: bool,
    pub has_more: bool,
}

/// Sole writer of canonical source indexes and the bounded live recovery checkpoint.
pub struct CanonicalRecovery {
    pub(super) index: RecoveryIndex,
    fingerprint: [u8; 32],
    inherited_journal_through: Option<SequenceId>,
}
impl CanonicalRecovery {
    /// Open a compatible index and validate its canonical prefix and runtime registry.
    ///
    /// # Errors
    /// Rejects unsafe storage, stale/corrupt checkpoints and changed mode definitions.
    pub fn open(
        source: &JournalReadView,
        modes: &ModeRegistry,
        inherited_journal_through: Option<SequenceId>,
    ) -> Result<Self, RecoveryError> {
        let recovery = Self {
            index: RecoveryIndex::open(
                source,
                rw_store::session::recovery_index::RecoveryProjection::Conversation,
                VERSION,
            )?,
            fingerprint: registry_fingerprint(modes),
            inherited_journal_through,
        };
        recovery.head()?;
        Ok(recovery)
    }
    /// Explicitly reset only derived recovery state before incremental rebuild.
    ///
    /// # Errors
    /// Rejects unsafe descriptors or concurrent ownership; canonical data is unchanged.
    pub fn rebuild(
        source: &JournalReadView,
        modes: &ModeRegistry,
        inherited_journal_through: Option<SequenceId>,
    ) -> Result<Self, RecoveryError> {
        Ok(Self {
            index: RecoveryIndex::rebuild(
                source,
                rw_store::session::recovery_index::RecoveryProjection::Conversation,
                VERSION,
            )?,
            fingerprint: registry_fingerprint(modes),
            inherited_journal_through,
        })
    }
    /// Read only the bounded control checkpoint, without historical message bodies.
    ///
    /// # Errors
    /// Rejects incompatible checkpoint metadata or registry identity.
    pub fn head(&self) -> Result<RecoveryHead, RecoveryError> {
        let read = self.index.read()?;
        self.decode_head(&read)
    }
    pub(super) fn decode_head(
        &self,
        read: &RecoveryReadView,
    ) -> Result<RecoveryHead, RecoveryError> {
        let stored = read.head();
        if stored.checkpoint.is_empty() {
            if stored.prefix != JournalPrefixIdentity::empty() {
                return Err(RecoveryError::Invalid("missing recovery checkpoint"));
            }
            return Ok(RecoveryHead::new(
                self.fingerprint,
                self.inherited_journal_through,
            ));
        }
        let head: RecoveryHead = serde_json::from_slice(&stored.checkpoint)?;
        head.validate()?;
        if head.next_sequence != stored.prefix.next_sequence {
            return Err(RecoveryError::Invalid("head/source prefix mismatch"));
        }
        if head.inherited_journal_through != self.inherited_journal_through {
            return Err(RecoveryError::Invalid("inherited journal identity changed"));
        }
        if head.registry_fingerprint != self.fingerprint {
            return Err(RecoveryError::RegistryChanged);
        }
        Ok(head)
    }
    /// Apply one bounded raw page or one resumable rewind/clear maintenance step.
    ///
    /// # Errors
    /// Rejects malformed durable transitions, resource overflow and storage failure.
    #[tracing::instrument(target = "rw_performance", level = "trace", name = "recovery.project", skip_all, fields(source_next_sequence = source.prefix_identity().next_sequence))]
    pub fn advance(
        &mut self,
        source: &JournalReadView,
        modes: &ModeRegistry,
    ) -> Result<RecoveryProgress, RecoveryError> {
        if registry_fingerprint(modes) != self.fingerprint {
            return Err(RecoveryError::RegistryChanged);
        }
        let read = self.index.read()?;
        let mut head = self.decode_head(&read)?;
        if head.maintenance.is_some() {
            return self.maintain(source, &read, head);
        }
        let previous = read.head().prefix;
        let page = source.verified_page::<EngineEvent>(
            previous.next_sequence.checked_sub(1).map(SequenceId),
            SessionEventPageLimits {
                max_page_events: EVENTS_PER_BATCH,
                max_page_bytes: SessionEventPageLimits::default().max_line_bytes as u64 + 1,
                ..SessionEventPageLimits::default()
            },
        )?;
        let mut rows = BatchRows::new(read);
        let mut interpreted = 0;
        for envelope in &page.page.events {
            if envelope
                .event
                .meta()
                .is_none_or(|meta| meta.sequence_id != envelope.sequence)
            {
                return Err(RecoveryError::Invalid("envelope/event sequence mismatch"));
            }
            let mut next = head.clone();
            rows.begin_event();
            super::reduce::reduce(&mut next, &envelope.event, modes, &mut rows, source)?;
            let checkpoint = encode(&next, MAX_RECOVERY_HEAD_BYTES)?;
            if !rows.fits(checkpoint.len()) {
                rows.rollback_event();
                if interpreted == 0 {
                    return Err(RecoveryError::Limit("single event metadata batch"));
                }
                break;
            }
            rows.commit_event();
            head = next;
            interpreted += 1;
            if head.maintenance.is_some() {
                break;
            }
        }
        if interpreted == 0 {
            return Ok(progress(&head, 0, source));
        }
        let through = head.next_sequence.checked_sub(1).map(SequenceId);
        let advance = page.proof.advance(previous, through)?;
        let lookups: Vec<_> = rows.lookups.into_values().collect();
        let mutations: Vec<_> = rows.changes.into_values().collect();
        self.index.apply(
            &advance,
            &encode(&head, MAX_RECOVERY_HEAD_BYTES)?,
            &mutations,
            &lookups,
        )?;
        Ok(progress(&head, interpreted, source))
    }
}

pub(super) fn progress(
    head: &RecoveryHead,
    interpreted: usize,
    source: &JournalReadView,
) -> RecoveryProgress {
    RecoveryProgress {
        applied_next_sequence: head.next_sequence,
        interpreted_events: interpreted,
        maintenance: head.maintenance.is_some(),
        has_more: head.maintenance.is_some()
            || head.next_sequence < source.prefix_identity().next_sequence,
    }
}
fn registry_fingerprint(modes: &ModeRegistry) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    for mode in modes.iter() {
        hash.update(&(mode.id().0.len() as u64).to_le_bytes());
        hash.update(mode.id().0.as_bytes());
        hash.update(mode.semantic_fingerprint().as_bytes());
    }
    *hash.finalize().as_bytes()
}

pub(super) struct BatchRows {
    pub(super) read: RecoveryReadView,
    pub(super) changes: BTreeMap<RecoveryKey, RecoveryMutation>,
    undo: BTreeMap<RecoveryKey, Option<RecoveryMutation>>,
    pub(super) lookups: BTreeMap<(u8, Vec<u8>), RecoveryLookup>,
    lookup_undo: BTreeMap<(u8, Vec<u8>), Option<RecoveryLookup>>,
}
impl BatchRows {
    pub(super) fn new(read: RecoveryReadView) -> Self {
        Self {
            read,
            changes: BTreeMap::new(),
            undo: BTreeMap::new(),
            lookups: BTreeMap::new(),
            lookup_undo: BTreeMap::new(),
        }
    }
    pub(super) fn begin_event(&mut self) {
        self.undo.clear();
        self.lookup_undo.clear();
    }
    pub(super) fn commit_event(&mut self) {
        self.undo.clear();
        self.lookup_undo.clear();
    }
    pub(super) fn rollback_event(&mut self) {
        for (key, value) in std::mem::take(&mut self.lookup_undo) {
            if let Some(value) = value {
                self.lookups.insert(key, value);
            } else {
                self.lookups.remove(&key);
            }
        }

        for (key, value) in std::mem::take(&mut self.undo) {
            match value {
                Some(value) => {
                    self.changes.insert(key, value);
                }
                None => {
                    self.changes.remove(&key);
                }
            }
        }
    }
    pub(super) fn get<T: DeserializeOwned>(
        &self,
        key: RecoveryKey,
    ) -> Result<Option<T>, RecoveryError> {
        let row = match self.changes.get(&key) {
            Some(RecoveryMutation::Put(row)) => Some(row.clone()),
            Some(RecoveryMutation::Delete(_)) => None,
            None => self.read.get(key)?,
        };
        row.map(|row| serde_json::from_slice(&row.payload).map_err(RecoveryError::from))
            .transpose()
    }
    pub(super) fn delete(&mut self, key: RecoveryKey) {
        self.undo
            .entry(key)
            .or_insert_with(|| self.changes.get(&key).cloned());
        self.changes.insert(key, RecoveryMutation::Delete(key));
    }

    /// Resolve the latest row in the transaction's bounded mutation overlay.
    pub(super) fn last_before(
        &self,
        namespace: u8,
        scope: u64,
        before: u64,
    ) -> Result<Option<RecoveryRow>, RecoveryError> {
        let pending = self
            .changes
            .range(key(namespace, scope, 0)..key(namespace, scope, before))
            .rev()
            .find_map(|(_, change)| match change {
                RecoveryMutation::Put(row) => Some(row.clone()),
                RecoveryMutation::Delete(_) => None,
            });
        let mut end = before;
        loop {
            let persisted = self.read.last_before(namespace, scope, end)?;
            let Some(row) = persisted else {
                return Ok(pending);
            };
            if pending
                .as_ref()
                .is_some_and(|pending| pending.key.ordinal >= row.key.ordinal)
            {
                return Ok(pending);
            }
            match self.changes.get(&row.key) {
                Some(RecoveryMutation::Delete(_)) => end = row.key.ordinal,
                Some(RecoveryMutation::Put(row)) => return Ok(Some(row.clone())),
                None => return Ok(Some(row)),
            }
        }
    }

    pub(super) fn put(
        &mut self,
        key: RecoveryKey,
        value: &impl Serialize,
    ) -> Result<(), RecoveryError> {
        self.undo
            .entry(key)
            .or_insert_with(|| self.changes.get(&key).cloned());
        self.changes.insert(
            key,
            RecoveryMutation::Put(RecoveryRow {
                key,
                payload: encode(value, MAX_RECOVERY_ROW_BYTES)?,
            }),
        );
        Ok(())
    }
    pub(super) fn lookup<T: DeserializeOwned>(
        &self,
        namespace: u8,
        key: &[u8],
    ) -> Result<Option<T>, RecoveryError> {
        let payload = self
            .lookups
            .get(&(namespace, key.to_vec()))
            .map(|row| row.payload.clone())
            .map_or_else(|| self.read.lookup(namespace, key), |value| Ok(Some(value)))?;
        payload
            .map(|bytes| serde_json::from_slice(&bytes).map_err(RecoveryError::from))
            .transpose()
    }
    pub(super) fn put_lookup(
        &mut self,
        namespace: u8,
        key: Vec<u8>,
        value: &impl Serialize,
    ) -> Result<(), RecoveryError> {
        if key.len() > rw_store::session::recovery_index::MAX_RECOVERY_LOOKUP_KEY_BYTES {
            return Err(RecoveryError::Limit("lookup identity"));
        }
        let identity = (namespace, key.clone());
        self.lookup_undo
            .entry(identity.clone())
            .or_insert_with(|| self.lookups.get(&identity).cloned());
        self.lookups.insert(
            identity,
            RecoveryLookup {
                namespace,
                key,
                payload: encode(value, MAX_RECOVERY_ROW_BYTES)?,
            },
        );
        Ok(())
    }
    pub(super) fn fits(&self, head_bytes: usize) -> bool {
        let lookup_bytes = self
            .lookups
            .values()
            .map(|row| row.key.len() + row.payload.len() + 24)
            .sum::<usize>();
        self.changes.len() + self.lookups.len() <= MAX_RECOVERY_BATCH_ROWS
            && self
                .changes
                .values()
                .fold(head_bytes + lookup_bytes, |bytes, change| {
                    bytes
                        + 24
                        + match change {
                            RecoveryMutation::Put(row) => row.payload.len(),
                            RecoveryMutation::Delete(_) => 0,
                        }
                })
                <= MAX_RECOVERY_BATCH_BYTES
    }
}
pub(super) const fn key(namespace: u8, scope: u64, ordinal: u64) -> RecoveryKey {
    RecoveryKey {
        namespace,
        scope,
        ordinal,
    }
}
