use std::{panic::AssertUnwindSafe, sync::Arc};

use futures_util::FutureExt;
use tokio::sync::{oneshot, watch};

use super::super::{
    CreateSessionRequest, EngineHost, ForkSessionRequest, HostError, HostedSession, Ordering,
    SessionId, SessionSlot,
};

pub(super) enum OpenRequest {
    Create(CreateSessionRequest),
    Resume(SessionId),
    Fork(Box<ForkSessionRequest>),
}

impl OpenRequest {
    pub(super) fn session_id(&self) -> &SessionId {
        match self {
            Self::Create(request) => &request.session_id,
            Self::Resume(id) => id,
            Self::Fork(request) => &request.child_session_id,
        }
    }

    async fn compose(self, host: &EngineHost) -> Result<HostedSession, HostError> {
        match self {
            Self::Create(request) => host.factory.create(request).await,
            Self::Resume(id) => host.factory.resume(&id).await,
            Self::Fork(request) => host.factory.fork(*request).await,
        }
    }
}

impl EngineHost {
    pub(super) fn start_opening(
        &self,
        request: OpenRequest,
        completed: watch::Sender<bool>,
    ) -> oneshot::Receiver<Result<Arc<HostedSession>, HostError>> {
        let host = self.clone();
        let (respond, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let session_id = request.session_id().clone();
            let result = host.finish_opening(request).await;
            if result.is_err() {
                let mut registry = host.registry.lock().await;
                // Failed settlement keeps the reserved slot and its capacity.
                if registry.shutdown_failure.is_none() {
                    registry.sessions.remove(&session_id);
                }
            }
            completed.send_replace(true);
            let _ = respond.send(result);
        });
        receiver
    }

    async fn finish_opening(&self, request: OpenRequest) -> Result<Arc<HostedSession>, HostError> {
        let session_id = request.session_id().clone();
        let composed = AssertUnwindSafe(request.compose(self)).catch_unwind().await;
        let session = if let Ok(result) = composed {
            Arc::new(result?)
        } else {
            self.retain_failed_owners(
                "session factory panicked before ownership proof".to_owned(),
                (),
            )
            .await;
            return Err(HostError::Persistence(
                "session factory panicked".to_owned(),
            ));
        };
        let validation = if session.descriptor().session_id != session_id
            || session.handle().session_id() != &session_id
        {
            Err(HostError::SessionIdentityMismatch)
        } else {
            AssertUnwindSafe(session.project_durable_descriptor())
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    Err(HostError::Persistence(
                        "session descriptor projection panicked".to_owned(),
                    ))
                })
        };
        if let Err(error) = validation {
            self.close_hosted_session(session).await?;
            return Err(error);
        }
        let mut registry = self.registry.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            drop(registry);
            self.close_hosted_session(session).await?;
            return Err(HostError::ShuttingDown);
        }
        registry
            .sessions
            .insert(session_id, SessionSlot::Ready(Arc::clone(&session)));
        Ok(session)
    }
}
