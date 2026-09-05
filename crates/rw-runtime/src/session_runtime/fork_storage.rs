use super::checkpoint_journal::CHECKPOINT_ROOTS_VERSION;
use super::checkpoint_journal::CheckpointRootGeneration;
use super::checkpoint_journal::CheckpointRootMapping;
use super::checkpoint_journal::load_checkpoint_root_generation_exact;
use super::checkpoint_journal::open_checkpoint_stores;
use super::checkpoint_journal::persist_private_json;
use super::session_metadata::SESSION_METADATA_VERSION;
use super::session_metadata::SessionMetadata;
use super::session_metadata::encode_session_metadata;
use super::session_metadata::ensure_real_directory;
use super::session_metadata::load_session_metadata;
#[cfg(not(unix))]
use super::session_metadata::persist_session_metadata_portable;
#[cfg(unix)]
use super::session_metadata::persist_session_metadata_unix;
use super::session_metadata::validate_session_id;
use super::session_selection::checkpoint_root;
use crate::journal_service::JournalService;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_core::ClientId;
use rw_core::EngineEvent;
use rw_core::SequenceId;
use rw_core::project_session_events;
use rw_store::session::SessionEventLog;
use rw_store::session::SessionStoreError;
use rw_types::SessionId;
use std::io;
use std::path::Path;
use std::path::PathBuf;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn fork_hosted_session_storage(
    journal_service: &JournalService,
    storage_root: &Path,
    workspace: &Path,
    parent_session_id: &str,
    child_session_id: &str,
    through_turn: u64,
    through_sequence: Option<SequenceId>,
    include_idle_tail: bool,
    driver_client_id: ClientId,
    fork_operation_id: Option<&str>,
    mode_registry: &rw_ext::ModeRegistry,
) -> Result<()> {
    validate_session_id(parent_session_id)?;
    validate_session_id(child_session_id)?;
    let parent_metadata = load_session_metadata(storage_root, parent_session_id, workspace)?;
    let lease = journal_service.capture(parent_session_id)?;
    let (parent_events, _) = crate::history::load_events_from_view(
        &lease.view,
        parent_session_id,
        crate::history::MAX_HISTORY_BYTES,
    )?;
    let through_sequence = if include_idle_tail {
        through_sequence
    } else if through_turn == 0 {
        None
    } else {
        Some(
            parent_events
                .iter()
                .rev()
                .find_map(|event| match &event.event {
                    EngineEvent::TurnFinished { turn_id, .. }
                        if turn_id.0.parse::<u64>().ok() == Some(through_turn) =>
                    {
                        Some(event.sequence)
                    }
                    _ => None,
                })
                .ok_or_else(|| miette!("fork turn is not a durable completed boundary"))?,
        )
    };
    let prefix_end = through_sequence
        .map(|sequence| {
            usize::try_from(sequence.0)
                .map_err(|_| miette!("fork sequence cannot be represented"))?
                .checked_add(1)
                .ok_or_else(|| miette!("fork sequence cannot be represented"))
        })
        .transpose()?;
    let prefix = prefix_end.map_or(Ok(&[][..]), |end| {
        parent_events
            .get(..end)
            .ok_or_else(|| miette!("fork sequence is beyond the durable parent tail"))
    })?;
    if prefix
        .iter()
        .enumerate()
        .any(|(index, event)| event.sequence.0 != index as u64)
    {
        return Err(miette!("fork parent envelope sequence is not contiguous"));
    }
    let prefix_events = prefix
        .iter()
        .map(|event| event.event.clone())
        .collect::<Vec<_>>();
    // This preliminary projection reads only the non-policy workspace
    // generation needed to locate the historical root set. The registry-aware
    // projection below validates all mode semantics before any child path is
    // created or event is written.
    let workspace_projection = project_session_events(&prefix_events)
        .map_err(|error| miette!("fork prefix projection failed: {error}"))?;
    let source_checkpoint_root = checkpoint_root(storage_root, workspace, parent_session_id);
    let target_checkpoint_root = checkpoint_root(storage_root, workspace, child_session_id);
    if target_checkpoint_root.exists() {
        return Err(miette!("fork target checkpoint root already exists"));
    }
    let fork_roots = load_checkpoint_root_generation_exact(
        &source_checkpoint_root,
        workspace_projection.workspace_generation,
    )?
    .filter(|generation| generation.committed)
    .map(|generation| generation.roots)
    .ok_or_else(|| miette!("fork workspace-root generation is unavailable"))?;
    let projected = crate::mode_recovery::project(&prefix_events, mode_registry)
        .map_err(|error| miette!("fork mode projection failed: {error}"))?;
    let mapping = CheckpointRootMapping {
        version: CHECKPOINT_ROOTS_VERSION,
        generations: vec![CheckpointRootGeneration {
            generation: projected.workspace_generation,
            effective_from_turn: projected.completed_turns.saturating_add(1),
            roots: fork_roots.clone(),
            committed: true,
        }],
    };

    let result = (|| -> Result<()> {
        std::fs::create_dir_all(&target_checkpoint_root).into_diagnostic()?;
        persist_private_json(
            &target_checkpoint_root.join("workspace-roots.json"),
            &mapping,
        )?;
        // Forks share the live workspace but not checkpoint history. A child starts
        // with an empty mutation baseline so review/rewind only describe its own
        // changes instead of attributing post-boundary parent changes to the child.
        let _target_stores = open_checkpoint_stores(&target_checkpoint_root, &fork_roots)?;
        let child_id = SessionId(child_session_id.to_owned());
        let child_id_for_map = child_id.clone();
        let log = SessionEventLog::fork_mapped_view::<EngineEvent, _>(
            storage_root,
            parent_session_id,
            child_session_id,
            &lease.view,
            through_sequence,
            move |mut event| {
                let meta = event.meta_mut().ok_or(SessionStoreError::CorruptEvent(
                    "fork source contains a connection-scoped event",
                ))?;
                meta.session_id = child_id_for_map.clone();
                match &mut event {
                    EngineEvent::SessionCreated {
                        driver_client_id: event_driver,
                        ..
                    }
                    | EngineEvent::DriverChanged {
                        driver_client_id: event_driver,
                        ..
                    } => *event_driver = driver_client_id.clone(),
                    _ => {}
                }
                Ok(event)
            },
        )
        .map_err(|error| miette!("fork event log could not persist: {error}"))?;
        drop(log);
        persist_forked_session_metadata(
            storage_root,
            child_session_id,
            &parent_metadata,
            projected.workspace_generation,
            &fork_roots,
            through_sequence,
            through_turn,
            fork_operation_id,
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&target_checkpoint_root);
        let _ = std::fs::remove_dir_all(storage_root.join("sessions").join(child_session_id));
    }
    result
}

pub(crate) fn remove_forked_session_storage(
    storage_root: &Path,
    workspace: &Path,
    child_session_id: &str,
) -> Result<()> {
    validate_session_id(child_session_id)?;
    for path in [
        checkpoint_root(storage_root, workspace, child_session_id),
        storage_root.join("sessions").join(child_session_id),
    ] {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(miette!(
                    "fork child storage cleanup failed at {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_forked_session_commit(
    storage_root: &Path,
    workspace: &Path,
    child_session_id: &str,
    operation_id: &str,
    parent_session_id: &str,
) -> Result<()> {
    let metadata = load_session_metadata(storage_root, child_session_id, workspace)?;
    if metadata.fork_operation_id.as_deref() != Some(operation_id)
        || metadata.fork_parent_session_id.as_deref() != Some(parent_session_id)
    {
        return Err(miette!(
            "fork metadata provenance does not match its journal"
        ));
    }
    if metadata.workspace_roots.is_empty()
        || metadata.workspace_roots.first().map(PathBuf::as_path) != Some(workspace)
    {
        return Err(miette!("fork workspace-root mapping is empty"));
    }
    if metadata
        .workspace_roots
        .iter()
        .any(|root| !root.is_absolute())
    {
        return Err(miette!(
            "fork workspace-root metadata contains a relative path"
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn persist_forked_session_metadata(
    storage_root: &Path,
    child_session_id: &str,
    parent: &SessionMetadata,
    workspace_generation: u64,
    workspace_roots: &[PathBuf],
    inherited_journal_through: Option<SequenceId>,
    fork_at_turn: u64,
    fork_operation_id: Option<&str>,
) -> Result<()> {
    let directory = storage_root.join("sessions").join(child_session_id);
    ensure_real_directory(&directory, false)?;
    let metadata = SessionMetadata {
        version: SESSION_METADATA_VERSION,
        session_id: child_session_id.to_owned(),
        budget_session_id: parent.budget_session_id.clone(),
        workspace: parent.workspace.clone(),
        model_alias: parent.model_alias.clone(),
        initial_session_context: parent.initial_session_context.clone(),
        workspace_generation,
        workspace_roots: workspace_roots.to_vec(),
        initial_context_workspace_root_count: parent.initial_context_workspace_root_count,
        inherited_journal_through,
        fork_parent_session_id: Some(parent.session_id.clone()),
        fork_at_turn: Some(fork_at_turn),
        fork_operation_id: fork_operation_id.map(str::to_owned),
    };
    let bytes = encode_session_metadata(&metadata)?;
    let path = directory.join("metadata.json");
    #[cfg(unix)]
    {
        persist_session_metadata_unix(&directory, &path, &bytes)
    }
    #[cfg(not(unix))]
    {
        persist_session_metadata_portable(&directory, &path, &bytes)
    }
}
