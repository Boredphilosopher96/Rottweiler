//! Shared commit admission and ownership until actual worker settlement.
use rw_core::{AdmittedEventBatch, AgentLoopError, EventBatchPlan, EventBatchReservation};
use rw_store::session::journal::JournalAppendPlan;
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinHandle,
};
use tracing::Instrument;

const MAX_BATCHES: usize = 32;
const MAX_BYTES: u32 = 64 * 1024 * 1024;
const MAX_EXECUTING: usize = 4;
const PROOF_TIMEOUT: Duration = Duration::from_secs(30);
type Outcome = Result<Arc<AdmittedEventBatch>, AgentLoopError>;

pub(crate) struct JournalCommits {
    batches: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    execution: Arc<Semaphore>,
    state: Mutex<State>,
    closed: Notify,
    proof_timeout: Duration,
}
struct State {
    closed: bool,
    next_id: u64,
    jobs: BTreeMap<u64, Arc<Completion>>,
    failed: Vec<RetainedCommit>,
}
struct Completion {
    result: Mutex<Option<Result<(), String>>>,
    changed: Notify,
}
struct RetainedCommit {
    _owner: Arc<dyn Send + Sync>,
    batch: Arc<AdmittedEventBatch>,
    _order: OwnedMutexGuard<()>,
    execution: Option<OwnedSemaphorePermit>,
    worker: Option<JoinHandle<Outcome>>,
}
struct JobOwner {
    queue: Arc<JournalCommits>,
    id: u64,
    completion: Arc<Completion>,
    retained: Option<RetainedCommit>,
}
impl Drop for JobOwner {
    fn drop(&mut self) {
        if let Some(retained) = self.retained.take() {
            self.queue.batches.close();
            self.queue.bytes.close();
            self.queue.execution.close();
            let mut state = self
                .queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.failed.push(retained);
            drop(state);
            self.queue.closed.notify_waiters();
            self.completion.finish(Err(
                "journal commit ownership ended without settlement".to_owned()
            ));
        }
    }
}
impl Completion {
    fn finish(&self, result: Result<(), String>) {
        let mut current = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_none() {
            *current = Some(result);
        }
        drop(current);
        self.changed.notify_waiters();
    }
    async fn wait(&self) -> Result<(), String> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}
impl JournalCommits {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            batches: Arc::new(Semaphore::new(MAX_BATCHES)),
            bytes: Arc::new(Semaphore::new(MAX_BYTES as usize)),
            execution: Arc::new(Semaphore::new(MAX_EXECUTING)),
            closed: Notify::new(),
            proof_timeout: PROOF_TIMEOUT,
            state: Mutex::new(State {
                closed: false,
                next_id: 0,
                jobs: BTreeMap::new(),
                failed: Vec::new(),
            }),
        })
    }
    pub(crate) async fn enter(
        &self,
        order: Arc<tokio::sync::Mutex<()>>,
    ) -> Result<OwnedMutexGuard<()>, AgentLoopError> {
        let notified = self.closed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.batches.is_closed() {
            return Err(failure("journal commit admission is closed"));
        }
        tokio::select! {
            guard = order.lock_owned() => Ok(guard),
            () = notified => Err(failure("journal commit admission is closed")),
        }
    }
    #[cfg(test)]
    pub(crate) fn pending_jobs(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .jobs
            .len()
    }

    pub(crate) fn reserve(
        &self,
        plan: &EventBatchPlan,
    ) -> Result<EventBatchReservation, AgentLoopError> {
        let first = plan
            .events()
            .first()
            .and_then(rw_types::EngineEvent::meta)
            .ok_or_else(|| failure("journal batch must contain durable events"))?
            .sequence_id;
        let encoded =
            JournalAppendPlan::measure(first, plan.events()).map_err(|e| failure(e.to_string()))?;
        let bytes = plan
            .retained_bytes()
            .checked_add(encoded.encoded_bytes())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<RetainedCommit>()))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .filter(|bytes| *bytes <= MAX_BYTES)
            .ok_or_else(|| failure("journal batch exceeds its aggregate byte allowance"))?;
        let item = Arc::clone(&self.batches)
            .try_acquire_owned()
            .map_err(|_| failure("journal commit admission is exhausted or closed"))?;
        let bytes = Arc::clone(&self.bytes)
            .try_acquire_many_owned(bytes)
            .map_err(|_| failure("journal commit byte admission is exhausted or closed"))?;
        Ok(EventBatchReservation::new((item, bytes)))
    }
    pub(crate) async fn execute<S, F>(
        self: &Arc<Self>,
        owner: Arc<S>,
        batch: Arc<AdmittedEventBatch>,
        order: OwnedMutexGuard<()>,
        work: F,
    ) -> Outcome
    where
        S: Send + Sync + 'static,
        F: Future<Output = Outcome> + Send + 'static,
    {
        let completion = Arc::new(Completion {
            result: Mutex::new(None),
            changed: Notify::new(),
        });
        let mut guard = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed {
                return Err(failure("journal commit admission is closed"));
            }
            let id = state.next_id;
            state.next_id = id
                .checked_add(1)
                .ok_or_else(|| failure("journal commit identity exhausted"))?;
            state.jobs.insert(id, Arc::clone(&completion));
            JobOwner {
                queue: Arc::clone(self),
                id,
                completion,
                retained: Some(RetainedCommit {
                    _owner: owner,
                    batch,
                    _order: order,
                    execution: None,
                    worker: None,
                }),
            }
        };
        let (send, receive) = oneshot::channel();
        tokio::spawn(async move {
            let span = tracing::trace_span!(target: "rw_performance", "journal.worker_queue", commit_id = guard.id);
            let permit = Arc::clone(&guard.queue.execution)
                .acquire_owned()
                .instrument(span)
                .await;
            let Ok(permit) = permit else {
                let _ = send.send(Err(failure("journal execution admission closed")));
                return;
            };
            let Some(retained) = guard.retained.as_mut() else {
                let _ = send.send(Err(failure("journal commit owner is missing")));
                return;
            };
            retained.execution = Some(permit);
            retained.worker = Some(tokio::spawn(work));
            let Some(worker) = retained.worker.as_mut() else {
                let _ = send.send(Err(failure("journal commit worker is missing")));
                return;
            };
            let result = tokio::time::timeout(guard.queue.proof_timeout, &mut *worker).await;
            let Ok(result) = result else {
                guard.queue.batches.close();
                guard.queue.bytes.close();
                guard.queue.execution.close();
                guard.queue.closed.notify_waiters();
                guard.completion.finish(Err(
                    "journal commit settlement exceeded its deadline".to_owned()
                ));
                let _ = send.send(Err(failure(
                    "journal commit settlement exceeded its deadline",
                )));
                let _ = worker.await;
                return;
            };
            match result {
                Ok(Ok(committed)) if Arc::ptr_eq(&retained.batch, &committed) => {
                    guard.retained.take();
                    // Delivery also releases an abandoned reply before settlement is visible.
                    let _ = send.send(Ok(committed));
                    guard.completion.finish(Ok(()));
                    guard
                        .queue
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .jobs
                        .remove(&guard.id);
                }
                Ok(Err(error)) => {
                    guard.completion.finish(Err(error.to_string()));
                    let _ = send.send(Err(error));
                }
                Ok(Ok(_)) => {
                    let _ = send.send(Err(failure("journal worker substituted its batch")));
                }
                Err(error) => {
                    let _ = send.send(Err(failure(format!("journal worker failed: {error}"))));
                }
            }
        });
        receive
            .await
            .map_err(|_| failure("journal commit owner ended without an acknowledgement"))?
    }
    pub(crate) async fn shutdown(&self) -> Result<(), AgentLoopError> {
        let jobs = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            self.batches.close();
            self.bytes.close();
            self.closed.notify_waiters();
            state.jobs.values().cloned().collect::<Vec<_>>()
        };
        let mut failed = None;
        for job in jobs {
            if let Err(error) = job.wait().await {
                failed.get_or_insert(error);
            }
        }
        match failed {
            Some(error) => Err(failure(error)),
            None => Ok(()),
        }
    }
}
fn failure(message: impl Into<String>) -> AgentLoopError {
    AgentLoopError::Persistence(message.into())
}

#[cfg(test)]
mod tests;
