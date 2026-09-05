//! Accepted host work stays owned through caller loss and host closure.
use std::{future::Future, sync::Mutex};

use tokio::task::JoinSet;

use super::HostError;

#[derive(Debug, Default)]
pub(super) struct ControlOwner {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    closed: bool,
    tasks: JoinSet<()>,
    shutdown_tasks: JoinSet<()>,
    failed: bool,
}

impl ControlOwner {
    pub(super) fn spawn(
        &self,
        work: impl Future<Output = ()> + Send + 'static,
        shutdown: bool,
    ) -> Result<(), HostError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(result) = state.tasks.try_join_next() {
            state.failed |= result.is_err();
        }
        while let Some(result) = state.shutdown_tasks.try_join_next() {
            state.failed |= result.is_err();
        }
        if (state.closed || state.failed) && !shutdown {
            return Err(HostError::ShuttingDown);
        }
        if shutdown {
            // A shutdown request owns its acknowledgement task but cannot wait
            // on itself in the barrier that proves other controls settled.
            state.shutdown_tasks.spawn(work);
        } else {
            state.tasks.spawn(work);
        }
        Ok(())
    }

    pub(super) fn close_admission(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
    }

    pub(super) async fn settle(&self) -> Result<(), HostError> {
        self.close_admission();
        while let Some(result) = std::future::poll_fn(|context| {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tasks
                .poll_join_next(context)
        })
        .await
        {
            if result.is_err() {
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .failed = true;
            }
        }
        if self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failed
        {
            return Err(HostError::Persistence(
                "host control task ended without completion proof".into(),
            ));
        }
        Ok(())
    }
}
