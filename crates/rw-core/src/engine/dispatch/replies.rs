use crate::engine::RoutedEvent;
use crate::engine::session::ActorState;
use rw_types::ClientId;
use rw_types::CommandAckMeta;
use rw_types::CommandMeta;
use rw_types::CommandOutcome;
use rw_types::EngineError;
use rw_types::EngineErrorCategory;
use rw_types::EngineEvent;
use rw_types::PROTOCOL_VERSION;
use rw_types::SessionId;
use tokio::sync::broadcast;

pub(super) fn protocol_rejection(code: &str, message: impl Into<String>) -> CommandOutcome {
    CommandOutcome::Rejected {
        error: EngineError {
            category: EngineErrorCategory::Protocol,
            code: code.to_owned(),
            message: message.into(),
            retryable: false,
            details: None,
        },
    }
}

pub(super) fn send_ack(
    state: &ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    meta: &CommandMeta,
    session_id: Option<SessionId>,
    outcome: CommandOutcome,
) {
    let _ = events.send(RoutedEvent {
        target: Some(meta.client_id.clone()),
        event: EngineEvent::CommandAcknowledged {
            meta: CommandAckMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: meta.client_id.clone(),
                request_id: meta.request_id.clone(),
                emitted_at: state.event_clock.emitted_at(),
            },
            session_id,
            outcome,
        },
    });
}

pub(super) fn send_connection_event(
    events: &broadcast::Sender<RoutedEvent>,
    client_id: &ClientId,
    event: EngineEvent,
) {
    let _ = events.send(RoutedEvent {
        target: Some(client_id.clone()),
        event,
    });
}

pub(super) fn query_meta(state: &ActorState, meta: &CommandMeta) -> CommandAckMeta {
    CommandAckMeta {
        protocol_version: PROTOCOL_VERSION,
        client_id: meta.client_id.clone(),
        request_id: meta.request_id.clone(),
        emitted_at: state.event_clock.emitted_at(),
    }
}
