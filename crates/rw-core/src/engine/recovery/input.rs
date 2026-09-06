//! Resolve a committed input against its one authoritative accepted body.
use super::RecoveryError;
use rw_store::session::{SessionEventPageLimits, journal::JournalReadView};
use rw_types::{EngineEvent, Turn, conversation_input::InputSelection};
use std::borrow::Cow;

/// Resolve only the input reference; all other events remain borrowed. The returned
/// materialization preserves the commit's identity and is never appended to a journal.
/// The caller's source-read allowance owns the accepted decode and resulting IR.
///
/// # Errors
/// Rejects missing, forward, foreign-session, wrong-turn, or redundant text selectors.
pub fn materialize_input_event<'a>(
    source: &JournalReadView,
    event: &'a EngineEvent,
) -> Result<Cow<'a, EngineEvent>, RecoveryError> {
    let EngineEvent::ConversationInputCommitted {
        meta,
        agent_turn,
        accepted_source,
        ..
    } = event
    else {
        return Ok(Cow::Borrowed(event));
    };
    if *accepted_source >= meta.sequence_id {
        return Err(RecoveryError::Invalid(
            "accepted input must precede its commit",
        ));
    }
    let limits = SessionEventPageLimits::default();
    let accepted = source
        .page::<EngineEvent>(
            accepted_source.0.checked_sub(1).map(rw_types::SequenceId),
            SessionEventPageLimits {
                max_page_events: 1,
                max_page_bytes: limits.max_line_bytes as u64 + 1,
                ..limits
            },
        )?
        .events
        .into_iter()
        .next()
        .ok_or(RecoveryError::Invalid("missing accepted input source"))?
        .event;
    let turn = resolve_input(event, &accepted)?;
    Ok(Cow::Owned(EngineEvent::ConversationTurnCommitted {
        meta: meta.clone(),
        agent_turn: *agent_turn,
        turn,
    }))
}

pub(super) fn resolve_input(
    commit: &EngineEvent,
    accepted: &EngineEvent,
) -> Result<Turn, RecoveryError> {
    let EngineEvent::ConversationInputCommitted {
        meta,
        agent_turn,
        accepted_source,
        selection,
    } = commit
    else {
        return Err(RecoveryError::Invalid("input commit selector"));
    };
    let EngineEvent::UserMessageAccepted {
        meta: input_meta,
        agent_turn: input_turn,
        content,
        attachments,
    } = accepted
    else {
        return Err(RecoveryError::Invalid(
            "input source is not an accepted message",
        ));
    };
    if input_meta.sequence_id != *accepted_source
        || input_meta.sequence_id >= meta.sequence_id
        || input_meta.session_id != meta.session_id
        || input_turn != agent_turn
        || input_meta.protocol_version != meta.protocol_version
    {
        return Err(RecoveryError::Invalid("accepted input source identity"));
    }
    let message = crate::engine::dispatch::recover_user_message(content, attachments)
        .map_err(crate::engine::SessionProjectionError::InvalidAttachment)?;
    Ok(message.turn(selected_text(content, selection)?.to_owned()))
}

pub(in crate::engine) fn selected_text<'a>(
    accepted: &'a str,
    selection: &'a InputSelection,
) -> Result<&'a str, RecoveryError> {
    match selection {
        InputSelection::Accepted {} => Ok(accepted),
        InputSelection::Transformed { text } if text != accepted => Ok(text),
        InputSelection::Transformed { .. } => Err(RecoveryError::Invalid(
            "unchanged input requires the accepted selector",
        )),
    }
}
