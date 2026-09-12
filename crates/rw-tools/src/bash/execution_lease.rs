use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::time::Duration;

use crate::registry::ToolError;

/// A data-less, inheritable session lease used to order crash recovery after
/// process-group cleanup.
///
/// On Unix the parent descriptor uses `CLOEXEC`. Only the parent-death
/// watchdog receives a duplicate mapped to one of its standard descriptors;
/// arbitrary commands never inherit a usable lease descriptor. A replacement
/// session therefore cannot recover checkpoints until the watchdog has killed
/// and observed the command group exit. The event log is separate and is never
/// exposed.
#[derive(Debug)]
pub struct ExecutionLease {
    pub(super) file: std::fs::File,
}

impl ExecutionLease {
    /// Opens and exclusively locks a private regular lease file.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or unsafe parent directory, an unsafe
    /// lease file, insecure permissions, or a lock failure.
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, ToolError> {
        acquire_execution_lease(path.as_ref(), true)
    }

    /// Opens an execution lease without waiting behind another process.
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` through [`ToolError::Io`] when another process
    /// already owns the workspace lease, in addition to the safety errors
    /// documented by [`Self::acquire`].
    pub fn try_acquire(path: impl AsRef<Path>) -> Result<Self, ToolError> {
        acquire_execution_lease(path.as_ref(), false)
    }

    /// Opens an execution lease for recovery, waiting no longer than `timeout`.
    ///
    /// # Errors
    ///
    /// Returns an error when the lease remains owned at the deadline or the
    /// private lease file cannot be opened safely.
    pub fn acquire_for(path: impl AsRef<Path>, timeout: Duration) -> Result<Self, ToolError> {
        #[cfg(unix)]
        {
            acquire_execution_lease_with_wait(
                path.as_ref(),
                ExecutionLeaseWait::Until(std::time::Instant::now() + timeout),
            )
        }
        #[cfg(not(unix))]
        {
            let _ = timeout;
            acquire_execution_lease(path.as_ref(), false)
        }
    }

    #[cfg(unix)]
    pub(super) fn watchdog_stdio(&self) -> Result<Stdio, ToolError> {
        self.file
            .try_clone()
            .map(Stdio::from)
            .map_err(|source| ToolError::Io {
                operation: "duplicate execution lease for watchdog",
                path: PathBuf::from("execution.lock"),
                source,
            })
    }

    #[cfg(all(unix, test))]
    pub(super) fn test_watchdog_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.file)
    }
}

#[cfg(unix)]
pub(super) fn acquire_execution_lease(
    path: &Path,
    wait: bool,
) -> Result<ExecutionLease, ToolError> {
    acquire_execution_lease_with_wait(
        path,
        if wait {
            ExecutionLeaseWait::Forever
        } else {
            ExecutionLeaseWait::Never
        },
    )
}

#[cfg(unix)]
#[derive(Clone, Copy)]
pub(super) enum ExecutionLeaseWait {
    Never,
    Forever,
    Until(std::time::Instant),
}

#[cfg(unix)]
pub(super) fn acquire_execution_lease_with_wait(
    path: &Path,
    wait: ExecutionLeaseWait,
) -> Result<ExecutionLease, ToolError> {
    let parent_path = path
        .parent()
        .ok_or_else(|| ToolError::Command("execution lease has no parent".to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| ToolError::Command("execution lease has no file name".to_owned()))?;
    let parent = rustix::fs::open(
        parent_path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ToolError::Io {
        operation: "open execution lease directory",
        path: parent_path.to_path_buf(),
        source,
    })?;
    let parent_stat = rustix::fs::fstat(&parent)
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "inspect execution lease directory",
            path: parent_path.to_path_buf(),
            source,
        })?;
    if !rustix::fs::FileType::from_raw_mode(parent_stat.st_mode).is_dir()
        || parent_stat.st_uid != rustix::process::geteuid().as_raw()
        || parent_stat.st_mode & 0o022 != 0
    {
        return Err(ToolError::Command(
            "execution lease directory must be owner-controlled and not group/other writable"
                .to_owned(),
        ));
    }
    let descriptor = rustix::fs::openat(
        &parent,
        file_name,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)
    .map_err(|source| ToolError::Io {
        operation: "open execution lease",
        path: path.to_path_buf(),
        source,
    })?;
    let file = std::fs::File::from(descriptor);
    let stat = rustix::fs::fstat(&file)
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "inspect execution lease",
            path: path.to_path_buf(),
            source,
        })?;
    if !rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(ToolError::Command(
            "execution lease must be a regular file, never a symlink or special file".to_owned(),
        ));
    }
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(ToolError::Command(
            "execution lease must be owned by the current user".to_owned(),
        ));
    }
    if stat.st_mode & 0o077 != 0 {
        return Err(ToolError::Command(
            "execution lease permissions must not grant group or other access".to_owned(),
        ));
    }
    lock_execution_lease(&file, path, wait)?;
    rustix::fs::fsync(&parent)
        .map_err(std::io::Error::from)
        .map_err(|source| ToolError::Io {
            operation: "synchronize execution lease directory",
            path: parent_path.to_path_buf(),
            source,
        })?;
    Ok(ExecutionLease { file })
}

#[cfg(unix)]
pub(super) fn lock_execution_lease(
    file: &std::fs::File,
    path: &Path,
    wait: ExecutionLeaseWait,
) -> Result<(), ToolError> {
    let operation = if matches!(wait, ExecutionLeaseWait::Forever) {
        rustix::fs::FlockOperation::LockExclusive
    } else {
        rustix::fs::FlockOperation::NonBlockingLockExclusive
    };
    loop {
        match rustix::fs::flock(file, operation) {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::WOULDBLOCK) if matches!(wait, ExecutionLeaseWait::Until(_)) => {
                let ExecutionLeaseWait::Until(deadline) = wait else {
                    unreachable!();
                };
                if std::time::Instant::now() >= deadline {
                    return Err(ToolError::Command(format!(
                        "execution lease remained busy until the recovery timeout at {}",
                        path.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(source) => {
                return Err(ToolError::Io {
                    operation: "lock execution lease",
                    path: path.to_path_buf(),
                    source: std::io::Error::from(source),
                });
            }
        }
    }
}

#[cfg(not(unix))]
pub(super) fn acquire_execution_lease(
    path: &Path,
    _wait: bool,
) -> Result<ExecutionLease, ToolError> {
    Err(ToolError::Command(format!(
        "execution leases are unavailable on this platform; refusing unlocked session startup at {}",
        path.display()
    )))
}
