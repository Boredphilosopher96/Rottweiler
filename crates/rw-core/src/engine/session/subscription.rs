use crate::engine::AgentLoopError;
use crate::engine::durability::SessionEventSink;
use crate::engine::live_events::{LiveReceiver, Received, SessionEventDelivery};
use crate::engine::replay::SessionEventReadView;
use crate::engine::replay::SessionReplayLimits;
use rw_types::EngineEvent;
use rw_types::PROTOCOL_VERSION;
use rw_types::SequenceId;
use rw_types::SessionId;
use std::sync::Arc;
use tracing::Instrument;

/// One client-filtered view of the single engine event channel. A lagged live
/// receiver catches up from the durable source and suppresses duplicate live
/// deliveries by sequence id.
pub struct SessionSubscription {
    pub(super) session_id: SessionId,
    pub(in crate::engine) receiver: LiveReceiver,
    pub(super) sink: Arc<dyn SessionEventSink>,
    pub(super) last_sequence: Option<SequenceId>,
    pub(super) initial_tail: Option<SequenceId>,
    pub(super) pending: std::collections::VecDeque<SessionEventDelivery>,
    pub(super) replay: Option<Arc<dyn SessionEventReadView>>,
    pub(super) read: Option<
        tokio::task::JoinHandle<
            Result<std::collections::VecDeque<SessionEventDelivery>, AgentLoopError>,
        >,
    >,
    pub(super) needs_initial_replay: bool,
}

impl SessionSubscription {
    /// Durable tail captured before this subscription was returned to its caller.
    #[must_use]
    pub const fn initial_tail(&self) -> Option<SequenceId> {
        self.initial_tail
    }

    /// Loads and validates the first page of the prefix captured at subscription
    /// creation. Callers can validate storage before sending a protocol command.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the durable replay is invalid.
    pub async fn prime(&mut self) -> Result<(), AgentLoopError> {
        if self.needs_initial_replay {
            self.refill_replay().await?;
            self.needs_initial_replay = false;
        }
        Ok(())
    }

    pub(super) async fn refill_replay(&mut self) -> Result<(), AgentLoopError> {
        let Some(view) = &self.replay else {
            return Ok(());
        };
        if self.last_sequence == view.last_sequence() {
            self.replay = None;
            return Ok(());
        }
        if self.read.is_none() {
            let credit = SessionEventDelivery::replay_credit().await?;
            let view = Arc::clone(view);
            let after = self.last_sequence;
            let session = self.session_id.clone();
            // The task, not the recv future, owns decode/prepare admission. A
            // cancelled select keeps this handle for the next poll; dropping
            // the subscription leaves the finite source read charged to completion.
            self.read = Some(tokio::spawn(
                async move {
                    let page = view
                        .read_page(after, SessionReplayLimits::live_delivery())
                        .await?;
                    validate_gap(after, &page, &session)?;
                    if page.is_empty()
                        || page.len() > 256
                        || page.last().and_then(EngineEvent::meta).is_some_and(|meta| {
                            view.last_sequence()
                                .is_none_or(|tail| meta.sequence_id > tail)
                        })
                    {
                        return Err(AgentLoopError::Persistence(
                            "replay page does not advance inside its captured prefix and allowance"
                                .into(),
                        ));
                    }
                    let cpu = rw_resources::acquire(
                        rw_resources::ResourceClass::Cpu,
                        std::future::pending(),
                    )
                    .await
                    .map_err(|_| AgentLoopError::EventDeliverySaturated)?;
                    let span = tracing::Span::current();
                    tokio::task::spawn_blocking(move || {
                        let _cpu = cpu;
                        span.in_scope(|| SessionEventDelivery::from_replay(page, credit))
                    })
                    .await
                    .map_err(|error| {
                        AgentLoopError::Persistence(format!("replay preparation failed: {error}"))
                    })?
                }
                .instrument(tracing::Span::current()),
            ));
        }
        let Some(read) = self.read.as_mut() else {
            return Err(AgentLoopError::EventDeliverySaturated);
        };
        let result = read.await;
        self.read = None;
        self.pending = result.map_err(|error| {
            AgentLoopError::Persistence(format!("replay worker failed: {error}"))
        })??;
        Ok(())
    }

    /// Receives the next protocol event for this client.
    ///
    /// # Errors
    ///
    /// Returns a persistence error if a broadcast gap cannot be replayed, or
    /// [`AgentLoopError::Closed`] after the actor event channel closes.
    pub async fn recv(&mut self) -> Result<SessionEventDelivery, AgentLoopError> {
        loop {
            self.receiver.check_failure()?;
            self.prime().await?;
            if self.pending.is_empty() {
                self.refill_replay().await?;
            }
            if let Some(event) = self.pending.pop_front() {
                self.observe(&event);
                return Ok(event);
            }
            match self.receiver.recv().await? {
                Received::Event(event) => {
                    if let Some(meta) = event.meta() {
                        if self
                            .last_sequence
                            .is_some_and(|last| meta.sequence_id <= last)
                        {
                            continue;
                        }
                        if meta.sequence_id.0
                            != self
                                .last_sequence
                                .map_or(0, |last| last.0.saturating_add(1))
                        {
                            self.replay = Some(self.sink.capture_read_view()?);
                            continue;
                        }
                    }
                    self.observe(&event);
                    return Ok(event);
                }
                Received::CatchUp => self.replay = Some(self.sink.capture_read_view()?),
                Received::Closed => {
                    let source = self.sink.capture_read_view()?;
                    if self.last_sequence == source.last_sequence() {
                        return Err(AgentLoopError::Closed);
                    }
                    self.replay = Some(source);
                }
            }
        }
    }

    pub(super) fn observe(&mut self, event: &EngineEvent) {
        if let Some(meta) = event.meta() {
            self.last_sequence = Some(meta.sequence_id);
        }
    }
}

pub(in crate::engine) fn validate_gap(
    last_seen: Option<SequenceId>,
    gap: &[EngineEvent],
    session_id: &SessionId,
) -> Result<(), AgentLoopError> {
    let mut expected = last_seen.map_or(0, |sequence| sequence.0.saturating_add(1));
    for event in gap {
        let meta = event.meta().ok_or_else(|| {
            AgentLoopError::Persistence(
                "durable gap contained a connection-scoped acknowledgement".to_owned(),
            )
        })?;
        if meta.protocol_version != PROTOCOL_VERSION {
            return Err(AgentLoopError::Persistence(format!(
                "durable gap returned protocol version {}, expected {PROTOCOL_VERSION}",
                meta.protocol_version
            )));
        }
        if &meta.session_id != session_id {
            return Err(AgentLoopError::Persistence(
                "durable gap returned an event for a different session".to_owned(),
            ));
        }
        if meta.sequence_id.0 != expected {
            return Err(AgentLoopError::Persistence(format!(
                "durable gap returned sequence {}, expected {expected}",
                meta.sequence_id.0
            )));
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
