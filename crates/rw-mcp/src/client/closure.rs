//! A connection owns its exact transport and process close futures until proof.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::FutureExt as _;
use rmcp::service::{RoleClient, RunningService};
use rw_tools::ProtocolProcessHandle;
use tokio::{
    sync::{oneshot, watch},
    time::Instant,
};

type Proof = Result<(), Arc<str>>;
const DROP_PROOF_DEADLINE: Duration = Duration::from_secs(3);

pub(super) struct ConnectionClosure {
    stop: Mutex<Option<oneshot::Sender<(Instant, Duration)>>>,
    completion: watch::Receiver<Option<Proof>>,
    closed: AtomicBool,
}

impl ConnectionClosure {
    pub(super) fn new(
        service: RunningService<RoleClient, super::McpInboundRouter>,
        child: Option<Box<dyn ProtocolProcessHandle>>,
    ) -> Self {
        Self::from_resources(Resources {
            service: Some(service),
            child,
        })
    }

    fn from_resources(resources: Resources) -> Self {
        let (stop, wait) = oneshot::channel();
        let (finished, completion) = watch::channel(None);
        let owner = CloseOwner {
            resources: Some(resources),
            finished,
            proven: false,
            armed: true,
        };
        // The guard exists before spawning, including if this task never polls.
        tokio::spawn(owner.run(wait));
        Self {
            stop: Mutex::new(Some(stop)),
            completion,
            closed: AtomicBool::new(false),
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn request(&self, timeout: Duration) {
        self.closed.store(true, Ordering::Release);
        if let Some(stop) = self
            .stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = stop.send((Instant::now() + timeout, timeout));
        }
    }

    pub(super) async fn close(&self, timeout: Duration) -> Proof {
        self.request(timeout);
        let mut completion = self.completion.clone();
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result;
            }
            completion
                .changed()
                .await
                .map_err(|_| Arc::from("MCP close owner exited without proof"))?;
        }
    }
}

impl Drop for ConnectionClosure {
    fn drop(&mut self) {
        self.request(DROP_PROOF_DEADLINE);
    }
}

struct Resources {
    service: Option<RunningService<RoleClient, super::McpInboundRouter>>,
    child: Option<Box<dyn ProtocolProcessHandle>>,
}

struct CloseOwner {
    resources: Option<Resources>,
    finished: watch::Sender<Option<Proof>>,
    proven: bool,
    armed: bool,
}

impl CloseOwner {
    async fn run(mut self, wait: oneshot::Receiver<(Instant, Duration)>) {
        let (deadline, timeout) = wait
            .await
            .unwrap_or_else(|_| (Instant::now() + DROP_PROOF_DEADLINE, DROP_PROOF_DEADLINE));
        let Some(resources) = self.resources.as_mut() else {
            return;
        };
        let result = {
            let work = settle(resources, timeout);
            tokio::pin!(work);
            if let Ok(result) = tokio::time::timeout_at(deadline, &mut work).await {
                result
            } else {
                self.finished.send_replace(Some(Err(Arc::from(
                    "MCP close proof deadline expired; resources remain owned",
                ))));
                // Dropping this future would detach rmcp's worker handle. Its
                // completion remains owned even after the caller receives failure.
                work.await
            }
        };
        self.proven = result.is_ok();
        self.finished.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(result);
            true
        });
        self.armed = false;
        if !self.proven {
            self.retain();
        }
    }

    fn retain(&mut self) {
        if let Some(resources) = self.resources.take() {
            std::mem::forget(resources);
        }
    }
}

impl Drop for CloseOwner {
    fn drop(&mut self) {
        if self.armed {
            self.finished.send_if_modified(|current| {
                if current.is_some() {
                    return false;
                }
                *current = Some(Err(Arc::from("MCP close task was dropped before proof")));
                true
            });
            self.retain();
        }
    }
}

async fn settle(resources: &mut Resources, timeout: Duration) -> Proof {
    let service = async {
        if let Some(service) = &mut resources.service {
            service
                .close()
                .await
                .map(|_| ())
                .map_err(|_| Arc::from("MCP service close task failed"))
        } else {
            Ok(())
        }
    };
    let process = async {
        if let Some(child) = &mut resources.child {
            child
                .terminate_and_reap(timeout)
                .await
                .map_err(|_| Arc::from("MCP native process retirement is unproven"))
        } else {
            Ok(())
        }
    };
    let (service, process) = tokio::join!(prove(service), prove(process));
    service.and(process)
}

async fn prove(work: impl std::future::Future<Output = Proof>) -> Proof {
    std::panic::AssertUnwindSafe(work)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(Arc::from("MCP cleanup panicked before proof")))
}

pub(super) async fn retire_process(
    child: Box<dyn ProtocolProcessHandle>,
    timeout: Duration,
) -> Proof {
    ConnectionClosure::from_resources(Resources {
        service: None,
        child: Some(child),
    })
    .close(timeout)
    .await
}

#[cfg(test)]
mod tests;
