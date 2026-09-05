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
