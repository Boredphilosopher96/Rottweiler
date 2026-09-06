//! Application-owned helper reuse. Callers own cancellation; jobs own cleanup.
use super::{
    Arc, AsyncReadExt, AsyncWriteExt, Duration, MAX_WASM_HOST_HEADER_BYTES,
    MAX_WASM_HOST_RESPONSE_BYTES, PluginManifest, Stdio, WasmHookHostError, WasmHookLimits,
    WasmHostRequest, WasmHostResponse, helper_deadline_error, io_error,
};
use futures_util::FutureExt as _;
use rw_tools::{ApprovedExecutable, CancellationToken, ExecutableLaunch};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

mod ownership;
use ownership::{JobOwner, JobState, settlement_error};

const MAX_ADMITTED: usize = 32;
const DEFAULT_WORKERS: usize = 2;
const WORKER_RETIREMENT_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Generation {
    pub helper: ApprovedExecutable,
    pub manifest: PluginManifest,
    pub component: Arc<[u8]>,
    pub limits: WasmHookLimits,
    pub digest: blake3::Hash,
    pub jobs: Mutex<Vec<Arc<JobState>>>,
}

impl Generation {
    pub(super) async fn settle(&self) -> Result<(), WasmHookHostError> {
        let pending: Vec<_> = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|job| job.cancellation.is_cancelled())
            .cloned()
            .collect();
        let mut failure = None;
        for job in pending {
            if let Err(error) = job.settle().await {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), |error| Err(settlement_error(&error)))
    }
}

/// Bounded local diagnostics; these counters contain no guest payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WasmWorkerStats {
    pub process_starts: u64,
    pub component_loads: u64,
    pub cache_hits: u64,
}

/// Shared by every session in one application host. Construction starts no process.
pub struct WasmWorkerPool {
    admission: Arc<Semaphore>,
    execution: Arc<Semaphore>,
    idle: Mutex<Vec<Worker>>,
    quarantined: Mutex<Vec<Worker>>,
    jobs: Mutex<Vec<Arc<JobState>>>,
    failure: Mutex<Option<Arc<str>>>,
    shutdown: tokio::sync::Mutex<()>,
    stopping: CancellationToken,
    starts: AtomicU64,
    loads: AtomicU64,
    hits: AtomicU64,
}

impl WasmWorkerPool {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::capacity(DEFAULT_WORKERS)
    }

    /// Constructs a measured application policy with one or two workers.
    ///
    /// # Errors
    /// Rejects a process count outside the bounded policy range.
    pub fn with_worker_limit(workers: usize) -> Result<Arc<Self>, WasmHookHostError> {
        if !(1..=2).contains(&workers) {
            return Err(unavailable());
        }
        Ok(Self::capacity(workers))
    }

    fn capacity(workers: usize) -> Arc<Self> {
        Arc::new(Self {
            admission: Arc::new(Semaphore::new(MAX_ADMITTED)),
            execution: Arc::new(Semaphore::new(workers)),
            idle: Mutex::new(Vec::with_capacity(workers)),
            quarantined: Mutex::default(),
            jobs: Mutex::default(),
            failure: Mutex::default(),
            shutdown: tokio::sync::Mutex::new(()),
            stopping: CancellationToken::default(),
            starts: AtomicU64::new(0),
            loads: AtomicU64::new(0),
            hits: AtomicU64::new(0),
        })
    }

    #[must_use]
    pub fn stats(&self) -> WasmWorkerStats {
        WasmWorkerStats {
            process_starts: self.starts.load(Ordering::Relaxed),
            component_loads: self.loads.load(Ordering::Relaxed),
            cache_hits: self.hits.load(Ordering::Relaxed),
        }
    }

    /// Returns actual idle worker identities for local process diagnostics.
    #[must_use]
    pub fn idle_process_ids(&self) -> Vec<u32> {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|worker| worker.child.id())
            .collect()
    }

    /// Closes admission and waits for all admitted jobs and helpers to settle.
    ///
    /// # Errors
    /// Reports failed ownership or reap proof. Failed capacity is never reused.
    pub async fn shutdown(&self) -> Result<(), WasmHookHostError> {
        let _shutdown = self.shutdown.lock().await;
        let jobs = {
            let jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.stopping.cancel();
            self.admission.close();
            jobs.clone()
        };
        for job in jobs {
            let _ = job.settle().await;
        }
        let workers = {
            let mut workers = std::mem::take(
                &mut *self
                    .idle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            workers.append(
                &mut *self
                    .quarantined
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            workers
        };
        // This guard preserves ownership even when the shutdown future is dropped.
        let mut retirement = ownership::Retirement::new(self, workers);
        retirement.settle().await;
        let failure = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        failure
            .as_ref()
            .map_or(Ok(()), |error| Err(settlement_error(error)))
    }

    fn fail(&self, error: Arc<str>) {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(error);
        self.stopping.cancel();
        self.admission.close();
        self.execution.close();
    }

    pub(super) async fn request(
        self: &Arc<Self>,
        generation: Arc<Generation>,
        call: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<WasmHostResponse, WasmHookHostError> {
        let (mut owner, cancellation) = {
            let mut jobs = self
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.stopping.is_cancelled() {
                return Err(unavailable());
            }
            let admission = Arc::clone(&self.admission)
                .try_acquire_owned()
                .map_err(|_| unavailable())?;
            let job = Arc::new(JobState::new());
            jobs.push(Arc::clone(&job));
            generation
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::clone(&job));
            let cancellation = job.cancellation.clone();
            (
                JobOwner::new(Arc::clone(self), Arc::clone(&generation), job, admission),
                cancellation,
            )
        };
        let guard = CancelOnDrop(cancellation.clone());
        let pool = Arc::clone(self);
        let deadline = tokio::time::Instant::now() + timeout;
        let (send, receive) = tokio::sync::oneshot::channel();
        // Ownership exists before spawn, including an unpolled task's drop path.
        tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(pool.run(
                generation,
                call,
                deadline,
                &cancellation,
                &mut owner,
            ))
            .catch_unwind()
            .await;
            let result = match result {
                Ok(result) => {
                    owner.finish();
                    result
                }
                Err(_) => Err(settlement_error("WASM job owner panicked")),
            };
            drop(owner);
            let _ = send.send(result);
        });
        let result = receive.await.map_err(|_| unavailable())?;
        drop(guard);
        result
    }

    async fn run(
        &self,
        generation: Arc<Generation>,
        call: Option<(String, String)>,
        deadline: tokio::time::Instant,
        cancellation: &CancellationToken,
        owner: &mut JobOwner,
    ) -> Result<WasmHostResponse, WasmHookHostError> {
        owner.execution = Some(tokio::select! {
            biased;
            () = self.stopping.cancelled() => return Err(unavailable()),
            () = cancellation.cancelled() => return Err(cancelled()),
            () = tokio::time::sleep_until(deadline) => return Err(helper_deadline_error()),
            slot = Arc::clone(&self.execution).acquire_owned() => slot.map_err(|_| unavailable())?,
        });
        let cached = {
            let mut idle = self
                .idle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let matching = idle.iter().position(|worker| worker.matches(&generation));
            matching.map(|index| idle.swap_remove(index)).or_else(|| {
                if idle.len() <= self.execution.available_permits() {
                    // An unused slot can retain another generation instead of
                    // evicting every sequential hook after the first plugin.
                    None
                } else {
                    Some(idle.remove(0))
                }
            })
        };
        owner.worker = Some(if let Some(worker) = cached {
            worker
        } else {
            self.starts.fetch_add(1, Ordering::Relaxed);
            Worker::start(&generation.helper)?
        });
        let exchange = async {
            if owner
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.executable.matches(&generation.helper))
            {
                owner.retire_worker().await?;
                self.starts.fetch_add(1, Ordering::Relaxed);
                owner.worker = Some(Worker::start(&generation.helper)?);
            }
            let worker = owner.worker.as_mut().ok_or_else(unavailable)?;
            if worker.matches(&generation) {
                self.hits.fetch_add(1, Ordering::Relaxed);
            } else {
                self.loads.fetch_add(1, Ordering::Relaxed);
                let loaded = worker
                    .exchange(
                        &WasmHostRequest::Load {
                            manifest: Box::new(generation.manifest.clone()),
                            limits: generation.limits,
                        },
                        &generation.component,
                    )
                    .await?;
                if loaded != (WasmHostResponse::Valid {}) {
                    return Ok(loaded);
                }
                worker.digest = Some(generation.digest);
            }
            if let Some((event, input)) = call {
                worker
                    .exchange(&WasmHostRequest::Invoke { event, input }, &[])
                    .await
            } else {
                Ok(WasmHostResponse::Valid {})
            }
        };
        let outcome = tokio::select! {
            biased;
            () = self.stopping.cancelled() => Err(unavailable()),
            () = cancellation.cancelled() => Err(cancelled()),
            () = tokio::time::sleep_until(deadline) => Err(helper_deadline_error()),
            result = exchange => result,
        };
        if outcome
            .as_ref()
            .is_ok_and(|response| !matches!(response, WasmHostResponse::Error { .. }))
            && !self.stopping.is_cancelled()
            && !cancellation.is_cancelled()
        {
            if let Some(worker) = owner.worker.take() {
                self.idle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(worker);
            }
        } else {
            owner.retire_worker().await?;
        }
        outcome
    }
}

impl Drop for WasmWorkerPool {
    fn drop(&mut self) {
        let mut workers = std::mem::take(
            self.idle
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        workers.append(
            self.quarantined
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        // The explicit shutdown boundary reports proof. This fallback retains
        // handles if no executor exists or its cleanup task is itself dropped.
        let mut workers = ownership::DetachedRetirement(workers);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                workers.settle().await;
            });
        }
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Worker {
    executable: ExecutableLaunch,
    child: tokio::process::Child,
    digest: Option<blake3::Hash>,
}
impl Worker {
    fn start(helper: &ApprovedExecutable) -> Result<Self, WasmHookHostError> {
        let executable = helper
            .launch()
            .map_err(|error| WasmHookHostError::Execution {
                message: error.to_string(),
            })?;
        let child = tokio::process::Command::new(executable.path())
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| io_error(&error))?;
        Ok(Self {
            executable,
            child,
            digest: None,
        })
    }
    fn matches(&self, generation: &Generation) -> bool {
        self.executable.matches(&generation.helper) && self.digest == Some(generation.digest)
    }
    async fn exchange(
        &mut self,
        request: &WasmHostRequest,
        component: &[u8],
    ) -> Result<WasmHostResponse, WasmHookHostError> {
        let header = serde_json::to_vec(request).map_err(|error| WasmHookHostError::Execution {
            message: error.to_string(),
        })?;
        if header.len() > MAX_WASM_HOST_HEADER_BYTES {
            return Err(WasmHookHostError::InputTooLarge {
                limit: MAX_WASM_HOST_HEADER_BYTES,
            });
        }
        let header_len = u32::try_from(header.len()).map_err(|_| unavailable())?;
        let component_len = u32::try_from(component.len()).map_err(|_| unavailable())?;
        let stdin = self.child.stdin.as_mut().ok_or_else(unavailable)?;
        stdin
            .write_all(&header_len.to_be_bytes())
            .await
            .map_err(|error| io_error(&error))?;
        stdin
            .write_all(&component_len.to_be_bytes())
            .await
            .map_err(|error| io_error(&error))?;
        stdin
            .write_all(&header)
            .await
            .map_err(|error| io_error(&error))?;
        stdin
            .write_all(component)
            .await
            .map_err(|error| io_error(&error))?;
        stdin.flush().await.map_err(|error| io_error(&error))?;
        let stdout = self.child.stdout.as_mut().ok_or_else(unavailable)?;
        let response_len = stdout.read_u32().await.map_err(|error| io_error(&error))? as usize;
        if response_len > MAX_WASM_HOST_RESPONSE_BYTES {
            return Err(WasmHookHostError::OutputTooLarge {
                limit: MAX_WASM_HOST_RESPONSE_BYTES,
            });
        }
        let mut bytes = vec![0; response_len];
        stdout
            .read_exact(&mut bytes)
            .await
            .map_err(|error| io_error(&error))?;
        serde_json::from_slice(&bytes).map_err(|error| WasmHookHostError::InvalidDirective {
            message: error.to_string(),
        })
    }
    async fn retire(&mut self) -> Result<(), WasmHookHostError> {
        let _ = self.child.start_kill();
        reap_before(
            self.child.wait(),
            tokio::time::Instant::now() + WORKER_RETIREMENT_TIMEOUT,
        )
        .await
    }
}
async fn reap_before(
    wait: impl std::future::Future<Output = std::io::Result<std::process::ExitStatus>>,
    deadline: tokio::time::Instant,
) -> Result<(), WasmHookHostError> {
    match tokio::time::timeout_at(deadline, wait).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(settlement_error(&format!(
            "private WASM helper reap failed: {error}"
        ))),
        Err(_) => Err(settlement_error(
            "private WASM helper retirement proof deadline expired",
        )),
    }
}

fn unavailable() -> WasmHookHostError {
    WasmHookHostError::Execution {
        message: "private WASM worker admission is unavailable".to_owned(),
    }
}
fn cancelled() -> WasmHookHostError {
    WasmHookHostError::Execution {
        message: "private WASM hook was cancelled".to_owned(),
    }
}

#[cfg(all(test, unix))]
#[path = "pool/tests.rs"]
mod tests;
