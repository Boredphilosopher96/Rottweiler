//! Bounded durable reads shared by reconnect, history and recovery consumers.

use super::{AgentLoopError, EngineEvent, SequenceId};
use async_trait::async_trait;

/// Retention ceilings for one replay page. The implementation may return fewer
/// events when the byte limit is reached, but must make progress before its tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionReplayLimits {
    /// Maximum envelopes retained in one page.
    pub max_events: usize,
    /// Maximum serialized event bytes retained in one page.
    pub max_bytes: usize,
}

impl Default for SessionReplayLimits {
    fn default() -> Self {
        Self {
            max_events: 256,
            max_bytes: 8 * 1024 * 1024,
        }
    }
}

/// A captured, immutable logical prefix of acknowledged durable events.
///
/// The source owns admission and releases it when the last view reference drops.
/// Blocking file I/O and decoding must run outside the append owner's mutex.
#[async_trait]
pub trait SessionEventReadView: Send + Sync + std::fmt::Debug {
    /// Fixed durable tail of this view; later appends cannot change it.
    fn last_sequence(&self) -> Option<SequenceId>;

    /// Returns a cursor-exclusive, contiguous bounded page from the fixed prefix.
    /// Connection-scoped events never belong in this stream.
    async fn read_page(
        &self,
        after: Option<SequenceId>,
        limits: SessionReplayLimits,
    ) -> Result<Vec<EngineEvent>, AgentLoopError>;
}

#[derive(Debug)]
pub(super) struct MemoryEventReadView {
    events: std::sync::Arc<std::sync::Mutex<Vec<EngineEvent>>>,
    tail: Option<SequenceId>,
}

impl MemoryEventReadView {
    pub(super) fn new(
        events: std::sync::Arc<std::sync::Mutex<Vec<EngineEvent>>>,
        tail: Option<SequenceId>,
    ) -> Self {
        Self { events, tail }
    }
}

#[derive(Default)]
struct ByteCount(usize);
impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("event byte count overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl SessionEventReadView for MemoryEventReadView {
    fn last_sequence(&self) -> Option<SequenceId> {
        self.tail
    }

    async fn read_page(
        &self,
        after: Option<SequenceId>,
        limits: SessionReplayLimits,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        if limits.max_events == 0 || limits.max_bytes == 0 {
            return Err(AgentLoopError::Persistence(
                "replay page limits must be positive".to_owned(),
            ));
        }
        if after.is_some_and(|after| self.tail.is_none_or(|tail| after > tail)) {
            return Err(AgentLoopError::Persistence(
                "replay cursor is ahead of its view".to_owned(),
            ));
        }
        let events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = events.partition_point(|event| {
            event
                .meta()
                .is_some_and(|meta| after.is_some_and(|after| meta.sequence_id <= after))
        });
        let mut page = Vec::new();
        let mut bytes = 0;
        for event in &events[start..] {
            if page.len() == limits.max_events
                || event
                    .meta()
                    .is_some_and(|meta| self.tail.is_none_or(|tail| meta.sequence_id > tail))
            {
                break;
            }
            let mut count = ByteCount::default();
            serde_json::to_writer(&mut count, event)
                .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
            if count.0 > limits.max_bytes - bytes {
                if page.is_empty() {
                    return Err(AgentLoopError::Persistence(
                        "event exceeds replay page byte limit".to_owned(),
                    ));
                }
                break;
            }
            bytes += count.0;
            page.push(event.clone());
        }
        Ok(page)
    }
}
