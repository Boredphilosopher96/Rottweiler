use super::{
    Arc, CreateSessionRequest, EngineHost, ForkSessionRequest, HostError, HostedSession, Ordering,
    SessionId, SessionSlot, watch,
};

impl EngineHost {
    pub(super) async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Arc<HostedSession>, HostError> {
        {
            let mut registry = self.registry.lock().await;
            if self.shutting_down.load(Ordering::Acquire) {
                return Err(HostError::ShuttingDown);
            }
            if registry
                .sessions
                .len()
                .saturating_add(registry.anonymous_openings)
                >= self.config.max_sessions
            {
                return Err(HostError::SessionCapacity);
            }
            if registry.sessions.contains_key(&request.session_id) {
                return Err(HostError::Protocol(
                    "allocated session id already exists".to_owned(),
                ));
            }
            registry.anonymous_openings = registry.anonymous_openings.saturating_add(1);
        }
        let created = match self.factory.create(request.clone()).await {
            Ok(session)
                if session.descriptor().session_id == request.session_id
                    && session.handle().session_id() == &request.session_id =>
            {
                let session = Arc::new(session);
                match session.project_durable_descriptor().await {
                    Ok(()) => Ok(session),
                    Err(error) => Err(error),
                }
            }
            Ok(_) => Err(HostError::SessionIdentityMismatch),
            Err(error) => Err(error),
        };
        let mut registry = self.registry.lock().await;
        registry.anonymous_openings = registry.anonymous_openings.saturating_sub(1);
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(HostError::ShuttingDown);
        }
        let session = created?;
        registry
            .sessions
            .insert(request.session_id, SessionSlot::Ready(Arc::clone(&session)));
        Ok(session)
    }

    pub(super) async fn fork_session(
        &self,
        request: ForkSessionRequest,
    ) -> Result<Arc<HostedSession>, HostError> {
        loop {
            let wait = {
                let mut registry = self.registry.lock().await;
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err(HostError::ShuttingDown);
                }
                match registry.sessions.get(&request.child_session_id) {
                    Some(SessionSlot::Ready(session)) => return Ok(Arc::clone(session)),
                    Some(SessionSlot::Opening(completed)) => Some(completed.subscribe()),
                    None => {
                        if registry
                            .sessions
                            .len()
                            .saturating_add(registry.anonymous_openings)
                            >= self.config.max_sessions
                        {
                            return Err(HostError::SessionCapacity);
                        }
                        let (completed, _) = watch::channel(false);
                        registry.sessions.insert(
                            request.child_session_id.clone(),
                            SessionSlot::Opening(completed),
                        );
                        None
                    }
                }
            };
            let Some(mut wait) = wait else { break };
            wait.changed()
                .await
                .map_err(|_| HostError::SessionNotLoaded(request.child_session_id.0.clone()))?;
        }
        let forked = match self.factory.fork(request.clone()).await {
            Ok(session)
                if session.descriptor().session_id == request.child_session_id
                    && session.handle().session_id() == &request.child_session_id =>
            {
                let session = Arc::new(session);
                session.project_durable_descriptor().await.map(|()| session)
            }
            Ok(_) => Err(HostError::SessionIdentityMismatch),
            Err(error) => Err(error),
        };
        let mut registry = self.registry.lock().await;
        let completed = match registry.sessions.remove(&request.child_session_id) {
            Some(SessionSlot::Opening(completed)) => Some(completed),
            Some(SessionSlot::Ready(_)) | None => None,
        };
        if self.shutting_down.load(Ordering::Acquire) {
            if let Some(completed) = completed {
                completed.send_replace(true);
            }
            return Err(HostError::ShuttingDown);
        }
        let session = match forked {
            Ok(session) => session,
            Err(error) => {
                if let Some(completed) = completed {
                    completed.send_replace(true);
                }
                return Err(error);
            }
        };
        registry.sessions.insert(
            request.child_session_id,
            SessionSlot::Ready(Arc::clone(&session)),
        );
        if let Some(completed) = completed {
            completed.send_replace(true);
        }
        Ok(session)
    }

    pub(super) async fn resume_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<HostedSession>, HostError> {
        self.resume_session_after_reservation::<fn()>(session_id, None)
            .await
    }

    pub(super) async fn resume_session_after_reservation<F>(
        &self,
        session_id: &SessionId,
        mut on_reserved: Option<F>,
    ) -> Result<Arc<HostedSession>, HostError>
    where
        F: FnOnce(),
    {
        loop {
            let (ready, wait, owns_opening) = {
                let mut registry = self.registry.lock().await;
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err(HostError::ShuttingDown);
                }
                match registry.sessions.get(session_id) {
                    Some(SessionSlot::Ready(session)) => (Some(Arc::clone(session)), None, false),
                    Some(SessionSlot::Opening(completed)) => {
                        (None, Some(completed.subscribe()), false)
                    }
                    None => {
                        if registry
                            .sessions
                            .len()
                            .saturating_add(registry.anonymous_openings)
                            >= self.config.max_sessions
                        {
                            return Err(HostError::SessionCapacity);
                        }
                        let (completed, receiver) = watch::channel(false);
                        drop(receiver);
                        registry
                            .sessions
                            .insert(session_id.clone(), SessionSlot::Opening(completed));
                        (None, None, true)
                    }
                }
            };
            if let Some(on_reserved) = on_reserved.take() {
                on_reserved();
            }
            if let Some(session) = ready {
                return Ok(session);
            }
            if let Some(mut completed) = wait {
                if !*completed.borrow_and_update() {
                    let _ = completed.changed().await;
                }
                continue;
            }

            if !owns_opening {
                continue;
            }

            let opened = match self.factory.resume(session_id).await {
                Ok(session)
                    if session.descriptor().session_id == *session_id
                        && session.handle().session_id() == session_id =>
                {
                    let session = Arc::new(session);
                    match session.project_durable_descriptor().await {
                        Ok(()) => Ok(session),
                        Err(error) => Err(error),
                    }
                }
                Ok(_) => Err(HostError::SessionIdentityMismatch),
                Err(error) => Err(error),
            };
            let mut registry = self.registry.lock().await;
            let completed = match registry.sessions.remove(session_id) {
                Some(SessionSlot::Opening(completed)) => Some(completed),
                Some(SessionSlot::Ready(session)) => {
                    registry
                        .sessions
                        .insert(session_id.clone(), SessionSlot::Ready(session));
                    None
                }
                None => None,
            };
            let result = if self.shutting_down.load(Ordering::Acquire) {
                Err(HostError::ShuttingDown)
            } else {
                match opened {
                    Ok(session) => {
                        registry
                            .sessions
                            .insert(session_id.clone(), SessionSlot::Ready(Arc::clone(&session)));
                        Ok(session)
                    }
                    Err(error) => Err(error),
                }
            };
            drop(registry);
            if let Some(completed) = completed {
                completed.send_replace(true);
            }
            return result;
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
