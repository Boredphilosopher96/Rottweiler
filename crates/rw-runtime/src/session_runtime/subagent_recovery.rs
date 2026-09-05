use super::durable_session::DurableEventSink;
use super::durable_session::load_session_events;
use crate::journal_reads::JournalReads;
use rw_core::AgentLoopError;
use rw_core::EngineEvent;
use rw_core::EventMeta;
use rw_core::SESSION_EVENT_VERSION;
use rw_core::SequenceId;
use rw_core::SubagentMetadataStore;
use rw_core::SubagentOrchestrator;
use rw_core::SystemEventClock;
use rw_core::{EventClock, SessionEventSink};
use rw_store::session::SessionEventLog;
use rw_tools::CancellationToken;
use rw_tools::WorktreeIsolation;
use rw_types::SessionId;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn effective_subagent_events(
    events: &[EngineEvent],
) -> std::result::Result<Vec<EngineEvent>, AgentLoopError> {
    let mut active_turn = None;
    let mut retained: Vec<(u64, EngineEvent)> = Vec::new();
    for event in events {
        match event {
            EngineEvent::TurnStarted { turn_id, .. } => {
                active_turn = Some(turn_id.0.parse::<u64>().map_err(|_| {
                    AgentLoopError::Persistence("durable turn id is not numeric".to_owned())
                })?);
            }
            EngineEvent::TurnFinished { turn_id, .. } => {
                let turn = turn_id.0.parse::<u64>().map_err(|_| {
                    AgentLoopError::Persistence("durable turn id is not numeric".to_owned())
                })?;
                if active_turn != Some(turn) {
                    return Err(AgentLoopError::Persistence(
                        "durable turn lifecycle is inconsistent".to_owned(),
                    ));
                }
                active_turn = None;
            }
            EngineEvent::ConversationRewound { to_agent_turn, .. } => {
                retained.retain(|(turn, _)| turn <= to_agent_turn);
                active_turn = None;
            }
            EngineEvent::SubagentSpawned { .. } => {
                let turn = active_turn.ok_or_else(|| {
                    AgentLoopError::Persistence(
                        "durable child spawn occurred outside an active turn".to_owned(),
                    )
                })?;
                retained.push((turn, event.clone()));
            }
            EngineEvent::SubagentFinished { subagent_id, .. } => {
                let turn = active_turn
                    .or_else(|| unmatched_retained_spawn_turn(&retained, subagent_id))
                    .ok_or_else(|| {
                        AgentLoopError::Persistence(
                            "durable child result has no active or retained spawn".to_owned(),
                        )
                    })?;
                retained.push((turn, event.clone()));
            }
            _ => {}
        }
    }
    let mut active = HashMap::new();
    for (_, event) in &retained {
        match event {
            EngineEvent::SubagentSpawned {
                subagent_id,
                child_session_id,
                ..
            } => {
                if active
                    .insert(subagent_id.clone(), child_session_id.clone())
                    .is_some()
                {
                    return Err(AgentLoopError::Persistence(
                        "durable child spawned twice without a terminal result".to_owned(),
                    ));
                }
            }
            EngineEvent::SubagentFinished {
                subagent_id,
                result,
                ..
            } => {
                let session = active.remove(subagent_id).ok_or_else(|| {
                    AgentLoopError::Persistence(
                        "durable child result has no effective spawn".to_owned(),
                    )
                })?;
                if result.subagent_id != *subagent_id || result.session_id != session {
                    return Err(AgentLoopError::Persistence(
                        "durable child result identity is inconsistent".to_owned(),
                    ));
                }
            }
            _ => unreachable!(),
        }
    }
    Ok(retained.into_iter().map(|(_, event)| event).collect())
}

pub(super) fn unmatched_retained_spawn_turn(
    retained: &[(u64, EngineEvent)],
    target: &rw_types::SubagentId,
) -> Option<u64> {
    let mut unmatched = None;
    for (turn, event) in retained {
        match event {
            EngineEvent::SubagentSpawned { subagent_id, .. } if subagent_id == target => {
                unmatched = Some(*turn);
            }
            EngineEvent::SubagentFinished { subagent_id, .. } if subagent_id == target => {
                unmatched = None;
            }
            _ => {}
        }
    }
    unmatched
}

pub(super) fn validate_subagent_recovery_record(
    record: &rw_core::SubagentRecoveryRecord,
    events: &[EngineEvent],
) -> std::result::Result<(), AgentLoopError> {
    let durable = events.iter().any(|event| {
        matches!(
            event,
            EngineEvent::SubagentSpawned {
                subagent_id,
                child_session_id,
                ..
            } if subagent_id == &record.handle.subagent_id
                && child_session_id == &record.handle.session_id
        )
    });
    if !durable {
        return Err(AgentLoopError::Persistence(
            "host-private child metadata has no matching durable spawn event".to_owned(),
        ));
    }
    Ok(())
}

pub(super) async fn repair_incomplete_subagent_lifecycles(
    sink: &DurableEventSink,
    parent_session_id: &SessionId,
    events: &[EngineEvent],
) -> std::result::Result<Vec<EngineEvent>, AgentLoopError> {
    let effective = effective_subagent_events(events)?;
    let incomplete = rw_core::incomplete_subagent_lifecycles(&effective)
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    if incomplete.is_empty() {
        return Ok(events.to_vec());
    }
    let first_sequence = events
        .last()
        .and_then(EngineEvent::meta)
        .map_or(0, |meta| meta.sequence_id.0.saturating_add(1));
    let emitted_at = SystemEventClock.emitted_at();
    let repairs = incomplete
        .iter()
        .enumerate()
        .map(|(offset, handle)| EngineEvent::SubagentFinished {
            meta: EventMeta {
                protocol_version: SESSION_EVENT_VERSION,
                session_id: parent_session_id.clone(),
                sequence_id: SequenceId(
                    first_sequence.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX)),
                ),
                emitted_at: emitted_at.clone(),
                caused_by: None,
            },
            subagent_id: handle.subagent_id.clone(),
            result: rw_core::interrupted_subagent_recovery_result(handle),
        })
        .collect::<Vec<_>>();
    sink.append_batch(repairs).await?;
    sink.load()
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))
}

pub(super) fn recovery_workspace_authorized(
    record: &rw_core::SubagentRecoveryRecord,
    allowed_roots: &[PathBuf],
) -> bool {
    let Ok(canonical_record) = std::fs::canonicalize(&record.workspace_root) else {
        return false;
    };
    if canonical_record != record.workspace_root || !canonical_record.is_dir() {
        return false;
    }
    allowed_roots.iter().any(|allowed| {
        std::fs::canonicalize(allowed).is_ok_and(|canonical_allowed| {
            canonical_allowed == *allowed
                && canonical_allowed.is_dir()
                && (canonical_record == canonical_allowed
                    || canonical_record.starts_with(&canonical_allowed))
        })
    })
}

pub(super) async fn promote_pending_recovery_record(
    record: &mut rw_core::SubagentRecoveryRecord,
    metadata: &dyn SubagentMetadataStore,
) -> std::result::Result<(), AgentLoopError> {
    if record.phase != rw_core::SubagentRecoveryPhase::Pending {
        return Ok(());
    }
    record.phase = rw_core::SubagentRecoveryPhase::Active;
    if let Err(error) = metadata.save(record.clone()).await {
        record.phase = rw_core::SubagentRecoveryPhase::Pending;
        return Err(AgentLoopError::Persistence(format!(
            "durable child metadata could not promote: {error}"
        )));
    }
    Ok(())
}

pub(super) async fn discard_rewound_subagent_record(
    record: &rw_core::SubagentRecoveryRecord,
    effective_events: &[EngineEvent],
    raw_events: &[EngineEvent],
    worktree_manager: Option<&WorktreeIsolation>,
    metadata: &dyn SubagentMetadataStore,
) -> std::result::Result<bool, AgentLoopError> {
    let Err(effective_error) = validate_subagent_recovery_record(record, effective_events) else {
        return Ok(false);
    };
    let raw_spawn_exists = validate_subagent_recovery_record(record, raw_events).is_ok();
    let uncommitted_pending =
        record.phase == rw_core::SubagentRecoveryPhase::Pending && !raw_spawn_exists;
    if !raw_spawn_exists && !uncommitted_pending {
        return Err(effective_error);
    }
    if let Some(lease) = &record.worktree {
        let manager = worktree_manager.ok_or_else(|| {
            AgentLoopError::Persistence("rewound worktree cannot be safely reclaimed".to_owned())
        })?;
        manager
            .discard_tombstoned(lease, CancellationToken::default())
            .await
            .map_err(|error| {
                AgentLoopError::Persistence(format!(
                    "rewound worktree could not be removed safely: {error}"
                ))
            })?;
    }
    metadata
        .remove(&record.parent_session_id, &record.handle.subagent_id)
        .await
        .map_err(|error| {
            AgentLoopError::Persistence(format!(
                "rewound child metadata could not be removed: {error}"
            ))
        })?;
    Ok(true)
}

pub(super) struct SubagentRecoveryNode {
    pub(super) parent_session_id: SessionId,
    pub(super) parent_depth: usize,
    pub(super) authorized_roots: Vec<PathBuf>,
    pub(super) events: Option<Vec<EngineEvent>>,
}

pub(super) fn open_subagent_recovery_log(
    journal_reads: Arc<JournalReads>,
    storage_root: &Path,
    session_id: &SessionId,
) -> std::result::Result<(DurableEventSink, Vec<EngineEvent>), AgentLoopError> {
    let log = SessionEventLog::open(storage_root, &session_id.0)
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    let events = load_session_events(&log)
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    let sink = DurableEventSink::new(
        log,
        storage_root.to_path_buf(),
        session_id.0.clone(),
        journal_reads,
    )
    .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    Ok((sink, events))
}

/// Repairs and rebinds a complete persisted subagent tree. Discovery is kept
/// separate from actor creation so every descendant log is repaired before a
/// recovered actor opens it and caches its next durable sequence.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn recover_subagent_tree(
    storage_root: &Path,
    root_session_id: &SessionId,
    root_sink: &DurableEventSink,
    root_events: &[EngineEvent],
    root_authorized_roots: &[PathBuf],
    max_depth: usize,
    orchestrator: &SubagentOrchestrator,
    metadata: &crate::subagent_metadata::PrivateSubagentMetadataStore,
    worktree_manager: Option<&WorktreeIsolation>,
) -> std::result::Result<(), AgentLoopError> {
    let mut queue = VecDeque::from([SubagentRecoveryNode {
        parent_session_id: root_session_id.clone(),
        parent_depth: 0,
        authorized_roots: root_authorized_roots.to_vec(),
        events: Some(root_events.to_vec()),
    }]);
    let mut visited = HashSet::new();
    let mut records = Vec::new();

    while let Some(node) = queue.pop_front() {
        if !visited.insert(node.parent_session_id.clone()) {
            return Err(AgentLoopError::Persistence(
                "persisted child session topology contains a loop or duplicate".to_owned(),
            ));
        }
        let (sink, events) = if let Some(events) = node.events {
            (None, events)
        } else {
            let (sink, events) = open_subagent_recovery_log(
                Arc::clone(&root_sink.journal_reads),
                storage_root,
                &node.parent_session_id,
            )?;
            (Some(sink), events)
        };
        let repaired = repair_incomplete_subagent_lifecycles(
            sink.as_ref().unwrap_or(root_sink),
            &node.parent_session_id,
            &events,
        )
        .await?;
        let effective = effective_subagent_events(&repaired)?;
        orchestrator
            .rebuild_artifact_authority(&node.parent_session_id, &effective)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;

        let expected_depth = node.parent_depth.checked_add(1).ok_or_else(|| {
            AgentLoopError::Persistence("persisted child depth overflow".to_owned())
        })?;
        for mut record in metadata
            .load_parent(&node.parent_session_id)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
        {
            if record.depth != expected_depth || record.depth > max_depth {
                return Err(AgentLoopError::Persistence(format!(
                    "persisted child depth {} does not match expected depth {expected_depth} or configured maximum {max_depth}",
                    record.depth
                )));
            }
            if record.phase == rw_core::SubagentRecoveryPhase::Closed {
                let published = repaired.iter().any(|event| matches!(event, EngineEvent::SubagentSpawned {subagent_id, ..} if subagent_id == &record.handle.subagent_id));
                if published {
                    validate_subagent_recovery_record(&record, &repaired)?;
                    if validate_subagent_recovery_record(&record, &effective).is_ok()
                        && !effective.iter().any(|event| matches!(event, EngineEvent::SubagentFinished {subagent_id, result, ..} if subagent_id == &record.handle.subagent_id && result.session_id == record.handle.session_id)) {
                        return Err(AgentLoopError::Persistence("closed child still lacks durable terminal proof".to_owned()));
                    }
                }
                metadata
                    .remove(&record.parent_session_id, &record.handle.subagent_id)
                    .await
                    .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
                continue;
            }
            if !recovery_workspace_authorized(&record, &node.authorized_roots) {
                return Err(AgentLoopError::Persistence(
                    "persisted child workspace root is outside its recovered parent workspace"
                        .to_owned(),
                ));
            }
            if discard_rewound_subagent_record(
                &record,
                &effective,
                &repaired,
                worktree_manager,
                metadata,
            )
            .await?
            {
                continue;
            }
            let child_root = if let Some(lease) = record.worktree.as_ref() {
                let manager = worktree_manager.ok_or_else(|| {
                    AgentLoopError::Persistence(
                        "persisted nested worktree cannot be validated".to_owned(),
                    )
                })?;
                manager
                    .rebind(lease, CancellationToken::default())
                    .await
                    .map_err(|error| {
                        AgentLoopError::Persistence(format!(
                            "persisted child worktree could not be validated: {error}"
                        ))
                    })?
                    .path()
                    .to_path_buf()
            } else {
                record.workspace_root.clone()
            };
            promote_pending_recovery_record(&mut record, metadata).await?;
            queue.push_back(SubagentRecoveryNode {
                parent_session_id: record.handle.session_id.clone(),
                parent_depth: record.depth,
                authorized_roots: vec![child_root],
                events: None,
            });
            records.push(record);
        }
    }

    // Every actor opens a fully repaired log. Descendant-first rebinding also
    // makes the recovered depth map complete before any parent follow-up runs.
    for record in records.into_iter().rev() {
        orchestrator
            .recover_record(record)
            .await
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    }
    Ok(())
}
