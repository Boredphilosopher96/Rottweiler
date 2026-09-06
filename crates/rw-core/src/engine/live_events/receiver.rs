use super::*;

pub(in crate::engine) struct LiveReceiver {
    pub(super) state: Arc<State>,
    pub(super) subscriber: Arc<Subscriber>,
}
pub(in crate::engine) enum Received {
    Event(SessionEventDelivery),
    CatchUp,
    Closed,
}
impl LiveReceiver {
    pub(in crate::engine) fn check_failure(&self) -> Result<(), AgentLoopError> {
        if self.subscriber.failed.load(Ordering::Acquire) {
            Err(AgentLoopError::EventDeliverySaturated)
        } else {
            Ok(())
        }
    }
    fn poll(&self) -> Result<Option<Received>, AgentLoopError> {
        let channel = self
            .state
            .channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check_failure()?;
        loop {
            let seen = self.subscriber.seen.load(Ordering::Acquire);
            let Some(first) = channel.frames.front() else {
                return Ok(channel.closed.then_some(Received::Closed));
            };
            if seen < first.ordinal - 1 {
                self.subscriber
                    .seen
                    .store(first.ordinal - 1, Ordering::Release);
                return Ok(Some(Received::CatchUp));
            }
            let offset = usize::try_from(seen.saturating_sub(first.ordinal - 1))
                .map_err(|_| AgentLoopError::EventDeliverySaturated)?;
            let Some(frame) = channel.frames.get(offset) else {
                return Ok(channel.closed.then_some(Received::Closed));
            };
            self.subscriber.seen.store(frame.ordinal, Ordering::Release);
            if frame
                .target
                .as_ref()
                .is_some_and(|target| target != &self.subscriber.client)
            {
                continue;
            }
            return Ok(Some(
                frame
                    .payload
                    .as_ref()
                    .map_or(Received::CatchUp, |event| Received::Event(event.clone())),
            ));
        }
    }
    pub(in crate::engine) async fn recv(&mut self) -> Result<Received, AgentLoopError> {
        loop {
            let changed = self.state.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(value) = self.poll()? {
                return Ok(value);
            }
            changed.await;
        }
    }
    #[cfg(test)]
    pub(in crate::engine) fn try_recv(&mut self) -> Result<SessionEventDelivery, AgentLoopError> {
        match self.poll()? {
            Some(Received::Event(event)) => Ok(event),
            _ => Err(AgentLoopError::Closed),
        }
    }
}
