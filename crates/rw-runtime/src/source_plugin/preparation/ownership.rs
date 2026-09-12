//! A launched helper's ownership cannot disappear with its async caller.
use super::{
    Arc, CompletedPreparation, Mutex, Ordering, OwnedSemaphorePermit, PrivateMcpScratch, Result,
    SourcePreparations, miette,
};
use rw_ext::SupervisedPluginProcess;
use rw_tools::CancellationToken;
use tokio::sync::Notify;

type Settlement = std::result::Result<(), Arc<str>>;

pub(super) struct Operation {
    pub cancellation: CancellationToken,
    outcome: Mutex<Option<Settlement>>,
    changed: Notify,
}
impl Operation {
    pub fn new() -> Self {
        Self {
            cancellation: CancellationToken::default(),
            outcome: Mutex::default(),
            changed: Notify::new(),
        }
    }
    pub async fn wait(&self) -> Settlement {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(outcome) = self
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return outcome;
            }
            changed.await;
        }
    }
    fn publish(&self, result: Settlement) {
        *self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        self.changed.notify_waiters();
    }
}

pub(super) struct Ownership {
    pool: Arc<SourcePreparations>,
    pub operation: Arc<Operation>,
    scratch: Option<Arc<PrivateMcpScratch>>,
    #[cfg(target_os = "linux")]
    pub view_directory: Option<tempfile::TempDir>,
    pub process: Option<Arc<dyn SupervisedPluginProcess>>,
    admission: Option<OwnedSemaphorePermit>,
    pub execution: Option<OwnedSemaphorePermit>,
    pub proof_required: bool,
    complete: bool,
}
impl Ownership {
    pub fn new(
        pool: Arc<SourcePreparations>,
        operation: Arc<Operation>,
        admission: OwnedSemaphorePermit,
        scratch: Arc<PrivateMcpScratch>,
    ) -> Self {
        Self {
            pool,
            operation,
            scratch: Some(scratch),
            #[cfg(target_os = "linux")]
            view_directory: None,
            process: None,
            admission: Some(admission),
            execution: None,
            proof_required: false,
            complete: false,
        }
    }
    pub fn complete<T>(&mut self, result: Result<T>) -> CompletedPreparation<T> {
        if self.proof_required {
            return CompletedPreparation {
                result: result.and_then(|_| Err(miette!("source helper settlement is unproven"))),
                _admission: None,
            };
        }
        self.execution.take();
        self.scratch.take();
        #[cfg(target_os = "linux")]
        self.view_directory.take();
        self.pool
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|job| !Arc::ptr_eq(job, &self.operation));
        self.complete = true;
        self.operation.publish(Ok(()));
        CompletedPreparation {
            result,
            _admission: self.admission.take(),
        }
    }
}
impl Drop for Ownership {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        self.pool.closed.store(true, Ordering::Release);
        for operation in self
            .pool
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            operation.cancellation.cancel();
        }
        if let Some(process) = self.process.take() {
            let _ = process.kill_tree();
            std::mem::forget(process);
        }
        #[cfg(target_os = "linux")]
        if let Some(directory) = self.view_directory.take() {
            std::mem::forget(directory);
        }
        if let Some(scratch) = self.scratch.take() {
            std::mem::forget(scratch);
        }
        if let Some(permit) = self.admission.take() {
            permit.forget();
        }
        if let Some(permit) = self.execution.take() {
            permit.forget();
        }
        self.operation.publish(Err(Arc::from(
            "source preparation owner lost its settlement proof",
        )));
    }
}
