//! Bind source-qualified timeline intent before the ordered actor mutates anything.
use crate::engine::{
    AgentLoopError,
    session::{ActorState, SessionActorConfig},
};
use rw_types::{ClientCommand, RewindTarget, TurnId};

pub(super) async fn resolve(
    command: &mut ClientCommand,
    state: &ActorState,
    config: &SessionActorConfig,
) -> Result<(), AgentLoopError> {
    let ClientCommand::Rewind { target, .. } = command else {
        return Ok(());
    };
    let RewindTarget::Source {
        expected_through,
        source,
        turn_id,
        position,
    } = target
    else {
        return Ok(());
    };
    if state.sequence != Some(expected_through.0) {
        return Err(AgentLoopError::InvalidConfiguration(
            "transcript view changed; refresh the selected source".into(),
        ));
    }
    if super::action_availability::ActionState::from_actor(state, config)
        .unavailable(rw_types::SessionActionKind::Rewind)
        .is_some()
    {
        return Err(AgentLoopError::InvalidConfiguration(
            "source rewind requires an idle session".into(),
        ));
    }
    let turn = super::super::projection::parse_turn_id(turn_id)
        .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
    let resolved = config
        .event_sink
        .source_rewind_target(*expected_through, *source, turn, *position)
        .await?;
    *target = RewindTarget::Turn {
        turn_id: TurnId(resolved.to_string()),
    };
    Ok(())
}
