//! UI input resolves canonical source and live generation before command execution.
use crate::engine::{
    AgentLoopError,
    session::{ActorState, SessionActorConfig},
};
use crate::ui::BoundUiCommand;
use rw_types::extension_ui::{UiActionRequest, UiActionTarget};

pub(super) fn validate_admission(
    state: &ActorState,
    config: &SessionActorConfig,
    request: &UiActionRequest,
) -> Result<(), AgentLoopError> {
    request
        .validate()
        .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
    if state.pending_command.is_some()
        || state.pending_model_preparation.is_some()
        || state.running.is_some()
        || state.active_shell.is_some()
        || state.initialization_running
        || state.closing
        || state.poisoned
        || state.unsettled.is_some()
        || !state.pending_model_switches.is_empty()
    {
        return Err(AgentLoopError::InvalidConfiguration(
            "UI actions require an idle settled session".into(),
        ));
    }
    if !config.ui.owns(&request.owner) {
        return Err(AgentLoopError::InvalidConfiguration(
            "UI action generation is unavailable".into(),
        ));
    }
    Ok(())
}

pub(super) async fn resolve(
    state: &ActorState,
    config: &SessionActorConfig,
    request: &UiActionRequest,
) -> Result<BoundUiCommand, AgentLoopError> {
    validate_admission(state, config, request)?;
    let source = match &request.target {
        UiActionTarget::Tool { invocation_id } => {
            let through = config.event_sink.last_sequence().await?;
            config
                .ui_tool_source
                .presentation(invocation_id, through)
                .await?
        }
        UiActionTarget::Panel { .. } => None,
    };
    config.ui.resolve_action(request, source.as_ref())
}
