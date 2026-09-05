//! Session identity reservations outlive the caller that requested composition.
mod opening;
mod shutdown;
pub(super) use shutdown::HostClosure;

use super::{
    Arc, CreateSessionRequest, EngineHost, ForkSessionRequest, HostError, HostedSession, Ordering,
    SessionId, SessionSlot, watch,
};
use opening::OpenRequest;

impl EngineHost {
    pub(super) async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Arc<HostedSession>, HostError> {
        self.open_session(OpenRequest::Create(request), false, None::<fn()>)
            .await
    }

    pub(super) async fn fork_session(
        &self,
        request: ForkSessionRequest,
    ) -> Result<Arc<HostedSession>, HostError> {
        self.open_session(OpenRequest::Fork(Box::new(request)), true, None::<fn()>)
            .await
    }

    pub(super) async fn resume_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<HostedSession>, HostError> {
        self.resume_session_after_reservation::<fn()>(session_id, None)
            .await
    }

    pub(super) async fn resume_session_after_reservation<F: FnOnce()>(
        &self,
        session_id: &SessionId,
        on_reserved: Option<F>,
    ) -> Result<Arc<HostedSession>, HostError> {
        self.open_session(OpenRequest::Resume(session_id.clone()), true, on_reserved)
            .await
    }

    pub(super) async fn prepare_fresh_session_after_reservation<F: FnOnce()>(
        &self,
        request: CreateSessionRequest,
        on_reserved: Option<F>,
    ) -> Result<Arc<HostedSession>, HostError> {
        self.open_session(OpenRequest::Create(request), true, on_reserved)
            .await
    }

    async fn open_session<F: FnOnce()>(
        &self,
        request: OpenRequest,
        join_existing: bool,
        mut on_reserved: Option<F>,
    ) -> Result<Arc<HostedSession>, HostError> {
        loop {
            let mut registry = self.registry.lock().await;
            if self.shutting_down.load(Ordering::Acquire) {
                return Err(HostError::ShuttingDown);
            }
            match registry.sessions.get(request.session_id()) {
                Some(_) if !join_existing => {
                    return Err(HostError::Protocol(
                        "allocated session id already exists".to_owned(),
                    ));
                }
                Some(SessionSlot::Ready(session)) => {
                    let session = Arc::clone(session);
                    drop(registry);
                    if let Some(callback) = on_reserved.take() {
                        callback();
                    }
                    return Ok(session);
                }
                Some(SessionSlot::Opening(completed)) => {
                    let mut completed = completed.subscribe();
                    drop(registry);
                    if let Some(callback) = on_reserved.take() {
                        callback();
                    }
                    if !*completed.borrow_and_update() {
                        tokio::select! {
                            _ = completed.changed() => {},
                            () = self.closure.started.cancelled() => return Err(HostError::ShuttingDown),
                        }
                    }
                }
                None => {
                    if registry.sessions.len() >= self.config.max_sessions {
                        return Err(HostError::SessionCapacity);
                    }
                    let (completed, _) = watch::channel(false);
                    registry.sessions.insert(
                        request.session_id().clone(),
                        SessionSlot::Opening(completed.clone()),
                    );
                    // No await between reservation and transfer to the owned task.
                    let receiver = self.start_opening(request, completed);
                    drop(registry);
                    if let Some(callback) = on_reserved.take() {
                        callback();
                    }
                    return receiver.await.map_err(|_| {
                        HostError::Persistence("session opening owner disappeared".to_owned())
                    })?;
                }
            }
        }
    }

    pub(super) async fn ready_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<HostedSession>, HostError> {
        self.session(session_id)
            .await
            .ok_or_else(|| HostError::SessionNotLoaded(session_id.0.clone()))
    }
}
