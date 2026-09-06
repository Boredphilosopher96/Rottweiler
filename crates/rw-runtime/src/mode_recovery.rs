//! Registry-independent routing is followed by exact mode-aware prefix validation.
use crate::journal_service::{JournalReadLease, JournalService};
use miette::{IntoDiagnostic, Result, miette};
use rw_core::recovery::{CanonicalRecovery, SessionRoutingIndex};
use rw_ext::{ExtensionCatalog, ModeRegistry, compose_mode_registry};
use rw_store::session::{SessionEventPageLimits, journal::JournalReadView};
use rw_types::{EngineEvent, SequenceId};

pub(crate) struct ForkState {
    pub(crate) workspace_generation: u64,
    pub(crate) completed_turns: u64,
    pub(crate) model_alias: Option<String>,
}
pub(crate) struct ForkRoute {
    pub(crate) lease: JournalReadLease,
    pub(crate) through: Option<SequenceId>,
    pub(crate) workspace_generation: u64,
}
/// Called inside the accepted fork's owned blocking transaction.
pub(crate) fn fork_route(
    journals: &JournalService,
    session: &str,
    turn: u64,
    requested: Option<SequenceId>,
    include_idle_tail: bool,
) -> Result<ForkRoute> {
    let order = journals.routing_projection_order(session)?;
    let _order = order
        .lock()
        .map_err(|_| miette!("routing owner poisoned"))?;
    let lease = journals.capture(session)?;
    let mut index = SessionRoutingIndex::open(&lease.view).into_diagnostic()?;
    while index.advance(&lease.view).into_diagnostic()? {}
    let through = if include_idle_tail {
        requested
    } else {
        index.completed(&lease.view, turn).into_diagnostic()?
    };
    let workspace_generation = index.workspace_at(&lease.view, through).into_diagnostic()?;
    Ok(ForkRoute {
        lease,
        through,
        workspace_generation,
    })
}
pub(crate) fn current_workspace_generation(
    journals: &JournalService,
    session: &str,
) -> Result<u64> {
    let order = journals.routing_projection_order(session)?;
    let _order = order
        .lock()
        .map_err(|_| miette!("routing owner poisoned"))?;
    let lease = journals.capture(session)?;
    let mut index = SessionRoutingIndex::open(&lease.view).into_diagnostic()?;
    while index.advance(&lease.view).into_diagnostic()? {}
    index
        .workspace_at(&lease.view, lease.view.last_sequence())
        .into_diagnostic()
}
pub(crate) fn validate_fork(
    source: &JournalReadView,
    modes: &ModeRegistry,
    inherited_journal_through: Option<SequenceId>,
) -> Result<ForkState> {
    let mut recovery =
        CanonicalRecovery::for_fork(source, modes, inherited_journal_through).into_diagnostic()?;
    while recovery.advance(source, modes).into_diagnostic()?.has_more {}
    let head = recovery.head().into_diagnostic()?;
    let model_alias = head
        .control
        .model
        .map(|sequence| {
            let mut page = source
                .page::<EngineEvent>(
                    sequence.0.checked_sub(1).map(SequenceId),
                    SessionEventPageLimits {
                        max_page_events: 1,
                        ..SessionEventPageLimits::default()
                    },
                )
                .into_diagnostic()?;
            match page.events.pop().map(|event| event.event) {
                Some(EngineEvent::ModelChanged { meta, model, .. })
                    if meta.sequence_id == sequence =>
                {
                    Ok(model.0)
                }
                _ => Err(miette!("fork model source is unavailable")),
            }
        })
        .transpose()?;
    Ok(ForkState {
        workspace_generation: head.control.workspace_generation,
        completed_turns: head.control.completed_turns,
        model_alias,
    })
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
