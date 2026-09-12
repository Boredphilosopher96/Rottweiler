use crate::engine::MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS;
use crate::engine::MAX_LIVE_TOOL_OUTPUT_BYTES;
use crate::engine::MAX_LIVE_TOOL_OUTPUT_CHUNKS;
use crate::engine::pending_event::PendingEvent;
use crate::engine::redaction::SecretRedactor;
use crate::engine::turn::signals::TurnSignal;
use async_trait::async_trait;
use rw_tools::CancellationToken;
use rw_tools::ToolError;
use rw_tools::ToolOutputChunk;
use rw_tools::ToolOutputSink;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

pub(super) struct OrderedOutputState {
    pub(super) current: usize,
    pub(super) buffered: BTreeMap<usize, Vec<BoundedOutputChunk>>,
}

pub(super) struct BoundedOutputChunk {
    pub(super) id: String,
    pub(super) invocation_id: rw_types::ToolInvocationId,
    pub(super) chunk: ToolOutputChunk,
    pub(super) permit: OwnedSemaphorePermit,
    pub(super) background_permit: Option<OwnedSemaphorePermit>,
}

pub(super) struct OrderedOutputCoordinator {
    pub(super) turn: u64,
    pub(super) signals: mpsc::UnboundedSender<TurnSignal>,
    pub(super) state: Mutex<OrderedOutputState>,
    pub(super) permits: Arc<Semaphore>,
    pub(super) background_permits: Arc<Semaphore>,
    pub(super) advanced: Notify,
    pub(super) redactor: Arc<dyn SecretRedactor>,
}

impl OrderedOutputCoordinator {
    pub(super) fn new(
        turn: u64,
        signals: mpsc::UnboundedSender<TurnSignal>,
        redactor: Arc<dyn SecretRedactor>,
    ) -> Self {
        Self {
            turn,
            signals,
            state: Mutex::new(OrderedOutputState {
                current: 0,
                buffered: BTreeMap::new(),
            }),
            permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS)),
            background_permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS - 1)),
            advanced: Notify::new(),
            redactor,
        }
    }

    pub(super) async fn emit(
        &self,
        index: usize,
        id: &str,
        invocation_id: &rw_types::ToolInvocationId,
        mut chunk: ToolOutputChunk,
    ) -> Result<(), ToolError> {
        let closed = || ToolError::Output("tool output channel is closed".to_owned());
        loop {
            let advanced = self.advanced.notified();
            tokio::pin!(advanced);
            advanced.as_mut().enable();
            let current = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .current;
            if index < current {
                return Err(ToolError::Output(
                    "tool output stream has already completed".to_owned(),
                ));
            }
            // Later tools may occupy at most 31 of the 32 global slots. The
            // current tool can always emit, even when every later tool blocks.
            let background_permit = if index > current {
                Some(tokio::select! {
                    biased;
                    () = self.signals.closed() => return Err(closed()),
                    () = &mut advanced => continue,
                    permit = Arc::clone(&self.background_permits).acquire_owned() =>
                        permit.map_err(|_| closed())?,
                })
            } else {
                None
            };
            let permit = tokio::select! {
                biased;
                () = self.signals.closed() => return Err(closed()),
                () = &mut advanced => continue,
                permit = Arc::clone(&self.permits).acquire_owned() =>
                    permit.map_err(|_| closed())?,
            };
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if index < state.current {
                return Err(ToolError::Output(
                    "tool output stream has already completed".to_owned(),
                ));
            }
            chunk.content = self.redactor.redact(&chunk.content);
            let bounded = BoundedOutputChunk {
                id: id.to_owned(),
                invocation_id: invocation_id.clone(),
                chunk,
                permit,
                background_permit,
            };
            if index == state.current {
                // Enqueue under the same lock as advance so a promoted tool
                // cannot overtake a chunk that already passed the index check.
                return self.send_chunk(bounded);
            }
            state.buffered.entry(index).or_default().push(bounded);
            return Ok(());
        }
    }

    pub(super) fn advance(&self, next: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.current = next;
        for chunk in state.buffered.remove(&next).unwrap_or_default() {
            let _ = self.send_chunk(chunk);
        }
        drop(state);
        // A promoted producer must leave the background semaphore wait queue;
        // later buffered tools may retain all its permits until it finishes.
        self.advanced.notify_waiters();
    }

    pub(super) fn send_chunk(&self, bounded: BoundedOutputChunk) -> Result<(), ToolError> {
        drop(bounded.background_permit);
        self.signals
            .send(TurnSignal::ToolOutput {
                event: PendingEvent::ToolOutput {
                    turn: self.turn,
                    id: bounded.id,
                    invocation_id: bounded.invocation_id,
                    stream: format!("{:?}", bounded.chunk.stream).to_ascii_lowercase(),
                    chunk: bounded.chunk.content,
                },
                _permit: bounded.permit,
            })
            .map_err(|_| ToolError::Output("tool output channel is closed".to_owned()))
    }
}

pub(super) struct OrderedOutputSink {
    pub(super) index: usize,
    pub(super) id: String,
    pub(super) invocation_id: rw_types::ToolInvocationId,
    pub(super) coordinator: Arc<OrderedOutputCoordinator>,
    pub(super) open: Arc<AtomicBool>,
    pub(super) cancellation: CancellationToken,
    pub(super) totals: Mutex<(usize, usize, bool)>,
}

#[cfg(test)]
mod tests;

#[async_trait]
impl ToolOutputSink for OrderedOutputSink {
    async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
        if !self.open.load(Ordering::Acquire) {
            return Err(ToolError::Output("tool output stream is closed".to_owned()));
        }
        let chunk = {
            let mut totals = self
                .totals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            totals.0 = totals.0.saturating_add(chunk.content.len());
            totals.1 = totals.1.saturating_add(1);
            if totals.0 > MAX_LIVE_TOOL_OUTPUT_BYTES || totals.1 > MAX_LIVE_TOOL_OUTPUT_CHUNKS {
                if totals.2 {
                    return Ok(());
                }
                totals.2 = true;
                ToolOutputChunk {
                    stream: chunk.stream,
                    content: "[live tool output truncated; command output continues to drain]"
                        .to_owned(),
                }
            } else {
                chunk
            }
        };
        tokio::select! {
            biased;
            result = self.coordinator.emit(self.index, &self.id, &self.invocation_id, chunk) => result,
            () = self.cancellation.cancelled() => Err(ToolError::Cancelled),
        }
    }
}
