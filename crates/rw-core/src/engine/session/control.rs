//! The committed driver lease and exact cancellation target share one lock.
use crate::engine::RoutedEvent;
use crate::engine::event_clock::EventClock;
use rw_tools::CancellationToken;
use rw_types::{
    ClientId, CommandAckMeta, CommandMeta, CommandOutcome, EngineError, EngineErrorCategory,
    EngineEvent, PROTOCOL_VERSION, SessionId,
};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

struct State {
    driver: Option<ClientId>,
    running: Option<(u64, CancellationToken)>,
    closed: bool,
}

pub(in crate::engine) struct SessionControl {
    session: SessionId,
    state: Mutex<State>,
    clock: Arc<dyn EventClock>,
}

impl SessionControl {
    pub(in crate::engine) fn new(
        session: SessionId,
        driver: Option<ClientId>,
        clock: Arc<dyn EventClock>,
    ) -> Self {
        Self {
            session,
            state: Mutex::new(State {
                driver,
                running: None,
                closed: false,
            }),
            clock,
        }
    }

    pub(in crate::engine) fn driver(&self) -> Option<ClientId> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .driver
            .clone()
    }

    /// Called only after the actor commits the lease event or recovers its journal.
    pub(in crate::engine) fn commit_driver(&self, driver: Option<ClientId>) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .driver = driver;
    }

    pub(in crate::engine) fn start(&self, turn: u64, cancellation: CancellationToken) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            cancellation.cancel();
        }
        state.running = Some((turn, cancellation));
    }

    pub(in crate::engine) fn finish(&self, turn: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.running.as_ref().is_some_and(|(id, _)| *id == turn) {
            state.running = None;
        }
    }

    pub(in crate::engine) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        if let Some((_, cancellation)) = &state.running {
            cancellation.cancel();
        }
    }

    /// Cancellation and acknowledgement complete without entering the actor's
    /// storage queue. No delayed command can cancel a subsequent turn.
    pub(in crate::engine) fn interrupt(
        &self,
        meta: &CommandMeta,
        session: &SessionId,
        events: &broadcast::Sender<RoutedEvent>,
    ) -> CommandOutcome {
        let outcome = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if meta.protocol_version != PROTOCOL_VERSION {
                reject(
                    "unsupported_protocol_version",
                    "unsupported protocol version",
                )
            } else if session != &self.session {
                reject(
                    "session_mismatch",
                    "command session id does not match this actor",
                )
            } else if state.closed {
                reject("session_closing", "session is closing")
            } else if state.driver.as_ref() != Some(&meta.client_id) {
                reject(
                    "driver_required",
                    "interrupt requires the committed driver lease",
                )
            } else {
                if let Some((_, cancellation)) = &state.running {
                    cancellation.cancel();
                }
                CommandOutcome::Accepted {}
            }
        };
        let _ = events.send(RoutedEvent {
            target: Some(meta.client_id.clone()),
            event: EngineEvent::CommandAcknowledged {
                meta: CommandAckMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: meta.client_id.clone(),
                    request_id: meta.request_id.clone(),
                    emitted_at: self.clock.emitted_at(),
                },
                session_id: Some(session.clone()),
                outcome: outcome.clone(),
            },
        });
        outcome
    }
}

fn reject(code: &str, message: &str) -> CommandOutcome {
    CommandOutcome::Rejected {
        error: EngineError {
            category: EngineErrorCategory::Protocol,
            code: code.to_owned(),
            message: message.to_owned(),
            retryable: false,
            details: None,
        },
    }
}
