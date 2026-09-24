//! Folder-trust ledger and project extension inventory.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const TRUST_FORMAT_VERSION: u16 = 1;
const MAX_INVENTORY_FILES: usize = 4_096;
const MAX_INVENTORY_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INVENTORY_TOTAL_BYTES: u64 = 32 * 1024 * 1024;

/// Persisted trust state for the current project extension inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderTrustState {
    /// No decision has been recorded for this canonical workspace.
    Untrusted,
    /// The workspace was trusted, but project extension content changed.
    Changed,
    /// The canonical path and project-extension inventory hash match the ledger.
    Trusted,
    /// The project-extension inventory could not be completed safely.
    Untrustable,
}

/// Why a project root cannot currently participate in folder trust.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderTrustInventoryFailure {
    path: PathBuf,
    message: String,
}

impl FolderTrustInventoryFailure {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One project-local file that can influence executable agent behavior.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrustInventoryItem {
    /// Path relative to the canonical workspace, never an absolute machine path.
    pub path: String,
    /// Stable UI-facing category used by the trust prompt.
    pub kind: String,
    /// BLAKE3 content hash.
    pub content_hash: String,
    /// Exact file length considered by the inventory.
    pub bytes: u64,
}

/// A change since the last trusted inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrustInventoryChange {
    Added(TrustInventoryItem),
    Removed(TrustInventoryItem),
    Modified {
        before: TrustInventoryItem,
        after: TrustInventoryItem,
    },
}

/// Current trust decision plus the complete prompt inventory and diff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderTrustAssessment {
    workspace: PathBuf,
    executable_hash: Option<String>,
    inventory: Vec<TrustInventoryItem>,
    changes: Vec<TrustInventoryChange>,
    state: FolderTrustState,
    inventory_failure: Option<FolderTrustInventoryFailure>,
}

impl FolderTrustAssessment {
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    #[must_use]
    pub fn executable_hash(&self) -> Option<&str> {
        self.executable_hash.as_deref()
    }

    #[must_use]
    pub fn inventory(&self) -> &[TrustInventoryItem] {
        &self.inventory
    }

    /// Whether this workspace currently contains project extension artifacts
    /// whose activation requires an explicit trust decision.
    #[must_use]
    pub fn requires_confirmation(&self) -> bool {
        self.inventory_failure.is_none()
            && !self.project_execution_enabled()
            && !self.inventory.is_empty()
    }

    #[must_use]
    pub fn changes(&self) -> &[TrustInventoryChange] {
        &self.changes
    }

    #[must_use]
    pub const fn state(&self) -> FolderTrustState {
        self.state
    }

    #[must_use]
    pub const fn project_execution_enabled(&self) -> bool {
        matches!(self.state, FolderTrustState::Trusted)
    }

    /// The precise inventory failure that prevents this root from being trusted.
    #[must_use]
    pub const fn inventory_failure(&self) -> Option<&FolderTrustInventoryFailure> {
        self.inventory_failure.as_ref()
    }

    /// Stable, path-relative inventory suitable for an interactive prompt.
    #[must_use]
    pub fn render_prompt(&self) -> String {
        self.render_prompt_with_workspace(&self.workspace.display().to_string())
    }

    /// Stable inventory rendered with a caller-supplied non-sensitive
    /// workspace label such as `@root/0`.
    #[must_use]
    pub fn render_prompt_with_workspace(&self, workspace: &str) -> String {
        let mut lines = vec![format!(
            "workspace: {}\nstate: {:?}\nproject extension inventory:",
            workspace, self.state
        )];
        if self.inventory.is_empty() {
            if let Some(failure) = &self.inventory_failure {
                lines.push("  (unavailable; no fingerprint was produced)".to_owned());
                lines.push(format!(
                    "inventory failure: {}: {}",
                    failure.path.display(),
                    failure.message
                ));
            } else {
                lines.push("  (none)".to_owned());
            }
        } else {
            lines.extend(self.inventory.iter().map(|item| {
                format!(
                    "  {} [{}] {} bytes hash {}",
                    item.path, item.kind, item.bytes, item.content_hash
                )
            }));
        }
        if !self.changes.is_empty() {
            lines.push("changes since last trust:".to_owned());
            lines.extend(self.changes.iter().map(|change| match change {
                TrustInventoryChange::Added(item) => format!("  + {}", item.path),
                TrustInventoryChange::Removed(item) => format!("  - {}", item.path),
                TrustInventoryChange::Modified { after, .. } => format!("  ~ {}", after.path),
            }));
        }
        lines.join("\n") + "\n"
    }
}

/// Fail-closed trust-store and inventory errors.
#[derive(Debug, Error)]
pub enum FolderTrustError {
    #[error("workspace is unavailable: {path}: {source}")]
    Workspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsafe project extension entry {0}")]
    UnsafeEntry(PathBuf),
    #[error("unsafe project extension link {path}: {reason}")]
    UnsafeLink { path: PathBuf, reason: &'static str },
    #[error("project extension inventory exceeded its {limit}-file limit")]
    FileLimit { limit: usize },
    #[error("project extension file exceeds its {limit}-byte limit: {path}")]
    FileSize { path: PathBuf, limit: u64 },
    #[error("project extension inventory exceeds its {limit}-byte total limit")]
    TotalSize { limit: u64 },
    #[error("project extension path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("could not read trust ledger {path}: {source}")]
    ReadLedger {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid trust ledger {path}: {source}")]
    ParseLedger {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not write trust ledger {path}: {source}")]
    WriteLedger {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace executable content changed while trust was being granted")]
    ChangedDuringGrant,
    #[error(
        "refusing to grant trust because project extension inventory is incomplete at {path}: {message}"
    )]
    Untrustable { path: PathBuf, message: String },
    #[error("trust ledger is locked by another writer: {0}")]
    LedgerLocked(PathBuf),
}

impl FolderTrustError {
    fn is_inventory_failure(&self) -> bool {
        matches!(
            self,
            Self::Workspace { .. }
                | Self::UnsafeEntry(_)
                | Self::UnsafeLink { .. }
                | Self::FileLimit { .. }
                | Self::FileSize { .. }
                | Self::TotalSize { .. }
                | Self::NonUtf8Path(_)
        )
    }

    fn inventory_failure_path(&self, workspace: &Path) -> PathBuf {
        let path = match self {
            Self::Workspace { path, .. }
            | Self::FileSize { path, .. }
            | Self::UnsafeLink { path, .. }
            | Self::UnsafeEntry(path)
            | Self::NonUtf8Path(path)
            | Self::ReadLedger { path, .. }
            | Self::ParseLedger { path, .. }
            | Self::WriteLedger { path, .. }
            | Self::LedgerLocked(path) => path.clone(),
            Self::FileLimit { .. }
            | Self::TotalSize { .. }
            | Self::ChangedDuringGrant
            | Self::Untrustable { .. } => workspace.to_owned(),
        };
        if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrustLedger {
    version: u16,
    workspaces: BTreeMap<String, TrustedWorkspace>,
}

impl Default for TrustLedger {
    fn default() -> Self {
        Self {
            version: TRUST_FORMAT_VERSION,
            workspaces: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrustedWorkspace {
    executable_hash: String,
    inventory: Vec<TrustInventoryItem>,
}

/// User-scoped persisted trust decisions.
#[derive(Clone, Debug)]
pub struct FolderTrustStore {
    path: PathBuf,
    user_home: Option<PathBuf>,
}

impl FolderTrustStore {
    /// Opens the ledger at `path`. Project skill links may resolve into the
    /// user's home directory, taken from an absolute `HOME`; without one, links
    /// must stay inside the project.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        let user_home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        Self { path, user_home }
    }

    /// Sets the home directory that bounds project skill links, matching the
    /// home used by extension discovery.
    #[must_use]
    pub fn with_user_home(mut self, user_home: impl Into<PathBuf>) -> Self {
        self.user_home = Some(user_home.into());
        self
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Assess a workspace without changing trust state.
    ///
    /// # Errors
    ///
    /// Unsafe, unbounded, or unreadable project inventories produce an
    /// untrustable assessment with no fingerprint. Errors are reserved for an
    /// unavailable workspace identity or unreadable/corrupt trust ledger.
    pub fn assess(&self, workspace: &Path) -> Result<FolderTrustAssessment, FolderTrustError> {
        let workspace =
            fs::canonicalize(workspace).map_err(|source| FolderTrustError::Workspace {
                path: workspace.to_owned(),
                source,
            })?;
        let inventory = match executable_inventory(&workspace, self.user_home.as_deref()) {
            Ok(inventory) => inventory,
            Err(error) if error.is_inventory_failure() => {
                let failure = FolderTrustInventoryFailure {
                    path: error.inventory_failure_path(&workspace),
                    message: error.to_string(),
                };
                return Ok(FolderTrustAssessment {
                    workspace,
                    executable_hash: None,
                    inventory: Vec::new(),
                    changes: Vec::new(),
                    state: FolderTrustState::Untrustable,
                    inventory_failure: Some(failure),
                });
            }
            Err(error) => return Err(error),
        };
        let executable_hash = inventory_hash(&inventory);
        let ledger = self.read_ledger()?;
        let key = workspace_key(&workspace)?;
        let (state, changes) = match ledger.workspaces.get(&key) {
            None => (
                FolderTrustState::Untrusted,
                inventory
                    .iter()
                    .cloned()
                    .map(TrustInventoryChange::Added)
                    .collect(),
            ),
            Some(record) if record.executable_hash == executable_hash => {
                (FolderTrustState::Trusted, Vec::new())
            }
            Some(record) => (
                FolderTrustState::Changed,
                inventory_diff(&record.inventory, &inventory),
            ),
        };
        Ok(FolderTrustAssessment {
            workspace,
            executable_hash: Some(executable_hash),
            inventory,
            changes,
            state,
            inventory_failure: None,
        })
    }

    /// Persist the exact assessment after rechecking the workspace.
    ///
    /// # Errors
    ///
    /// Fails if the assessment is untrustable, executable content changed
    /// since the prompt, or the private ledger cannot be safely replaced.
    pub fn grant(&self, assessment: &FolderTrustAssessment) -> Result<(), FolderTrustError> {
        self.grant_all(std::slice::from_ref(assessment))
    }

    /// Atomically grant trust to several exact assessed workspace inventories.
    ///
    /// Every inventory is rechecked under one writer lock before the ledger is
    /// replaced, so a multi-root grant cannot partially commit.
    ///
    /// # Errors
    ///
    /// Fails if any assessment is untrustable, any inventory changed, or the
    /// private ledger cannot be locked, read, or atomically replaced.
    pub fn grant_all(&self, assessments: &[FolderTrustAssessment]) -> Result<(), FolderTrustError> {
        if let Some(failure) = assessments
            .iter()
            .find_map(FolderTrustAssessment::inventory_failure)
        {
            return Err(FolderTrustError::Untrustable {
                path: failure.path.clone(),
                message: failure.message.clone(),
            });
        }
        let _lock = self.acquire_write_lock()?;
        for assessment in assessments {
            let current = self.assess(&assessment.workspace)?;
            if current.executable_hash != assessment.executable_hash
                || current.inventory != assessment.inventory
            {
                return Err(FolderTrustError::ChangedDuringGrant);
            }
        }
        let mut ledger = self.read_ledger()?;
        for assessment in assessments {
            let key = workspace_key(&assessment.workspace)?;
            ledger.workspaces.insert(
                key,
                TrustedWorkspace {
                    executable_hash: assessment.executable_hash.clone().ok_or_else(|| {
                        FolderTrustError::Untrustable {
                            path: assessment.workspace.clone(),
                            message: "project extension inventory has no fingerprint".to_owned(),
                        }
                    })?,
                    inventory: assessment.inventory.clone(),
                },
            );
        }
        self.write_ledger(&ledger)
    }

    /// Remove any persisted decision for a workspace.
    ///
    /// # Errors
    ///
    /// Fails if the workspace or private ledger cannot be read/written.
    pub fn revoke(&self, workspace: &Path) -> Result<(), FolderTrustError> {
        self.revoke_all(std::slice::from_ref(&workspace.to_path_buf()))
    }

    /// Atomically revoke trust for several canonical workspace identities.
    ///
    /// # Errors
    ///
    /// Fails if a workspace cannot be canonicalized or the private ledger
    /// cannot be locked, read, or atomically replaced.
    pub fn revoke_all(&self, workspaces: &[PathBuf]) -> Result<(), FolderTrustError> {
        let workspaces = workspaces
            .iter()
            .map(|workspace| {
                fs::canonicalize(workspace).map_err(|source| FolderTrustError::Workspace {
                    path: workspace.to_owned(),
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let _lock = self.acquire_write_lock()?;
        let mut ledger = self.read_ledger()?;
        for workspace in workspaces {
            ledger.workspaces.remove(&workspace_key(&workspace)?);
        }
        self.write_ledger(&ledger)
    }

    fn read_ledger(&self) -> Result<TrustLedger, FolderTrustError> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(FolderTrustError::UnsafeEntry(self.path.clone()))
            }
            Ok(_) => {
                let bytes =
                    fs::read(&self.path).map_err(|source| FolderTrustError::ReadLedger {
                        path: self.path.clone(),
                        source,
                    })?;
                let ledger: TrustLedger = serde_json::from_slice(&bytes).map_err(|source| {
                    FolderTrustError::ParseLedger {
                        path: self.path.clone(),
                        source,
                    }
                })?;
                if ledger.version != TRUST_FORMAT_VERSION {
                    return Err(FolderTrustError::ParseLedger {
                        path: self.path.clone(),
                        source: serde_json::Error::io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "unsupported trust-ledger version",
                        )),
                    });
                }
                Ok(ledger)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(TrustLedger::default())
            }
            Err(source) => Err(FolderTrustError::ReadLedger {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn write_ledger(&self, ledger: &TrustLedger) -> Result<(), FolderTrustError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| FolderTrustError::WriteLedger {
                path: self.path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "ledger has no parent",
                ),
            })?;
        fs::create_dir_all(parent).map_err(|source| FolderTrustError::WriteLedger {
            path: parent.to_owned(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|source| {
                FolderTrustError::WriteLedger {
                    path: parent.to_owned(),
                    source,
                }
            })?;
        }
        let bytes =
            serde_json::to_vec_pretty(ledger).map_err(|source| FolderTrustError::ParseLedger {
                path: self.path.clone(),
                source,
            })?;
        let temporary = parent.join(format!(
            ".trust.{}.{}.tmp",
            std::process::id(),
            blake3::hash(&bytes).to_hex()
        ));
        let result = (|| -> Result<(), std::io::Error> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        })();
        if let Err(source) = result {
            let _ = fs::remove_file(&temporary);
            return Err(FolderTrustError::WriteLedger {
                path: self.path.clone(),
                source,
            });
        }
        Ok(())
    }

    fn acquire_write_lock(&self) -> Result<TrustLedgerLock, FolderTrustError> {
        let lock_path = self.path.with_extension("lock");
        let parent = lock_path
            .parent()
            .ok_or_else(|| FolderTrustError::WriteLedger {
                path: lock_path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "trust lock has no parent",
                ),
            })?;
        fs::create_dir_all(parent).map_err(|source| FolderTrustError::WriteLedger {
            path: parent.to_owned(),
            source,
        })?;
        match fs::create_dir(&lock_path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o700)).map_err(
                        |source| FolderTrustError::WriteLedger {
                            path: lock_path.clone(),
                            source,
                        },
                    )?;
                }
                Ok(TrustLedgerLock { path: lock_path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(FolderTrustError::LedgerLocked(lock_path))
            }
            Err(source) => Err(FolderTrustError::WriteLedger {
                path: lock_path,
                source,
            }),
        }
    }
}

struct TrustLedgerLock {
    path: PathBuf,
}

impl Drop for TrustLedgerLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn inventory_hash(inventory: &[TrustInventoryItem]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler-folder-trust-v1\0");
    for item in inventory {
        hasher.update(item.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(item.kind.as_bytes());
        hasher.update(b"\0");
        hasher.update(item.content_hash.as_bytes());
        hasher.update(b"\0");
        hasher.update(&item.bytes.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn inventory_diff(
    before: &[TrustInventoryItem],
    after: &[TrustInventoryItem],
) -> Vec<TrustInventoryChange> {
    let before = before
        .iter()
        .map(|item| (item.path.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    let after = after
        .iter()
        .map(|item| (item.path.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    let paths = before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    paths
        .into_iter()
        .filter_map(|path| match (before.get(path), after.get(path)) {
            (None, Some(item)) => Some(TrustInventoryChange::Added((*item).clone())),
            (Some(item), None) => Some(TrustInventoryChange::Removed((*item).clone())),
            (Some(left), Some(right)) if left != right => Some(TrustInventoryChange::Modified {
                before: (*left).clone(),
                after: (*right).clone(),
            }),
            _ => None,
        })
        .collect()
}

fn workspace_key(workspace: &Path) -> Result<String, FolderTrustError> {
    workspace
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| FolderTrustError::NonUtf8Path(workspace.to_owned()))
}

mod inventory;
use inventory::executable_inventory;

#[cfg(test)]
mod tests;
