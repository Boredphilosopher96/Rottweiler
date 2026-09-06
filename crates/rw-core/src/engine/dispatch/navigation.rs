//! Navigation remains a driver-scoped client request, never a session authority transfer.
use crate::engine::{AgentLoopError, session::ActorState};
use rw_types::{CommandMeta, EngineEvent, extension_control::SessionNavigationTarget};

pub(super) fn request(
    state: &ActorState,
    events: &crate::engine::live_events::LiveEvents,
    meta: &CommandMeta,
    target: SessionNavigationTarget,
) -> Result<(), AgentLoopError> {
    validate(state, meta, &target)?;
    super::replies::send_connection_event(
        events,
        &meta.client_id,
        EngineEvent::SessionNavigationRequested {
            meta: super::replies::query_meta(state, meta),
            session_id: state.session_id.clone(),
            target,
        },
    );
    Ok(())
}

pub(super) fn validate(
    state: &ActorState,
    meta: &CommandMeta,
    target: &SessionNavigationTarget,
) -> Result<(), AgentLoopError> {
    target
        .validate()
        .map_err(|message| AgentLoopError::InvalidConfiguration(message.into()))?;
    if state.control.driver().as_ref() != Some(&meta.client_id) {
        return Err(AgentLoopError::InvalidConfiguration(
            "navigation requires the initiating driver".into(),
        ));
    }
    if let SessionNavigationTarget::Transcript { sequence } = target
        && state.sequence.is_none_or(|tail| sequence.0 > tail)
    {
        return Err(AgentLoopError::InvalidConfiguration(
            "navigation sequence is outside the session source".into(),
        ));
    }
    Ok(())
}
