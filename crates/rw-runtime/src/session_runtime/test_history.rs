//! Real source ownership for runtime actor fixtures.
#![cfg(test)]
use super::durable_session::DurableEventSink;
use crate::journal_service::JournalService;
use rw_core::{AgentLoopError, SessionActorRecovery, SessionEventSink, recovery::SessionHistory};
use rw_store::session::SessionEventLog;
use rw_types::{
    Block, ClientId, EngineEvent, EventMeta, Role, SequenceId, SessionId, Turn, TurnMeta,
};
use std::{path::Path, sync::Arc};

pub(crate) struct ActorHistory {
    pub sink: Arc<dyn SessionEventSink>,
    pub history: Arc<dyn SessionHistory>,
    pub recovered: SessionActorRecovery,
}

pub(crate) async fn open(
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
        if turn.role == Role::User {
            let [Block::Text { text }] = turn.blocks.as_slice() else {
                return Err(AgentLoopError::InvalidConfiguration(
                    "input fixture requires one text body".into(),
                ));
            };
            if turn.meta != TurnMeta::default() {
                return Err(AgentLoopError::InvalidConfiguration(
                    "input fixture metadata must be empty".into(),
                ));
            }
            log.append_batch(input_events(meta(log.next_sequence()), 1, text.clone()))
                .map_err(failure)?;
        } else {
            log.append(EngineEvent::ConversationTurnCommitted {
                meta: meta(log.next_sequence()),
                agent_turn: 1,
                turn,
            })
            .map_err(failure)?;
        }
    }
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
    let recovered =
        SessionActorRecovery::from_bootstrap(sink.capture_history().await?.bootstrap().await?)?;
    Ok(ActorHistory {
        history: sink.clone(),
        sink,
        recovered,
    })
}

/// Seed actual accepted input and its exact immutable commit reference.
#[allow(clippy::expect_used)]
pub(crate) fn input_events(meta: EventMeta, agent_turn: u64, content: String) -> [EngineEvent; 2] {
    let mut committed = meta.clone();
    committed.sequence_id =
        SequenceId(meta.sequence_id.0.checked_add(1).expect("fixture sequence"));
    let accepted_source = meta.sequence_id;
    [
        EngineEvent::UserMessageAccepted {
            meta,
            agent_turn,
            content,
            attachments: vec![],
        },
        EngineEvent::ConversationInputCommitted {
            meta: committed,
            agent_turn,
            accepted_source,
            selection: rw_types::conversation_input::InputSelection::Accepted {},
        },
    ]
}
