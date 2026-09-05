use super::durable_session::ChildLifecycleReader;
use super::durable_session::DurableEventSink;
use rw_core::AgentLoopError;
use rw_core::EngineEvent;
use rw_core::EventClock;
use rw_core::EventMeta;
use rw_core::SESSION_EVENT_VERSION;
use rw_core::SequenceId;
use rw_core::SubagentMetadataStore;
use rw_core::SubagentOrchestrator;
use rw_core::SystemEventClock;
use rw_tools::CancellationToken;
use rw_tools::WorktreeIsolation;
use rw_types::SessionId;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) async fn repair_incomplete_subagent_lifecycles(
    sink: &Arc<DurableEventSink>,
    parent: &SessionId,
    history: &ChildLifecycleReader,
) -> Result<(), AgentLoopError> {
    loop {
        let (through, pending) = history.pending(parent, None).await?;
        if pending.is_empty() {
            return Ok(());
        }
        let first = through
            .map_or(Some(0), |value| value.0.checked_add(1))
            .ok_or_else(|| AgentLoopError::Persistence("durable sequence overflow".into()))?;
        let emitted_at = SystemEventClock.emitted_at();
        let mut repairs = Vec::with_capacity(pending.len());
        for (offset, binding) in pending.into_iter().enumerate() {
            let sequence = first
                .checked_add(
                    u64::try_from(offset)
                        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?,
                )
                .ok_or_else(|| AgentLoopError::Persistence("durable sequence overflow".into()))?;
            let handle = rw_core::SubagentHandle {
                subagent_id: binding.subagent_id,
                session_id: binding.session_id,
            };
            repairs.push(EngineEvent::SubagentFinished {
                meta: EventMeta {
                    protocol_version: SESSION_EVENT_VERSION,
                    session_id: parent.clone(),
                    sequence_id: SequenceId(sequence),
                    emitted_at: emitted_at.clone(),
                    caused_by: None,
                },
                subagent_id: handle.subagent_id.clone(),
                result: rw_core::interrupted_subagent_recovery_result(&handle),
            });
        }
        rw_core::commit_session_events(Arc::clone(sink), repairs).await?;
    }
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
    history: &ChildLifecycleReader,
    worktree_manager: Option<&WorktreeIsolation>,
    metadata: &dyn SubagentMetadataStore,
) -> std::result::Result<bool, AgentLoopError> {
    let effective = history
        .binding(&record.parent_session_id, &record.handle.subagent_id)
        .await?;
    if effective
        .as_ref()
        .is_some_and(|binding| binding.session_id == record.handle.session_id)
    {
        return Ok(false);
    }
    let published = history
        .published(
            &record.parent_session_id,
            &record.handle.subagent_id,
            &record.handle.session_id,
        )
        .await?;
    if !published && record.phase != rw_core::SubagentRecoveryPhase::Pending {
        return Err(AgentLoopError::Persistence(
            "host-private child metadata has no matching durable spawn event".into(),
        ));
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
}

/// Repairs and rebinds a complete persisted subagent tree. Discovery is kept
/// separate from actor creation so every descendant log is repaired before a
/// recovered actor opens it and caches its next durable sequence.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn recover_subagent_tree(
    storage_root: &Path,
    root_session_id: &SessionId,
    root_sink: &Arc<DurableEventSink>,
    history: &ChildLifecycleReader,
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
    }]);
    let mut visited = HashSet::new();
    let mut records = Vec::new();

    while let Some(node) = queue.pop_front() {
        if !visited.insert(node.parent_session_id.clone()) {
            return Err(AgentLoopError::Persistence(
                "persisted child session topology contains a loop or duplicate".to_owned(),
            ));
        }
        let sink = if node.parent_session_id == *root_session_id {
            Arc::clone(root_sink)
        } else {
            history
                .open_sink(storage_root, &node.parent_session_id)
                .await?
        };
        repair_incomplete_subagent_lifecycles(&sink, &node.parent_session_id, history).await?;

        let expected_depth = node.parent_depth.checked_add(1).ok_or_else(|| {
            AgentLoopError::Persistence("persisted child depth overflow".to_owned())
        })?;
        let mut after = None;
        loop {
            let page = metadata
                .load_parent_page(&node.parent_session_id, after.as_ref())
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
            after = page.next;
            for (mut record, _) in page.records {
                if records.len() == rw_core::MAX_RETAINED_SUBAGENTS {
                    return Err(AgentLoopError::Persistence(
                        "subagent recovery child capacity exceeded".into(),
                    ));
                }
                if record.depth != expected_depth || record.depth > max_depth {
                    return Err(AgentLoopError::Persistence(format!(
                        "persisted child depth {} does not match expected depth {expected_depth} or configured maximum {max_depth}",
                        record.depth
                    )));
                }
                if record.phase == rw_core::SubagentRecoveryPhase::Closed {
                    if let Some(binding) = history
                        .binding(&node.parent_session_id, &record.handle.subagent_id)
                        .await?
                        && binding.session_id == record.handle.session_id
                        && binding.terminal.is_none()
                    {
                        return Err(AgentLoopError::Persistence(
                            "closed child still lacks durable terminal proof".into(),
                        ));
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
                if discard_rewound_subagent_record(&record, history, worktree_manager, metadata)
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
                });
                let fingerprint = crate::subagent_metadata::record_fingerprint(&record)
                    .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
                records.push((
                    record.parent_session_id,
                    record.handle.subagent_id,
                    fingerprint,
                ));
            }
            if after.is_none() {
                break;
            }
        }
    }

    // Every actor opens a fully repaired log. Descendant-first rebinding also
    // makes the recovered depth map complete before any parent follow-up runs.
    for (parent, child, expected) in records.into_iter().rev() {
        let record = metadata
            .load_record(&parent, &child)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
        if crate::subagent_metadata::record_fingerprint(&record)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
            != expected
        {
            return Err(AgentLoopError::Persistence(
                "subagent metadata changed during recovery".into(),
            ));
        }
        orchestrator
            .recover_record(record)
            .await
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    }
    Ok(())
}
