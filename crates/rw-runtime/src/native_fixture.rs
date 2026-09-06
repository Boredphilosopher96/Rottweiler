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
pub(crate) fn sandbox_helper() -> std::io::Result<rw_tools::SandboxHelper> {
    static HELPER: std::sync::OnceLock<Result<rw_tools::SandboxHelper, String>> =
        std::sync::OnceLock::new();
    HELPER
        .get_or_init(|| load_helper().map_err(|error| error.to_string()))
        .clone()
        .map_err(std::io::Error::other)
}

fn load_helper() -> std::io::Result<rw_tools::SandboxHelper> {
    use std::io::Read as _;
    let supplied = std::env::var_os("ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT").ok_or_else(|| {
        std::io::Error::other("native fixture prerequisite: run scripts/build-test-helper.py and set ROTTWEILER_TEST_SANDBOX_HELPER_RECEIPT to its artifact receipt")
    })?;
    let mut bytes = Vec::new();
    std::fs::File::open(supplied)?
        .take(4097)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(std::io::Error::other(
            "sandbox helper receipt exceeds 4096 bytes",
        ));
    }
    let identity: rw_tools::ExecutableArtifactIdentity =
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    rw_tools::SandboxHelper::from_artifact(&identity)
        .map_err(|error| std::io::Error::other(format!("sandbox helper receipt rejected: {error}")))
}
