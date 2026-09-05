//! Admission and native ownership survive caller cancellation and task destruction.
use super::{
    Arc, Generation, Mutex, OwnedSemaphorePermit, WasmHookHostError, WasmWorkerPool, Worker,
};
use rw_tools::CancellationToken;

type Settlement = Result<(), Arc<str>>;

pub(in crate::wasm_process) struct JobState {
    pub cancellation: CancellationToken,
    outcome: Mutex<Option<Settlement>>,
    done: tokio::sync::Notify,
}

impl JobState {
    pub fn new() -> Self {
        Self {
            cancellation: CancellationToken::default(),
            outcome: Mutex::default(),
            done: tokio::sync::Notify::new(),
        }
    }

    pub async fn settle(&self) -> Settlement {
        loop {
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return outcome;
            }
            notified.await;
        }
    }

    fn publish(&self, outcome: Settlement) {
        *self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
        self.done.notify_waiters();
    }
}

pub(super) struct JobOwner {
    pool: Arc<WasmWorkerPool>,
    generation: Arc<Generation>,
    job: Arc<JobState>,
    admission: Option<OwnedSemaphorePermit>,
    pub execution: Option<OwnedSemaphorePermit>,
    pub worker: Option<Worker>,
    finished: bool,
}

impl JobOwner {
    pub fn new(
        pool: Arc<WasmWorkerPool>,
        generation: Arc<Generation>,
        job: Arc<JobState>,
        admission: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            pool,
            generation,
            job,
            admission: Some(admission),
            execution: None,
            worker: None,
            finished: false,
        }
    }

    pub async fn retire_worker(&mut self) -> Result<(), WasmHookHostError> {
        if let Some(worker) = self.worker.as_mut() {
            worker.retire().await?;
            self.worker.take();
        }
        Ok(())
    }

    pub fn finish(&mut self) {
        if self.worker.is_some() {
            // A failed reap leaves the worker with this owner for quarantine.
            return;
        }
        self.execution.take();
        self.admission.take();
        self.generation
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|entry| !Arc::ptr_eq(entry, &self.job));
        self.pool
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|entry| !Arc::ptr_eq(entry, &self.job));
        self.finished = true;
        self.job.publish(Ok(()));
    }
}

impl Drop for JobOwner {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let error: Arc<str> = Arc::from("WASM job lost its settlement proof");
        self.job.cancellation.cancel();
        self.pool.fail(Arc::clone(&error));
        if let Some(mut worker) = self.worker.take() {
            let _ = worker.child.start_kill();
            self.pool
                .quarantined
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(worker);
        }
        if let Some(permit) = self.admission.take() {
            permit.forget();
        }
        if let Some(permit) = self.execution.take() {
            permit.forget();
        }
        // Failed records stay charged and discoverable in their generation.
        self.pool
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|entry| !Arc::ptr_eq(entry, &self.job));
        self.job.publish(Err(error));
    }
}

pub(super) struct Retirement<'a> {
    pool: &'a WasmWorkerPool,
    workers: Vec<Worker>,
}

impl<'a> Retirement<'a> {
    pub fn new(pool: &'a WasmWorkerPool, workers: Vec<Worker>) -> Self {
        Self { pool, workers }
    }

    pub async fn settle(&mut self) {
        // Failed workers remain owned while every other retirement is attempted.
        let mut index = 0;
        while index < self.workers.len() {
            match self.workers[index].retire().await {
                Ok(()) => {
                    self.workers.swap_remove(index);
                }
                Err(error) => {
                    self.pool.fail(Arc::from(error.to_string()));
                    index += 1;
                }
            }
        }
    }
}

impl Drop for Retirement<'_> {
    fn drop(&mut self) {
        if !self.workers.is_empty() {
            self.pool.fail(Arc::from(
                "WASM shutdown retained helpers without settlement proof",
            ));
            for worker in &mut self.workers {
                let _ = worker.child.start_kill();
            }
            self.pool
                .quarantined
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .append(&mut self.workers);
        }
    }
}

pub(super) struct DetachedRetirement(pub Vec<Worker>);
impl DetachedRetirement {
    pub async fn settle(&mut self) {
        let mut index = 0;
        while index < self.0.len() {
            if self.0[index].retire().await.is_ok() {
                self.0.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }
}
impl Drop for DetachedRetirement {
    fn drop(&mut self) {
        for mut worker in self.0.drain(..) {
            let _ = worker.child.start_kill();
            std::mem::forget(worker);
        }
    }
}

pub(super) fn settlement_error(message: &str) -> WasmHookHostError {
    WasmHookHostError::Execution {
        message: format!("WASM effects unsettled: {message}"),
    }
}
