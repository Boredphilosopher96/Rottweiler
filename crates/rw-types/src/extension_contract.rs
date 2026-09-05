//! Session-scoped extension state and durable event acknowledgement.

use std::collections::BTreeSet;

use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::{SequenceId, SessionId};

pub const MAX_EXTENSION_NAMESPACES: usize = 64;
pub const MAX_EXTENSION_STATE_KEYS: usize = 64;
pub const MAX_EXTENSION_STATE_MUTATIONS: usize = 32;
pub const MAX_EXTENSION_STATE_KEY_BYTES: usize = 128;
pub const MAX_EXTENSION_STATE_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_EXTENSION_NAMESPACE_BYTES: usize = 256 * 1024;
pub const MAX_SESSION_EXTENSION_STATE_BYTES: usize = 1024 * 1024;

/// The journal identity of one delivered event, independent of transport retries.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDeliveryCursor {
    pub session_id: SessionId,
    pub sequence: SequenceId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionStateMutation {
    Set { key: String, value: Value },
    Delete { key: String },
}

impl ExtensionStateMutation {
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Set { key, .. } | Self::Delete { key } => key,
        }
    }
}

/// One compare-and-swap against the host-bound plugin namespace.
/// The committed event sequence becomes its revision. An acknowledgement and
/// its state changes share the same durable commit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionStateTransaction {
    #[serde(deserialize_with = "Option::deserialize")]
    pub expected_revision: Option<SequenceId>,
    pub mutations: Vec<ExtensionStateMutation>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub acknowledged: Option<ExtensionDeliveryCursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionStateEntry {
    pub key: String,
    pub value: Value,
}

/// A bounded projection of one namespace at a captured journal prefix.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ExtensionStateSnapshot {
    #[serde(deserialize_with = "Option::deserialize")]
    pub revision: Option<SequenceId>,
    pub entries: Vec<ExtensionStateEntry>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub acknowledged: Option<ExtensionDeliveryCursor>,
    /// Host-selected lower bound of this session's delivery stream. A fork
    /// starts after its inherited prefix instead of redelivering parent effects.
    #[serde(deserialize_with = "Option::deserialize")]
    pub delivery_start: Option<ExtensionDeliveryCursor>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid extension state: {0}")]
pub struct ExtensionStateError(pub &'static str);

/// Validate transaction-local bounds before actor admission or journal replay.
/// Namespace and session aggregate limits are enforced by the state owner.
///
/// # Errors
/// Rejects empty transactions, duplicate keys, malformed keys and oversized values.
pub fn validate_state_transaction(
    transaction: &ExtensionStateTransaction,
) -> Result<(), ExtensionStateError> {
    if transaction.mutations.len() > MAX_EXTENSION_STATE_MUTATIONS {
        return Err(ExtensionStateError("mutation count exceeds limit"));
    }
    if transaction.mutations.is_empty() && transaction.acknowledged.is_none() {
        return Err(ExtensionStateError(
            "transaction has no state or acknowledgement",
        ));
    }
    let mut keys = BTreeSet::new();
    for mutation in &transaction.mutations {
        validate_state_key(mutation.key())?;
        if !keys.insert(mutation.key()) {
            return Err(ExtensionStateError("transaction contains a duplicate key"));
        }
        if let ExtensionStateMutation::Set { value, .. } = mutation {
            state_value_bytes(value)?;
        }
    }
    Ok(())
}

/// # Errors
/// Rejects keys outside the bounded, case-sensitive state key grammar.
pub fn validate_state_key(key: &str) -> Result<(), ExtensionStateError> {
    if key.is_empty()
        || key.len() > MAX_EXTENSION_STATE_KEY_BYTES
        || !key.as_bytes()[0].is_ascii_alphanumeric()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(ExtensionStateError("key grammar or byte limit"));
    }
    Ok(())
}

/// Counts the serialized JSON value without allocating an encoded copy.
///
/// # Errors
/// Rejects values whose serialized representation exceeds the value bound.
pub fn state_value_bytes(value: &Value) -> Result<usize, ExtensionStateError> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("extension state byte count overflow"))?;
            if self.0 > MAX_EXTENSION_STATE_VALUE_BYTES {
                return Err(std::io::Error::other("extension state value exceeds limit"));
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| ExtensionStateError("serialized value exceeds byte limit"))?;
    Ok(counter.0)
}

#[cfg(test)]
mod tests;
