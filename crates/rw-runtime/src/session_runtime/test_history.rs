//! Real source ownership for runtime actor fixtures.
#![cfg(test)]
use super::durable_session::{DurableEventSink, load_session_events};
use crate::journal_service::JournalService;
use rw_core::{AgentLoopError, SessionEventSink, SessionRecoveredState, recovery::SessionHistory};
use rw_store::session::SessionEventLog;
use rw_types::{ClientId, EngineEvent, EventMeta, SequenceId, SessionId, Turn};
use std::{path::Path, sync::Arc};

pub(crate) struct ActorHistory {
    pub sink: Arc<dyn SessionEventSink>,
    pub history: Arc<dyn SessionHistory>,
    pub recovered: SessionRecoveredState,
}

pub(crate) fn open(
    root: &Path,
    session: &SessionId,
    driver: Option<ClientId>,
    conversation: Vec<Turn>,
) -> Result<ActorHistory, AgentLoopError> {
    let failure = |error: rw_store::session::SessionStoreError| {
        AgentLoopError::Persistence(error.to_string())
    };
    let mut log = SessionEventLog::open(root, &session.0).map_err(failure)?;
    if log.last_sequence().is_some() && (driver.is_some() || !conversation.is_empty()) {
        return Err(AgentLoopError::InvalidConfiguration(
            "fixture seeds require an empty source".into(),
        ));
    }
    let meta = |sequence| EventMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        session_id: session.clone(),
        sequence_id: SequenceId(sequence),
        emitted_at: "2026-09-05T00:00:00.000Z".into(),
        caused_by: None,
    };
    if let Some(driver_client_id) = driver {
        log.append(EngineEvent::SessionCreated {
            meta: meta(log.next_sequence()),
            driver_client_id,
        })
        .map_err(failure)?;
    }
    for turn in conversation {
        log.append(EngineEvent::ConversationTurnCommitted {
            meta: meta(log.next_sequence()),
            agent_turn: 1,
            turn,
        })
        .map_err(failure)?;
    }
    let events = load_session_events(&log)
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    let recovered = rw_core::project_session_events(&events)
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    let sink = DurableEventSink::new(
        log,
        root.to_path_buf(),
        session.0.clone(),
        JournalService::new(root)
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?,
    )
    .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    let modes = Arc::new(
        rw_ext::ModeRegistry::builtins()
            .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?,
    );
    sink.configure_canonical(modes, None)?;
    Ok(ActorHistory {
        history: sink.clone(),
        sink,
        recovered,
    })
}
