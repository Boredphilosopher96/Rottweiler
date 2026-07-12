//! Git worktree isolation and explicit diff-artifact application.
//!
//! Child sessions never merge into their parent. They run under a randomized,
//! private path outside the repository and return a bounded artifact. The only
//! mutation boundary is [`ApplyWorktreeDiffTool`], which participates in the
//! ordinary permission and checkpoint pipeline.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::OwnedFd;

use async_trait::async_trait;
use rw_types::{
    Cost, DiffArtifact, SessionId, ToolCapability, TouchedFile, TouchedFileStatus, Usage,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::bash::audited_system_git;
use crate::registry::{
    CancellationToken, CapabilityManifest, MutationScope, Tool, ToolContext, ToolDescriptor,
    ToolError, ToolResult, WorkspaceBinding, input_schema, parse_input,
};

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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    repository_common_dir: PathBuf,
    private_root: PathBuf,
    limits: WorktreeLimits,
    registry_gate: Arc<tokio::sync::Mutex<()>>,
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
        let repository_common_dir = git_common_directory(&repository_root, &cancellation).await?;
        let registry_gate = process_worktree_registry_gate(&repository_common_dir);
        Ok(Self {
            repository_root,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            repository_common_dir,
            private_root,
            limits,
            registry_gate,
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
        let process = tokio::select! {
            guard = Arc::clone(&self.registry_gate).lock_owned() => guard,
            () = cancellation.cancelled() => return Err(ToolError::Cancelled),
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let cross_process = {
            let descriptor = open_worktree_registry_lock(&self.repository_common_dir)?;
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
                            path: self.repository_common_dir.join(".rottweiler-worktree.lock"),
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

    /// Creates a detached worktree at the repository's current `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot resolve `HEAD`, private allocation or
    /// worktree creation fails, or cancellation is requested.
    pub async fn create(
        &self,
        cancellation: CancellationToken,
    ) -> Result<WorktreeLease, ToolError> {
        // `git worktree add` mutates the repository's shared worktree
        // registry. Git does not provide one transaction spanning its
        // discovery, allocation, registration, and our failure cleanup, so
        // concurrent adds can transiently reject one otherwise independent
        // child. Serialize only this short allocation boundary; child turns
        // and worktree contents remain fully parallel after their leases exist.
        let _registry = self.lock_registry(&cancellation).await?;
        cancellation.check()?;
        let head = run_git(
            &self.repository_root,
            [
                OsString::from("rev-parse"),
                OsString::from("--verify"),
                OsString::from("HEAD^{commit}"),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("resolve worktree base commit", &head)?;
        let base_commit = text_stdout(&head.stdout, "base commit")?;
        validate_oid(&base_commit)?;

        let temporary = tempfile::Builder::new()
            .prefix("rw-agent-")
            .tempdir_in(&self.private_root)
            .map_err(|source| ToolError::Io {
                operation: "allocate private worktree path",
                path: self.private_root.clone(),
                source,
            })?;
        let path = temporary.path().to_path_buf();
        std::fs::remove_dir(&path).map_err(|source| ToolError::Io {
            operation: "prepare private worktree path",
            path: path.clone(),
            source,
        })?;
        let _preserved = temporary.keep();
        let output = run_git(
            &self.repository_root,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                OsString::from("--"),
                path.as_os_str().to_os_string(),
                OsString::from(&base_commit),
            ],
            None,
            &cancellation,
        )
        .await;
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                self.cleanup_partial_creation(&path).await;
                return Err(error);
            }
        };
        if let Err(error) = require_success("create isolated worktree", &output) {
            self.cleanup_partial_creation(&path).await;
            return Err(error);
        }
        if let Err(error) = set_private_permissions(&path) {
            self.cleanup_partial_creation(&path).await;
            return Err(error);
        }
        let identity = match DirectoryIdentity::capture(&path) {
            Ok(identity) => identity,
            Err(error) => {
                self.cleanup_partial_creation(&path).await;
                return Err(error);
            }
        };
        if !identity.canonical.starts_with(&self.private_root) {
            self.cleanup_partial_creation(&path).await;
            return Err(ToolError::Command(
                "git created the worktree outside private storage".to_owned(),
            ));
        }
        Ok(WorktreeLease {
            path,
            base_commit,
            identity,
        })
    }

    async fn cleanup_partial_creation(&self, path: &Path) {
        let cancellation = CancellationToken::default();
        let _ = run_git(
            &self.repository_root,
            [
                OsString::from("worktree"),
                OsString::from("remove"),
                OsString::from("--force"),
                OsString::from("--"),
                path.as_os_str().to_os_string(),
            ],
            None,
            &cancellation,
        )
        .await;
        let safe_directory = std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.file_type().is_dir()
                && !metadata.file_type().is_symlink()
                && path
                    .canonicalize()
                    .is_ok_and(|canonical| canonical.starts_with(&self.private_root))
        });
        if safe_directory {
            let _ = std::fs::remove_dir_all(path);
        }
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
            .collect(lease, "", usage, cost, cancellation.clone())
            .await?;
        if current.diff.as_ref() != Some(artifact) {
            return Ok(false);
        }
        let _registry = self.lock_registry(&cancellation).await?;
        self.verify_lease(lease, &cancellation).await?;
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

async fn append_untracked_patches(
    root: &Path,
    mut patch: Vec<u8>,
    paths: Vec<PathBuf>,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, ToolError> {
    for path in paths {
        let added = run_git(
            root,
            [
                OsString::from("diff"),
                OsString::from("--no-index"),
                OsString::from("--binary"),
                OsString::from("--full-index"),
                OsString::from("--no-ext-diff"),
                OsString::from("--no-textconv"),
                OsString::from("--"),
                OsString::from("/dev/null"),
                path.as_os_str().to_os_string(),
            ],
            None,
            cancellation,
        )
        .await?;
        if added.status.code() != Some(1) || added.stdout.is_empty() {
            return Err(ToolError::Command(format!(
                "capture isolated untracked file failed: {}",
                bounded_diagnostic(&added)
            )));
        }
        patch.extend_from_slice(&added.stdout);
        if patch.len() > limit {
            return Err(ToolError::SizeLimit { limit });
        }
    }
    Ok(patch)
}

async fn git_common_directory(
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<PathBuf, ToolError> {
    let output = run_git(
        root,
        [
            OsString::from("rev-parse"),
            OsString::from("--git-common-dir"),
        ],
        None,
        cancellation,
    )
    .await?;
    require_success("discover git common directory", &output)?;
    let path = path_from_stdout(&output.stdout, "git common directory")?;
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.canonicalize().map_err(|source| ToolError::Io {
        operation: "canonicalize git common directory",
        path,
        source,
    })
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyWorktreeDiffInput {
    /// The complete durable artifact returned by the isolated child.
    /// Exactly one of `artifact` and `artifact_id` must be supplied.
    #[serde(default)]
    pub artifact: Option<DiffArtifact>,
    /// A compact reference to an artifact retained by the authenticated parent session.
    /// Exactly one of `artifact` and `artifact_id` must be supplied.
    #[serde(default)]
    pub artifact_id: Option<String>,
}

/// Session-scoped provenance check for durable child artifacts.
pub trait DiffArtifactAuthority: Send + Sync {
    /// Resolves a full artifact only after it was durably recorded for the parent session.
    fn resolve(&self, parent_session: &SessionId, artifact_id: &str) -> Option<DiffArtifact>;
}

/// Rebuildable authority populated only from durable `SubagentFinished` records.
#[derive(Debug, Default)]
pub struct SessionDiffArtifactAuthority {
    grants: Mutex<HashMap<(SessionId, String), DiffArtifact>>,
}

impl SessionDiffArtifactAuthority {
    /// Validates an artifact without granting authority to apply it.
    ///
    /// # Errors
    ///
    /// Returns when the digest, base commit, or touched manifest is malformed.
    pub fn validate(&self, artifact: &DiffArtifact) -> Result<(), ToolError> {
        verify_artifact(artifact)
    }

    /// Grants one exact artifact after its durable child-result event commits.
    /// Calling this while rebuilding a session from the same durable records is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error when the artifact digest, commit, or manifest is invalid.
    pub fn record_durable(
        &self,
        parent_session: SessionId,
        artifact: &DiffArtifact,
    ) -> Result<(), ToolError> {
        verify_artifact(artifact)?;
        let key = (parent_session, artifact.id.clone());
        let mut grants = self
            .grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = grants.get(&key) {
            if existing != artifact {
                return Err(ToolError::InvalidInput(
                    "worktree diff artifact id was already bound to different contents".to_owned(),
                ));
            }
            return Ok(());
        }
        grants.insert(key, artifact.clone());
        Ok(())
    }

    /// Drops every in-memory grant when a parent session is permanently deleted.
    pub fn revoke_session(&self, parent_session: &SessionId) {
        self.grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(session, _), _| session != parent_session);
    }
}

impl DiffArtifactAuthority for SessionDiffArtifactAuthority {
    fn resolve(&self, parent_session: &SessionId, artifact_id: &str) -> Option<DiffArtifact> {
        self.grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(parent_session.clone(), artifact_id.to_owned()))
            .cloned()
    }
}

/// The only supported merge-back boundary. Core checkpoints its exact manifest
/// before execution because this tool declares filesystem mutation.
#[derive(Clone)]
pub struct ApplyWorktreeDiffTool {
    authority: Arc<dyn DiffArtifactAuthority>,
    apply_lock: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for ApplyWorktreeDiffTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ApplyWorktreeDiffTool").finish()
    }
}

impl ApplyWorktreeDiffTool {
    #[must_use]
    pub fn new(authority: Arc<dyn DiffArtifactAuthority>) -> Self {
        Self {
            authority,
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn resolve_input(
        &self,
        context: &ToolContext,
        input: ApplyWorktreeDiffInput,
    ) -> Result<DiffArtifact, ToolError> {
        let session = context.session_id().ok_or_else(|| {
            ToolError::InvalidInput(
                "apply_worktree_diff requires an authenticated parent session".to_owned(),
            )
        })?;
        let (artifact_id, supplied) = match (input.artifact, input.artifact_id) {
            (Some(artifact), None) => {
                verify_artifact(&artifact)?;
                (artifact.id.clone(), Some(artifact))
            }
            (None, Some(artifact_id)) => {
                validate_artifact_reference_id(&artifact_id)?;
                (artifact_id, None)
            }
            (Some(_), Some(_)) | (None, None) => {
                return Err(ToolError::InvalidInput(
                    "apply_worktree_diff requires exactly one of artifact or artifact_id"
                        .to_owned(),
                ));
            }
        };
        let resolved = self
            .authority
            .resolve(session, &artifact_id)
            .ok_or_else(|| {
                ToolError::InvalidInput(
                    "worktree diff was not durably produced for this parent session".to_owned(),
                )
            })?;
        verify_artifact(&resolved)?;
        if supplied
            .as_ref()
            .is_some_and(|artifact| artifact != &resolved)
        {
            return Err(ToolError::InvalidInput(
                "worktree diff was not durably produced for this parent session".to_owned(),
            ));
        }
        Ok(resolved)
    }
}

fn apply_worktree_diff_input_schema() -> Value {
    let mut schema = input_schema::<ApplyWorktreeDiffInput>();
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "oneOf".to_owned(),
            json!([
                { "required": ["artifact"] },
                { "required": ["artifact_id"] }
            ]),
        );
    }
    schema
}

#[async_trait]
impl Tool for ApplyWorktreeDiffTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "apply_worktree_diff".to_owned(),
            description: "Explicitly apply one isolated child diff with git 3-way conflict checks."
                .to_owned(),
            input_schema: apply_worktree_diff_input_schema(),
            capabilities: CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
                ToolCapability::Execute,
            ]),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    fn mutation_scope(&self, input: &Value) -> MutationScope {
        // The artifact crosses a durable/model-visible boundary. Even with its
        // integrity digest, it is not authenticated as engine-created at this
        // point, so checkpoint the full workspace before parsing it with Git.
        let _ = input;
        MutationScope::OpaqueWorkspace
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let input: ApplyWorktreeDiffInput = parse_input(input)?;
        let artifact = self.resolve_input(context, input)?;
        context.cancellation.check()?;
        let _guard = self.apply_lock.lock().await;
        validate_repository_root(context.workspace_root(), &context.cancellation).await?;
        validate_patch_manifest(context.workspace_root(), &artifact, &context.cancellation).await?;

        let index = git_index_path(context.workspace_root(), &context.cancellation).await?;
        let temporary_index = tempfile::NamedTempFile::new().map_err(|source| ToolError::Io {
            operation: "allocate isolated git apply index",
            path: std::env::temp_dir(),
            source,
        })?;
        std::fs::copy(&index, temporary_index.path()).map_err(|source| ToolError::Io {
            operation: "copy git index for isolated preflight",
            path: index,
            source,
        })?;
        let temporary_worktree = tempfile::tempdir().map_err(|source| ToolError::Io {
            operation: "allocate isolated git apply worktree",
            path: std::env::temp_dir(),
            source,
        })?;
        let check = run_git_with_paths(
            context.workspace_root(),
            [
                OsString::from("apply"),
                OsString::from("--3way"),
                OsString::from("--cached"),
                OsString::from("--binary"),
                OsString::from("--whitespace=nowarn"),
                OsString::from("-"),
            ],
            Some(artifact.unified_diff.as_bytes()),
            &context.cancellation,
            Some(temporary_index.path()),
            Some(temporary_worktree.path()),
        )
        .await?;
        if !check.status.success() {
            return Err(ToolError::Command(format!(
                "worktree diff conflict; parent tree was not changed: {}",
                bounded_diagnostic(&check)
            )));
        }
        let apply = run_git(
            context.workspace_root(),
            [
                OsString::from("apply"),
                OsString::from("--3way"),
                OsString::from("--binary"),
                OsString::from("--whitespace=nowarn"),
                OsString::from("-"),
            ],
            Some(artifact.unified_diff.as_bytes()),
            &context.cancellation,
        )
        .await?;
        if !apply.status.success() {
            return Err(ToolError::Command(format!(
                "worktree diff failed after checkpointed preflight: {}",
                bounded_diagnostic(&apply)
            )));
        }
        Ok(ToolResult::new(
            format!(
                "Applied isolated diff {} to {} file(s).",
                artifact.id,
                artifact.touched_files.len()
            ),
            json!({
                "artifact_id": artifact.id,
                "base_commit": artifact.base_commit,
                "touched_files": artifact.touched_files,
            }),
        ))
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<crate::ApprovalPreview>, ToolError> {
        let input: ApplyWorktreeDiffInput = parse_input(input.clone())?;
        let artifact = self.resolve_input(context, input)?;
        let after = serde_json::to_vec_pretty(&artifact)
            .map_err(|source| ToolError::Output(source.to_string()))?;
        Ok(Some(crate::ApprovalPreview {
            path: PathBuf::from(format!(".rottweiler/diff-artifacts/{}.json", artifact.id)),
            before: None,
            after,
        }))
    }
}

async fn validate_patch_manifest(
    root: &Path,
    artifact: &DiffArtifact,
    cancellation: &CancellationToken,
) -> Result<(), ToolError> {
    let output = run_git(
        root,
        [
            OsString::from("apply"),
            OsString::from("--numstat"),
            OsString::from("-z"),
            OsString::from("--binary"),
            OsString::from("-"),
        ],
        Some(artifact.unified_diff.as_bytes()),
        cancellation,
    )
    .await?;
    require_success("inspect worktree diff manifest", &output)?;
    let mut patch_paths = BTreeSet::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record)
            .map_err(|_| ToolError::Output("git emitted a non-UTF-8 patch path".to_owned()))?;
        let mut fields = record.splitn(3, '\t');
        let _added = fields.next();
        let _deleted = fields.next();
        let path = fields.next().ok_or_else(|| {
            ToolError::Output("git emitted malformed patch statistics".to_owned())
        })?;
        validate_relative_path(Path::new(path))?;
        patch_paths.insert(path.to_owned());
    }
    let manifest_paths = artifact
        .touched_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    if patch_paths != manifest_paths {
        return Err(ToolError::InvalidInput(
            "worktree diff manifest does not match the patch paths".to_owned(),
        ));
    }
    Ok(())
}

async fn git_index_path(
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<PathBuf, ToolError> {
    let output = run_git(
        root,
        [
            OsString::from("rev-parse"),
            OsString::from("--git-path"),
            OsString::from("index"),
        ],
        None,
        cancellation,
    )
    .await?;
    require_success("discover git index", &output)?;
    let path = path_from_stdout(&output.stdout, "git index")?;
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.canonicalize().map_err(|source| ToolError::Io {
        operation: "canonicalize git index",
        path,
        source,
    })
}

async fn validate_repository_root(
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<(), ToolError> {
    let canonical = canonical_directory(root, "apply repository root")?;
    if canonical != root {
        return Err(ToolError::Command(
            "apply workspace root changed after context creation".to_owned(),
        ));
    }
    let output = run_git(
        root,
        [
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ],
        None,
        cancellation,
    )
    .await?;
    require_success("verify apply repository", &output)?;
    let reported = canonical_directory(
        &path_from_stdout(&output.stdout, "repository root")?,
        "repository root",
    )?;
    if reported != root {
        return Err(ToolError::Command(
            "apply workspace is not the exact repository root".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct GitOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_git<I>(
    cwd: &Path,
    args: I,
    stdin: Option<&[u8]>,
    cancellation: &CancellationToken,
) -> Result<GitOutput, ToolError>
where
    I: IntoIterator<Item = OsString>,
{
    run_git_with_paths(cwd, args, stdin, cancellation, None, None).await
}

async fn run_git_with_paths<I>(
    cwd: &Path,
    args: I,
    stdin: Option<&[u8]>,
    cancellation: &CancellationToken,
    index_file: Option<&Path>,
    work_tree: Option<&Path>,
) -> Result<GitOutput, ToolError>
where
    I: IntoIterator<Item = OsString>,
{
    let configured = run_git_raw_with_paths(
        cwd,
        [
            OsString::from("config"),
            OsString::from("--local"),
            OsString::from("--name-only"),
            OsString::from("--get-regexp"),
            OsString::from(r"^filter\..*\.(clean|smudge|process|required)$"),
        ],
        None,
        cancellation,
        index_file,
        work_tree,
    )
    .await?;
    if !configured.status.success() && configured.status.code() != Some(1) {
        return Err(ToolError::Command(format!(
            "inspect repository filter configuration failed: {}",
            bounded_diagnostic(&configured)
        )));
    }
    let mut drivers = BTreeSet::new();
    for key in String::from_utf8_lossy(&configured.stdout).lines() {
        let Some(body) = key.strip_prefix("filter.") else {
            continue;
        };
        let Some((driver, _property)) = body.rsplit_once('.') else {
            continue;
        };
        if !driver.is_empty() {
            drivers.insert(driver.to_owned());
        }
    }
    let mut safe_args = Vec::new();
    for driver in drivers {
        for (property, value) in [
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            safe_args.push(OsString::from("-c"));
            safe_args.push(OsString::from(format!(
                "filter.{driver}.{property}={value}"
            )));
        }
    }
    safe_args.extend(args);
    run_git_raw_with_paths(cwd, safe_args, stdin, cancellation, index_file, work_tree).await
}

async fn run_git_raw_with_paths<I>(
    cwd: &Path,
    args: I,
    stdin: Option<&[u8]>,
    cancellation: &CancellationToken,
    index_file: Option<&Path>,
    work_tree: Option<&Path>,
) -> Result<GitOutput, ToolError>
where
    I: IntoIterator<Item = OsString>,
{
    cancellation.check()?;
    let git = audited_system_git().ok_or_else(|| {
        ToolError::Command("no audited root-owned system git executable is available".to_owned())
    })?;
    let mut command = configured_git_command(git, cwd, args, index_file, work_tree);
    let mut child = command
        .spawn()
        .map_err(|source| ToolError::Command(format!("could not spawn audited git: {source}")))?;
    let child_id = child.id();
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| ToolError::Command("git stdin pipe was unavailable".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Command("git stdout pipe was unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Command("git stderr pipe was unavailable".to_owned()))?;
    let stdout_task = tokio::spawn(read_bounded(stdout, MAX_GIT_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(read_bounded(stderr, DIAGNOSTIC_LIMIT));
    let write = async {
        if let Some(bytes) = stdin {
            child_stdin.write_all(bytes).await.map_err(|source| {
                ToolError::Command(format!("could not write git input: {source}"))
            })?;
        }
        child_stdin
            .shutdown()
            .await
            .map_err(|source| ToolError::Command(format!("could not close git input: {source}")))
    };
    let write_result = tokio::select! {
        result = write => result,
        () = cancellation.cancelled() => Err(ToolError::Cancelled),
    };
    drop(child_stdin);
    if let Err(error) = write_result {
        terminate_process_group(child_id);
        let _ = child.start_kill();
        let _ = child.wait().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        return Err(error);
    }
    let status = tokio::select! {
        status = child.wait() => status.map_err(|source| ToolError::Command(format!("could not wait for git: {source}")))?,
        () = cancellation.cancelled() => {
            terminate_process_group(child_id);
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ToolError::Cancelled);
        }
    };
    let (stdout, stdout_truncated) = stdout_task
        .await
        .map_err(|source| ToolError::Output(source.to_string()))??;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .map_err(|source| ToolError::Output(source.to_string()))??;
    if stdout_truncated || stderr_truncated {
        return Err(ToolError::SizeLimit {
            limit: if stdout_truncated {
                MAX_GIT_OUTPUT_BYTES
            } else {
                DIAGNOSTIC_LIMIT
            },
        });
    }
    Ok(GitOutput {
        status,
        stdout,
        stderr,
    })
}

fn configured_git_command<I>(
    git: &Path,
    cwd: &Path,
    args: I,
    index_file: Option<&Path>,
    work_tree: Option<&Path>,
) -> Command
where
    I: IntoIterator<Item = OsString>,
{
    let mut command = Command::new(git);
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("diff.external=")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", OsStr::new("/dev/null"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(index_file) = index_file {
        command.env("GIT_INDEX_FILE", index_file);
    }
    if let Some(work_tree) = work_tree {
        command.env("GIT_WORK_TREE", work_tree);
    }
    #[cfg(unix)]
    command.process_group(0);
    command
}

async fn read_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<(Vec<u8>, bool), ToolError> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|source| ToolError::Output(source.to_string()))?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let retain = remaining.min(read);
        bytes.extend_from_slice(&chunk[..retain]);
        truncated |= retain < read;
    }
    Ok((bytes, truncated))
}

#[cfg(unix)]
fn terminate_process_group(child_id: Option<u32>) {
    let Some(raw) = child_id.and_then(|id| i32::try_from(id).ok()) else {
        return;
    };
    if let Some(pid) = rustix::process::Pid::from_raw(raw) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_child_id: Option<u32>) {}

fn parse_touched_files(bytes: &[u8], limit: usize) -> Result<Vec<TouchedFile>, ToolError> {
    let mut touched = Vec::new();
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(status) = fields.next() {
        if touched.len() >= limit {
            return Err(ToolError::SizeLimit { limit });
        }
        let status = std::str::from_utf8(status)
            .map_err(|_| ToolError::Output("git emitted a non-UTF-8 change status".to_owned()))?;
        let path = fields.next().ok_or_else(|| {
            ToolError::Output("git emitted a changed status without a path".to_owned())
        })?;
        let path = std::str::from_utf8(path)
            .map_err(|_| ToolError::Output("git emitted a non-UTF-8 changed path".to_owned()))?;
        let status = match status.as_bytes().first() {
            Some(b'A') => TouchedFileStatus::Added,
            Some(b'M') => TouchedFileStatus::Modified,
            Some(b'D') => TouchedFileStatus::Deleted,
            Some(b'T') => TouchedFileStatus::TypeChanged,
            _ => {
                return Err(ToolError::Output(format!(
                    "git emitted unsupported change status {status:?}"
                )));
            }
        };
        let path = PathBuf::from(path);
        validate_relative_path(&path)?;
        touched.push(TouchedFile {
            path: path.to_string_lossy().into_owned(),
            status,
        });
    }
    touched.sort_by(|left, right| left.path.cmp(&right.path));
    touched.dedup_by(|left, right| left.path == right.path);
    Ok(touched)
}

fn parse_untracked_paths(bytes: &[u8]) -> Result<Vec<PathBuf>, ToolError> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| {
            let field = std::str::from_utf8(field).map_err(|_| {
                ToolError::Output("git emitted a non-UTF-8 untracked path".to_owned())
            })?;
            let path = PathBuf::from(field);
            validate_relative_path(&path)?;
            Ok(path)
        })
        .collect()
}

fn validate_relative_path(path: &Path) -> Result<(), ToolError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ToolError::InvalidInput(format!(
            "unsafe worktree artifact path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn artifact_id(
    base_commit: &str,
    touched_files: &[TouchedFile],
    unified_diff: &str,
) -> Result<String, ToolError> {
    let manifest = serde_json::to_vec(touched_files)
        .map_err(|source| ToolError::Output(source.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler.worktree-diff.v1\0");
    hasher.update(base_commit.as_bytes());
    hasher.update(b"\0");
    hasher.update(&manifest);
    hasher.update(b"\0");
    hasher.update(unified_diff.as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

fn verify_artifact(artifact: &DiffArtifact) -> Result<(), ToolError> {
    let expected = artifact_id(
        &artifact.base_commit,
        &artifact.touched_files,
        &artifact.unified_diff,
    )?;
    if artifact.id != expected {
        return Err(ToolError::InvalidInput(
            "worktree diff artifact digest did not match its contents".to_owned(),
        ));
    }
    validate_oid(&artifact.base_commit)?;
    for touched in &artifact.touched_files {
        validate_relative_path(Path::new(&touched.path))?;
    }
    Ok(())
}

fn validate_artifact_reference_id(artifact_id: &str) -> Result<(), ToolError> {
    if artifact_id.len() != 64
        || !artifact_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ToolError::InvalidInput(
            "worktree diff artifact reference is malformed".to_owned(),
        ));
    }
    Ok(())
}

fn validate_oid(oid: &str) -> Result<(), ToolError> {
    if !(oid.len() == 40 || oid.len() == 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ToolError::Output(
            "git returned an invalid commit id".to_owned(),
        ));
    }
    Ok(())
}

fn validate_limits(limits: WorktreeLimits) -> Result<(), ToolError> {
    if limits.max_diff_bytes == 0 || limits.max_diff_bytes > MAX_DIFF_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "worktree diff bound must be between 1 and {MAX_DIFF_BYTES} bytes"
        )));
    }
    if limits.max_final_text_bytes == 0 || limits.max_final_text_bytes > MAX_FINAL_TEXT_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "child final-text bound must be between 1 and {MAX_FINAL_TEXT_BYTES} bytes"
        )));
    }
    if limits.max_touched_files == 0 || limits.max_touched_files > MAX_TOUCHED_FILES {
        return Err(ToolError::InvalidInput(format!(
            "touched-file bound must be between 1 and {MAX_TOUCHED_FILES}"
        )));
    }
    Ok(())
}

fn canonical_directory(path: &Path, label: &'static str) -> Result<PathBuf, ToolError> {
    let canonical = path.canonicalize().map_err(|source| ToolError::Io {
        operation: "canonicalize directory",
        path: path.to_path_buf(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(ToolError::InvalidInput(format!(
            "{label} is not a directory: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

fn absolute_without_parent_components(path: &Path) -> Result<PathBuf, ToolError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ToolError::InvalidInput(
            "private worktree storage cannot contain parent traversal".to_owned(),
        ));
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|source| ToolError::Io {
                operation: "resolve private worktree root",
                path: path.to_path_buf(),
                source,
            })
    }
}

fn projected_canonical_path(path: &Path) -> Result<PathBuf, ToolError> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            ToolError::InvalidInput("private worktree root has no existing ancestor".to_owned())
        })?;
        suffix.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            ToolError::InvalidInput("private worktree root has no existing ancestor".to_owned())
        })?;
    }
    let mut projected = existing.canonicalize().map_err(|source| ToolError::Io {
        operation: "canonicalize private worktree ancestor",
        path: existing.to_path_buf(),
        source,
    })?;
    for component in suffix.iter().rev() {
        projected.push(component);
    }
    Ok(projected)
}

fn path_from_stdout(bytes: &[u8], label: &str) -> Result<PathBuf, ToolError> {
    Ok(PathBuf::from(text_stdout(bytes, label)?))
}

fn text_stdout(bytes: &[u8], label: &str) -> Result<String, ToolError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ToolError::Output(format!("git emitted non-UTF-8 {label}")))?
        .trim();
    if text.is_empty() {
        return Err(ToolError::Output(format!("git emitted empty {label}")));
    }
    Ok(text.to_owned())
}

fn require_success(operation: &str, output: &GitOutput) -> Result<(), ToolError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(ToolError::Command(format!(
            "{operation} failed: {}",
            bounded_diagnostic(output)
        )))
    }
}

fn bounded_diagnostic(output: &GitOutput) -> String {
    let bytes = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    String::from_utf8_lossy(&bytes[..bytes.len().min(DIAGNOSTIC_LIMIT)])
        .trim()
        .to_owned()
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

fn empty_accounting() -> (Usage, Cost) {
    (
        Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
        },
        Cost::Unavailable {
            reason: "not applicable to worktree finalization".to_owned(),
        },
    )
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        ToolError::Io {
            operation: "secure private worktree directory",
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(unix)]
fn require_private_permissions(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::symlink_metadata(path)
        .map_err(|source| ToolError::Io {
            operation: "inspect private worktree directory permissions",
            path: path.to_path_buf(),
            source,
        })?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(ToolError::InvalidInput(
            "private worktree storage must not be accessible by group or other users".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), ToolError> {
    Ok(())
}

#[cfg(not(unix))]
fn require_private_permissions(_path: &Path) -> Result<(), ToolError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::process::Command as StdCommand;

    use super::*;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = StdCommand::new(audited_system_git().expect("audited git"))
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Rottweiler Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Rottweiler Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repository() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("repository tempdir");
        git(repo.path(), &["init", "--quiet"]);
        std::fs::write(repo.path().join("shared.txt"), b"base\n").expect("write base");
        git(repo.path(), &["add", "shared.txt"]);
        git(repo.path(), &["commit", "--quiet", "-m", "base"]);
        repo
    }

    fn accounting() -> (Usage, Cost) {
        (
            Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 3,
                cache_write_tokens: 4,
                reasoning_tokens: 5,
            },
            Cost::Monetary {
                amount_micros: 7,
                currency: "USD".to_owned(),
            },
        )
    }

    fn authorized_apply_tool(artifacts: &[&DiffArtifact]) -> (ApplyWorktreeDiffTool, SessionId) {
        let session = SessionId("parent-session".to_owned());
        let authority = Arc::new(SessionDiffArtifactAuthority::default());
        for artifact in artifacts {
            authority
                .record_durable(session.clone(), artifact)
                .expect("record durable artifact");
        }
        (ApplyWorktreeDiffTool::new(authority), session)
    }

    async fn isolation(repo: &Path, private: &Path) -> WorktreeIsolation {
        WorktreeIsolation::new(
            repo,
            private,
            WorktreeLimits::default(),
            CancellationToken::default(),
        )
        .await
        .expect("create isolation")
    }

    #[tokio::test]
    async fn three_parallel_explorers_leave_parent_diff_untouched() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let (one, two, three) = tokio::join!(
            manager.create(CancellationToken::default()),
            manager.create(CancellationToken::default()),
            manager.create(CancellationToken::default()),
        );
        let leases = [one.expect("one"), two.expect("two"), three.expect("three")];
        for (index, lease) in leases.iter().enumerate() {
            std::fs::write(
                lease.path().join(format!("explorer-{index}.txt")),
                format!("result {index}\n"),
            )
            .expect("write explorer result");
        }
        let parent_before = git(repo.path(), &["diff", "--binary", "HEAD", "--"]);
        assert!(parent_before.is_empty());
        let (usage, cost) = accounting();
        for lease in &leases {
            let artifact = manager
                .collect(
                    lease,
                    "done",
                    usage.clone(),
                    cost.clone(),
                    CancellationToken::default(),
                )
                .await
                .expect("collect");
            assert!(artifact.diff.is_some());
        }
        assert!(git(repo.path(), &["diff", "--binary", "HEAD", "--"]).is_empty());
        assert!(git(repo.path(), &["status", "--porcelain"]).is_empty());
    }

    #[tokio::test]
    async fn separate_managers_serialize_overlapping_add_and_remove_registry_mutations() {
        let repo = repository();
        let private_one = tempfile::tempdir().expect("first private tempdir");
        let private_two = tempfile::tempdir().expect("second private tempdir");
        let first = isolation(repo.path(), private_one.path()).await;
        let second = isolation(repo.path(), private_two.path()).await;
        assert!(Arc::ptr_eq(&first.registry_gate, &second.registry_gate));

        let existing = first
            .create(CancellationToken::default())
            .await
            .expect("existing lease");
        let held = first
            .lock_registry(&CancellationToken::default())
            .await
            .expect("hold registry gate");
        let create_manager = second.clone();
        let create =
            tokio::spawn(async move { create_manager.create(CancellationToken::default()).await });
        let remove_manager = first.clone();
        let remove = tokio::spawn(async move {
            remove_manager
                .cleanup_if_untouched(&existing, CancellationToken::default())
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !create.is_finished(),
            "add bypassed the shared registry gate"
        );
        assert!(
            !remove.is_finished(),
            "remove bypassed the shared registry gate"
        );
        drop(held);

        let created = create
            .await
            .expect("add task")
            .expect("create after gate release");
        assert!(
            remove
                .await
                .expect("remove task")
                .expect("remove after gate release")
        );
        assert!(
            second
                .cleanup_if_untouched(&created, CancellationToken::default())
                .await
                .expect("cleanup created lease")
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn cross_process_registry_lock_refuses_a_symlink() {
        use std::os::unix::fs::symlink;

        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let target = private.path().join("lock-target");
        std::fs::write(&target, "must remain unchanged").expect("lock target");
        symlink(&target, repo.path().join(".git/.rottweiler-worktree.lock"))
            .expect("malicious lock symlink");
        let manager = isolation(repo.path(), private.path()).await;
        let error = manager
            .create(CancellationToken::default())
            .await
            .expect_err("symlink lock must fail closed");
        assert!(error.to_string().contains("Git worktree lock"));
        assert_eq!(
            std::fs::read_to_string(target).expect("unchanged target"),
            "must remain unchanged"
        );
    }

    #[tokio::test]
    async fn isolated_diff_applies_only_through_explicit_tool() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("shared.txt"), b"implemented\n").expect("edit");
        std::fs::write(lease.path().join("--help;touch owned"), b"exact argv\n")
            .expect("write injection-shaped path");
        let (usage, cost) = accounting();
        let child = manager
            .collect(
                &lease,
                "implemented",
                usage,
                cost,
                CancellationToken::default(),
            )
            .await
            .expect("artifact");
        assert_eq!(
            std::fs::read(repo.path().join("shared.txt")).expect("read"),
            b"base\n"
        );
        let artifact = child.diff.expect("diff");
        let (tool, session) = authorized_apply_tool(&[&artifact]);
        let context = ToolContext::new(repo.path())
            .expect("context")
            .with_session_id(session);
        tool.execute(&context, json!({"artifact_id": artifact.id}))
            .await
            .expect("apply");
        assert_eq!(
            std::fs::read(repo.path().join("shared.txt")).expect("read"),
            b"implemented\n"
        );
        assert_eq!(
            std::fs::read(repo.path().join("--help;touch owned")).expect("read exact path"),
            b"exact argv\n"
        );
        assert!(!repo.path().join("owned").exists());
    }

    #[tokio::test]
    async fn conflicting_pair_is_a_tool_error_and_keeps_first_result() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let first = manager
            .create(CancellationToken::default())
            .await
            .expect("first");
        let second = manager
            .create(CancellationToken::default())
            .await
            .expect("second");
        std::fs::write(first.path().join("shared.txt"), b"first\n").expect("first write");
        std::fs::write(second.path().join("shared.txt"), b"second\n").expect("second write");
        let (usage, cost) = accounting();
        let first = manager
            .collect(
                &first,
                "first",
                usage.clone(),
                cost.clone(),
                CancellationToken::default(),
            )
            .await
            .expect("first artifact");
        let second = manager
            .collect(&second, "second", usage, cost, CancellationToken::default())
            .await
            .expect("second artifact");
        let first_artifact = first.diff.expect("first diff");
        let second_artifact = second.diff.expect("second diff");
        let (tool, session) = authorized_apply_tool(&[&first_artifact, &second_artifact]);
        let context = ToolContext::new(repo.path())
            .expect("context")
            .with_session_id(session);
        tool.execute(&context, json!({"artifact": first_artifact}))
            .await
            .expect("first apply");
        let bytes_before = std::fs::read(repo.path().join("shared.txt")).expect("bytes before");
        let status_before = git(repo.path(), &["status", "--porcelain=v1"]);
        let index = git(repo.path(), &["rev-parse", "--git-path", "index"]);
        let index = if Path::new(&index).is_absolute() {
            PathBuf::from(index)
        } else {
            repo.path().join(index)
        };
        let index_before = std::fs::read(&index).expect("index before");
        let error = tool
            .execute(&context, json!({"artifact": second_artifact}))
            .await
            .expect_err("second conflicts");
        assert!(error.to_string().contains("conflict"), "{error}");
        assert_eq!(
            std::fs::read(repo.path().join("shared.txt")).expect("read"),
            bytes_before
        );
        assert_eq!(
            git(repo.path(), &["status", "--porcelain=v1"]),
            status_before
        );
        assert_eq!(std::fs::read(index).expect("index after"), index_before);
        assert!(
            !repo
                .path()
                .join("shared.txt")
                .with_extension("txt.rej")
                .exists()
        );
    }

    #[tokio::test]
    async fn post_preflight_apply_failure_preserves_parent_bytes_and_index() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("shared.txt"), b"child\n").expect("child write");
        let (usage, cost) = accounting();
        let artifact = manager
            .collect(&lease, "done", usage, cost, CancellationToken::default())
            .await
            .expect("collect")
            .diff
            .expect("artifact");

        std::fs::write(repo.path().join("shared.txt"), b"unstaged parent\n").expect("parent write");
        let bytes_before = std::fs::read(repo.path().join("shared.txt")).expect("bytes before");
        let status_before = git(repo.path(), &["status", "--porcelain=v1"]);
        let index = repo
            .path()
            .join(git(repo.path(), &["rev-parse", "--git-path", "index"]));
        let index_before = std::fs::read(&index).expect("index before");
        let (tool, session) = authorized_apply_tool(&[&artifact]);
        let context = ToolContext::new(repo.path())
            .expect("context")
            .with_session_id(session);
        let error = tool
            .execute(&context, json!({"artifact": artifact}))
            .await
            .expect_err("unstaged parent blocks apply");
        assert!(
            error
                .to_string()
                .contains("failed after checkpointed preflight")
        );
        assert_eq!(
            std::fs::read(repo.path().join("shared.txt")).expect("bytes after"),
            bytes_before
        );
        assert_eq!(
            git(repo.path(), &["status", "--porcelain=v1"]),
            status_before
        );
        assert_eq!(std::fs::read(index).expect("index after"), index_before);
    }

    #[tokio::test]
    async fn correctly_hashed_forgery_is_rejected_by_session_authority() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("shared.txt"), b"authorized\n").expect("authorized edit");
        let (usage, cost) = accounting();
        let authorized = manager
            .collect(&lease, "done", usage, cost, CancellationToken::default())
            .await
            .expect("collect")
            .diff
            .expect("artifact");
        let mut forged = authorized.clone();
        forged.unified_diff = forged.unified_diff.replace("authorized", "forged");
        forged.id = artifact_id(
            &forged.base_commit,
            &forged.touched_files,
            &forged.unified_diff,
        )
        .expect("forge valid digest");
        verify_artifact(&forged).expect("unkeyed integrity is internally valid");

        let (tool, session) = authorized_apply_tool(&[&authorized]);
        let context = ToolContext::new(repo.path())
            .expect("context")
            .with_session_id(session);
        let error = tool
            .execute(&context, json!({"artifact": forged}))
            .await
            .expect_err("forged artifact rejected");
        assert!(error.to_string().contains("not durably produced"));
        assert_eq!(
            std::fs::read(repo.path().join("shared.txt")).expect("parent bytes"),
            b"base\n"
        );

        let preview = tool
            .approval_preview(&context, &json!({"artifact": authorized.clone()}))
            .await
            .expect("authorized preview")
            .expect("preview");
        let preview_artifact: DiffArtifact =
            serde_json::from_slice(&preview.after).expect("full artifact preview");
        assert_eq!(preview_artifact, authorized);
    }

    #[tokio::test]
    async fn artifact_reference_is_session_scoped_and_preview_expands_full_artifact() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("shared.txt"), b"referenced\n").expect("referenced edit");
        let (usage, cost) = accounting();
        let artifact = manager
            .collect(&lease, "done", usage, cost, CancellationToken::default())
            .await
            .expect("collect")
            .diff
            .expect("artifact");
        let artifact_id = artifact.id.clone();
        let (tool, session) = authorized_apply_tool(&[&artifact]);
        let context = ToolContext::new(repo.path())
            .expect("context")
            .with_session_id(session);

        let preview = tool
            .approval_preview(&context, &json!({"artifact_id": artifact_id}))
            .await
            .expect("preview")
            .expect("approval preview");
        assert_eq!(
            serde_json::from_slice::<DiffArtifact>(&preview.after).expect("preview artifact"),
            artifact
        );

        let other_context = ToolContext::new(repo.path())
            .expect("other context")
            .with_session_id(SessionId("other-parent".to_owned()));
        let error = tool
            .execute(&other_context, json!({"artifact_id": artifact.id}))
            .await
            .expect_err("cross-session reference rejected");
        assert!(error.to_string().contains("not durably produced"));
    }

    #[tokio::test]
    async fn apply_input_requires_exactly_one_artifact_form() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("shared.txt"), b"exactly-one\n").expect("edit");
        let (usage, cost) = accounting();
        let artifact = manager
            .collect(&lease, "done", usage, cost, CancellationToken::default())
            .await
            .expect("collect")
            .diff
            .expect("artifact");
        let (tool, session) = authorized_apply_tool(&[&artifact]);
        let context = ToolContext::new(repo.path())
            .expect("context")
            .with_session_id(session);

        for input in [
            json!({}),
            json!({"artifact": artifact.clone(), "artifact_id": artifact.id.clone()}),
        ] {
            let error = tool
                .approval_preview(&context, &input)
                .await
                .expect_err("invalid union rejected");
            assert!(error.to_string().contains("exactly one"));
        }
        let malformed = tool
            .approval_preview(&context, &json!({"artifact_id": "not-a-digest"}))
            .await
            .expect_err("malformed reference rejected");
        assert!(malformed.to_string().contains("reference is malformed"));
    }

    #[tokio::test]
    async fn cancellation_and_safe_cleanup_fail_closed() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let cancellation = CancellationToken::default();
        cancellation.cancel();
        let error = manager.create(cancellation).await.expect_err("cancelled");
        assert!(matches!(error, ToolError::Cancelled));

        let clean = manager
            .create(CancellationToken::default())
            .await
            .expect("clean");
        let clean_path = clean.path().to_path_buf();
        assert!(
            manager
                .cleanup_if_untouched(&clean, CancellationToken::default())
                .await
                .expect("cleanup")
        );
        assert!(!clean_path.exists());

        let dirty = manager
            .create(CancellationToken::default())
            .await
            .expect("dirty");
        std::fs::write(dirty.path().join("keep.txt"), b"keep\n").expect("dirty write");
        assert!(
            !manager
                .cleanup_if_untouched(&dirty, CancellationToken::default())
                .await
                .expect("refuse dirty cleanup")
        );
        assert!(dirty.path().exists());
    }

    #[tokio::test]
    async fn tombstoned_changed_lease_is_discarded_without_parent_mutation() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("rewound.txt"), b"discard me\n").expect("dirty write");
        let record = lease.durable_record();
        let lease_path = lease.path().to_path_buf();

        manager
            .discard_tombstoned(&record, CancellationToken::default())
            .await
            .expect("discard tombstoned lease");

        assert!(!lease_path.exists());
        manager
            .discard_tombstoned(&record, CancellationToken::default())
            .await
            .expect("already-absent unregistered lease is idempotent");
        assert!(!repo.path().join("rewound.txt").exists());
        assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
    }

    #[tokio::test]
    async fn tombstoned_discard_honors_cancellation_without_removing_the_lease() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("keep.txt"), b"keep\n").expect("dirty write");
        let record = lease.durable_record();
        let cancellation = CancellationToken::default();
        cancellation.cancel();

        let error = manager
            .discard_tombstoned(&record, cancellation)
            .await
            .expect_err("cancelled discard");

        assert!(matches!(error, ToolError::Cancelled));
        assert!(lease.path().exists());
        assert!(!repo.path().join("keep.txt").exists());
    }

    #[tokio::test]
    async fn tombstoned_discard_rejects_tampered_path_and_base() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        let record = lease.durable_record();

        let mut path_tampered = record.clone();
        path_tampered.path = repo.path().to_path_buf();
        path_tampered.canonical_path = repo.path().to_path_buf();
        assert!(
            manager
                .discard_tombstoned(&path_tampered, CancellationToken::default())
                .await
                .is_err()
        );
        assert!(repo.path().join("shared.txt").exists());

        let mut base_tampered = record.clone();
        base_tampered.base_commit = "0".repeat(40);
        assert!(
            manager
                .discard_tombstoned(&base_tampered, CancellationToken::default())
                .await
                .is_err()
        );
        assert!(lease.path().exists());
        manager
            .discard_tombstoned(&record, CancellationToken::default())
            .await
            .expect("discard untampered lease");
    }

    #[tokio::test]
    async fn tombstoned_discard_rejects_absent_but_registered_worktree() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        let record = lease.durable_record();
        let path = lease.path().to_path_buf();
        let parked = manager.private_root().join("parked-registered-lease");
        std::fs::rename(&path, &parked).expect("park registered lease");

        let error = manager
            .discard_tombstoned(&record, CancellationToken::default())
            .await
            .expect_err("registered missing lease rejected");
        assert!(error.to_string().contains("remains registered"), "{error}");

        std::fs::rename(&parked, &path).expect("restore registered lease");
        manager
            .discard_tombstoned(&record, CancellationToken::default())
            .await
            .expect("discard restored lease");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tombstoned_discard_rejects_symlink_and_inode_swaps() {
        use std::os::unix::fs::symlink;

        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;

        let symlink_lease = manager
            .create(CancellationToken::default())
            .await
            .expect("symlink lease");
        let symlink_record = symlink_lease.durable_record();
        let symlink_path = symlink_lease.path().to_path_buf();
        let symlink_parked = manager.private_root().join("parked-symlink-lease");
        std::fs::rename(&symlink_path, &symlink_parked).expect("park symlink lease");
        symlink(repo.path(), &symlink_path).expect("replace lease with symlink");
        assert!(
            manager
                .discard_tombstoned(&symlink_record, CancellationToken::default())
                .await
                .is_err()
        );
        assert!(repo.path().join("shared.txt").exists());
        std::fs::remove_file(&symlink_path).expect("remove swap symlink");
        std::fs::rename(&symlink_parked, &symlink_path).expect("restore symlink lease");
        manager
            .discard_tombstoned(&symlink_record, CancellationToken::default())
            .await
            .expect("discard restored symlink lease");

        let inode_lease = manager
            .create(CancellationToken::default())
            .await
            .expect("inode lease");
        let inode_record = inode_lease.durable_record();
        let inode_path = inode_lease.path().to_path_buf();
        let inode_parked = manager.private_root().join("parked-inode-lease");
        std::fs::rename(&inode_path, &inode_parked).expect("park inode lease");
        std::fs::create_dir(&inode_path).expect("replace lease directory");
        assert!(
            manager
                .discard_tombstoned(&inode_record, CancellationToken::default())
                .await
                .is_err()
        );
        assert!(inode_path.exists());
        assert!(inode_parked.exists());
        std::fs::remove_dir(&inode_path).expect("remove replacement directory");
        std::fs::rename(&inode_parked, &inode_path).expect("restore inode lease");
        manager
            .discard_tombstoned(&inode_record, CancellationToken::default())
            .await
            .expect("discard restored inode lease");
        assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
    }

    #[tokio::test]
    async fn active_git_process_is_reaped_on_cancellation() {
        let repo = repository();
        let input = vec![b'x'; 32 * 1024 * 1024];
        let cancellation = CancellationToken::default();
        let trigger = cancellation.clone();
        let cancel_task = tokio::spawn(async move {
            tokio::task::yield_now().await;
            trigger.cancel();
        });
        let result = run_git_raw_with_paths(
            repo.path(),
            [OsString::from("hash-object"), OsString::from("--stdin")],
            Some(&input),
            &cancellation,
            None,
            None,
        )
        .await;
        cancel_task.await.expect("cancellation task");
        assert!(matches!(result, Err(ToolError::Cancelled)));
        assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
    }

    #[tokio::test]
    async fn captured_changes_finalize_only_while_artifact_still_matches() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("new.txt"), b"captured\n").expect("write");
        let (usage, cost) = accounting();
        let child = manager
            .collect(&lease, "done", usage, cost, CancellationToken::default())
            .await
            .expect("collect");
        let artifact = child.diff.expect("diff");
        std::fs::write(lease.path().join("new.txt"), b"changed later\n").expect("change later");
        assert!(
            !manager
                .finalize_captured(&lease, &artifact, CancellationToken::default())
                .await
                .expect("refuse changed finalization")
        );
        assert!(lease.path().exists());
        std::fs::write(lease.path().join("new.txt"), b"captured\n").expect("restore captured");
        assert!(
            manager
                .finalize_captured(&lease, &artifact, CancellationToken::default())
                .await
                .expect("finalize captured")
        );
        assert!(!lease.path().exists());
        assert!(git(repo.path(), &["status", "--porcelain=v1"]).is_empty());
    }

    #[tokio::test]
    async fn durable_rebind_continues_in_the_same_worktree() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        std::fs::write(lease.path().join("first.txt"), b"first turn\n").expect("first turn");
        let (usage, cost) = accounting();
        let first = manager
            .collect(
                &lease,
                "first",
                usage.clone(),
                cost.clone(),
                CancellationToken::default(),
            )
            .await
            .expect("first collect");
        assert_eq!(first.touched_files.len(), 1);

        let encoded = serde_json::to_vec(&lease.durable_record()).expect("encode lease");
        let record: WorktreeLeaseRecord = serde_json::from_slice(&encoded).expect("decode lease");
        let rebound = manager
            .rebind(&record, CancellationToken::default())
            .await
            .expect("rebind");
        assert_eq!(rebound.path(), lease.path());
        std::fs::write(rebound.path().join("second.txt"), b"follow-up turn\n")
            .expect("follow-up turn");
        let second = manager
            .collect(
                &rebound,
                "second",
                usage,
                cost,
                CancellationToken::default(),
            )
            .await
            .expect("second collect");
        assert_eq!(
            second
                .touched_files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["first.txt", "second.txt"]
        );
        let artifact = second.diff.expect("cumulative diff");
        assert!(
            manager
                .finalize_captured(&rebound, &artifact, CancellationToken::default())
                .await
                .expect("finalize rebound lease")
        );
    }

    #[tokio::test]
    async fn rejects_nested_roots_subdirectories_and_symlink_swaps() {
        let repo = repository();
        let nested = repo.path().join("nested");
        std::fs::create_dir(&nested).expect("nested");
        let private = tempfile::tempdir().expect("private tempdir");
        let error = WorktreeIsolation::new(
            &nested,
            private.path(),
            WorktreeLimits::default(),
            CancellationToken::default(),
        )
        .await
        .expect_err("subdirectory rejected");
        assert!(error.to_string().contains("exact git top level"));

        let error = WorktreeIsolation::new(
            repo.path(),
            repo.path().join("private"),
            WorktreeLimits::default(),
            CancellationToken::default(),
        )
        .await
        .expect_err("nested private rejected");
        assert!(error.to_string().contains("outside"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let insecure_parent = tempfile::tempdir().expect("insecure parent");
            let insecure = insecure_parent.path().join("shared-private-root");
            std::fs::create_dir(&insecure).expect("insecure root");
            std::fs::set_permissions(&insecure, std::fs::Permissions::from_mode(0o755))
                .expect("insecure mode");
            let private_manager = WorktreeIsolation::new(
                repo.path(),
                &insecure,
                WorktreeLimits::default(),
                CancellationToken::default(),
            )
            .await
            .expect("dedicated private child");
            assert_eq!(
                std::fs::metadata(&insecure)
                    .expect("insecure metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
            assert_eq!(
                std::fs::metadata(private_manager.private_root())
                    .expect("private child metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );

            let aliases = tempfile::tempdir().expect("alias tempdir");
            let alias = aliases.path().join("repo-alias");
            std::os::unix::fs::symlink(repo.path(), &alias).expect("private root alias");
            let escaped = alias.join("must-not-be-created");
            let error = WorktreeIsolation::new(
                repo.path(),
                &escaped,
                WorktreeLimits::default(),
                CancellationToken::default(),
            )
            .await
            .expect_err("symlinked private parent rejected before creation");
            assert!(error.to_string().contains("outside"));
            assert!(!escaped.exists());

            let manager = isolation(repo.path(), private.path()).await;
            let lease = manager
                .create(CancellationToken::default())
                .await
                .expect("lease");
            let original = lease.path().to_path_buf();
            let moved = private.path().join("moved-worktree");
            std::fs::rename(&original, &moved).expect("move worktree");
            std::os::unix::fs::symlink(repo.path(), &original).expect("swap symlink");
            let (usage, cost) = accounting();
            let error = manager
                .collect(&lease, "unsafe", usage, cost, CancellationToken::default())
                .await
                .expect_err("symlink swap rejected");
            assert!(
                error.to_string().contains("real directory")
                    || error.to_string().contains("identity")
            );
            assert!(git(repo.path(), &["status", "--porcelain"]).is_empty());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_hooks_and_filters_never_execute_during_isolation() {
        use std::os::unix::fs::PermissionsExt as _;

        let repo = repository();
        std::fs::write(
            repo.path().join(".gitattributes"),
            "filtered.txt filter=evil\n",
        )
        .expect("attributes");
        std::fs::write(repo.path().join("filtered.txt"), "safe\n").expect("filtered file");
        git(repo.path(), &["add", ".gitattributes", "filtered.txt"]);
        git(repo.path(), &["commit", "--quiet", "-m", "attributes"]);
        let marker = repo
            .path()
            .parent()
            .expect("parent")
            .join("project-code-ran");
        let filter = format!("sh -c 'printf owned > {}; cat'", marker.display());
        git(repo.path(), &["config", "filter.evil.smudge", &filter]);
        git(repo.path(), &["config", "filter.evil.clean", &filter]);
        let hook = repo.path().join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nprintf owned > {}\n", marker.display()),
        )
        .expect("hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).expect("hook mode");

        let private = tempfile::tempdir().expect("private tempdir");
        let manager = isolation(repo.path(), private.path()).await;
        let lease = manager
            .create(CancellationToken::default())
            .await
            .expect("lease");
        assert!(!marker.exists(), "project filter or hook executed");
        std::fs::write(lease.path().join("filtered.txt"), "changed\n").expect("change");
        let (usage, cost) = accounting();
        manager
            .collect(&lease, "done", usage, cost, CancellationToken::default())
            .await
            .expect("collect without project process execution");
        assert!(!marker.exists(), "project filter or hook executed");
    }

    #[test]
    fn artifact_paths_and_text_are_bounded() {
        assert!(validate_relative_path(Path::new("--help;touch owned")).is_ok());
        assert!(validate_relative_path(Path::new("../escape")).is_err());
        assert!(validate_relative_path(Path::new("/absolute")).is_err());
        let (text, truncated) = truncate_utf8("hello 🐕", 8);
        assert_eq!(text, "hello ");
        assert!(truncated);
    }
}
