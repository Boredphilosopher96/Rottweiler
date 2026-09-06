//! Ordered task transactions retain their request and result through caller abandonment.
use super::{journal_events::emit, signals::TurnSignal};
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use crate::engine::{AgentLoopError, SessionActorConfig};
use async_trait::async_trait;
use rw_tools::{
    CancellationToken, TodoAction, TodoAdmission, TodoStateStore, ToolError, ToolResult,
    prepare_todo_update,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
const MAX_REQUESTS: usize = 16;
const SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct ActorTodoStore {
    turn: u64,
    signals: mpsc::UnboundedSender<TurnSignal>,
    jobs: Arc<Mutex<BTreeMap<u64, Arc<Job>>>>,
    next: AtomicU64,
    credits: Arc<Semaphore>,
}
impl ActorTodoStore {
    pub(super) fn new(turn: u64, signals: mpsc::UnboundedSender<TurnSignal>) -> Self {
        Self {
            turn,
            signals,
            jobs: Arc::default(),
            next: AtomicU64::new(0),
            credits: Arc::new(Semaphore::new(MAX_REQUESTS)),
        }
    }
}
struct Job {
    result: Mutex<Option<Result<ToolResult, ToolError>>>,
    failure: Mutex<Option<String>>,
    done: AtomicBool,
    abandoned: AtomicBool,
    notify: Notify,
    _credit: OwnedSemaphorePermit,
}
impl Job {
    fn finish(&self, result: Result<ToolResult, ToolError>, failure: Option<String>) {
        if let Some(failure) = failure {
            self.failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert(failure);
        }
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.done.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    fn proof(&self) -> Result<(), ToolError> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or(Ok(()), |message| {
                Err(ToolError::EffectsUnsettled(message.clone()))
            })
    }
}
struct Waiter {
    id: u64,
    job: Arc<Job>,
    jobs: Arc<Mutex<BTreeMap<u64, Arc<Job>>>>,
    consumed: bool,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        if self.consumed && self.job.proof().is_ok() {
            self.jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.id);
        } else {
            self.job.abandoned.store(true, Ordering::Release);
        }
    }
}

pub(in crate::engine) struct TodoRequest {
    turn: u64,
    action: Option<TodoAction>,
    admission: TodoAdmission,
    cancellation: CancellationToken,
    job: Arc<Job>,
    complete: bool,
}
impl TodoRequest {
    fn finish(&mut self, result: Result<ToolResult, ToolError>, failure: Option<String>) {
        self.job.finish(result, failure);
        self.complete = true;
    }
}
impl Drop for TodoRequest {
    fn drop(&mut self) {
        if !self.complete {
            let message = "task transaction owner ended without a durable outcome".to_owned();
            self.job.finish(
                Err(ToolError::EffectsUnsettled(message.clone())),
                Some(message),
            );
        }
    }
}
#[async_trait]
impl TodoStateStore for ActorTodoStore {
    async fn transact(
        &self,
        action: TodoAction,
        admission: TodoAdmission,
        cancellation: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let credit = Arc::clone(&self.credits)
            .try_acquire_owned()
            .map_err(|_| ToolError::EffectsUnsettled("task request admission exhausted".into()))?;
        let id = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| ToolError::EffectsUnsettled("task request identity exhausted".into()))?;
        let job = Arc::new(Job {
            result: Mutex::new(None),
            failure: Mutex::new(None),
            done: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            notify: Notify::new(),
            _credit: credit,
        });
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Arc::clone(&job));
        let mut waiter = Waiter {
            id,
            job: Arc::clone(&job),
            jobs: Arc::clone(&self.jobs),
            consumed: false,
        };
        let request = TodoRequest {
            turn: self.turn,
            action: Some(action),
            admission,
            cancellation,
            job: Arc::clone(&job),
            complete: false,
        };
        if let Err(error) = self.signals.send(TurnSignal::Todo(request))
            && let TurnSignal::Todo(mut request) = error.0
        {
            request.finish(
                Err(ToolError::InvalidInput("task session is closed".into())),
                None,
            );
        }
        job.wait().await;
        waiter.consumed = true;
        job.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| ToolError::EffectsUnsettled("task reply missing".into()))?
    }
    async fn settle_effects(&self) -> Result<(), ToolError> {
        let jobs: Vec<_> = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, job)| job.abandoned.load(Ordering::Acquire))
            .map(|(id, job)| (*id, Arc::clone(job)))
            .collect();
        let mut failure = None;
        let deadline = tokio::time::Instant::now() + SETTLEMENT_TIMEOUT;
        for (id, job) in jobs {
            if tokio::time::timeout_at(deadline, job.wait()).await.is_err() {
                *job.failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some("task transaction settlement timed out".into());
            }
            match job.proof() {
                Ok(()) => {
                    self.jobs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id);
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

pub(in crate::engine) async fn handle(
    mut request: TodoRequest,
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &crate::engine::live_events::LiveEvents,
) -> Result<(), AgentLoopError> {
    if state.running.as_ref().map(|turn| turn.id) != Some(request.turn)
        || state.closing
        || state.poisoned
    {
        request.finish(
            Err(ToolError::InvalidInput(
                "task request does not own the active turn".into(),
            )),
            None,
        );
        return Ok(());
    }
    if request.cancellation.is_cancelled() {
        request.finish(Err(ToolError::Cancelled), None);
        return Ok(());
    }
    let current = match config.event_sink.todo_state().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            request.finish(Err(ToolError::Output(error.to_string())), None);
            return Err(error);
        }
    };
    let Some(action) = request.action.take() else {
        return Err(AgentLoopError::Persistence("task action missing".into()));
    };
    let (snapshot, result, changed) = match prepare_todo_update(&current, action, request.admission)
    {
        Ok(prepared) => prepared,
        Err(error) => {
            request.finish(Err(error), None);
            return Ok(());
        }
    };
    if request.cancellation.is_cancelled() {
        request.finish(Err(ToolError::Cancelled), None);
        return Ok(());
    }
    if changed
        && let Err(error) = emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::TodoStateCommitted { snapshot },
        )
        .await
    {
        let message = error.to_string();
        request.finish(
            Err(ToolError::EffectsUnsettled(message.clone())),
            Some(message),
        );
        return Err(error);
    }
    request.finish(Ok(result), None);
    Ok(())
}

#[cfg(test)]
mod tests;
