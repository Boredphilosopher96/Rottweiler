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
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    time::Instant,
};

mod io;
mod ownership;
use ownership::{Operation, Ownership};

const MAX_PREPARATIONS: usize = 32;
const CONCURRENT_PREPARATIONS: usize = 2;
const PREPARATION_SETTLEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    closed: AtomicBool,
}
impl SourcePreparations {
    pub(crate) fn new(budget: Arc<SourcePreparationBudget>) -> Self {
        Self {
            budget,
            jobs: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
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
struct CompletedPreparation<T = PreparationOutput> {
    result: Result<T>,
    _admission: Option<OwnedSemaphorePermit>,
}
struct CancelOnDrop(Option<Arc<Operation>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(operation) = &self.0 {
            operation.cancellation.cancel();
        }
    }
}
impl SourcePreparations {
    pub(super) async fn execute(
        self: &Arc<Self>,
        request: PreparationRequest,
        deadline: Instant,
    ) -> Result<PreparationOutput> {
        let mut ownership = {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.closed.load(Ordering::Acquire) {
                return Err(miette!("TypeScript preparation generation is closed"));
            }
            let admission = Arc::clone(&self.budget.admission)
                .try_acquire_owned()
                .map_err(|_| miette!("TypeScript preparation admission is exhausted"))?;
            let operation = Arc::new(Operation::new());
            jobs.push(Arc::clone(&operation));
            Ownership::new(
                Arc::clone(self),
                operation,
                admission,
                Arc::clone(&request.scratch),
            )
        };
        let operation = Arc::clone(&ownership.operation);
        let mut guard = CancelOnDrop(Some(Arc::clone(&operation)));
        let pool = Arc::clone(self);
        let (send, receive) = oneshot::channel();
        tokio::spawn(async move {
            let outcome = AssertUnwindSafe(pool.run(
                request,
                &operation.cancellation,
                deadline,
                &mut ownership,
            ))
            .catch_unwind()
            .await;
            let completed = match outcome {
                Ok(outcome) => ownership.complete(outcome),
                Err(_) => CompletedPreparation {
                    result: Err(miette!(
                        "source preparation executor panicked; effects are unsettled"
                    )),
                    _admission: None,
                },
            };
            drop(ownership);
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
            let identity = request.config.executable_identity();
            let executable = rw_tools::PreparationExecutable::from_identity(
                identity.canonical_path.clone(),
                identity.device,
                identity.inode,
                identity.length,
                identity.content_blake3.clone(),
            )
            .map_err(|error| miette!(error.to_string()))?;
            let filesystem = rw_tools::PreparationFilesystem::new(
                request.config.cwd(),
                &work,
                &mount,
                request.output_root.as_deref(),
                executable,
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
        ownership.proof_required = true;
        let child = match request
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
        {
            Ok(child) => child,
            Err(rw_ext::PluginLaunchError::Rejected(error)) => {
                ownership.proof_required = false;
                return Err(miette!(error.to_string()));
            }
            Err(rw_ext::PluginLaunchError::EffectsUnsettled { message }) => {
                return Err(miette!("source launch effects are unsettled: {message}"));
            }
        };
        ownership.process = Some(Arc::clone(&child.process));
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
        match tokio::time::timeout(PREPARATION_SETTLEMENT_TIMEOUT, process.settle_effects()).await {
            Ok(Ok(())) => {
                ownership.proof_required = false;
                ownership.process.take();
            }
            Ok(Err(error)) => return Err(miette!("source helper effects are unsettled: {error}")),
            Err(_) => return Err(miette!("source helper settlement proof deadline expired")),
        }
        // Keep private staging alive until every helper effect has settled.
        drop(request.scratch);
        outcome
    }
    pub(crate) async fn settle_cancelled(&self) -> Result<()> {
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|job| job.cancellation.is_cancelled())
            .cloned()
            .collect::<Vec<_>>();
        let mut failure = None;
        for job in jobs {
            if let Err(error) = job.wait().await {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), |error| {
            Err(miette!("source preparation effects are unsettled: {error}"))
        })
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
