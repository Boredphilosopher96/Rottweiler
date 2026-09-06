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

/// Validate mode transitions through the captured source before checkpoint recovery
/// can mutate a workspace. The index advances in bounded source/metadata batches.
pub(crate) async fn compose_and_validate(
    catalog: &ExtensionCatalog,
    source: rw_store::session::journal::JournalReadView,
    inherited_journal_through: Option<rw_types::SequenceId>,
) -> Result<std::sync::Arc<ModeRegistry>> {
    let modes = std::sync::Arc::new(
        compose_mode_registry(catalog)
            .map_err(|error| miette!("mode registry could not compose: {error}"))?,
    );
    let selected = std::sync::Arc::clone(&modes);
    rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        let mut recovery = rw_core::recovery::CanonicalRecovery::open(
            &source,
            &selected,
            inherited_journal_through,
        )?;
        while recovery.advance(&source, &selected)?.has_more {}
        Ok::<_, rw_core::recovery::RecoveryError>(())
    })
    .await
    .map_err(|error| miette!("mode validation worker failed: {error}"))?
    .map_err(|error| miette!("session mode validation failed: {error}"))?;
    Ok(modes)
}
