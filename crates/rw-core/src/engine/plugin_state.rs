//! Actor-authorized state transactions for host-bound extension namespaces.

use std::{collections::BTreeMap, sync::Arc};

use rw_types::{
    SequenceId, SessionId,
    extension_contract::{
        ExtensionStateCommitOutcome, ExtensionStateMutation, ExtensionStateTransaction,
        MAX_EXTENSION_NAMESPACE_BYTES, MAX_EXTENSION_NAMESPACES, MAX_EXTENSION_STATE_KEYS,
        MAX_SESSION_EXTENSION_STATE_BYTES, state_value_bytes, validate_state_key,
        validate_state_transaction,
    },
};
use tokio::sync::broadcast;

use super::{
    AgentLoopError, ExtensionStateView, PendingEvent, RoutedEvent, SessionActorConfig,
    session::{ActorState, validate_plugin_id},
    turn::{emit, redacted_json},
};

fn invalid(message: impl Into<String>) -> AgentLoopError {
    AgentLoopError::InvalidConfiguration(message.into())
}

/// Validate against one captured canonical prefix before constructing a durable event.
/// Returns the resulting namespace byte charge, including keys.
pub(in crate::engine) fn validate_update(
    view: &ExtensionStateView,
    transaction: &ExtensionStateTransaction,
    session_id: &SessionId,
    tail: Option<SequenceId>,
) -> Result<usize, AgentLoopError> {
    validate_state_transaction(transaction).map_err(|error| invalid(error.to_string()))?;
    if transaction.expected_revision != view.snapshot.revision {
        return Err(invalid("extension state revision conflict"));
    }
    if let Some(cursor) = &transaction.acknowledged {
        let minimum = view
            .snapshot
            .acknowledged
            .as_ref()
            .or(view.snapshot.delivery_start.as_ref());
        if &cursor.session_id != session_id
            || tail.is_none_or(|tail| cursor.sequence > tail)
            || minimum.is_some_and(|previous| cursor.sequence <= previous.sequence)
        {
            return Err(invalid(
                "extension event acknowledgement is outside the delivery stream",
            ));
        }
    }
    let mut entries = BTreeMap::new();
    let mut previous_bytes = 0_usize;
    for entry in &view.snapshot.entries {
        validate_state_key(&entry.key).map_err(|error| invalid(error.to_string()))?;
        let bytes = entry.key.len()
            + state_value_bytes(&entry.value).map_err(|error| invalid(error.to_string()))?;
        previous_bytes = previous_bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid("extension state byte count overflow"))?;
        if entries.insert(entry.key.as_str(), bytes).is_some() {
            return Err(invalid("canonical extension state contains duplicate keys"));
        }
    }
    for mutation in &transaction.mutations {
        match mutation {
            ExtensionStateMutation::Set { key, value } => {
                let bytes = key.len()
                    + state_value_bytes(value).map_err(|error| invalid(error.to_string()))?;
                entries.insert(key.as_str(), bytes);
            }
            ExtensionStateMutation::Delete { key } => {
                entries.remove(key.as_str());
            }
        }
    }
    let bytes = entries
        .values()
        .try_fold(0_usize, |total, bytes| total.checked_add(*bytes))
        .ok_or_else(|| invalid("extension state byte count overflow"))?;
    let session_bytes = view
        .session_bytes
        .checked_sub(previous_bytes)
        .and_then(|total| total.checked_add(bytes))
        .ok_or_else(|| invalid("canonical extension state aggregate is inconsistent"))?;
    if entries.len() > MAX_EXTENSION_STATE_KEYS
        || bytes > MAX_EXTENSION_NAMESPACE_BYTES
        || session_bytes > MAX_SESSION_EXTENSION_STATE_BYTES
        || (view.snapshot.revision.is_none() && view.namespaces >= MAX_EXTENSION_NAMESPACES)
    {
        return Err(invalid("extension state admission limit exceeded"));
    }
    Ok(bytes)
}

pub(in crate::engine) async fn commit(
    plugin_id: String,
    mut transaction: ExtensionStateTransaction,
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
) -> Result<ExtensionStateCommitOutcome, AgentLoopError> {
    validate_plugin_id(&plugin_id)?;
    if state.poisoned || state.closing {
        return Err(invalid("session cannot admit extension state changes"));
    }
    validate_state_transaction(&transaction).map_err(|error| invalid(error.to_string()))?;
    for mutation in &mut transaction.mutations {
        if let ExtensionStateMutation::Set { value, .. } = mutation {
            *value = redacted_json(std::mem::take(value), config.secret_redactor.as_ref());
        }
    }
    let view = config.event_sink.extension_state(&plugin_id).await?;
    if transaction.expected_revision != view.snapshot.revision {
        return Ok(ExtensionStateCommitOutcome::Conflict {
            actual_revision: view.snapshot.revision,
        });
    }
    validate_update(
        &view,
        &transaction,
        &state.session_id,
        state.sequence.map(SequenceId),
    )?;
    let receipt = emit(
        state,
        events,
        &config.event_sink,
        PendingEvent::ExtensionStateCommitted {
            plugin_id,
            transaction,
        },
    )
    .await?;
    Ok(ExtensionStateCommitOutcome::Committed {
        revision: receipt.sequence_id,
    })
}

#[cfg(test)]
mod tests;
