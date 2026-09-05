//! Git worktree isolation and explicit diff-artifact application.
//!
//! Child sessions never merge into their parent. They run under a randomized,
//! private path outside the repository and return a bounded artifact. The only
//! mutation boundary is [`ApplyWorktreeDiffTool`], which participates in the
//! ordinary permission and checkpoint pipeline.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::OwnedFd;

use rw_types::{Cost, DiffArtifact, TouchedFile, TouchedFileStatus, Usage};
use serde::{Deserialize, Serialize};

use crate::background::begin_worktree_finalization;
use crate::registry::{CancellationToken, ToolError};

const DIAGNOSTIC_LIMIT: usize = 64 * 1024;
const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;
const MAX_FINAL_TEXT_BYTES: usize = 64 * 1024;
const MAX_TOUCHED_FILES: usize = 4_096;
const MAX_GIT_OUTPUT_BYTES: usize = MAX_DIFF_BYTES + 1;

/// Hard bounds for data returned from an isolated child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorktreeLimits {
    pub max_diff_bytes: usize,
    pub max_final_text_bytes: usize,
    pub max_touched_files: usize,
}

impl Default for WorktreeLimits {
    fn default() -> Self {
        Self {
            max_diff_bytes: 4 * 1024 * 1024,
            max_final_text_bytes: 64 * 1024,
            max_touched_files: 4_096,
        }
    }
}

/// Stable return envelope from a child session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChildReturnArtifact {
    pub final_text: String,
    pub final_text_truncated: bool,
    pub touched_files: Vec<TouchedFile>,
    pub diff: Option<DiffArtifact>,
    pub usage: Usage,
    pub cost: Cost,
}

/// One pinned worktree created by [`WorktreeIsolation`].
#[derive(Clone, Debug)]
pub struct WorktreeLease {
    path: PathBuf,
    base_commit: String,
    identity: DirectoryIdentity,
    finalization_gate: Arc<tokio::sync::Mutex<()>>,
}

/// Host-private durable metadata used to rebind a continuable child after restart.
/// This contains a local path and belongs in session metadata, never model context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorktreeLeaseRecord {
    path: PathBuf,
    base_commit: String,
    canonical_path: PathBuf,
    device: Option<u64>,
    inode: Option<u64>,
}

impl WorktreeLease {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    /// Captures the pinned identity needed for a later safe rebind.
    #[must_use]
    pub fn durable_record(&self) -> WorktreeLeaseRecord {
        WorktreeLeaseRecord {
            path: self.path.clone(),
            base_commit: self.base_commit.clone(),
            canonical_path: self.identity.canonical.clone(),
            #[cfg(unix)]
            device: Some(self.identity.device),
            #[cfg(not(unix))]
            device: None,
            #[cfg(unix)]
            inode: Some(self.identity.inode),
            #[cfg(not(unix))]
            inode: None,
        }
    }
}

#[derive(Clone, Debug)]
struct DirectoryIdentity {
    canonical: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl DirectoryIdentity {
    fn capture(path: &Path) -> Result<Self, ToolError> {
        let canonical = path.canonicalize().map_err(|source| ToolError::Io {
            operation: "canonicalize isolated worktree",
            path: path.to_path_buf(),
            source,
        })?;
        let metadata = std::fs::symlink_metadata(path).map_err(|source| ToolError::Io {
            operation: "inspect isolated worktree",
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(ToolError::Command(
                "isolated worktree path is not a real directory".to_owned(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Ok(Self {
                canonical,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        Ok(Self { canonical })
    }

    fn verify(&self, path: &Path) -> Result<(), ToolError> {
        let current = Self::capture(path)?;
        #[cfg(unix)]
        let unchanged = current.canonical == self.canonical
            && current.device == self.device
            && current.inode == self.inode;
        #[cfg(not(unix))]
        let unchanged = current.canonical == self.canonical;
        if !unchanged {
            return Err(ToolError::Command(
                "isolated worktree directory identity changed".to_owned(),
            ));
        }
        Ok(())
    }

    fn from_record(record: &WorktreeLeaseRecord) -> Result<Self, ToolError> {
        #[cfg(unix)]
        {
            let device = record.device.ok_or_else(|| {
                ToolError::InvalidInput("worktree lease record has no device identity".to_owned())
            })?;
            let inode = record.inode.ok_or_else(|| {
                ToolError::InvalidInput("worktree lease record has no inode identity".to_owned())
            })?;
            Ok(Self {
                canonical: record.canonical_path.clone(),
                device,
                inode,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {
            canonical: record.canonical_path.clone(),
        })
    }
}

/// Creates private detached worktrees and captures their changes.
#[derive(Clone, Debug)]
pub struct WorktreeIsolation {
    repository_root: PathBuf,
    private_root: PathBuf,
    limits: WorktreeLimits,
    registry_state: Arc<tokio::sync::OnceCell<WorktreeRegistryState>>,
    creation_failure: Arc<Mutex<Option<PathBuf>>>,
}

#[derive(Debug)]
struct WorktreeRegistryState {
    common_dir: PathBuf,
    process_gate: Arc<tokio::sync::Mutex<()>>,
}

struct WorktreeRegistryGuard {
    _process: tokio::sync::OwnedMutexGuard<()>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    _cross_process: OwnedFd,
}

fn process_worktree_registry_gate(common_dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(common_dir).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(common_dir.to_path_buf(), Arc::downgrade(&gate));
    gate
}

fn process_worktree_finalization_gate(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    gates.retain(|_, gate| gate.strong_count() != 0);
    if let Some(gate) = gates.get(path).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(path.to_path_buf(), Arc::downgrade(&gate));
    gate
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_worktree_registry_lock(common_dir: &Path) -> Result<OwnedFd, ToolError> {
    use rustix::fs::{FileType, Mode, OFlags};

    const LOCK_NAME: &str = ".rottweiler-worktree.lock";
    let directory = rustix::fs::open(
        common_dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|source| ToolError::Io {
        operation: "open Git common directory for worktree lock",
        path: common_dir.to_path_buf(),
        source: source.into(),
    })?;
    let (descriptor, created) = match rustix::fs::openat(
        &directory,
        LOCK_NAME,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    ) {
        Ok(descriptor) => (descriptor, true),
        Err(rustix::io::Errno::EXIST) => (
            rustix::fs::openat(
                &directory,
                LOCK_NAME,
                OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|source| ToolError::Io {
                operation: "open existing Git worktree lock",
                path: common_dir.join(LOCK_NAME),
                source: source.into(),
            })?,
            false,
        ),
        Err(source) => {
            return Err(ToolError::Io {
                operation: "create Git worktree lock",
                path: common_dir.join(LOCK_NAME),
                source: source.into(),
            });
        }
    };
    if created {
        rustix::fs::fchmod(&descriptor, Mode::from_raw_mode(0o600)).map_err(|source| {
            ToolError::Io {
                operation: "set Git worktree lock permissions",
                path: common_dir.join(LOCK_NAME),
                source: source.into(),
            }
        })?;
    }
    let stat = rustix::fs::fstat(&descriptor).map_err(|source| ToolError::Io {
        operation: "inspect Git worktree lock",
        path: common_dir.join(LOCK_NAME),
        source: source.into(),
    })?;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || stat.st_nlink != 1
        || (Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o777) != 0o600
    {
        return Err(ToolError::Command(
            "Git worktree lock must be one owner-private regular file".to_owned(),
        ));
    }
    Ok(descriptor)
}

impl WorktreeIsolation {
    /// Validates an exact repository root and a private storage root outside it.
    ///
    /// # Errors
    ///
    /// Returns an error when either root is unsafe, the repository is not an
    /// exact Git top level, Git is unavailable, or the operation is cancelled.
    pub async fn new(
        repository_root: impl AsRef<Path>,
        private_root: impl AsRef<Path>,
        limits: WorktreeLimits,
        cancellation: CancellationToken,
    ) -> Result<Self, ToolError> {
        let repository_root = canonical_directory(repository_root.as_ref(), "repository root")?;
        validate_limits(limits)?;
        let supplied_private = absolute_without_parent_components(private_root.as_ref())?;
        let projected_private = projected_canonical_path(&supplied_private)?;
        if projected_private.starts_with(&repository_root)
            || repository_root.starts_with(&projected_private)
        {
            return Err(ToolError::InvalidInput(
                "private worktree storage must be outside the repository".to_owned(),
            ));
        }
        std::fs::create_dir_all(private_root.as_ref()).map_err(|source| ToolError::Io {
            operation: "create worktree storage root",
            path: private_root.as_ref().to_path_buf(),
            source,
        })?;
        let storage_root = canonical_directory(private_root.as_ref(), "worktree storage root")?;
        if storage_root.starts_with(&repository_root) || repository_root.starts_with(&storage_root)
        {
            return Err(ToolError::InvalidInput(
                "private worktree storage must be outside the repository".to_owned(),
            ));
        }
        let private_root = storage_root.join(".rottweiler-worktrees");
        let private_root_existed = private_root.exists();
        std::fs::create_dir(&private_root)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|source| ToolError::Io {
                operation: "create private worktree directory",
                path: private_root.clone(),
                source,
            })?;
        if private_root_existed {
            require_private_permissions(&private_root)?;
        } else {
            set_private_permissions(&private_root)?;
        }
        let private_root = canonical_directory(&private_root, "private worktree root")?;
        if private_root.starts_with(&repository_root) || repository_root.starts_with(&private_root)
        {
            return Err(ToolError::InvalidInput(
                "private worktree storage must be outside the repository".to_owned(),
            ));
        }
        let output = run_git(
            &repository_root,
            [
                OsString::from("rev-parse"),
                OsString::from("--show-toplevel"),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("discover repository root", &output)?;
        let reported = path_from_stdout(&output.stdout, "repository root")?;
        let reported = canonical_directory(&reported, "reported repository root")?;
        if reported != repository_root {
            return Err(ToolError::InvalidInput(format!(
                "repository path must be the exact git top level: {}",
                reported.display()
            )));
        }
        Ok(Self {
            repository_root,
            private_root,
            limits,
            registry_state: Arc::new(tokio::sync::OnceCell::new()),
            creation_failure: Arc::new(Mutex::new(None)),
        })
    }

    #[must_use]
    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    #[must_use]
    pub fn private_root(&self) -> &Path {
        &self.private_root
    }

    async fn lock_registry(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<WorktreeRegistryGuard, ToolError> {
        let state = self
            .registry_state
            .get_or_try_init(|| async {
                let common_dir = git_common_directory(&self.repository_root, cancellation).await?;
                Ok::<_, ToolError>(WorktreeRegistryState {
                    process_gate: process_worktree_registry_gate(&common_dir),
                    common_dir,
                })
            })
            .await?;
        let process = tokio::select! {
            guard = Arc::clone(&state.process_gate).lock_owned() => guard,
            () = cancellation.cancelled() => return Err(ToolError::Cancelled),
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let cross_process = {
            let descriptor = open_worktree_registry_lock(&state.common_dir)?;
            loop {
                match rustix::fs::flock(
                    &descriptor,
                    rustix::fs::FlockOperation::NonBlockingLockExclusive,
                ) {
                    Ok(()) => break descriptor,
                    Err(rustix::io::Errno::AGAIN) => {
                        tokio::select! {
                            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {},
                            () = cancellation.cancelled() => return Err(ToolError::Cancelled),
                        }
                    }
                    Err(source) => {
                        return Err(ToolError::Io {
                            operation: "lock Git worktree registry",
                            path: state.common_dir.join(".rottweiler-worktree.lock"),
                            source: source.into(),
                        });
                    }
                }
            }
        };
        Ok(WorktreeRegistryGuard {
            _process: process,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            _cross_process: cross_process,
        })
    }

    /// Rebinds an identity-pinned worktree from host-private session metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the path, directory identity, repository ownership,
    /// base commit, private-root containment, or cancellation check fails.
    pub async fn rebind(
        &self,
        record: &WorktreeLeaseRecord,
        cancellation: CancellationToken,
    ) -> Result<WorktreeLease, ToolError> {
        let lease = self.lease_from_record(record)?;
        self.verify_lease(&lease, &cancellation).await?;
        Ok(lease)
    }

    /// Force-removes one identity-pinned worktree whose durable parent spawn was rewound away.
    /// This is an explicit host recovery operation, never a model-facing cleanup shortcut.
    /// The exact private lease is validated before Git may remove changed bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable path/base/identity is invalid, the directory was
    /// replaced, the worktree belongs to another repository, Git removal fails, or cancellation
    /// is requested. Validation failure leaves the path untouched.
    pub async fn discard_tombstoned(
        &self,
        record: &WorktreeLeaseRecord,
        cancellation: CancellationToken,
    ) -> Result<(), ToolError> {
        let lease = self.lease_from_record(record)?;
        let _registry = self.lock_registry(&cancellation).await?;
        match std::fs::symlink_metadata(&lease.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.ensure_worktree_unregistered(&lease.path, &cancellation)
                    .await?;
                if std::fs::symlink_metadata(&lease.path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    return Ok(());
                }
                return Err(ToolError::Command(
                    "tombstoned worktree path reappeared during cleanup".to_owned(),
                ));
            }
            Err(source) => {
                return Err(ToolError::Io {
                    operation: "inspect tombstoned worktree path",
                    path: lease.path.clone(),
                    source,
                });
            }
            Ok(_) => {}
        }
        self.verify_lease(&lease, &cancellation).await?;
        let output = run_git(
            &self.repository_root,
            [
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--force"),
                OsString::from("--"),
                lease.path.as_os_str().to_os_string(),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("discard tombstoned isolated worktree", &output)?;
        Ok(())
    }

    /// Captures all tracked and untracked changes without touching the parent tree.
    ///
    /// # Errors
    ///
    /// Returns an error when the lease identity or commit changed, Git fails,
    /// cancellation is requested, or a return bound is exceeded.
    pub async fn collect(
        &self,
        lease: &WorktreeLease,
        final_text: &str,
        usage: Usage,
        cost: Cost,
        cancellation: CancellationToken,
    ) -> Result<ChildReturnArtifact, ToolError> {
        self.verify_lease(lease, &cancellation).await?;
        let names = run_git(
            &lease.path,
            [
                OsString::from("diff"),
                OsString::from("--name-status"),
                OsString::from("-z"),
                OsString::from("--no-renames"),
                OsString::from(&lease.base_commit),
                OsString::from("--"),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("list isolated worktree changes", &names)?;
        let mut touched_files = parse_touched_files(&names.stdout, self.limits.max_touched_files)?;

        let untracked = run_git(
            &lease.path,
            [
                OsString::from("ls-files"),
                OsString::from("--others"),
                OsString::from("--exclude-standard"),
                OsString::from("-z"),
                OsString::from("--"),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("list isolated untracked files", &untracked)?;
        let untracked = parse_untracked_paths(&untracked.stdout)?;
        if touched_files.len().saturating_add(untracked.len()) > self.limits.max_touched_files {
            return Err(ToolError::SizeLimit {
                limit: self.limits.max_touched_files,
            });
        }
        touched_files.extend(untracked.iter().map(|path| TouchedFile {
            path: path.to_string_lossy().into_owned(),
            status: TouchedFileStatus::Added,
        }));
        touched_files.sort_by(|left, right| left.path.cmp(&right.path));

        let patch = run_git(
            &lease.path,
            [
                OsString::from("diff"),
                OsString::from("--binary"),
                OsString::from("--full-index"),
                OsString::from("--no-ext-diff"),
                OsString::from("--no-textconv"),
                OsString::from("--no-renames"),
                OsString::from(&lease.base_commit),
                OsString::from("--"),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("capture isolated worktree diff", &patch)?;
        let patch_bytes = append_untracked_patches(
            &lease.path,
            patch.stdout,
            untracked,
            self.limits.max_diff_bytes,
            &cancellation,
        )
        .await?;
        if patch_bytes.len() > self.limits.max_diff_bytes {
            return Err(ToolError::SizeLimit {
                limit: self.limits.max_diff_bytes,
            });
        }
        let unified_diff = String::from_utf8(patch_bytes)
            .map_err(|_| ToolError::Output("git emitted a non-UTF-8 diff artifact".to_owned()))?;
        let diff = if touched_files.is_empty() {
            None
        } else {
            let id = artifact_id(&lease.base_commit, &touched_files, &unified_diff)?;
            Some(DiffArtifact {
                id,
                base_commit: lease.base_commit.clone(),
                touched_files: touched_files.clone(),
                unified_diff,
            })
        };
        let (final_text, final_text_truncated) =
            truncate_utf8(final_text, self.limits.max_final_text_bytes);
        Ok(ChildReturnArtifact {
            final_text,
            final_text_truncated,
            touched_files,
            diff,
            usage,
            cost,
        })
    }

    /// Removes only a byte-clean, identity-pinned worktree at its original commit.
    /// Returns `false` when changes exist; it never force-deletes them.
    ///
    /// # Errors
    ///
    /// Returns an error when identity verification, Git cleanup, or cancellation fails.
    pub async fn cleanup_if_untouched(
        &self,
        lease: &WorktreeLease,
        cancellation: CancellationToken,
    ) -> Result<bool, ToolError> {
        let _registry = self.lock_registry(&cancellation).await?;
        self.verify_lease(lease, &cancellation).await?;
        let status = run_git(
            &lease.path,
            [
                OsString::from("status"),
                OsString::from("--porcelain=v1"),
                OsString::from("-z"),
                OsString::from("--untracked-files=all"),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("inspect isolated worktree before cleanup", &status)?;
        if !status.stdout.is_empty() {
            return Ok(false);
        }
        let output = run_git(
            &self.repository_root,
            [
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--"),
                lease.path.as_os_str().to_os_string(),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("remove untouched isolated worktree", &output)?;
        Ok(true)
    }

    /// Removes a changed worktree only when its current bytes exactly match a
    /// self-contained artifact that has already been handed to the parent.
    /// Returns `false` if the child changed anything after capture.
    ///
    /// # Errors
    ///
    /// Returns an error when the lease/artifact is invalid, recapture fails,
    /// Git cleanup fails, or cancellation is requested.
    pub async fn finalize_captured(
        &self,
        lease: &WorktreeLease,
        artifact: &DiffArtifact,
        cancellation: CancellationToken,
    ) -> Result<bool, ToolError> {
        verify_artifact(artifact)?;
        if artifact.base_commit != lease.base_commit {
            return Err(ToolError::InvalidInput(
                "diff artifact belongs to a different worktree base".to_owned(),
            ));
        }
        let (usage, cost) = empty_accounting();
        let current = self
            .collect(lease, "", usage.clone(), cost.clone(), cancellation.clone())
            .await?;
        if current.diff.as_ref() != Some(artifact) {
            return Ok(false);
        }
        #[cfg(test)]
        run_finalize_after_capture_test_hook(&lease.path);
        let _finalization = tokio::select! {
            guard = Arc::clone(&lease.finalization_gate).lock_owned() => guard,
            () = cancellation.cancelled() => return Err(ToolError::Cancelled),
        };
        let _processes = begin_worktree_finalization(&lease.path)?;
        let _registry = self.lock_registry(&cancellation).await?;
        self.verify_lease(lease, &cancellation).await?;
        let current = self
            .collect(lease, "", usage, cost, cancellation.clone())
            .await?;
        if current.diff.as_ref() != Some(artifact) {
            return Err(ToolError::WorktreeChangedAfterCapture(lease.path.clone()));
        }
        let output = run_git(
            &self.repository_root,
            [
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--force"),
                OsString::from("--"),
                lease.path.as_os_str().to_os_string(),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("finalize captured isolated worktree", &output)?;
        Ok(true)
    }

    async fn verify_lease(
        &self,
        lease: &WorktreeLease,
        cancellation: &CancellationToken,
    ) -> Result<(), ToolError> {
        cancellation.check()?;
        lease.identity.verify(&lease.path)?;
        if !lease.identity.canonical.starts_with(&self.private_root) {
            return Err(ToolError::Command(
                "isolated worktree no longer belongs to private storage".to_owned(),
            ));
        }
        let head = run_git(
            &lease.path,
            [
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from("HEAD^{commit}"),
            ],
            None,
            cancellation,
        )
        .await?;
        require_success("verify isolated worktree commit", &head)?;
        if text_stdout(&head.stdout, "worktree commit")? != lease.base_commit {
            return Err(ToolError::Command(
                "isolated worktree HEAD changed after creation".to_owned(),
            ));
        }
        let parent_common = git_common_directory(&self.repository_root, cancellation).await?;
        let lease_common = git_common_directory(&lease.path, cancellation).await?;
        if parent_common != lease_common {
            return Err(ToolError::Command(
                "isolated worktree belongs to a different repository".to_owned(),
            ));
        }
        Ok(())
    }

    fn lease_from_record(&self, record: &WorktreeLeaseRecord) -> Result<WorktreeLease, ToolError> {
        validate_oid(&record.base_commit)?;
        let exact_private_child = record.path.is_absolute()
            && record.canonical_path.is_absolute()
            && record.path == record.canonical_path
            && record.path.parent() == Some(self.private_root.as_path())
            && record.canonical_path.parent() == Some(self.private_root.as_path());
        if !exact_private_child {
            return Err(ToolError::InvalidInput(
                "worktree lease record is not an exact private lease path".to_owned(),
            ));
        }
        Ok(WorktreeLease {
            finalization_gate: process_worktree_finalization_gate(&record.path),
            path: record.path.clone(),
            base_commit: record.base_commit.clone(),
            identity: DirectoryIdentity::from_record(record)?,
        })
    }

    async fn ensure_worktree_unregistered(
        &self,
        expected_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), ToolError> {
        cancellation.check()?;
        let output = run_git(
            &self.repository_root,
            [
                OsString::from("worktree"),
                OsString::from("list"),
                OsString::from("--porcelain"),
                OsString::from("-z"),
            ],
            None,
            cancellation,
        )
        .await?;
        require_success(
            "inspect registered worktrees after tombstone cleanup",
            &output,
        )?;
        for field in output.stdout.split(|byte| *byte == 0) {
            let Some(path) = field.strip_prefix(b"worktree ") else {
                continue;
            };
            let path = std::str::from_utf8(path).map_err(|_| {
                ToolError::Output("git emitted a non-UTF-8 registered worktree path".to_owned())
            })?;
            if Path::new(path) == expected_path {
                return Err(ToolError::Command(
                    "tombstoned worktree path is absent but remains registered with Git".to_owned(),
                ));
            }
        }
        cancellation.check()
    }
}

#[cfg(test)]
type FinalizeAfterCaptureTestHooks = HashMap<PathBuf, (PathBuf, Vec<u8>)>;

#[cfg(test)]
fn finalize_after_capture_test_hooks() -> &'static Mutex<FinalizeAfterCaptureTestHooks> {
    static HOOKS: OnceLock<Mutex<FinalizeAfterCaptureTestHooks>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn install_finalize_after_capture_test_write(lease_path: &Path, target: PathBuf, content: Vec<u8>) {
    finalize_after_capture_test_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(lease_path.to_path_buf(), (target, content));
}

#[cfg(test)]
fn run_finalize_after_capture_test_hook(lease_path: &Path) {
    let hook = finalize_after_capture_test_hooks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(lease_path);
    if let Some((target, content)) = hook {
        std::fs::write(target, content)
            .unwrap_or_else(|error| panic!("finalization race test write: {error}"));
    }
}

mod apply;
pub use apply::{
    ApplyWorktreeDiffInput, ApplyWorktreeDiffTool, DiffArtifactAuthority,
    SessionDiffArtifactAuthority,
};

mod creation;
pub use creation::WorktreeAllocation;

mod git;
use git::{
    append_untracked_patches, bounded_diagnostic, git_common_directory, git_index_path,
    path_from_stdout, require_success, run_git, run_git_with_paths, text_stdout, truncate_utf8,
    validate_repository_root,
};

mod validation;
use validation::{
    absolute_without_parent_components, artifact_id, canonical_directory, empty_accounting,
    parse_touched_files, parse_untracked_paths, projected_canonical_path,
    require_private_permissions, set_private_permissions, validate_artifact_reference_id,
    validate_limits, validate_oid, validate_relative_path, verify_artifact,
};

#[cfg(test)]
mod tests;
