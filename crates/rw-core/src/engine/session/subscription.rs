use crate::engine::AgentLoopError;
use crate::engine::RoutedEvent;
use crate::engine::durability::SessionEventSink;
use crate::engine::replay::SessionEventReadView;
use crate::engine::replay::SessionReplayLimits;
use rw_types::ClientId;
use rw_types::EngineEvent;
use rw_types::PROTOCOL_VERSION;
use rw_types::SequenceId;
use rw_types::SessionId;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::broadcast;

/// One client-filtered view of the single engine event channel. A lagged live
/// receiver catches up from the durable source and suppresses duplicate live
/// deliveries by sequence id.
pub struct SessionSubscription {
    pub(in crate::engine) client_id: ClientId,
    pub(super) session_id: SessionId,
    pub(in crate::engine) receiver: broadcast::Receiver<RoutedEvent>,
    pub(super) sink: Arc<dyn SessionEventSink>,
    pub(super) last_sequence: Option<SequenceId>,
    pub(super) initial_tail: Option<SequenceId>,
    pub(super) pending: VecDeque<EngineEvent>,
    pub(super) replay: Option<Arc<dyn SessionEventReadView>>,
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
        let page = view
            .read_page(self.last_sequence, SessionReplayLimits::default())
            .await?;
        validate_gap(self.last_sequence, &page, &self.session_id)?;
        if page.is_empty()
            || page.last().and_then(EngineEvent::meta).is_some_and(|meta| {
                view.last_sequence()
                    .is_none_or(|tail| meta.sequence_id > tail)
            })
        {
            return Err(AgentLoopError::Persistence(
                "replay page does not advance inside its captured prefix".to_owned(),
            ));
        }
        self.pending.extend(page);
        Ok(())
    }

    /// Receives the next protocol event for this client.
    ///
    /// # Errors
    ///
    /// Returns a persistence error if a broadcast gap cannot be replayed, or
    /// [`AgentLoopError::Closed`] after the actor event channel closes.
    pub async fn recv(&mut self) -> Result<EngineEvent, AgentLoopError> {
        loop {
            self.prime().await?;
            if self.pending.is_empty() {
                self.refill_replay().await?;
            }
            if let Some(event) = self.pending.pop_front() {
                self.observe(&event);
                return Ok(event);
            }
            match self.receiver.recv().await {
                Ok(routed) => {
                    if routed
                        .target
                        .as_ref()
                        .is_some_and(|target| target != &self.client_id)
                    {
                        continue;
                    }
                    if let Some(meta) = routed.event.meta()
                        && self
                            .last_sequence
                            .is_some_and(|last| meta.sequence_id <= last)
                    {
                        continue;
                    }
                    self.observe(&routed.event);
                    return Ok(routed.event);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.replay = Some(self.sink.capture_read_view()?);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(AgentLoopError::Closed);
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
