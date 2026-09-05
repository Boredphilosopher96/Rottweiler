use super::super::{RewindCoordinatorState, load_rewind_coordinator, remove_rewind_coordinator};
use super::paths::checkpoint_display_path;
use miette::{Result, miette};
use rw_core::{
    EngineEvent, EventClock, EventMeta, SESSION_EVENT_VERSION, SequenceId, SystemEventClock,
    UnrestorablePath,
};
use rw_store::{
    checkpoint::{CheckpointStore, RewindHandle},
    session::SessionEventLog,
};
use rw_types::SessionId;
use std::{path::Path, sync::Arc};

pub(in crate::session_runtime) fn recover_rewind_transactions(
    checkpoint_root: &Path,
    checkpoints: &[Arc<CheckpointStore>],
    log: &mut SessionEventLog,
) -> Result<()> {
    let existing = log
        .load::<EngineEvent>()
        .map_err(|error| miette!("session log could not load for rewind recovery: {error}"))?;
    let operations = existing
        .iter()
        .filter_map(|event| match &event.event {
            EngineEvent::ConversationRewound { operation_id, .. } => Some(operation_id.clone()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let Some(decision) = load_rewind_coordinator(checkpoint_root)? else {
        return Ok(());
    };
    if decision.root_count != checkpoints.len() {
        return Err(miette!(
            "rewind coordinator root count differs from the workspace mapping"
        ));
    }
    let handle = RewindHandle {
        session_id: decision.session_id.clone(),
        operation_id: decision.operation_id.clone(),
    };
    if decision.state == RewindCoordinatorState::Preparing {
        if operations.contains(&decision.operation_id) {
            return Err(miette!(
                "uncommitted rewind coordinator conflicts with a durable rewind event"
            ));
        }
        for (root_index, store) in checkpoints.iter().enumerate() {
            store
                .discard_prepared_rewind(&handle, decision.target_turn)
                .map_err(|error| {
                    miette!("prepared rewind cleanup failed for root {root_index}: {error}")
                })?;
        }
        remove_rewind_coordinator(checkpoint_root)?;
        return Ok(());
    }

    let mut unrestorable_paths = Vec::new();
    for (root_index, store) in checkpoints.iter().enumerate() {
        let prepared = store
            .prepare_rewind(
                &decision.session_id,
                decision.target_turn,
                &decision.operation_id,
            )
            .map_err(|error| {
                miette!("rewind recovery could not stage root {root_index}: {error}")
            })?;
        let commit = store.apply_rewind(&prepared).map_err(|error| {
            miette!("rewind recovery could not apply root {root_index}: {error}")
        })?;
        unrestorable_paths.extend(
            commit
                .report
                .unrestorable
                .into_iter()
                .map(|(path, reason)| UnrestorablePath {
                    path: checkpoint_display_path(root_index, &path),
                    reason,
                }),
        );
    }
    if !operations.contains(&decision.operation_id) {
        log.append(EngineEvent::ConversationRewound {
            meta: EventMeta {
                protocol_version: SESSION_EVENT_VERSION,
                session_id: SessionId(decision.session_id.clone()),
                sequence_id: SequenceId(log.next_sequence()),
                emitted_at: SystemEventClock.emitted_at(),
                caused_by: None,
            },
            to_agent_turn: decision.target_turn,
            operation_id: decision.operation_id,
            unrestorable_paths,
        })
        .map_err(|error| miette!("recovered rewind event could not persist: {error}"))?;
    }
    for (root_index, store) in checkpoints.iter().enumerate() {
        store.acknowledge_rewind(&handle).map_err(|error| {
            miette!("recovered rewind root {root_index} could not acknowledge: {error}")
        })?;
    }
    remove_rewind_coordinator(checkpoint_root)?;
    Ok(())
}
