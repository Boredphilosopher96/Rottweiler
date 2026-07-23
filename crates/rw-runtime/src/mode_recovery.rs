//! Fail-closed mode-registry reconstruction for mutation-capable recovery.

use miette::{Result, miette};
use rw_core::{EngineEvent, SessionRecoveredState, project_session_events_with_modes};
use rw_ext::{ExtensionCatalog, ModeRegistry, compose_mode_registry};

pub(crate) struct ValidatedModeProjection {
    pub(crate) modes: ModeRegistry,
    pub(crate) recovered: SessionRecoveredState,
}

pub(crate) fn compose_and_project(
    catalog: &ExtensionCatalog,
    events: &[EngineEvent],
) -> Result<ValidatedModeProjection> {
    let modes = compose_mode_registry(catalog)
        .map_err(|error| miette!("mode registry could not compose: {error}"))?;
    let recovered = project(events, &modes)?;
    Ok(ValidatedModeProjection { modes, recovered })
}

pub(crate) fn project(
    events: &[EngineEvent],
    modes: &ModeRegistry,
) -> Result<SessionRecoveredState> {
    project_session_events_with_modes(events, modes)
        .map_err(|error| miette!("session log mode projection failed: {error}"))
}
