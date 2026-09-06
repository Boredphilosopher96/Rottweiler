//! Shared live delivery and independent journal catch-up ownership.
//!
//! Commit credits never enter this ring. A full live byte allowance replaces a
//! durable body with its source fence, so the next append cannot wait on a ring
//! entry whose eviction itself requires that append. Replay has separate credits.
mod budget;
mod receiver;
use super::{AgentLoopError, RoutedEvent};
use budget::{Budget, Credit};
pub(super) use receiver::{LiveReceiver, Received};
use rw_types::{
    ClientId, EngineEvent, SequenceId,
    allocation::{AllocationPlan, PreparedAllocation},
};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::Notify;

pub(super) const MAX_EVENT_CAPACITY: usize = 1024;
const MAX_SESSION_SUBSCRIPTIONS: usize = 64;
const PAYLOAD_OVERHEAD: usize = 256;

/// Immutable shared event. Clones retain the original allocation credit; there
/// is deliberately no owned-value extraction that could discard that credit.
#[derive(Clone, Debug)]
pub struct SessionEventDelivery(Arc<Payload>, usize);
#[derive(Debug)]
struct Payload {
    event: PayloadEvents,
    _credit: Credit,
}
#[derive(Debug)]
enum PayloadEvents {
    Live(Box<PreparedAllocation<EngineEvent>>),
    Replay(PreparedAllocation<Vec<EngineEvent>>),
}
impl std::ops::Deref for SessionEventDelivery {
    type Target = EngineEvent;
    fn deref(&self) -> &EngineEvent {
        match &self.0.event {
            PayloadEvents::Live(event) => event.value(),
            PayloadEvents::Replay(events) => &events.value()[self.1],
        }
    }
}
impl AsRef<EngineEvent> for SessionEventDelivery {
    fn as_ref(&self) -> &EngineEvent {
        self
    }
}
impl SessionEventDelivery {
    pub(super) async fn replay_credit() -> Result<Credit, AgentLoopError> {
        budget::replay().await
    }
    pub(super) fn from_replay(
        events: Vec<EngineEvent>,
        mut credit: Credit,
    ) -> Result<std::collections::VecDeque<Self>, AgentLoopError> {
        let count = events.len();
        let plan =
            AllocationPlan::new(events).map_err(|_| AgentLoopError::EventDeliverySaturated)?;
        let descriptors = count
            .checked_mul(2 * std::mem::size_of::<Self>())
            .and_then(|bytes| bytes.checked_add(PAYLOAD_OVERHEAD))
            .ok_or(AgentLoopError::EventDeliverySaturated)?;
        let bytes = plan
            .bytes()
            .checked_add(descriptors)
            .ok_or(AgentLoopError::EventDeliverySaturated)?;
        if plan.bytes() > rw_store::session::journal::MAX_JOURNAL_DECODE_BYTES {
            return Err(AgentLoopError::EventDeliverySaturated);
        }
        let event = PayloadEvents::Replay(plan.prepare());
        credit.shrink(bytes)?;
        let payload = Arc::new(Payload {
            event,
            _credit: credit,
        });
        Ok((0..count)
            .map(|index| Self(Arc::clone(&payload), index))
            .collect())
    }
}

#[derive(Clone)]
pub(super) struct LiveEvents {
    state: Arc<State>,
    budget: Arc<Budget>,
}
struct State {
    channel: Mutex<Channel>,
    changed: Notify,
    _ring_credit: Credit,
}
struct Channel {
    frames: VecDeque<Frame>,
    capacity: usize,
    ordinal: u64,
    subscribers: Vec<Weak<Subscriber>>,
    closed: bool,
}
struct Subscriber {
    client: ClientId,
    seen: AtomicU64,
    failed: AtomicBool,
    _credit: Credit,
    _identity_credit: Credit,
}
struct Frame {
    target: Option<ClientId>,
    source: Option<SequenceId>,
    payload: Option<SessionEventDelivery>,
    ordinal: u64,
}
impl Channel {
    fn fail_unseen(&self, target: Option<&ClientId>, ordinal: u64) {
        for subscriber in self.subscribers.iter().filter_map(Weak::upgrade) {
            if target.is_none_or(|target| target == &subscriber.client)
                && subscriber.seen.load(Ordering::Acquire) < ordinal
            {
                subscriber.failed.store(true, Ordering::Release);
            }
        }
    }
    fn reclaim_observed(&mut self) {
        let through = self
            .subscribers
            .iter()
            .filter_map(Weak::upgrade)
            .map(|subscriber| subscriber.seen.load(Ordering::Acquire))
            .min()
            .unwrap_or(u64::MAX);
        while self
            .frames
            .front()
            .is_some_and(|frame| frame.ordinal <= through)
        {
            self.frames.pop_front();
        }
    }
    fn push(&mut self, frame: Frame) {
        if self.frames.len() == self.capacity
            && let Some(evicted) = self.frames.pop_front()
        {
            // Loss is decided when the slot is evicted, independently of payload
            // guards still held by a different consumer.
            if evicted.source.is_none() {
                self.fail_unseen(evicted.target.as_ref(), evicted.ordinal);
            }
        }
        self.frames.push_back(frame);
        self.reclaim_observed();
    }
}
impl LiveEvents {
    pub(super) fn new(capacity: usize) -> Result<Self, AgentLoopError> {
        if !(1..=MAX_EVENT_CAPACITY).contains(&capacity) {
            return Err(AgentLoopError::InvalidConfiguration(
                "event capacity must be in 1..=1024".into(),
            ));
        }
        Self::with_budget(capacity, budget::live())
    }
    fn with_budget(capacity: usize, budget: Arc<Budget>) -> Result<Self, AgentLoopError> {
        // Covers the exact bounded ring slots and subscriber weak registry.
        // Target string allocations belong to the payload's byte credit.
        let ring_credit = budget.reserve(capacity * 512 + MAX_SESSION_SUBSCRIPTIONS * 64)?;
        Ok(Self {
            budget,
            state: Arc::new(State {
                channel: Mutex::new(Channel {
                    frames: VecDeque::with_capacity(capacity),
                    capacity,
                    ordinal: 0,
                    subscribers: Vec::with_capacity(MAX_SESSION_SUBSCRIPTIONS),
                    closed: false,
                }),
                changed: Notify::new(),
                _ring_credit: ring_credit,
            }),
        })
    }
    #[cfg(test)]
    pub(in crate::engine) fn with_limit(
        capacity: usize,
        bytes: usize,
    ) -> Result<Self, AgentLoopError> {
        Self::with_budget(capacity, Budget::new(bytes))
    }
    pub(super) fn subscribe(&self, client: ClientId) -> Result<LiveReceiver, AgentLoopError> {
        let credit = budget::subscription()?;
        let identity_credit = self.budget.reserve(
            client
                .0
                .capacity()
                .checked_add(256)
                .ok_or(AgentLoopError::EventDeliverySaturated)?,
        )?;
        let mut channel = self
            .state
            .channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        channel
            .subscribers
            .retain(|subscriber| subscriber.strong_count() > 0);
        if channel.subscribers.len() >= MAX_SESSION_SUBSCRIPTIONS {
            return Err(AgentLoopError::EventDeliverySaturated);
        }
        let subscriber = Arc::new(Subscriber {
            client,
            seen: AtomicU64::new(channel.ordinal),
            failed: AtomicBool::new(false),
            _credit: credit,
            _identity_credit: identity_credit,
        });
        channel.subscribers.push(Arc::downgrade(&subscriber));
        Ok(LiveReceiver {
            state: Arc::clone(&self.state),
            subscriber,
        })
    }
    pub(super) fn send(&self, routed: RoutedEvent) -> Result<(), AgentLoopError> {
        let source = routed.event.meta().map(|meta| meta.sequence_id);
        // Durable events are session-wide; a source fence never owns an
        // uncharged client target string.
        let target = if source.is_some() {
            None
        } else {
            routed.target
        };
        let payload = AllocationPlan::new(routed.event).ok().and_then(|plan| {
            let bytes = plan
                .bytes()
                .checked_add(PAYLOAD_OVERHEAD)?
                .checked_add(target.as_ref().map_or(0, |id| id.0.capacity()))?;
            let credit = self.budget.reserve(bytes).ok()?;
            Some(SessionEventDelivery(
                Arc::new(Payload {
                    event: PayloadEvents::Live(Box::new(plan.prepare())),
                    _credit: credit,
                }),
                0,
            ))
        });
        self.publish(target, source, payload)
    }
    /// Transfers an already-normalized committed batch without traversing or
    /// preparing its bodies again. Queue/commit admission stays with the caller.
    pub(super) fn publish_committed(
        &self,
        events: PreparedAllocation<Vec<EngineEvent>>,
    ) -> Result<(), AgentLoopError> {
        let bytes = events
            .bytes()
            .checked_add(PAYLOAD_OVERHEAD)
            .ok_or(AgentLoopError::EventDeliverySaturated)?;
        if let Ok(credit) = self.budget.reserve(bytes) {
            let count = events.value().len();
            let payload = Arc::new(Payload {
                event: PayloadEvents::Replay(events),
                _credit: credit,
            });
            for index in 0..count {
                let event = SessionEventDelivery(Arc::clone(&payload), index);
                self.publish(None, event.meta().map(|meta| meta.sequence_id), Some(event))?;
            }
        } else {
            for event in events.value() {
                self.publish(None, event.meta().map(|meta| meta.sequence_id), None)?;
            }
        }
        Ok(())
    }
    fn publish(
        &self,
        target: Option<ClientId>,
        source: Option<SequenceId>,
        payload: Option<SessionEventDelivery>,
    ) -> Result<(), AgentLoopError> {
        let mut channel = self
            .state
            .channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if channel.closed {
            return Err(AgentLoopError::Closed);
        }
        let ordinal = channel
            .ordinal
            .checked_add(1)
            .ok_or(AgentLoopError::EventDeliverySaturated)?;
        if payload.is_none() && source.is_none() {
            channel.fail_unseen(target.as_ref(), ordinal);
            self.state.changed.notify_waiters();
            return Err(AgentLoopError::EventDeliverySaturated);
        }
        channel.ordinal = ordinal;
        channel.push(Frame {
            target,
            source,
            payload,
            ordinal,
        });
        self.state.changed.notify_waiters();
        Ok(())
    }
    pub(super) fn close(&self) {
        self.state
            .channel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        self.state.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
