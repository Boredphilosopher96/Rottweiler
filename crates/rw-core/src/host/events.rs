use super::{
    Arc, BoundClient, ClientId, ClientSubscriptionLease, EngineEvent, EngineHost,
    HOST_EVENT_CAPACITY, HOST_EVENT_STALL_TIMEOUT, HostError, HostEvent, HostEventBudget,
    ProviderAuthSubscriptionGuard, SequenceId, SessionId, join_all, mpsc, replay_completed,
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
    ) -> Result<mpsc::Receiver<Result<HostEvent, HostError>>, HostError> {
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
        let (channel, subscription_id, host_events, lease) = self
            .client_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .subscribe(&bound.client_id)?;
        let (send, receive) = mpsc::channel(HOST_EVENT_CAPACITY);
        let clock = Arc::clone(&self.clock);
        let event_budget = self.event_budget.clone();
        let provider_auth = Arc::clone(&self.provider_auth);
        let client_events = Arc::clone(&self.client_events);
        let lease = Arc::new(lease);
        tokio::spawn(async move {
            let mut subscription = ProviderAuthSubscriptionGuard {
                client_id: bound.client_id.clone(),
                subscription_id,
                receiver: host_events,
                _lease: Arc::clone(&lease),
                channel,
                registry: client_events,
                pending: provider_auth,
            };
            if let Some((session, captured_tail, mut session_events)) = session {
                let mut replay_complete = last_seen == captured_tail;
                if replay_complete
                    && !send_encoded(
                        &send,
                        &event_budget,
                        &lease,
                        &replay_completed(
                            &bound.client_id,
                            &session.descriptor().session_id,
                            captured_tail,
                            &*clock,
                        ),
                    )
                    .await
                {
                    return;
                }
                loop {
                    tokio::select! {
                        () = send.closed() => return,
                        host = subscription.receiver.recv() => match host {
                            Some(event) => if !send_result(&send, Ok(event.for_subscription(&lease))).await { return; },
                            None => return,
                        },
                        event = session_events.recv() => match event {
                            Ok(event) => {
                                if !matches!(event.as_ref(), EngineEvent::CommandAcknowledged { .. })
                                    && !send_encoded(&send, &event_budget, &lease, &event).await
                                {
                                    return;
                                }
                                if !replay_complete
                                    && event.meta().map(|meta| meta.sequence_id) == captured_tail
                                {
                                    replay_complete = true;
                                    if !send_encoded(&send, &event_budget, &lease, &replay_completed(
                                        &bound.client_id,
                                        &session.descriptor().session_id,
                                        captured_tail,
                                        &*clock,
                                    )).await {
                                        return;
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = send_result(&send, Err(HostError::from(error))).await;
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
                            Some(event) => if !send_result(&send, Ok(event.for_subscription(&lease))).await { return; },
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
            let Ok(event) = self.event_budget.encode(event).await else {
                for (id, _) in &senders {
                    channel.unsubscribe(*id);
                }
                return;
            };
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

async fn send_encoded(
    send: &mpsc::Sender<Result<HostEvent, HostError>>,
    budget: &HostEventBudget,
    lease: &Arc<ClientSubscriptionLease>,
    event: &EngineEvent,
) -> bool {
    let encoded = budget
        .encode(event)
        .await
        .map(|event| event.for_subscription(lease));
    let valid = encoded.is_ok();
    send_result(send, encoded).await && valid
}

// A full transport queue is retained work, never an acknowledgement. Once its
// fixed stall deadline expires, retire the forwarding task and close its stream;
// do not wait for space to enqueue another failure behind the stalled payload.
async fn send_result(
    send: &mpsc::Sender<Result<HostEvent, HostError>>,
    event: Result<HostEvent, HostError>,
) -> bool {
    match tokio::time::timeout(HOST_EVENT_STALL_TIMEOUT, send.send(event)).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            tracing::warn!("host event subscription closed after transport delivery stalled");
            false
        }
    }
}

#[cfg(test)]
#[path = "events/tests.rs"]
mod tests;
