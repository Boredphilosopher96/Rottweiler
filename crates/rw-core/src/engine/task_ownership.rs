//! Actor tasks keep their exact runtime generation until execution has settled.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::Instrument;

use super::{AgentLoopError, CancellationToken, SessionActorConfig};

const MAX_ACTOR_TASKS: usize = super::MAX_TOOL_EXECUTION_WINDOW + 3;

#[derive(Clone, Default)]
pub(super) struct ActorTasks(Arc<TaskSet>);

#[derive(Default)]
struct TaskSet {
    entries: Mutex<Vec<Arc<TaskEntry>>>,
    changed: tokio::sync::Notify,
}

struct TaskEntry {
    cancellation: CancellationToken,
    complete: AtomicBool,
    failed: AtomicBool,
    _owners: Arc<SessionActorConfig>,
}

struct TaskGuard {
    completed: bool,
    entry: Arc<TaskEntry>,
    tasks: ActorTasks,
}

impl ActorTasks {
    pub(super) fn spawn<T: Send + 'static>(
        &self,
        owners: Arc<SessionActorConfig>,
        cancellation: CancellationToken,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<T>, AgentLoopError> {
        let guard = self.admit(owners, cancellation)?;
        let span = tracing::Span::current();
        Ok(tokio::spawn(
            async move {
                let mut guard = guard;
                let result = work.await;
                guard.completed = true;
                result
            }
            .instrument(span),
        ))
    }

    /// Blocking work remains registered until the actual worker exits, even when
    /// the caller abandons the join handle.
    pub(super) fn spawn_blocking<T: Send + 'static>(
        &self,
        owners: Arc<SessionActorConfig>,
        cancellation: CancellationToken,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<T>, AgentLoopError> {
        let guard = self.admit(owners, cancellation)?;
        let span = tracing::Span::current();
        Ok(tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let mut guard = guard;
                let result = work();
                guard.completed = true;
                result
            })
        }))
    }

    fn admit(
        &self,
        owners: Arc<SessionActorConfig>,
        cancellation: CancellationToken,
    ) -> Result<TaskGuard, AgentLoopError> {
        let entry = Arc::new(TaskEntry {
            cancellation,
            complete: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            _owners: owners,
        });
        {
            let mut entries = self
                .0
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if entries.len() >= MAX_ACTOR_TASKS {
                return Err(AgentLoopError::EffectsUnsettled(
                    "actor task admission is saturated".to_owned(),
                ));
            }
            entries.push(Arc::clone(&entry));
        }
        Ok(TaskGuard {
            completed: false,
            entry,
            tasks: self.clone(),
        })
    }

    pub(super) fn cancel(&self) {
        for entry in self
            .0
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            entry.cancellation.cancel();
        }
    }

    pub(super) fn idle(&self) -> bool {
        self.0
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .all(|entry| entry.complete.load(Ordering::Acquire))
    }

    pub(super) fn failure(&self) -> Option<AgentLoopError> {
        self.0
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|entry| entry.failed.load(Ordering::Acquire))
            .then(|| {
                AgentLoopError::EffectsUnsettled(
                    "actor task exited without completion proof".to_owned(),
                )
            })
    }

    pub(super) async fn changed(&self) {
        self.0.changed.notified().await;
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let failed = !self.completed;
        self.entry.failed.store(failed, Ordering::Release);
        self.entry.complete.store(true, Ordering::Release);
        if !failed {
            self.tasks
                .0
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|entry| !Arc::ptr_eq(entry, &self.entry));
        }
        self.tasks.0.changed.notify_one();
    }
}
