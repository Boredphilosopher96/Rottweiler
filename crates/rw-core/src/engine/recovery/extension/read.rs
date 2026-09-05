use super::{
    Acknowledgement, NAMESPACES, Namespace, ROOTS, StateRoot, acknowledgement_key,
    validate_namespace, validate_plugin, validate_root,
};
use crate::engine::{
    ExtensionStateView,
    recovery::{CanonicalHistory, RecoveryError, projector::key, read::SourceReader},
};
use rw_types::{
    EngineEvent,
    extension_contract::{
        ExtensionDeliveryCursor, ExtensionStateEntry, ExtensionStateMutation,
        ExtensionStateSnapshot, state_value_bytes,
    },
};
use std::collections::VecDeque;

impl CanonicalHistory {
    /// Read one namespace's exact values and aggregate counters from the same captured prefix.
    /// Payloads remain source-backed; at most 64 values and 256 KiB of JSON are materialized.
    ///
    /// # Errors
    /// Rejects inconsistent source selectors, namespace/cursor identity and resource overflow.
    pub fn extension_state(&self, plugin: &str) -> Result<ExtensionStateView, RecoveryError> {
        validate_plugin(plugin)?;
        let root: StateRoot = match self.head.extension_root {
            Some(revision) => self
                .read
                .get(key(ROOTS, 0, revision.0))?
                .map(|row| serde_json::from_slice(&row.payload))
                .transpose()?
                .ok_or(RecoveryError::Invalid("missing extension root"))?,
            None => StateRoot::default(),
        };
        validate_root(&root)?;
        let current = root.namespaces.iter().find(|entry| entry.plugin == plugin);
        let mut snapshot = ExtensionStateSnapshot {
            revision: current.map(|entry| entry.revision),
            entries: Vec::new(),
            acknowledged: None,
            delivery_start: self
                .head
                .inherited_journal_through
                .map(|sequence| {
                    self.head
                        .session_id
                        .clone()
                        .map(|session_id| ExtensionDeliveryCursor {
                            session_id,
                            sequence,
                        })
                        .ok_or(RecoveryError::Invalid("missing fork session identity"))
                })
                .transpose()?,
        };
        if let Some(current) = current {
            let mut namespace: Namespace = self
                .read
                .get(key(NAMESPACES, 0, current.revision.0))?
                .map(|row| serde_json::from_slice(&row.payload))
                .transpose()?
                .ok_or(RecoveryError::Invalid("missing extension namespace"))?;
            validate_namespace(&namespace, current.bytes)?;
            let mut source = SourceReader {
                source: &self.source,
                events: VecDeque::new(),
            };
            namespace
                .entries
                .sort_unstable_by_key(|entry| (entry.sequence, entry.mutation));
            let mut cached: Option<(rw_types::SequenceId, EngineEvent)> = None;
            for entry in namespace.entries {
                if entry.sequence > current.revision {
                    return Err(RecoveryError::Invalid("extension source revision"));
                }
                if cached
                    .as_ref()
                    .is_none_or(|(sequence, _)| *sequence != entry.sequence)
                {
                    cached = Some((entry.sequence, source.event(entry.sequence)?));
                }
                let Some((
                    _,
                    EngineEvent::ExtensionStateCommitted {
                        plugin_id,
                        transaction,
                        ..
                    },
                )) = &cached
                else {
                    return Err(RecoveryError::Invalid("extension value source"));
                };
                let Some(ExtensionStateMutation::Set { key, value }) =
                    transaction.mutations.get(entry.mutation)
                else {
                    return Err(RecoveryError::Invalid("extension mutation source"));
                };
                if plugin_id != plugin
                    || key != &entry.key
                    || state_value_bytes(value)
                        .map_err(|_| RecoveryError::Invalid("extension source value bytes"))?
                        != entry.bytes
                {
                    return Err(RecoveryError::Invalid("extension value identity"));
                }
                snapshot.entries.push(ExtensionStateEntry {
                    key: key.clone(),
                    value: value.clone(),
                });
            }
        }
        snapshot.entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        if let Some(row) = self.read.get(acknowledgement_key(plugin))? {
            let ack: Acknowledgement = serde_json::from_slice(&row.payload)?;
            if ack.plugin != plugin
                || self.head.session_id.as_ref() != Some(&ack.cursor.session_id)
                || ack.cursor.sequence.0 >= self.head.next_sequence
            {
                return Err(RecoveryError::Invalid("extension delivery identity"));
            }
            snapshot.acknowledged = Some(ack.cursor);
        }
        Ok(ExtensionStateView {
            snapshot,
            session_bytes: root.bytes,
            namespaces: root.namespaces.len(),
        })
    }
}
