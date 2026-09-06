mod mapping;
use super::runtime_options::MAX_WORKSPACE_ROOTS;
use super::session_metadata::validate_session_id;
use super::session_selection::checkpoint_root;
use super::workspace_roots::canonical_workspace_roots;
use crate::journal_service::JournalService;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_store::checkpoint::CheckpointStore;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(super) const CHECKPOINT_ROOTS_VERSION: u16 = 1;

pub(super) const REWIND_COORDINATOR_VERSION: u16 = 1;

pub(super) const MAX_REWIND_COORDINATOR_BYTES: u64 = 16 * 1024;

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointRootMapping {
    pub(super) version: u16,
    pub(super) generations: Vec<CheckpointRootGeneration>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointRootGeneration {
    pub(super) generation: u64,
    pub(super) effective_from_turn: u64,
    pub(super) roots: Vec<PathBuf>,
    pub(super) committed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RewindCoordinatorState {
    Preparing,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RewindCoordinatorDecision {
    pub(super) version: u16,
    pub(super) session_id: String,
    pub(super) operation_id: String,
    pub(super) target_turn: u64,
    pub(super) root_count: usize,
    pub(super) state: RewindCoordinatorState,
}

pub(super) fn open_checkpoint_stores(
    root: &Path,
    workspace_roots: &[PathBuf],
) -> Result<Arc<Vec<Arc<CheckpointStore>>>> {
    if workspace_roots.is_empty() {
        return Err(miette!("checkpoint root mapping cannot be empty"));
    }
    std::fs::create_dir_all(root).into_diagnostic()?;
    let mapping_path = root.join("workspace-roots.json");
    let initial = CheckpointRootMapping {
        version: CHECKPOINT_ROOTS_VERSION,
        generations: vec![CheckpointRootGeneration {
            generation: 0,
            effective_from_turn: 1,
            roots: workspace_roots.to_vec(),
            committed: true,
        }],
    };
    match mapping::read(&mapping_path) {
        Ok(bytes) => {
            let existing: CheckpointRootMapping = mapping::decode(&bytes)
                .map_err(|error| miette!("checkpoint root mapping is corrupt: {error}"))?;
            if existing.version != CHECKPOINT_ROOTS_VERSION
                || existing.generations.last().map(|entry| &entry.roots)
                    != Some(&workspace_roots.to_vec())
            {
                return Err(miette!(
                    "checkpoint root mapping changed; refusing to resume with reordered or replaced workspace roots"
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            persist_root_mapping(&mapping_path, &initial)?;
        }
        Err(error) => return Err(miette!("checkpoint root mapping could not load: {error}")),
    }
    let stores = workspace_roots
        .iter()
        .enumerate()
        .map(|(index, workspace)| {
            CheckpointStore::open(&root.join(format!("root-{index:04}")), workspace)
                .map(Arc::new)
                .map_err(|error| {
                    miette!("checkpoint store for root {index} could not open: {error}")
                })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Arc::new(stores))
}

pub(super) fn append_checkpoint_root_generation(
    root: &Path,
    current_roots: &[PathBuf],
    roots: &[PathBuf],
    generation: u64,
    effective_from_turn: u64,
) -> Result<()> {
    let path = root.join("workspace-roots.json");
    let mut mapping: CheckpointRootMapping = mapping::decode(
        &mapping::read(&path)
            .map_err(|error| miette!("checkpoint root journal could not load: {error}"))?,
    )
    .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    let previous = mapping
        .generations
        .last()
        .ok_or_else(|| miette!("checkpoint root journal is empty"))?;
    if mapping.version != CHECKPOINT_ROOTS_VERSION
        || previous.roots != current_roots
        || generation != previous.generation.saturating_add(1)
        || roots.len() != current_roots.len() + 1
        || roots.iter().take(current_roots.len()).ne(current_roots)
        || effective_from_turn < previous.effective_from_turn
    {
        return Err(miette!(
            "checkpoint root generation is not a strict stable-index append"
        ));
    }
    mapping.generations.push(CheckpointRootGeneration {
        generation,
        effective_from_turn,
        roots: roots.to_vec(),
        committed: false,
    });
    persist_root_mapping(&path, &mapping)
}

pub(super) fn commit_checkpoint_root_generation(root: &Path, generation: u64) -> Result<()> {
    let path = root.join("workspace-roots.json");
    let mut mapping: CheckpointRootMapping = mapping::decode(
        &mapping::read(&path)
            .map_err(|error| miette!("checkpoint root journal could not load: {error}"))?,
    )
    .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    let entry = mapping
        .generations
        .last_mut()
        .filter(|entry| entry.generation == generation)
        .ok_or_else(|| miette!("prepared workspace generation is unavailable"))?;
    entry.committed = true;
    persist_root_mapping(&path, &mapping)
}

pub(super) fn abort_checkpoint_root_generation(root: &Path, generation: u64) -> Result<()> {
    let path = root.join("workspace-roots.json");
    let mut mapping: CheckpointRootMapping = mapping::decode(
        &mapping::read(&path)
            .map_err(|error| miette!("checkpoint root journal could not load: {error}"))?,
    )
    .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    if mapping
        .generations
        .last()
        .is_some_and(|entry| entry.generation == generation)
    {
        mapping.generations.pop();
        if mapping.generations.is_empty() {
            return Err(miette!(
                "checkpoint root journal cannot remove its base generation"
            ));
        }
        persist_root_mapping(&path, &mapping)?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn load_checkpoint_root_generation(
    root: &Path,
) -> Result<Option<CheckpointRootGeneration>> {
    let path = root.join("workspace-roots.json");
    let bytes = match mapping::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(miette!("checkpoint root journal could not load: {error}")),
    };
    let mapping: CheckpointRootMapping = mapping::decode(&bytes)
        .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    if mapping.version != CHECKPOINT_ROOTS_VERSION {
        return Err(miette!("checkpoint root journal version is unsupported"));
    }
    Ok(mapping
        .generations
        .iter()
        .rev()
        .find(|generation| generation.committed)
        .cloned())
}

pub(super) fn load_checkpoint_root_generation_exact(
    root: &Path,
    generation: u64,
) -> Result<Option<CheckpointRootGeneration>> {
    let path = root.join("workspace-roots.json");
    let bytes = match mapping::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(miette!("checkpoint root journal could not load: {error}")),
    };
    let mapping: CheckpointRootMapping = mapping::decode(&bytes)
        .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    if mapping.version != CHECKPOINT_ROOTS_VERSION {
        return Err(miette!("checkpoint root journal version is unsupported"));
    }
    Ok(mapping
        .generations
        .into_iter()
        .find(|entry| entry.generation == generation && entry.committed))
}

pub(crate) fn load_checkpoint_roots_exact(
    root: &Path,
    generation: u64,
) -> Result<Option<Vec<PathBuf>>> {
    load_checkpoint_root_generation_exact(root, generation)
        .map(|entry| entry.map(|entry| entry.roots))
}

pub(crate) fn load_session_workspace_roots(
    journal_service: &JournalService,
    storage_root: &Path,
    workspace: &Path,
    session_id: &str,
) -> Result<Vec<PathBuf>> {
    let root = checkpoint_root(storage_root, workspace, session_id);
    let generation =
        crate::mode_recovery::current_workspace_generation(journal_service, session_id)?;
    if generation == 0 {
        return Ok(vec![workspace.to_path_buf()]);
    }
    let roots = load_checkpoint_root_generation_exact(&root, generation)?
        .map(|entry| entry.roots)
        .ok_or_else(|| {
            miette!("durable workspace event generation is absent from the local root journal")
        })?;
    if roots.len() > MAX_WORKSPACE_ROOTS {
        return Err(miette!(
            "durable workspace root count exceeds the supported maximum"
        ));
    }
    Ok(roots)
}

pub(super) fn restore_persisted_workspace_roots(
    root: &Path,
    primary: &Path,
    supplied: &[PathBuf],
    committed_generation: u64,
) -> Result<Option<CheckpointRootGeneration>> {
    let path = root.join("workspace-roots.json");
    let bytes = match mapping::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && committed_generation == 0 => {
            return Ok(None);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(miette!(
                "committed workspace generation is missing its local root journal"
            ));
        }
        Err(error) => return Err(miette!("checkpoint root journal could not load: {error}")),
    };
    let mut mapping: CheckpointRootMapping = mapping::decode(&bytes)
        .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    let Some(position) = mapping
        .generations
        .iter()
        .position(|entry| entry.generation == committed_generation)
    else {
        return Err(miette!(
            "committed workspace generation is absent from the local root journal"
        ));
    };
    let needs_rewrite =
        position + 1 < mapping.generations.len() || !mapping.generations[position].committed;
    if position + 1 < mapping.generations.len() {
        mapping.generations.truncate(position + 1);
    }
    mapping.generations[position].committed = true;
    if needs_rewrite {
        persist_root_mapping(&path, &mapping)?;
    }
    let Some(mut generation) = mapping.generations.last().cloned() else {
        return Ok(None);
    };
    generation.roots = canonical_workspace_roots(primary, &generation.roots[1..])?;
    if supplied.len() > 1 && supplied != generation.roots {
        return Err(miette!(
            "resume workspace roots differ from the durable stable-index generation"
        ));
    }
    Ok(Some(generation))
}

/// Resolves the historical root generation without repairing or rewriting its
/// journal. A matching uncommitted generation is intentionally visible here:
/// the durable event is the commit record, and repair marks/truncates the local
/// journal only after mode validation. Resume uses this preview to compose the
/// exact mode registry before any crash-recovery mutation.
pub(super) fn preview_persisted_workspace_roots(
    root: &Path,
    primary: &Path,
    supplied: &[PathBuf],
    committed_generation: u64,
) -> Result<Option<CheckpointRootGeneration>> {
    let path = root.join("workspace-roots.json");
    let bytes = match mapping::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && committed_generation == 0 => {
            return Ok(None);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(miette!(
                "committed workspace generation is missing its local root journal"
            ));
        }
        Err(error) => return Err(miette!("checkpoint root journal could not load: {error}")),
    };
    let mapping: CheckpointRootMapping = mapping::decode(&bytes)
        .map_err(|error| miette!("checkpoint root journal is corrupt: {error}"))?;
    if mapping.version != CHECKPOINT_ROOTS_VERSION {
        return Err(miette!("checkpoint root journal version is unsupported"));
    }
    let Some(mut generation) = mapping
        .generations
        .into_iter()
        .find(|entry| entry.generation == committed_generation)
    else {
        return Err(miette!(
            "committed workspace generation is absent from the local root journal"
        ));
    };
    generation.roots = canonical_workspace_roots(primary, &generation.roots[1..])?;
    if supplied.len() > 1 && supplied != generation.roots {
        return Err(miette!(
            "resume workspace roots differ from the durable stable-index generation"
        ));
    }
    Ok(Some(generation))
}

pub(super) fn persist_root_mapping(path: &Path, mapping: &CheckpointRootMapping) -> Result<()> {
    mapping::validate(mapping)?;
    persist_private_json(path, mapping)
}

pub(super) fn persist_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = mapping::encode(value)?;
    let parent = path
        .parent()
        .ok_or_else(|| miette!("private JSON path has no parent"))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".roots-{}-{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).into_diagnostic()?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes).into_diagnostic()?;
        file.flush().into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        std::fs::rename(&temporary, path).into_diagnostic()?;
        std::fs::File::open(parent)
            .into_diagnostic()?
            .sync_all()
            .into_diagnostic()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(super) fn rewind_coordinator_path(checkpoint_root: &Path) -> PathBuf {
    checkpoint_root.join("rewind-coordinator.json")
}

pub(super) fn persist_rewind_coordinator(
    checkpoint_root: &Path,
    decision: &RewindCoordinatorDecision,
) -> Result<()> {
    persist_private_json(&rewind_coordinator_path(checkpoint_root), decision)
}

pub(super) fn load_rewind_coordinator(
    checkpoint_root: &Path,
) -> Result<Option<RewindCoordinatorDecision>> {
    let path = rewind_coordinator_path(checkpoint_root);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => return Err(miette!("rewind coordinator has an unsafe file type")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(miette!(
                "rewind coordinator could not be inspected: {error}"
            ));
        }
    };
    if metadata.len() > MAX_REWIND_COORDINATOR_BYTES {
        return Err(miette!("rewind coordinator exceeds its size limit"));
    }
    let decision: RewindCoordinatorDecision = serde_json::from_slice(
        &mapping::read_limit(
            &path,
            usize::try_from(MAX_REWIND_COORDINATOR_BYTES).into_diagnostic()?,
        )
        .map_err(|error| miette!("rewind coordinator could not load: {error}"))?,
    )
    .map_err(|error| miette!("rewind coordinator is corrupt: {error}"))?;
    validate_rewind_coordinator(&decision)?;
    Ok(Some(decision))
}

pub(super) fn validate_rewind_coordinator(decision: &RewindCoordinatorDecision) -> Result<()> {
    validate_session_id(&decision.session_id)?;
    if decision.version != REWIND_COORDINATOR_VERSION
        || decision.root_count == 0
        || decision.root_count > MAX_WORKSPACE_ROOTS
        || decision.operation_id.is_empty()
        || decision.operation_id.len() > 128
        || !decision
            .operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(miette!("rewind coordinator identity is invalid"));
    }
    Ok(())
}

pub(super) fn remove_rewind_coordinator(checkpoint_root: &Path) -> Result<()> {
    let path = rewind_coordinator_path(checkpoint_root);
    match std::fs::remove_file(path) {
        Ok(()) => std::fs::File::open(checkpoint_root)
            .into_diagnostic()?
            .sync_all()
            .into_diagnostic(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(miette!("rewind coordinator could not be removed: {error}")),
    }
}
