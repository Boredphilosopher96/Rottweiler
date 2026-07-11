//! Folder-trust ledger and executable project inventory.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const TRUST_FORMAT_VERSION: u16 = 1;
const MAX_INVENTORY_FILES: usize = 4_096;
const MAX_INVENTORY_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INVENTORY_TOTAL_BYTES: u64 = 32 * 1024 * 1024;

/// Persisted trust state for the current executable project configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolderTrustState {
    /// No decision has been recorded for this canonical workspace.
    Untrusted,
    /// The workspace was trusted, but executable project content changed.
    Changed,
    /// The canonical path and executable-content hash match the ledger.
    Trusted,
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
    executable_hash: String,
    inventory: Vec<TrustInventoryItem>,
    changes: Vec<TrustInventoryChange>,
    state: FolderTrustState,
}

impl FolderTrustAssessment {
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    #[must_use]
    pub fn executable_hash(&self) -> &str {
        &self.executable_hash
    }

    #[must_use]
    pub fn inventory(&self) -> &[TrustInventoryItem] {
        &self.inventory
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
            "workspace: {}\nstate: {:?}\nexecutable project inventory:",
            workspace, self.state
        )];
        if self.inventory.is_empty() {
            lines.push("  (none)".to_owned());
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
    #[error("project executable inventory exceeded its {limit}-file limit")]
    FileLimit { limit: usize },
    #[error("project executable file exceeds its {limit}-byte limit: {path}")]
    FileSize { path: PathBuf, limit: u64 },
    #[error("project executable inventory exceeds its {limit}-byte total limit")]
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
    #[error("trust ledger is locked by another writer: {0}")]
    LedgerLocked(PathBuf),
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
}

impl FolderTrustStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Assess a workspace without changing trust state.
    ///
    /// # Errors
    ///
    /// Fails closed for unsafe extension entries, unbounded content, or an
    /// unreadable/corrupt ledger.
    pub fn assess(&self, workspace: &Path) -> Result<FolderTrustAssessment, FolderTrustError> {
        let workspace =
            fs::canonicalize(workspace).map_err(|source| FolderTrustError::Workspace {
                path: workspace.to_owned(),
                source,
            })?;
        let inventory = executable_inventory(&workspace)?;
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
            executable_hash,
            inventory,
            changes,
            state,
        })
    }

    /// Persist the exact assessment after rechecking the workspace.
    ///
    /// # Errors
    ///
    /// Fails if executable content changed since the prompt or the private
    /// ledger cannot be safely replaced.
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
    /// Fails if any inventory changed or the private ledger cannot be locked,
    /// read, or atomically replaced.
    pub fn grant_all(&self, assessments: &[FolderTrustAssessment]) -> Result<(), FolderTrustError> {
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
                    executable_hash: assessment.executable_hash.clone(),
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

fn executable_inventory(workspace: &Path) -> Result<Vec<TrustInventoryItem>, FolderTrustError> {
    let mut files = Vec::new();
    for discovery in [".agents", ".rottweiler"] {
        let root = workspace.join(discovery);
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(FolderTrustError::UnsafeEntry(root));
            }
            Ok(_) => collect_files(workspace, &root, &mut files)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(FolderTrustError::Workspace { path: root, source });
            }
        }
    }
    files.sort();
    let mut total = 0_u64;
    let mut inventory = Vec::with_capacity(files.len());
    for path in files {
        let relative_path = path
            .strip_prefix(workspace)
            .map_err(|_| FolderTrustError::UnsafeEntry(path.clone()))?;
        let bytes = read_inventory_file(workspace, relative_path)?;
        let byte_count = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        total = total.saturating_add(byte_count);
        if total > MAX_INVENTORY_TOTAL_BYTES {
            return Err(FolderTrustError::TotalSize {
                limit: MAX_INVENTORY_TOTAL_BYTES,
            });
        }
        let relative = relative_path
            .to_str()
            .ok_or_else(|| FolderTrustError::NonUtf8Path(relative_path.to_owned()))?
            .replace('\\', "/");
        inventory.push(TrustInventoryItem {
            kind: inventory_kind(&relative).to_owned(),
            path: relative,
            content_hash: blake3::hash(&bytes).to_hex().to_string(),
            bytes: byte_count,
        });
    }
    Ok(inventory)
}

fn read_inventory_file(workspace: &Path, relative: &Path) -> Result<Vec<u8>, FolderTrustError> {
    #[cfg(unix)]
    let file = {
        use std::os::fd::OwnedFd;

        let mut directory: OwnedFd = rustix::fs::open(
            workspace,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| FolderTrustError::Workspace {
            path: workspace.to_owned(),
            source: source.into(),
        })?;
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                let std::path::Component::Normal(name) = component else {
                    return Err(FolderTrustError::UnsafeEntry(relative.to_owned()));
                };
                directory = rustix::fs::openat(
                    &directory,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )
                .map_err(|source| FolderTrustError::Workspace {
                    path: workspace.join(relative),
                    source: source.into(),
                })?;
            }
        }
        let file_name = relative
            .file_name()
            .ok_or_else(|| FolderTrustError::UnsafeEntry(relative.to_owned()))?;
        let descriptor = rustix::fs::openat(
            &directory,
            file_name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|source| FolderTrustError::Workspace {
            path: workspace.join(relative),
            source: source.into(),
        })?;
        let file = fs::File::from(descriptor);
        if !file
            .metadata()
            .map_err(|source| FolderTrustError::Workspace {
                path: workspace.join(relative),
                source,
            })?
            .is_file()
        {
            return Err(FolderTrustError::UnsafeEntry(workspace.join(relative)));
        }
        file
    };
    #[cfg(not(unix))]
    let file = {
        let path = workspace.join(relative);
        let file = fs::File::open(&path).map_err(|source| FolderTrustError::Workspace {
            path: path.clone(),
            source,
        })?;
        if !file
            .metadata()
            .map_err(|source| FolderTrustError::Workspace {
                path: path.clone(),
                source,
            })?
            .is_file()
        {
            return Err(FolderTrustError::UnsafeEntry(path));
        }
        file
    };
    let mut bytes = Vec::new();
    file.take(MAX_INVENTORY_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| FolderTrustError::Workspace {
            path: workspace.join(relative),
            source,
        })?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_INVENTORY_FILE_BYTES {
        return Err(FolderTrustError::FileSize {
            path: workspace.join(relative),
            limit: MAX_INVENTORY_FILE_BYTES,
        });
    }
    Ok(bytes)
}

fn collect_files(
    workspace: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), FolderTrustError> {
    let entries = fs::read_dir(directory).map_err(|source| FolderTrustError::Workspace {
        path: directory.to_owned(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| FolderTrustError::Workspace {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| FolderTrustError::Workspace {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(FolderTrustError::UnsafeEntry(path));
        }
        if metadata.is_dir() {
            collect_files(workspace, &path, files)?;
        } else if metadata.is_file() {
            files.push(path);
            if files.len() > MAX_INVENTORY_FILES {
                return Err(FolderTrustError::FileLimit {
                    limit: MAX_INVENTORY_FILES,
                });
            }
        } else {
            return Err(FolderTrustError::UnsafeEntry(path));
        }
    }
    let _ = workspace;
    Ok(())
}

fn inventory_kind(path: &str) -> &'static str {
    let normalized = path
        .strip_prefix(".agents/")
        .or_else(|| path.strip_prefix(".rottweiler/"))
        .unwrap_or(path);
    if normalized.starts_with("commands/") {
        "command"
    } else if normalized.starts_with("skills/") {
        "skill"
    } else if normalized.starts_with("agents/") {
        "agent"
    } else if normalized.starts_with("modes/") {
        "mode"
    } else if normalized.starts_with("workflows/") {
        "workflow"
    } else if normalized == "hooks.toml" {
        "hook"
    } else if normalized == "toolchain.toml" {
        "toolchain"
    } else if normalized == "plugins.toml" {
        "plugin"
    } else if matches!(normalized, "mcp.toml" | "mcp.json") {
        "mcp"
    } else if normalized == "config.toml" {
        "project_config"
    } else {
        "project_extension"
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn malicious_project_is_inert_until_exact_inventory_is_trusted() {
        let canary = Path::new("/tmp/rottweiler-untrusted-folder-pwned");
        let _ = fs::remove_file(canary);
        let root = TempDir::new().expect("root");
        let workspace = root.path().join("repo");
        let agents = workspace.join(".agents/commands");
        let rottweiler = workspace.join(".rottweiler");
        fs::create_dir_all(&agents).expect("agents");
        fs::create_dir_all(&rottweiler).expect("rottweiler");
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/untrusted-project");
        fs::write(
            agents.join("x.md"),
            fs::read(fixture.join(".agents/commands/x.md")).expect("command fixture"),
        )
        .expect("command");
        fs::write(
            rottweiler.join("plugins.toml"),
            fs::read(fixture.join(".rottweiler/plugins.toml")).expect("plugin fixture"),
        )
        .expect("plugin");
        let store = FolderTrustStore::new(root.path().join("user/trust.json"));
        let first = store.assess(&workspace).expect("assessment");
        assert_eq!(first.state(), FolderTrustState::Untrusted);
        assert!(!first.project_execution_enabled());
        assert!(first.inventory().iter().any(|item| item.kind == "command"));
        assert!(first.inventory().iter().any(|item| item.kind == "plugin"));
        let prompt = first.render_prompt();
        assert!(prompt.contains(".agents/commands/x.md"));
        assert!(prompt.contains(".rottweiler/plugins.toml"));
        assert!(
            !canary.exists(),
            "inventory must never execute project content"
        );

        store.grant(&first).expect("grant");
        assert_eq!(
            store.assess(&workspace).expect("trusted").state(),
            FolderTrustState::Trusted
        );
        assert!(
            !canary.exists(),
            "grant persistence must not execute project content"
        );

        fs::write(agents.join("x.md"), "!`touch /tmp/changed`\n").expect("change");
        let changed = store.assess(&workspace).expect("changed");
        assert_eq!(changed.state(), FolderTrustState::Changed);
        assert!(!changed.project_execution_enabled());
        assert!(matches!(
            changed.changes(),
            [TrustInventoryChange::Modified { after, .. }] if after.path == ".agents/commands/x.md"
        ));
    }

    #[test]
    fn decisions_are_keyed_by_canonical_absolute_workspace() {
        let root = TempDir::new().expect("root");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::create_dir_all(&first).expect("first");
        fs::create_dir_all(&second).expect("second");
        let store = FolderTrustStore::new(root.path().join("user/trust.json"));
        let first_assessment = store.assess(&first).expect("first assessment");
        store.grant(&first_assessment).expect("grant first");
        assert_eq!(
            store.assess(&first).expect("first trusted").state(),
            FolderTrustState::Trusted
        );
        assert_eq!(
            store.assess(&second).expect("second untrusted").state(),
            FolderTrustState::Untrusted
        );

        let second_assessment = store.assess(&second).expect("second assessment");
        store.grant(&second_assessment).expect("grant second");
        assert_eq!(
            store.assess(&first).expect("first retained").state(),
            FolderTrustState::Trusted
        );
        assert_eq!(
            store.assess(&second).expect("second trusted").state(),
            FolderTrustState::Trusted
        );
    }

    #[test]
    fn concurrent_writer_lock_fails_closed_instead_of_losing_a_decision() {
        let root = TempDir::new().expect("root");
        let workspace = root.path().join("repo");
        fs::create_dir_all(&workspace).expect("workspace");
        let path = root.path().join("user/trust.json");
        let store = FolderTrustStore::new(path.clone());
        let assessment = store.assess(&workspace).expect("assessment");
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::create_dir(path.with_extension("lock")).expect("competing lock");
        assert!(matches!(
            store.grant(&assessment),
            Err(FolderTrustError::LedgerLocked(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_project_extension_fails_closed() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().expect("root");
        let workspace = root.path().join("repo");
        fs::create_dir_all(workspace.join(".agents/commands")).expect("commands");
        fs::write(root.path().join("outside"), "payload").expect("outside");
        symlink(
            root.path().join("outside"),
            workspace.join(".agents/commands/x.md"),
        )
        .expect("symlink");
        let store = FolderTrustStore::new(root.path().join("user/trust.json"));
        assert!(matches!(
            store.assess(&workspace),
            Err(FolderTrustError::UnsafeEntry(_))
        ));
    }
}
