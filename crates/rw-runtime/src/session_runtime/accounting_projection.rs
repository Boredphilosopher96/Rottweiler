use super::durable_session::{DurableEventSink, HostedSessionProjection, load_session_events};
use miette::{IntoDiagnostic, Result, miette};
use rw_core::{AccountingAttribution, EngineEvent, SequenceId};
use rw_store::session::{
    AccountingLedger, SessionEventLog, SessionIndex, SessionProjection, TurnAccountingEntry,
    UtcTimestamp, garbage_collect_empty_sessions,
};
use std::{
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) fn inherited_accounting_through(
    storage_root: &Path,
    session_id: &str,
) -> Result<Option<SequenceId>> {
    super::session_metadata::load_session_metadata_any(storage_root, session_id)
        .map(|metadata| metadata.inherited_accounting_through)
}

pub(super) fn refresh_session_index(storage_root: &Path) -> Result<()> {
    let sessions_root = storage_root.join("sessions");
    let mut projections = Vec::new();
    let mut accounting_entries = Vec::new();
    match std::fs::read_dir(&sessions_root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.into_diagnostic()?;
                if !entry.file_type().into_diagnostic()?.is_dir() {
                    continue;
                }
                let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let log = SessionEventLog::open(storage_root, &id)
                    .map_err(|error| miette!("session {id:?} could not open: {error}"))?;
                let events = load_session_events(&log)?;
                if session_has_user_turn(&events) {
                    projections.push(project_session(&id, &events, log.path()));
                }
                let inherited_through = inherited_accounting_through(storage_root, &id)?;
                accounting_entries.extend(project_accounting(&id, &events, inherited_through)?);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).into_diagnostic(),
    }
    SessionIndex::rebuild(storage_root, &projections, &accounting_entries)
        .map_err(|error| miette!("session index rebuild failed: {error}"))?;
    Ok(())
}

pub(super) fn collect_abandoned_empty_sessions(storage_root: &Path) -> Result<()> {
    let removed = garbage_collect_empty_sessions(storage_root)
        .map_err(|error| miette!("empty session cleanup failed: {error}"))?;
    if removed.is_empty() || !storage_root.join("index.sqlite").is_file() {
        return Ok(());
    }
    let index = SessionIndex::open(storage_root)
        .map_err(|error| miette!("session index could not open: {error}"))?;
    for session_id in &removed {
        index
            .remove(session_id)
            .map_err(|error| miette!("empty session index cleanup failed: {error}"))?;
    }
    tracing::debug!(count = removed.len(), "removed abandoned empty sessions");
    Ok(())
}

pub(super) fn session_has_user_turn(events: &[EngineEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            EngineEvent::TurnStarted { .. } | EngineEvent::UserMessageAccepted { .. }
        )
    })
}

pub(super) fn update_one_session_index(
    storage_root: &Path,
    session_id: &str,
    sink: &DurableEventSink,
) -> Result<()> {
    let events = sink.load()?;
    if !session_has_user_turn(&events) {
        if storage_root.join("index.sqlite").is_file() {
            SessionIndex::open(storage_root)
                .and_then(|index| index.remove(session_id))
                .map_err(|error| miette!("empty session index cleanup failed: {error}"))?;
        }
        return Ok(());
    }
    let path = storage_root
        .join("sessions")
        .join(session_id)
        .join("journal");
    let projection = project_session(session_id, &events, &path);
    SessionIndex::open(storage_root)
        .map_err(|error| miette!("session index could not open: {error}"))?
        .upsert(&projection)
        .map_err(|error| miette!("session index could not update: {error}"))?;
    let accounting_entries = project_accounting(
        session_id,
        &events,
        inherited_accounting_through(storage_root, session_id)?,
    )?;
    AccountingLedger::open(storage_root)
        .and_then(|ledger| ledger.reconcile(&accounting_entries))
        .map_err(|error| miette!("session accounting could not update: {error}"))
}

pub(super) fn is_session_projection_boundary(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::SessionCreated { .. }
            | EngineEvent::UserMessageAccepted { .. }
            | EngineEvent::TurnFinished { .. }
            | EngineEvent::SessionTitleUpdated { .. }
            | EngineEvent::ConversationRewound { .. }
    )
}

pub(super) fn upsert_session_projection(
    storage_root: &Path,
    projection: &SessionProjection,
) -> Result<()> {
    SessionIndex::open(storage_root)
        .map_err(|error| miette!("session index could not open: {error}"))?
        .upsert(projection)
        .map_err(|error| miette!("session index could not update: {error}"))
}

pub(super) fn project_accounting(
    session_id: &str,
    events: &[EngineEvent],
    inherited_through: Option<SequenceId>,
) -> Result<Vec<TurnAccountingEntry>> {
    events
        .iter()
        .filter(|event| {
            inherited_through.is_none_or(|boundary| {
                event
                    .meta()
                    .is_none_or(|meta| meta.sequence_id.0 > boundary.0)
            })
        })
        .filter_map(|event| match event {
            EngineEvent::TurnFinished {
                meta,
                turn_id,
                usage,
                cost,
                ..
            } => Some((
                meta,
                turn_id.clone(),
                usage,
                cost,
                AccountingAttribution::Main,
            )),
            EngineEvent::CompactionFinished {
                meta,
                summary_turn_id,
                usage: Some(usage),
                cost: Some(cost),
                ..
            }
            | EngineEvent::CompactionAttemptFinished {
                meta,
                summary_turn_id,
                usage,
                cost,
            } => Some((
                meta,
                summary_turn_id.clone(),
                usage,
                cost,
                AccountingAttribution::Compaction,
            )),
            EngineEvent::SessionTitleUpdated {
                meta,
                usage: Some(usage),
                cost: Some(cost),
                ..
            } => Some((
                meta,
                rw_core::TurnId("title".to_owned()),
                usage,
                cost,
                AccountingAttribution::Title,
            )),
            _ => None,
        })
        .map(|(meta, turn_id, usage, cost, attribution)| {
            let emitted_at_utc = UtcTimestamp::parse(meta.emitted_at.clone()).map_err(|error| {
                miette!(
                    "turn {} has a malformed accounting timestamp: {error}",
                    turn_id.0
                )
            })?;
            Ok(TurnAccountingEntry {
                session_id: session_id.to_owned(),
                turn_id,
                sequence_id: meta.sequence_id,
                utc_day: emitted_at_utc.utc_day(),
                emitted_at_utc,
                attribution,
                usage: usage.clone(),
                cost: cost.clone(),
            })
        })
        .collect()
}

pub(super) fn project_session(
    session_id: &str,
    events: &[EngineEvent],
    path: &Path,
) -> SessionProjection {
    HostedSessionProjection::from_events(session_id, events, path).projection
}

pub(super) fn session_projection_updated_at(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|_| SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub(super) fn compact_title(content: &str) -> String {
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(80).collect()
}
