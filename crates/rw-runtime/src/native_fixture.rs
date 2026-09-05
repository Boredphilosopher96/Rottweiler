//! Bound native compiler, executable hashing, and sandbox fixtures before their deadlines.
use tokio::sync::{Semaphore, SemaphorePermit};

static NATIVE_FIXTURES: Semaphore = Semaphore::const_new(2);

pub(crate) async fn admit() -> SemaphorePermit<'static> {
    #[allow(clippy::expect_used)]
    NATIVE_FIXTURES
        .acquire()
        .await
        .expect("fixture admission remains open")
}

/// The ordinary test harness cannot execute trusted worker bootstrap arguments.
pub(crate) fn sandbox_helper() -> std::io::Result<std::path::PathBuf> {
    let supplied = std::env::var_os("ROTTWEILER_TEST_SANDBOX_HELPER").ok_or_else(|| {
        std::io::Error::other("native fixture prerequisite: build rw-sandbox-helper and set ROTTWEILER_TEST_SANDBOX_HELPER to its executable artifact")
    })?;
    let path = std::fs::canonicalize(supplied)?;
    let metadata = path.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other(
            "native fixture sandbox helper is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(std::io::Error::other(
                "native fixture sandbox helper is not executable",
            ));
        }
    }
    Ok(path)
}
