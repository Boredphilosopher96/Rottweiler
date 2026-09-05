//! Allocation remains provisional until its child owner is ready.
use super::*;

/// A newly allocated worktree awaiting transfer to a child session.
/// Dropping it without commit or successful rollback blocks further allocation.
#[derive(Debug)]
pub struct WorktreeAllocation {
    lease: WorktreeLease,
    guard: CreationGuard,
}
impl WorktreeAllocation {
    #[must_use]
    pub fn lease(&self) -> &WorktreeLease {
        &self.lease
    }

    #[must_use]
    pub fn commit(mut self) -> WorktreeLease {
        self.guard.armed = false;
        self.lease
    }

    /// Removes a failed startup's untouched allocation.
    ///
    /// # Errors
    /// Retains changed or unverifiable worktrees and blocks further allocation.
    pub async fn rollback(mut self) -> Result<(), ToolError> {
        match self
            .guard
            .isolation
            .cleanup_if_untouched(&self.lease, CancellationToken::default())
            .await
        {
            Ok(true) => {
                self.guard.armed = false;
                Ok(())
            }
            Ok(false) => Err(self.guard.fail("worktree changed before startup completed")),
            Err(error) => Err(self.guard.fail(&error.to_string())),
        }
    }
}

#[derive(Debug)]
struct CreationGuard {
    isolation: WorktreeIsolation,
    path: PathBuf,
    armed: bool,
}
impl CreationGuard {
    fn new(isolation: WorktreeIsolation, path: PathBuf) -> Self {
        Self {
            isolation,
            path,
            armed: true,
        }
    }
    fn record_failure(&self) {
        let mut failure = self
            .isolation
            .creation_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        failure.get_or_insert_with(|| self.path.clone());
    }
    fn fail(&self, reason: &str) -> ToolError {
        self.record_failure();
        ToolError::EffectsUnsettled(format!(
            "worktree allocation {} remains unconfirmed: {reason}",
            self.path.display()
        ))
    }
}
impl Drop for CreationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.record_failure();
        }
    }
}

impl WorktreeIsolation {
    fn ensure_creation_ready(&self) -> Result<(), ToolError> {
        let failure = self
            .creation_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(path) = failure.as_ref() {
            return Err(ToolError::EffectsUnsettled(format!(
                "worktree creation is blocked by unconfirmed allocation {}",
                path.display()
            )));
        }
        Ok(())
    }

    async fn creation_failed(&self, guard: &mut CreationGuard, cause: ToolError) -> ToolError {
        match self.cleanup_partial_creation(&guard.path).await {
            Ok(()) => {
                guard.armed = false;
                cause
            }
            Err(cleanup) => guard.fail(&format!("{cause}; cleanup failed: {cleanup}")),
        }
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
    ) -> Result<WorktreeAllocation, ToolError> {
        // `git worktree add` mutates the repository's shared worktree
        // registry. Git does not provide one transaction spanning its
        // discovery, allocation, registration, and our failure cleanup, so
        // concurrent adds can transiently reject one otherwise independent
        // child. Serialize only this short allocation boundary; child turns
        // and worktree contents remain fully parallel after their leases exist.
        let _registry = self.lock_registry(&cancellation).await?;
        self.ensure_creation_ready()?;
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
        let mut guard = CreationGuard::new(self.clone(), path.clone());
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
                return Err(self.creation_failed(&mut guard, error).await);
            }
        };
        if let Err(error) = require_success("create isolated worktree", &output) {
            return Err(self.creation_failed(&mut guard, error).await);
        }
        if let Err(error) = set_private_permissions(&path) {
            return Err(self.creation_failed(&mut guard, error).await);
        }
        let identity = match DirectoryIdentity::capture(&path) {
            Ok(identity) => identity,
            Err(error) => {
                return Err(self.creation_failed(&mut guard, error).await);
            }
        };
        if !identity.canonical.starts_with(&self.private_root) {
            return Err(self
                .creation_failed(
                    &mut guard,
                    ToolError::Command(
                        "git created the worktree outside private storage".to_owned(),
                    ),
                )
                .await);
        }
        Ok(WorktreeAllocation {
            lease: WorktreeLease {
                finalization_gate: process_worktree_finalization_gate(&path),
                path,
                base_commit,
                identity,
            },
            guard,
        })
    }

    async fn cleanup_partial_creation(&self, path: &Path) -> Result<(), ToolError> {
        let cancellation = CancellationToken::default();
        let removal = run_git(
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
        let listing = run_git(
            &self.repository_root,
            [
                OsString::from("worktree"),
                OsString::from("list"),
                OsString::from("--porcelain"),
                OsString::from("-z"),
            ],
            None,
            &cancellation,
        )
        .await?;
        require_success("verify partial worktree removal", &listing)?;
        let registered = listing.stdout.split(|byte| *byte == 0).any(|field| {
            field.strip_prefix(b"worktree ") == Some(path.as_os_str().as_encoded_bytes())
        });
        if registered {
            let diagnostic = removal.map_or_else(
                |error| error.to_string(),
                |output| bounded_diagnostic(&output),
            );
            return Err(ToolError::Command(format!(
                "partial worktree remains registered: {diagnostic}"
            )));
        }
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(ToolError::Io {
                    operation: "inspect partial worktree",
                    path: path.to_path_buf(),
                    source,
                });
            }
            Ok(_) => {}
        }
        let identity = DirectoryIdentity::capture(path)?;
        if !identity.canonical.starts_with(&self.private_root) {
            return Err(ToolError::Command(
                "partial worktree is outside private storage".to_owned(),
            ));
        }
        tokio::fs::remove_dir_all(path)
            .await
            .map_err(|source| ToolError::Io {
                operation: "remove unregistered partial worktree",
                path: path.to_path_buf(),
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use crate::worktree::tests::{isolation, repository};

    #[tokio::test]
    async fn failed_creation_removes_unregistered_partial_files_before_returning() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private root");
        let manager = isolation(repo.path(), private.path()).await;
        let path = manager.private_root().join("rw-agent-partial");
        std::fs::create_dir(&path).expect("partial directory");
        std::fs::write(path.join("partial.txt"), "incomplete checkout").expect("partial file");
        let mut guard = CreationGuard::new(manager.clone(), path.clone());
        assert!(matches!(
            manager
                .creation_failed(&mut guard, ToolError::Cancelled)
                .await,
            ToolError::Cancelled
        ));
        drop(guard);
        assert!(!path.exists());
        manager
            .create(CancellationToken::default())
            .await
            .expect("next allocation")
            .rollback()
            .await
            .expect("next cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_creation_never_follows_a_replaced_partial_directory() {
        let repo = repository();
        let private = tempfile::tempdir().expect("private root");
        let manager = isolation(repo.path(), private.path()).await;
        let outside = tempfile::tempdir().expect("unrelated directory");
        std::fs::write(outside.path().join("keep.txt"), "untouched").expect("unrelated file");
        let path = manager.private_root().join("rw-agent-replaced");
        std::os::unix::fs::symlink(outside.path(), &path).expect("replace partial directory");
        let mut guard = CreationGuard::new(manager.clone(), path);
        assert!(matches!(
            manager
                .creation_failed(&mut guard, ToolError::Cancelled)
                .await,
            ToolError::EffectsUnsettled(_)
        ));
        assert_eq!(
            std::fs::read_to_string(outside.path().join("keep.txt")).expect("unrelated file"),
            "untouched"
        );
        assert!(matches!(
            manager.create(CancellationToken::default()).await,
            Err(ToolError::EffectsUnsettled(_))
        ));
    }
}
