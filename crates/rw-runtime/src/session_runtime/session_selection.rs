use super::accounting_projection::refresh_session_index;
use super::session_metadata::{
    ensure_real_directory, load_session_metadata, new_session_id, validate_session_id,
};
use super::{RunAction, RunOptions};
use miette::{Result, miette};
use rw_store::session::SessionIndex;
use rw_tools::ExecutionLease;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

pub(super) fn select_session(
    storage_root: &Path,
    workspace: &Path,
    options: &RunOptions,
) -> Result<String> {
    if let Some(session) = &options.resume {
        return Ok(session.clone());
    }
    if options.continue_latest {
        // The SQLite index is a disposable projection. Print mode intentionally
        // leaves it stale so completing a headless turn never waits on SQLite;
        // an explicit continue operation rebuilds from authoritative JSONL.
        refresh_session_index(storage_root)?;
        if let Some(session) = latest_workspace_session(storage_root, workspace)? {
            return Ok(session);
        }
        if is_zero_turn_prompt_dump(options) {
            return new_session_id();
        }
        return Err(miette!(
            "there is no previous session for workspace {} to continue",
            workspace.display()
        ));
    }
    new_session_id()
}

/// Selects an explicit, latest, or newly allocated interactive session.
///
/// # Errors
/// Returns an error when durable session metadata cannot be inspected.
pub fn select_interactive_session(
    storage_root: &Path,
    workspace: &Path,
    resume: Option<&str>,
    continue_latest: bool,
) -> Result<String> {
    if let Some(session) = resume {
        validate_session_id(session)?;
        return Ok(session.to_owned());
    }
    if continue_latest {
        refresh_session_index(storage_root)?;
        return latest_workspace_session(storage_root, workspace)?.ok_or_else(|| {
            miette!(
                "there is no previous session for workspace {} to continue",
                workspace.display()
            )
        });
    }
    new_session_id()
}

pub(super) fn is_zero_turn_prompt_dump(options: &RunOptions) -> bool {
    matches!(options.action, RunAction::PromptDump { turn: None })
}

pub(super) fn latest_workspace_session(
    storage_root: &Path,
    workspace: &Path,
) -> Result<Option<String>> {
    let sessions = SessionIndex::open(storage_root)
        .map_err(|error| miette!("session index could not open: {error}"))?
        .list(10_000)
        .map_err(|error| miette!("sessions could not be listed: {error}"))?;
    for session in sessions {
        match load_session_metadata(storage_root, &session.id, workspace) {
            Ok(_) => return Ok(Some(session.id)),
            Err(error) => tracing::debug!(
                session_id = %session.id,
                reason = %error,
                "skipping session which does not belong to this workspace"
            ),
        }
    }
    Ok(None)
}

pub(crate) fn checkpoint_root(storage_root: &Path, workspace: &Path, session_id: &str) -> PathBuf {
    let digest = blake3::hash(workspace.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    storage_root
        .join("workspaces")
        .join(digest)
        .join("sessions")
        .join(session_id)
}

pub(super) fn workspace_execution_lease_path(
    storage_root: &Path,
    workspace: &Path,
) -> Result<PathBuf> {
    let digest = blake3::hash(workspace.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    let directory = storage_root.join("workspaces").join(digest);
    ensure_real_directory(&directory, true)?;
    Ok(directory.join("execution.lock"))
}

pub(super) fn acquire_shared_execution_lease(
    path: &Path,
    wait: bool,
) -> std::result::Result<Arc<ExecutionLease>, rw_tools::ToolError> {
    const RECOVERY_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    static LEASES: OnceLock<Mutex<HashMap<PathBuf, std::sync::Weak<ExecutionLease>>>> =
        OnceLock::new();
    let mut leases = LEASES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lease) = leases.get(path).and_then(std::sync::Weak::upgrade) {
        return Ok(lease);
    }
    let lease = Arc::new(if wait {
        // A replacement engine must wait for an old watchdog to finish killing
        // its command group before it can safely recover the workspace. The
        // wait is bounded so a competing live session can never look hung.
        ExecutionLease::acquire_for(path, RECOVERY_WAIT_TIMEOUT)?
    } else {
        // A competing interactive host must fail fast instead of waiting until
        // the supervisor's health deadline.
        ExecutionLease::try_acquire(path)?
    });
    leases.insert(path.to_path_buf(), Arc::downgrade(&lease));
    Ok(lease)
}
