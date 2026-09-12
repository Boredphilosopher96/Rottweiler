//! One physical Tokio worker shape serves typed admitted operations.
use super::{AdmissionError, Pool, ResourceClass, ResourceLease, WorkError};
use std::sync::{Arc, Mutex};

pub(super) async fn run<T: Send + 'static>(
    pool: &Pool,
    class: ResourceClass,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, WorkError> {
    // The private result slot is not a second completion channel. Only the
    // physical Tokio join proves settlement and preserves a worker panic.
    let output = Arc::new(Mutex::new(None));
    let destination = Arc::clone(&output);
    execute(
        pool,
        class,
        Box::new(move || {
            let result = work();
            *destination
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        }),
    )
    .await?;
    // The worker closure has been destroyed before its join completes. No
    // caller or worker can retain an independent alias to this private slot.
    Arc::try_unwrap(output)
        .map_err(|_| WorkError::ResultUnavailable)?
        .into_inner()
        .map_err(|_| WorkError::ResultUnavailable)?
        .ok_or(WorkError::ResultUnavailable)
}

async fn execute(
    pool: &Pool,
    class: ResourceClass,
    work: Box<dyn FnOnce() + Send>,
) -> Result<(), WorkError> {
    let lease = admit(pool, class).await?;
    let span = tracing::Span::current();
    Ok(tokio::task::spawn_blocking(move || {
        let _lease = lease;
        // Dropping a cancelled caller leaves both work and its typed result in
        // this physical scope, before the execution lease can be returned.
        span.in_scope(work);
    })
    .await?)
}

#[tracing::instrument(target = "rw_performance", level = "trace", name = "resource.admission_wait", skip(pool), fields(?class))]
async fn admit(pool: &Pool, class: ResourceClass) -> Result<ResourceLease, AdmissionError> {
    pool.acquire(std::future::pending()).await
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod measurement;
