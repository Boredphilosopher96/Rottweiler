use miette::{IntoDiagnostic, Result, miette};
use rw_core::{AccountingAttribution, EngineEvent, SequenceId};
#[cfg(test)]
use rw_store::session::SessionProjection;
use rw_store::session::{
    SessionEventLog, SessionIndex, TurnAccountingEntry, UtcTimestamp,
    garbage_collect_empty_sessions,
};
use std::{
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) fn inherited_journal_through(
    storage_root: &Path,
    session_id: &str,
) -> Result<Option<SequenceId>> {
    super::session_metadata::load_session_metadata_any(storage_root, session_id)
        .map(|metadata| metadata.inherited_journal_through)
}

pub(super) fn refresh_session_index(storage_root: &Path) -> Result<()> {
    SessionIndex::reset_derived(storage_root).into_diagnostic()?;
    match std::fs::read_dir(storage_root.join("sessions")) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.into_diagnostic()?;
                if !entry.file_type().into_diagnostic()?.is_dir() {
                    continue;
                }
                let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let log = SessionEventLog::open(storage_root, &id).into_diagnostic()?;
                let source = log.read_view();
                super::search_projection::synchronize(storage_root, &id, &source)?;
                reconcile_source_accounting(storage_root, &id, &source)?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).into_diagnostic(),
    }
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

pub(super) fn is_session_projection_boundary(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::ConversationTurnCommitted { .. }
            | EngineEvent::SessionCreated { .. }
            | EngineEvent::UserMessageAccepted { .. }
            | EngineEvent::TurnFinished { .. }
            | EngineEvent::SessionTitleUpdated { .. }
            | EngineEvent::ConversationRewound { .. }
    )
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

#[cfg(test)]
pub(super) fn project_session(
    session_id: &str,
    events: &[EngineEvent],
    path: &Path,
) -> SessionProjection {
    let mut projection = SessionProjection {
        summary: rw_store::session::SessionSummary {
            id: session_id.into(),
            title: "New session".into(),
            updated_unix_ms: session_projection_updated_at(path),
            cost_micros: 0,
            turn_count: 0,
        },
        explicit_title: false,
        complete: true,
        source: rw_store::session::journal::JournalPrefixIdentity::empty(),
    };
    for event in events {
        super::search_projection::metadata(&mut projection, event);
    }
    projection
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
    content
        .split_whitespace()
        .flat_map(|word| word.chars().chain(std::iter::once(' ')))
        .take(80)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn reconcile_source_accounting(
    root: &Path,
    session: &str,
    source: &rw_store::session::journal::JournalReadView,
) -> Result<()> {
    let inherited = inherited_journal_through(root, session)?;
    let ledger = rw_store::session::AccountingLedger::open(root).into_diagnostic()?;
    let mut after = None;
    loop {
        let page = source
            .page::<EngineEvent>(
                after,
                rw_store::session::SessionEventPageLimits {
                    max_page_events: 128,
                    max_page_bytes: 16 * 1024 * 1024,
                    ..Default::default()
                },
            )
            .into_diagnostic()?;
        let next = page.next_cursor;
        let more = page.has_more;
        let events = page
            .events
            .into_iter()
            .map(|envelope| envelope.event)
            .collect::<Vec<_>>();
        ledger
            .reconcile(&project_accounting(session, &events, inherited)?)
            .into_diagnostic()?;
        if !more {
            return Ok(());
        }
        if next == after {
            return Err(miette!("accounting refresh made no progress"));
        }
        after = next;
    }
}
