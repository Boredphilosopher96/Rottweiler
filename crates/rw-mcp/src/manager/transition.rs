//! One retained connection transition, shared by concurrent waiters.
use super::operations::unsettled;
use crate::McpError;
use rw_types::McpServerId;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

pub(super) struct Transition {
    server: McpServerId,
    completion: watch::Receiver<Option<Result<(), McpError>>>,
    cancelled: AtomicBool,
    waiters: AtomicUsize,
}

impl Transition {
    pub(super) fn start<F: Future<Output = Result<(), McpError>> + Send + 'static>(
        server: McpServerId,
        build: impl FnOnce(Arc<Self>) -> F,
    ) -> Arc<Self> {
        let (finished, completion) = watch::channel(None);
        let transition = Arc::new(Self {
            server: server.clone(),
            completion,
            cancelled: AtomicBool::new(false),
            waiters: AtomicUsize::new(0),
        });
        let owner = TransitionOwner {
            finished,
            server,
            completed: false,
        };
        let future = build(Arc::clone(&transition));
        tokio::spawn(async move {
            owner.finish(future.await);
        });
        transition
    }
    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub(super) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub(super) fn result(&self) -> Option<Result<(), McpError>> {
        self.completion.borrow().clone()
    }
    pub(super) async fn completed(&self) -> Result<(), McpError> {
        let mut completion = self.completion.clone();
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result;
            }
            if completion.changed().await.is_err() {
                return Err(unsettled(&self.server));
            }
        }
    }
    pub(super) async fn wait(self: &Arc<Self>, timeout: Duration) -> Result<(), McpError> {
        self.waiters.fetch_add(1, Ordering::AcqRel);
        let mut waiter = Waiter {
            transition: Arc::clone(self),
            completed: false,
        };
        let result = tokio::time::timeout(timeout, self.completed()).await;
        waiter.completed = result.is_ok();
        result.unwrap_or_else(|_| Err(unsettled(&self.server)))
    }
}
struct Waiter {
    transition: Arc<Transition>,
    completed: bool,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        if self.transition.waiters.fetch_sub(1, Ordering::AcqRel) == 1 && !self.completed {
            self.transition.cancel();
        }
    }
}
struct TransitionOwner {
    finished: watch::Sender<Option<Result<(), McpError>>>,
    server: McpServerId,
    completed: bool,
}
impl TransitionOwner {
    fn finish(mut self, result: Result<(), McpError>) {
        self.finished.send_replace(Some(result));
        self.completed = true;
    }
}
impl Drop for TransitionOwner {
    fn drop(&mut self) {
        if !self.completed {
            self.finished
                .send_replace(Some(Err(unsettled(&self.server))));
        }
    }
}
