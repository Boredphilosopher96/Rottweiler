//! Blocking file transactions retain their workspace, cleanup and result admission.
use super::transaction::FileTransaction;
use crate::{ToolContext, ToolError};
use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};

type Proof = Option<Result<(), Arc<str>>>;

#[derive(Clone)]
pub(super) struct FileOperations(Arc<Operations>);
struct Operations {
    admission: Arc<Semaphore>,
    next_id: AtomicU64,
    calls: Mutex<HashMap<u64, Arc<FileCall>>>,
    proof_timeout: Duration,
}
struct FileCall {
    context: ToolContext,
    abandoned: AtomicBool,
    completion: watch::Receiver<Proof>,
    transaction: Mutex<FileTransaction>,
    admission: Mutex<Option<OwnedSemaphorePermit>>,
}
struct Completed<T> {
    result: Result<T, ToolError>,
    _admission: OwnedSemaphorePermit,
}
struct Caller {
    call: Arc<FileCall>,
    armed: bool,
}
impl Drop for Caller {
    fn drop(&mut self) {
        if self.armed {
            self.call.abandoned.store(true, Ordering::Release);
            self.call.context.cancellation.cancel();
        }
    }
}
struct CompletionOwner {
    proof: watch::Sender<Proof>,
    admission: Arc<Semaphore>,
    completed: bool,
}
impl Drop for CompletionOwner {
    fn drop(&mut self) {
        if !self.completed {
            self.admission.close();
            self.proof
                .send_replace(Some(Err(Arc::from("file completion owner stopped"))));
        }
    }
}
impl std::fmt::Debug for FileOperations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileOperations").finish_non_exhaustive()
    }
}
impl FileOperations {
    pub(super) fn new() -> Self {
        Self::with_limits(16, Duration::from_secs(30))
    }
    fn with_limits(maximum: usize, proof_timeout: Duration) -> Self {
        Self(Arc::new(Operations {
            admission: Arc::new(Semaphore::new(maximum)),
            next_id: AtomicU64::new(0),
            calls: Mutex::new(HashMap::new()),
            proof_timeout,
        }))
    }

    pub(super) async fn run<T: Send + 'static>(
        &self,
        context: ToolContext,
        operation: impl FnOnce(&ToolContext, &mut FileTransaction) -> Result<T, ToolError>
        + Send
        + 'static,
    ) -> Result<T, ToolError> {
        self.settle().await?;
        context.cancellation.check()?;
        let admission = Arc::clone(&self.0.admission)
            .try_acquire_owned()
            .map_err(|_| {
                if self.0.admission.is_closed() {
                    ToolError::EffectsUnsettled("file operation owner is quarantined".to_owned())
                } else {
                    ToolError::Command("file operation capacity exhausted".to_owned())
                }
            })?;
        let (proof, completion) = watch::channel(None);
        let call = Arc::new(FileCall {
            context,
            abandoned: AtomicBool::new(false),
            completion,
            transaction: Mutex::new(FileTransaction::default()),
            admission: Mutex::new(Some(admission)),
        });
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        self.0
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Arc::clone(&call));
        let mut caller = Caller {
            call: Arc::clone(&call),
            armed: true,
        };
        let worker_call = Arc::clone(&call);
        let worker = tokio::task::spawn_blocking(move || {
            let mut transaction = worker_call
                .transaction
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                operation(&worker_call.context, &mut transaction)
            }))
            .unwrap_or_else(|_| Err(ToolError::Command("file operation panicked".to_owned())));
            let cleanup = std::panic::catch_unwind(AssertUnwindSafe(|| transaction.cleanup()))
                .unwrap_or_else(|_| {
                    Err(ToolError::EffectsUnsettled(
                        "file cleanup panicked".to_owned(),
                    ))
                });
            (result, cleanup)
        });
        let operations = Arc::clone(&self.0);
        let owner = CompletionOwner {
            proof,
            admission: Arc::clone(&self.0.admission),
            completed: false,
        };
        let (reply, receive) = oneshot::channel();
        tokio::spawn(async move {
            let outcome = match worker.await {
                Ok((result, cleanup)) => cleanup.map(|()| result),
                Err(error) => Err(ToolError::EffectsUnsettled(format!(
                    "file worker stopped without cleanup proof: {error}"
                ))),
            };
            let result = operations.finish(id, &call, owner, outcome);
            let _ = reply.send(result);
        });
        let result = receive
            .await
            .map_err(|_| ToolError::EffectsUnsettled("file completion owner stopped".to_owned()))?;
        caller.armed = false;
        result?.result
    }

    pub(super) async fn settle(&self) -> Result<(), ToolError> {
        if self.0.admission.is_closed() {
            return Err(ToolError::EffectsUnsettled(
                "file operation owner is quarantined".to_owned(),
            ));
        }
        let deadline = tokio::time::Instant::now() + self.0.proof_timeout;
        loop {
            let calls: Vec<_> = self
                .0
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter(|call| call.abandoned.load(Ordering::Acquire))
                .cloned()
                .collect();
            if calls.is_empty() {
                return Ok(());
            }
            for call in calls {
                let mut completion = call.completion.clone();
                loop {
                    if let Some(proof) = completion.borrow_and_update().clone() {
                        proof.map_err(|reason| ToolError::EffectsUnsettled(reason.to_string()))?;
                        break;
                    }
                    tokio::time::timeout_at(deadline, completion.changed())
                        .await
                        .map_err(|_| {
                            ToolError::EffectsUnsettled(
                                "file worker has not finished before the settlement deadline"
                                    .to_owned(),
                            )
                        })?
                        .map_err(|_| {
                            ToolError::EffectsUnsettled("file completion owner stopped".to_owned())
                        })?;
                }
            }
        }
    }
}

impl Operations {
    fn finish<T>(
        &self,
        id: u64,
        call: &FileCall,
        mut owner: CompletionOwner,
        outcome: Result<Result<T, ToolError>, ToolError>,
    ) -> Result<Completed<T>, ToolError> {
        let outcome = outcome.and_then(|result| {
            let admission = call
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .ok_or_else(|| {
                    ToolError::EffectsUnsettled("file completion lost admission".to_owned())
                })?;
            Ok(Completed {
                result,
                _admission: admission,
            })
        });
        match outcome {
            Ok(completed) => {
                self.calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id);
                owner.proof.send_replace(Some(Ok(())));
                owner.completed = true;
                Ok(completed)
            }
            Err(error) => {
                owner.admission.close();
                owner
                    .proof
                    .send_replace(Some(Err(Arc::from(error.to_string()))));
                owner.completed = true;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests;
