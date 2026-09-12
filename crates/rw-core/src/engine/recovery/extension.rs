//! Bounded source selectors for logical state, with independent physical delivery progress.

mod read;

use super::{
    RecoveryError, RecoveryHead,
    projector::{BatchRows, key},
};
use rw_types::{
    SequenceId,
    extension_contract::{
        ExtensionDeliveryCursor, ExtensionStateMutation, ExtensionStateTransaction,
        MAX_EXTENSION_NAMESPACE_BYTES, MAX_EXTENSION_NAMESPACES, MAX_EXTENSION_STATE_KEYS,
        MAX_SESSION_EXTENSION_STATE_BYTES, state_value_bytes, validate_state_transaction,
    },
};
use serde::{Deserialize, Serialize};

const ROOTS: u8 = 9;
const NAMESPACES: u8 = 10;
const ACKNOWLEDGEMENTS: u8 = 11;
const MAX_PLUGIN_ID_BYTES: usize = 128;

#[derive(Default, Deserialize, Serialize)]
struct StateRoot {
    namespaces: Vec<NamespaceRevision>,
    bytes: usize,
}
#[derive(Deserialize, Serialize)]
struct NamespaceRevision {
    plugin: String,
    revision: SequenceId,
    bytes: usize,
}
#[derive(Default, Deserialize, Serialize)]
struct Namespace {
    entries: Vec<ValueSource>,
}
#[derive(Deserialize, Serialize)]
struct ValueSource {
    key: String,
    sequence: SequenceId,
    mutation: usize,
    bytes: usize,
}
#[derive(Deserialize, Serialize)]
struct Acknowledgement {
    plugin: String,
    cursor: ExtensionDeliveryCursor,
}

fn validate_plugin(plugin: &str) -> Result<(), RecoveryError> {
    if plugin.is_empty() || plugin.len() > MAX_PLUGIN_ID_BYTES {
        return Err(RecoveryError::Invalid("extension namespace identity"));
    }
    Ok(())
}

fn acknowledgement_key(plugin: &str) -> rw_store::session::recovery_index::RecoveryKey {
    let hash = blake3::hash(plugin.as_bytes());
    let bytes = hash.as_bytes();
    let mut scope = [0; 8];
    let mut ordinal = [0; 8];
    scope.copy_from_slice(&bytes[..8]);
    ordinal.copy_from_slice(&bytes[8..16]);
    key(
        ACKNOWLEDGEMENTS,
        u64::from_le_bytes(scope),
        u64::from_le_bytes(ordinal),
    )
}

pub(super) fn apply(
    head: &mut RecoveryHead,
    rows: &mut BatchRows,
    sequence: SequenceId,
    plugin: &str,
    transaction: &ExtensionStateTransaction,
) -> Result<(), RecoveryError> {
    validate_plugin(plugin)?;
    validate_state_transaction(transaction)
        .map_err(|_| RecoveryError::Invalid("extension transaction"))?;
    let mut root: StateRoot = match head.extension_root {
        Some(revision) => rows
            .get(key(ROOTS, 0, revision.0))?
            .ok_or(RecoveryError::Invalid("missing extension root"))?,
        None => StateRoot::default(),
    };
    validate_root(&root)?;
    let position = root
        .namespaces
        .iter()
        .position(|entry| entry.plugin == plugin);
    let previous = position.map(|index| &root.namespaces[index]);
    if previous.map(|entry| entry.revision) != transaction.expected_revision {
        return Err(RecoveryError::Invalid("extension revision conflict"));
    }
    let old_bytes = previous.map_or(0, |entry| entry.bytes);
    let mut namespace: Namespace = match previous {
        Some(previous) => rows
            .get(key(NAMESPACES, 0, previous.revision.0))?
            .ok_or(RecoveryError::Invalid("missing extension namespace"))?,
        None => Namespace::default(),
    };
    validate_namespace(&namespace, old_bytes)?;
    apply_values(&mut namespace, sequence, transaction)?;
    let bytes = namespace_bytes(&namespace)?;
    if bytes > MAX_EXTENSION_NAMESPACE_BYTES {
        return Err(RecoveryError::Limit("extension namespace bytes"));
    }
    root.bytes = root
        .bytes
        .checked_sub(old_bytes)
        .and_then(|value| value.checked_add(bytes))
        .ok_or(RecoveryError::Invalid("extension aggregate byte counter"))?;
    let next = NamespaceRevision {
        plugin: plugin.to_owned(),
        revision: sequence,
        bytes,
    };
    match position {
        Some(index) => root.namespaces[index] = next,
        None => root.namespaces.push(next),
    }
    validate_root(&root)?;
    apply_acknowledgement(head, rows, sequence, plugin, transaction)?;
    rows.put(key(NAMESPACES, 0, sequence.0), &namespace)?;
    rows.put(key(ROOTS, 0, sequence.0), &root)?;
    head.extension_root = Some(sequence);
    Ok(())
}

fn apply_values(
    namespace: &mut Namespace,
    sequence: SequenceId,
    transaction: &ExtensionStateTransaction,
) -> Result<(), RecoveryError> {
    for (mutation, change) in transaction.mutations.iter().enumerate() {
        namespace.entries.retain(|entry| entry.key != change.key());
        if let ExtensionStateMutation::Set { key, value } = change {
            let bytes = state_value_bytes(value)
                .map_err(|_| RecoveryError::Limit("extension value bytes"))?;
            namespace.entries.push(ValueSource {
                key: key.clone(),
                sequence,
                mutation,
                bytes,
            });
        }
    }
    namespace.entries.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    if namespace.entries.len() > MAX_EXTENSION_STATE_KEYS {
        return Err(RecoveryError::Limit("extension key count"));
    }
    Ok(())
}

fn apply_acknowledgement(
    head: &RecoveryHead,
    rows: &mut BatchRows,
    sequence: SequenceId,
    plugin: &str,
    transaction: &ExtensionStateTransaction,
) -> Result<(), RecoveryError> {
    let Some(cursor) = &transaction.acknowledged else {
        return Ok(());
    };
    if cursor.sequence >= sequence {
        return Err(RecoveryError::Invalid(
            "extension acknowledgement is not a prior event",
        ));
    }
    if head
        .inherited_journal_through
        .is_some_and(|cut| sequence <= cut)
    {
        // Forked state keeps the parent's CAS history, but never inherits its delivery progress.
        return Ok(());
    }
    if head.session_id.as_ref() != Some(&cursor.session_id)
        || head
            .inherited_journal_through
            .is_some_and(|cut| cursor.sequence <= cut)
    {
        return Err(RecoveryError::Invalid(
            "extension acknowledgement stream identity",
        ));
    }
    let index = acknowledgement_key(plugin);
    if let Some(previous) = rows.get::<Acknowledgement>(index)? {
        if previous.plugin != plugin || previous.cursor.session_id != cursor.session_id {
            return Err(RecoveryError::Invalid(
                "extension acknowledgement namespace identity",
            ));
        }
        if previous.cursor.sequence > cursor.sequence {
            return Err(RecoveryError::Invalid(
                "extension acknowledgement moved backwards",
            ));
        }
    }
    rows.put(
        index,
        &Acknowledgement {
            plugin: plugin.to_owned(),
            cursor: cursor.clone(),
        },
    )
}

fn namespace_bytes(namespace: &Namespace) -> Result<usize, RecoveryError> {
    namespace.entries.iter().try_fold(0usize, |total, entry| {
        total
            .checked_add(entry.key.len())
            .and_then(|total| total.checked_add(entry.bytes))
            .ok_or(RecoveryError::Limit("extension byte counter"))
    })
}
fn validate_namespace(namespace: &Namespace, bytes: usize) -> Result<(), RecoveryError> {
    if namespace.entries.len() > MAX_EXTENSION_STATE_KEYS
        || namespace_bytes(namespace)? != bytes
        || bytes > MAX_EXTENSION_NAMESPACE_BYTES
    {
        return Err(RecoveryError::Invalid("extension namespace counters"));
    }
    let mut previous: Option<&str> = None;
    for entry in &namespace.entries {
        rw_types::extension_contract::validate_state_key(&entry.key)
            .map_err(|_| RecoveryError::Invalid("extension source key"))?;
        if previous.is_some_and(|previous| previous >= entry.key.as_str())
            || entry.bytes > rw_types::extension_contract::MAX_EXTENSION_STATE_VALUE_BYTES
        {
            return Err(RecoveryError::Invalid("extension source metadata"));
        }
        previous = Some(&entry.key);
    }
    Ok(())
}
fn validate_root(root: &StateRoot) -> Result<(), RecoveryError> {
    if root.namespaces.len() > MAX_EXTENSION_NAMESPACES
        || root.bytes > MAX_SESSION_EXTENSION_STATE_BYTES
    {
        return Err(RecoveryError::Limit("extension session aggregate"));
    }
    let mut bytes = 0usize;
    for (index, entry) in root.namespaces.iter().enumerate() {
        validate_plugin(&entry.plugin)?;
        if entry.bytes > MAX_EXTENSION_NAMESPACE_BYTES
            || root.namespaces[..index]
                .iter()
                .any(|other| other.plugin == entry.plugin)
        {
            return Err(RecoveryError::Invalid("extension namespace catalog"));
        }
        bytes = bytes
            .checked_add(entry.bytes)
            .ok_or(RecoveryError::Limit("extension byte counter"))?;
    }
    if bytes != root.bytes {
        return Err(RecoveryError::Invalid("extension session byte counter"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
