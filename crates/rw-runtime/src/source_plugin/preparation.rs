//! Source helper ownership outlives the caller that requested its work.

use super::MAX_REPORT_BYTES;
use crate::extension_runtime::PrivateMcpScratch;
use futures_util::FutureExt as _;
use miette::{Result, miette};
use rw_ext::{PluginLauncher, PluginProcessConfig, PluginSandboxMode, PluginSandboxProfile};
use rw_plugin_protocol::PluginCapabilities;
use rw_tools::CancellationToken;
use std::panic::AssertUnwindSafe;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    io::AsyncReadExt as _,
    sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot},
    time::Instant,
};

const MAX_PREPARATIONS: usize = 32;
const CONCURRENT_PREPARATIONS: usize = 2;

pub(crate) struct SourcePreparationBudget {
    admission: Arc<Semaphore>,
    execution: Arc<Semaphore>,
}
impl Default for SourcePreparationBudget {
    fn default() -> Self {
        Self {
            admission: Arc::new(Semaphore::new(MAX_PREPARATIONS)),
            execution: Arc::new(Semaphore::new(CONCURRENT_PREPARATIONS)),
        }
    }
}
#[derive(Default)]
pub(crate) struct SourcePreparations {
    budget: Arc<SourcePreparationBudget>,
    jobs: Mutex<Vec<Arc<Operation>>>,
}
impl SourcePreparations {
    pub(crate) fn new(budget: Arc<SourcePreparationBudget>) -> Self {
        Self {
            budget,
            jobs: Mutex::new(Vec::new()),
        }
    }
}
pub(super) struct PreparationRequest {
    pub config: PluginProcessConfig,
    pub output_root: Option<std::path::PathBuf>,
    pub launcher: Arc<dyn PluginLauncher>,
    pub scratch: Arc<PrivateMcpScratch>,
}
#[derive(Debug)]
pub(super) struct PreparationOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Option<i32>,
}
struct CompletedPreparation {
    result: Result<PreparationOutput>,
    _admission: Option<OwnedSemaphorePermit>,
}
struct Operation {
    cancellation: CancellationToken,
    complete: AtomicBool,
    changed: Notify,
}
impl Operation {
    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.complete.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
}
struct CancelOnDrop(Option<Arc<Operation>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(operation) = &self.0 {
            operation.cancellation.cancel();
        }
    }
}
// An aborted/panicking executor is not proof that launched native work stopped.
struct Ownership {
    scratch: Option<Arc<PrivateMcpScratch>>,
    #[cfg(target_os = "linux")]
    view_directory: Option<tempfile::TempDir>,
    admission: Option<OwnedSemaphorePermit>,
    execution: Option<OwnedSemaphorePermit>,
    settled: bool,
}
impl Drop for Ownership {
    fn drop(&mut self) {
        if !self.settled {
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
        }
    }
}
impl SourcePreparations {
    pub(super) async fn execute(
        self: &Arc<Self>,
        request: PreparationRequest,
        deadline: Instant,
    ) -> Result<PreparationOutput> {
        let admission = Arc::clone(&self.budget.admission)
            .try_acquire_owned()
            .map_err(|_| miette!("TypeScript preparation admission is exhausted"))?;
        let operation = Arc::new(Operation {
            cancellation: CancellationToken::default(),
            complete: AtomicBool::new(false),
            changed: Notify::new(),
        });
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::clone(&operation));
        let mut guard = CancelOnDrop(Some(Arc::clone(&operation)));
        let pool = Arc::clone(self);
        let (send, receive) = oneshot::channel();
        tokio::spawn(async move {
            let mut ownership = Ownership {
                scratch: Some(Arc::clone(&request.scratch)),
                #[cfg(target_os = "linux")]
                view_directory: None,
                admission: Some(admission),
                execution: None,
                settled: false,
            };
            let outcome = AssertUnwindSafe(pool.run(
                request,
                &operation.cancellation,
                deadline,
                &mut ownership,
            ))
            .catch_unwind()
            .await;
            let Ok(outcome) = outcome else {
                tracing::error!("source preparation panicked; settlement remains unproven");
                std::future::pending::<()>().await;
                return;
            };
            ownership.settled = true;
            let completed = CompletedPreparation {
                result: outcome,
                _admission: ownership.admission.take(),
            };
            drop(ownership);
            pool.jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|job| !Arc::ptr_eq(job, &operation));
            operation.complete.store(true, Ordering::Release);
            operation.changed.notify_waiters();
            let _ = send.send(completed);
        });
        let outcome = receive
            .await
            .map_err(|_| miette!("source preparation ownership was interrupted"))?;
        guard.0 = None;
        outcome.result
    }
    async fn run(
        &self,
        request: PreparationRequest,
        cancellation: &CancellationToken,
        deadline: Instant,
        ownership: &mut Ownership,
    ) -> Result<PreparationOutput> {
        ownership.execution = Some(tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(miette!("TypeScript preparation was cancelled")),
            () = tokio::time::sleep_until(deadline) => return Err(miette!("TypeScript preparation exceeded its deadline")),
            permit = Arc::clone(&self.budget.execution).acquire_owned() => permit.map_err(|_| miette!("TypeScript preparation admission closed"))?,
        });
        #[cfg(target_os = "linux")]
        let mode = {
            let directory = tempfile::Builder::new()
                .prefix("preparation-")
                .tempdir_in(request.scratch.path())
                .map_err(|error| miette!(error.to_string()))?;
            let work = directory.path().join("work");
            let mount = directory.path().join("view");
            std::fs::create_dir(&work).map_err(|error| miette!(error.to_string()))?;
            std::fs::create_dir(&mount).map_err(|error| miette!(error.to_string()))?;
            let filesystem = rw_tools::PreparationFilesystem::new(
                request.config.cwd(),
                &work,
                &mount,
                request.output_root.as_deref(),
            )
            .map_err(|error| miette!(error.to_string()))?;
            ownership.view_directory = Some(directory);
            PluginSandboxMode::Preparation {
                filesystem: Box::new(filesystem),
            }
        };
        #[cfg(not(target_os = "linux"))]
        let mode = {
            let _ = &request.output_root;
            PluginSandboxMode::Preparation {}
        };
        let child = request
            .launcher
            .launch(
                &request.config,
                &PluginSandboxProfile {
                    mode,
                    capabilities: PluginCapabilities::default(),
                    approved_roots: Vec::new(),
                    allowed_domains: Vec::new(),
                },
            )
            .await
            .map_err(|error| miette!(error.to_string()))?;
        let process = Arc::clone(&child.process);
        drop(child.stdin);
        // Pipe readers stay in this owned task. Revocation drops both readers
        // before process-tree settlement, and no output task is detached.
        let outcome = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(miette!("TypeScript preparation was cancelled")),
            () = tokio::time::sleep_until(deadline) => Err(miette!("TypeScript preparation exceeded its deadline")),
            result = async {
                let (stdout, stderr, status) = tokio::try_join!(
                    read_output(child.stdout),
                    read_output(child.stderr),
                    async { process.wait().await.map_err(|error| miette!(error.to_string())) },
                )?;
                Ok(PreparationOutput { stdout, stderr, status })
            } => result,
        };
        // Signal errors alone are diagnostic. The process owner's awaited proof
        // handles already-exited leaders and zombie-only groups on macOS.
        if let Err(error) = process.kill_tree() {
            tracing::debug!(%error, "source helper termination signal failed");
        }
        if let Err(error) = process.settle_effects().await {
            tracing::error!(%error, "source helper settlement remains unproven");
            std::future::pending::<()>().await;
        }
        // Keep private staging alive until every helper effect has settled.
        drop(request.scratch);
        outcome
    }
    pub(crate) async fn settle_cancelled(&self) {
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|job| job.cancellation.is_cancelled())
            .cloned()
            .collect::<Vec<_>>();
        for job in jobs {
            job.wait().await;
        }
    }
}

async fn read_output(stream: rw_ext::PluginStdout) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    stream
        .take(MAX_REPORT_BYTES.saturating_add(1))
        .read_to_end(&mut output)
        .await
        .map_err(|error| miette!(error.to_string()))?;
    if output.len() as u64 > MAX_REPORT_BYTES {
        return Err(miette!("TypeScript plugin host output exceeded its bound"));
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests;
