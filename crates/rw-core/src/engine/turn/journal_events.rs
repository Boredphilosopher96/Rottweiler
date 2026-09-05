use crate::engine::AgentLoopError;
use crate::engine::RoutedEvent;
use crate::engine::durability;
use crate::engine::durability::SessionEventSink;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::ActorState;
use rw_types::EngineEvent;
use rw_types::EventMeta;
use rw_types::PROTOCOL_VERSION;
use rw_types::SequenceId;
use std::sync::Arc;
use tokio::sync::broadcast;

pub(in crate::engine) async fn emit(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    sink: &Arc<dyn SessionEventSink>,
    kind: PendingEvent,
) -> Result<EventMeta, AgentLoopError> {
    emit_batch(state, events, sink, vec![kind])
        .await?
        .pop()
        .ok_or_else(|| AgentLoopError::Persistence("missing durable event receipt".into()))
}

pub(in crate::engine) async fn emit_batch(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    sink: &Arc<dyn SessionEventSink>,
    kinds: Vec<PendingEvent>,
) -> Result<Vec<EventMeta>, AgentLoopError> {
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let first_expected = match state.sequence {
        Some(sequence) => sequence
            .checked_add(1)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?,
        None => 0,
    };
    let caused_by = state.caused_by();
    let requested = kinds
        .into_iter()
        .enumerate()
        .map(|(offset, kind)| {
            let offset = u64::try_from(offset)
                .map_err(|_| AgentLoopError::Persistence("event batch overflow".to_owned()))?;
            let sequence = first_expected
                .checked_add(offset)
                .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
            Ok(kind.stamp(EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: state.session_id.clone(),
                sequence_id: SequenceId(sequence),
                emitted_at: state.event_clock.emitted_at(),
                caused_by: caused_by.clone(),
            }))
        })
        .collect::<Result<Vec<_>, AgentLoopError>>()?;
    let persisted = durability::commit_session_events(Arc::clone(sink), requested).await?;
    let persisted = Arc::try_unwrap(persisted).map_err(|_| {
        AgentLoopError::Persistence(
            "event sink retained the completed batch instead of transferring ownership".to_owned(),
        )
    })?;
    let (persisted, reservation) = persisted.into_parts();
    state.sequence = persisted
        .last()
        .and_then(EngineEvent::meta)
        .map(|meta| meta.sequence_id.0);
    let receipts = persisted
        .iter()
        .filter_map(EngineEvent::meta)
        .cloned()
        .collect();
    for event in persisted {
        state
            .live
            .observe(&event, state.running.as_ref().map(|turn| turn.id));
        if let EngineEvent::SessionCreated {
            driver_client_id, ..
        }
        | EngineEvent::DriverChanged {
            driver_client_id, ..
        } = &event
        {
            state.control.commit_driver(Some(driver_client_id.clone()));
        }
        let _ = events.send(RoutedEvent {
            target: None,
            event,
        });
    }
    drop(reservation);
    Ok(receipts)
}
