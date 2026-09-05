//! Application-owned helper reuse. Callers own cancellation; jobs own cleanup.
use super::{
    Arc, AsyncReadExt, AsyncWriteExt, Duration, MAX_WASM_HOST_HEADER_BYTES,
    MAX_WASM_HOST_RESPONSE_BYTES, PathBuf, PluginManifest, Stdio, WasmHookHostError,
    WasmHookLimits, WasmHostRequest, WasmHostResponse, helper_deadline_error, io_error,
};
use futures_util::FutureExt as _;
use rw_tools::CancellationToken;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_ADMITTED: usize = 32;
const DEFAULT_WORKERS: usize = 2;

pub(super) struct Generation {
    pub helper: PathBuf,
    pub manifest: PluginManifest,
    pub component: Arc<[u8]>,
    pub limits: WasmHookLimits,
    pub digest: blake3::Hash,
    pub jobs: Mutex<Vec<Arc<JobState>>>,
}

pub(super) struct JobState {
    cancellation: CancellationToken,
    complete: AtomicBool,
    done: tokio::sync::Notify,
}

impl Generation {
    pub(super) async fn settle(&self) {
        let pending: Vec<_> = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|job| job.cancellation.is_cancelled())
            .cloned()
            .collect();
        for job in pending {
            loop {
                let notified = job.done.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if job.complete.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        }
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

    /// Closes admission and waits for all admitted jobs and helpers to settle.
    pub async fn shutdown(&self) {
        self.stopping.cancel();
        // Existing jobs retain admission through cleanup, including caller drop.
        let Ok(_all) = Arc::clone(&self.admission).acquire_many_owned(32).await else {
            return;
        };
        self.admission.close();
        let workers = std::mem::take(
            &mut *self
                .idle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for mut worker in workers {
            worker.retire().await;
        }
    }

    pub(super) async fn request(
        self: &Arc<Self>,
        generation: Arc<Generation>,
        call: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<WasmHostResponse, WasmHookHostError> {
        if self.stopping.is_cancelled() {
            return Err(unavailable());
        }
        let admission = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| unavailable())?;
        let cancellation = CancellationToken::default();
        let job = Arc::new(JobState {
            cancellation: cancellation.clone(),
            complete: AtomicBool::new(false),
            done: tokio::sync::Notify::new(),
        });
        generation
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::clone(&job));
        let guard = CancelOnDrop(cancellation.clone());
        let pool = Arc::clone(self);
        let deadline = tokio::time::Instant::now() + timeout;
        let (send, receive) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = pool
                .run(
                    Arc::clone(&generation),
                    call,
                    deadline,
                    &cancellation,
                    admission,
                )
                .await;
            job.complete.store(true, Ordering::Release);
            job.done.notify_waiters();
            generation
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|entry| !Arc::ptr_eq(entry, &job));
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
        _admission: OwnedSemaphorePermit,
    ) -> Result<WasmHostResponse, WasmHookHostError> {
        let _slot = tokio::select! {
            biased;
            () = self.stopping.cancelled() => return Err(unavailable()),
            () = cancellation.cancelled() => return Err(cancelled()),
            () = tokio::time::sleep_until(deadline) => return Err(helper_deadline_error()),
            slot = self.execution.acquire() => slot.map_err(|_| unavailable())?,
        };
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
        let mut worker = if let Some(worker) = cached {
            worker
        } else {
            self.starts.fetch_add(1, Ordering::Relaxed);
            Worker::start(&generation.helper)?
        };
        let exchange = async {
            if worker.helper != generation.helper {
                worker.retire().await;
                self.starts.fetch_add(1, Ordering::Relaxed);
                worker = Worker::start(&generation.helper)?;
            }
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
                if loaded != WasmHostResponse::Valid {
                    return Ok(loaded);
                }
                worker.digest = Some(generation.digest);
            }
            if let Some((event, input)) = call {
                worker
                    .exchange(&WasmHostRequest::Invoke { event, input }, &[])
                    .await
            } else {
                Ok(WasmHostResponse::Valid)
            }
        };
        let outcome = tokio::select! {
            biased;
            () = self.stopping.cancelled() => Err(unavailable()),
            () = cancellation.cancelled() => Err(cancelled()),
            () = tokio::time::sleep_until(deadline) => Err(helper_deadline_error()),
            result = std::panic::AssertUnwindSafe(exchange).catch_unwind() => result.unwrap_or_else(|_| Err(unavailable())),
        };
        if outcome
            .as_ref()
            .is_ok_and(|response| !matches!(response, WasmHostResponse::Error { .. }))
            && !self.stopping.is_cancelled()
            && !cancellation.is_cancelled()
        {
            self.idle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(worker);
        } else {
            worker.retire().await;
        }
        outcome
    }
}

impl Drop for WasmWorkerPool {
    fn drop(&mut self) {
        let workers = std::mem::take(
            self.idle
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for mut worker in workers {
                    worker.retire().await;
                }
            });
        }
        // Without a runtime, kill_on_drop revokes the private helper. Explicit
        // shutdown is the awaitable cleanup boundary for embedders.
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Worker {
    helper: PathBuf,
    child: tokio::process::Child,
    digest: Option<blake3::Hash>,
}
impl Worker {
    fn start(helper: &std::path::Path) -> Result<Self, WasmHookHostError> {
        let child = tokio::process::Command::new(helper)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| io_error(&error))?;
        Ok(Self {
            helper: helper.to_owned(),
            child,
            digest: None,
        })
    }
    fn matches(&self, generation: &Generation) -> bool {
        self.helper == generation.helper && self.digest == Some(generation.digest)
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
    async fn retire(&mut self) {
        let _ = self.child.start_kill();
        // Keep the worker/admission permits until the actual reap. A deadline
        // bounds useful execution; it cannot manufacture resource settlement.
        if self.child.wait().await.is_err() {
            std::future::pending::<()>().await;
        }
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
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn generation(helper: PathBuf) -> Arc<Generation> {
        Arc::new(Generation {
            helper,
            manifest: PluginManifest {
                name: "pool-test".to_owned(),
                version: "1.0.0".to_owned(),
                protocol: rw_plugin_protocol::PROTOCOL_VERSION,
                capabilities: rw_plugin_protocol::PluginCapabilities::default(),
            },
            component: Arc::from(&b"component"[..]),
            limits: WasmHookLimits::default(),
            digest: blake3::hash(b"test"),
            jobs: Mutex::default(),
        })
    }

    #[tokio::test]
    async fn saturation_is_bounded_before_starting_or_copying_components() {
        let pool = WasmWorkerPool::capacity(1);
        let held = pool
            .execution
            .acquire()
            .await
            .expect("hold worker admission");
        let generation = generation(PathBuf::from("unused-helper"));
        let mut tasks = Vec::new();
        for _ in 0..MAX_ADMITTED {
            let pool = pool.clone();
            let generation = generation.clone();
            tasks.push(tokio::spawn(async move {
                pool.request(generation, None, Duration::from_secs(5)).await
            }));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while pool.admission.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all slots admitted");
        assert!(
            pool.request(generation.clone(), None, Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(pool.stats().process_starts, 0);
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        generation.settle().await;
        assert_eq!(pool.admission.available_permits(), MAX_ADMITTED);
        assert!(generation.jobs.lock().expect("jobs").is_empty());
        drop(held);
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn dropped_caller_reaps_worker_before_settlement_and_releases_its_slot() {
        let root = tempfile::tempdir().expect("fixture");
        let path = root.path().join("helper");
        let pid_path = root.path().join("pid");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nwhile :; do :; done\n",
                pid_path.display()
            ),
        )
        .expect("helper");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        let pool = WasmWorkerPool::capacity(1);
        let generation = generation(path);
        let call = {
            let pool = pool.clone();
            let generation = generation.clone();
            tokio::spawn(
                async move { pool.request(generation, None, Duration::from_secs(5)).await },
            )
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !pid_path.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("helper started");
        call.abort();
        let _ = call.await;
        tokio::time::timeout(Duration::from_secs(2), generation.settle())
            .await
            .expect("owned cleanup");
        let pid = std::fs::read_to_string(pid_path).expect("pid");
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("probe");
        assert!(!alive.success(), "settlement requires a reaped child");
        assert_eq!(pool.execution.available_permits(), 1);
        assert_eq!(pool.admission.available_permits(), MAX_ADMITTED);
        assert!(generation.jobs.lock().expect("jobs").is_empty());
        let timeout = pool
            .request(generation.clone(), None, Duration::from_millis(100))
            .await
            .expect_err("fixed deadline");
        assert!(timeout.to_string().contains("deadline"));
        let pid = std::fs::read_to_string(root.path().join("pid")).expect("replacement pid");
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("probe replacement");
        assert!(!alive.success(), "timeout also waits for reap");
        assert_eq!(pool.execution.available_permits(), 1);
        pool.shutdown().await;
    }
}
