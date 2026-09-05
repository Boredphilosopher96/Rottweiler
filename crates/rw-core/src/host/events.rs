use super::{
    Arc, BoundClient, ClientId, EngineEvent, EngineHost, HOST_EVENT_CAPACITY,
    HOST_EVENT_STALL_TIMEOUT, HostError, ProviderAuthSubscriptionGuard, SequenceId, SessionId,
    join_all, mpsc, replay_completed,
};

impl EngineHost {
    /// Subscribes to connection-scoped host results and, optionally, one
    /// session's durable replay/live stream. A replay-complete marker is emitted
    /// after the captured durable tail, never before it.
    /// Subscribes one authenticated client to host results and an optional
    /// durable session replay/live stream.
    ///
    /// # Errors
    ///
    /// Returns a typed host error when the session is unavailable or the
    /// requested replay cursor is invalid.
    #[allow(clippy::too_many_lines)]
    pub async fn subscribe(
        &self,
        bound: BoundClient,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> Result<mpsc::Receiver<Result<EngineEvent, HostError>>, HostError> {
        let session = if let Some(session_id) = &session_id {
            let session = self.ready_session(session_id).await?;
            let events = session
                .handle()
                .subscribe_client(bound.client_id.clone(), last_seen)?;
            let captured_tail = events.initial_tail();
            Some((session, captured_tail, events))
        } else {
            None
        };
        let (channel, subscription_id, host_events) = self
            .client_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscribe(&bound.client_id);
        let (send, receive) = mpsc::channel(HOST_EVENT_CAPACITY);
        let clock = Arc::clone(&self.clock);
        let provider_auth = Arc::clone(&self.provider_auth);
        let client_events = Arc::clone(&self.client_events);
        tokio::spawn(async move {
            let mut subscription = ProviderAuthSubscriptionGuard {
                client_id: bound.client_id.clone(),
                subscription_id,
                receiver: host_events,
                channel,
                registry: client_events,
                pending: provider_auth,
            };
            if let Some((session, captured_tail, mut session_events)) = session {
                let mut replay_complete = last_seen == captured_tail;
                if replay_complete {
                    let _ = send
                        .send(Ok(replay_completed(
                            &bound.client_id,
                            &session.descriptor().session_id,
                            captured_tail,
                            &*clock,
                        )))
                        .await;
                }
                loop {
                    tokio::select! {
                        () = send.closed() => return,
                        host = subscription.receiver.recv() => match host {
                            Some(event) => if send.send(Ok(event)).await.is_err() { return; },
                            None => return,
                        },
                        event = session_events.recv() => match event {
                            Ok(event) => {
                                if !matches!(event, EngineEvent::CommandAcknowledged { .. })
                                    && send.send(Ok(event.clone())).await.is_err()
                                {
                                    return;
                                }
                                if !replay_complete
                                    && event.meta().map(|meta| meta.sequence_id) == captured_tail
                                {
                                    replay_complete = true;
                                    if send.send(Ok(replay_completed(
                                        &bound.client_id,
                                        &session.descriptor().session_id,
                                        captured_tail,
                                        &*clock,
                                    ))).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = send.send(Err(HostError::from(error))).await;
                                return;
                            }
                        }
                    }
                }
            } else {
                loop {
                    tokio::select! {
                        () = send.closed() => return,
                        event = subscription.receiver.recv() => match event {
                            Some(event) => if send.send(Ok(event)).await.is_err() { return; },
                            None => return,
                        },
                    }
                }
            }
        });
        Ok(receive)
    }

    pub(super) async fn emit_many(&self, client_id: &ClientId, events: &[EngineEvent]) {
        let channel = self
            .client_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clients
            .get(client_id)
            .cloned();
        let Some(channel) = channel else {
            return;
        };
        let _delivery = channel.delivery.lock().await;
        let mut senders = channel.senders();
        for event in events {
            let outcomes = join_all(senders.iter().map(|(id, sender)| {
                let event = event.clone();
                async move {
                    (
                        *id,
                        tokio::time::timeout(HOST_EVENT_STALL_TIMEOUT, sender.send(event)).await,
                    )
                }
            }))
            .await;
            let failed = outcomes
                .into_iter()
                .filter_map(|(id, outcome)| match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(_)) | Err(_) => Some(id),
                })
                .collect::<Vec<_>>();
            for id in &failed {
                channel.unsubscribe(*id);
            }
            senders.retain(|(id, _)| !failed.contains(id));
            if senders.is_empty() {
                break;
            }
        }
    }
}
