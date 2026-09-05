//! Closure publishes success only after every accepted session owner has settled.
use std::{
    future::pending,
    panic::AssertUnwindSafe,
    sync::{Arc, OnceLock},
    time::Duration,
};

use futures_util::{FutureExt, future::join_all};
use rw_tools::CancellationToken;
use tokio::sync::watch;

use super::super::{EngineHost, HostError, HostedSession, Ordering, SessionSlot};

type Proof = Result<(), Arc<str>>;
const PROOF_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(in crate::host) struct HostClosure {
    pub(super) started: CancellationToken,
    proof: OnceLock<watch::Sender<Option<Proof>>>,
}

impl EngineHost {
    pub(in crate::host) async fn shutdown_sessions(&self) -> Result<(), HostError> {
        let sender = self.closure.proof.get_or_init(|| {
            let (sender, _) = watch::channel(None);
            let completion = sender.clone();
            let host = self.clone();
            tokio::spawn(async move {
                let work_host = host.clone();
                let mut work = tokio::spawn(async move { work_host.close_all_sessions().await });
                let result = match tokio::time::timeout(PROOF_TIMEOUT, &mut work).await {
                    Ok(Ok(result)) => result.map_err(|error| Arc::<str>::from(error.to_string())),
                    Ok(Err(_)) => Err(Arc::from("host cleanup task panicked before proof")),
                    Err(_) => Err(Arc::from(
                        "host cleanup did not settle before proof deadline",
                    )),
                };
                if let Err(error) = &result {
                    host.retain_failed_owners(error.to_string(), work).await;
                }
                completion.send_replace(Some(result));
            });
            sender
        });
        let mut completion = sender.subscribe();
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result.map_err(|error| HostError::Persistence(error.to_string()));
            }
            completion
                .changed()
                .await
                .map_err(|_| HostError::Persistence("host closure owner disappeared".to_owned()))?;
        }
    }

    async fn close_all_sessions(&self) -> Result<(), HostError> {
        let (ready, openings) = {
            let registry = self.registry.lock().await;
            self.shutting_down.store(true, Ordering::Release);
            self.closure.started.cancel();
            let mut ready = Vec::new();
            let mut openings = Vec::new();
            for slot in registry.sessions.values() {
                match slot {
                    SessionSlot::Ready(session) => ready.push(Arc::clone(session)),
                    SessionSlot::Opening(completed) => openings.push(completed.subscribe()),
                }
            }
            (ready, openings)
        };
        self.provider_auth.cancel_all();
        let close_sessions = join_all(ready.into_iter().map(|session| async move {
            let id = session.descriptor().session_id;
            let result = self.close_hosted_session(session).await;
            if result.is_ok() {
                self.registry.lock().await.sessions.remove(&id);
            }
            result
        }));
        let wait_openings = join_all(openings.into_iter().map(|mut done| async move {
            while !*done.borrow_and_update() {
                done.changed().await.map_err(|_| {
                    HostError::Persistence("session opening proof disappeared".to_owned())
                })?;
            }
            Ok::<(), HostError>(())
        }));
        let (sessions, openings) = tokio::join!(close_sessions, wait_openings);
        for result in sessions.into_iter().chain(openings) {
            result?;
        }
        if let Some(error) = &self.registry.lock().await.shutdown_failure {
            return Err(HostError::Persistence(error.to_string()));
        }
        // Final receipts and SessionEnd hooks still need these shared services.
        // Never close them while a dependent actor or opener is unproven.
        AssertUnwindSafe(self.factory.shutdown())
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(HostError::Persistence(
                    "session factory shutdown panicked".to_owned(),
                ))
            })?;
        Ok(())
    }

    pub(super) async fn close_hosted_session(
        &self,
        session: Arc<HostedSession>,
    ) -> Result<(), HostError> {
        let result = AssertUnwindSafe(session.handle().close())
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(crate::AgentLoopError::EffectsUnsettled(
                    "session close panicked".to_owned(),
                ))
            })
            .map_err(HostError::from);
        if let Err(error) = &result {
            self.retain_failed_owners(error.to_string(), session).await;
        }
        result
    }

    pub(super) async fn retain_failed_owners<T: Send + 'static>(&self, error: String, owners: T) {
        {
            let mut registry = self.registry.lock().await;
            registry
                .shutdown_failure
                .get_or_insert_with(|| Arc::from(error));
            self.shutting_down.store(true, Ordering::Release);
            self.closure.started.cancel();
        }
        let host = self.clone();
        tokio::spawn(async move {
            pending::<()>().await;
            drop((host, owners));
        });
    }
}
