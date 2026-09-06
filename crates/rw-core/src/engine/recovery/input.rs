//! Resolve a committed input against its one authoritative accepted body.
use super::RecoveryError;
use rw_store::session::{SessionEventPageLimits, journal::JournalReadView};
use rw_types::{EngineEvent, Turn, conversation_input::InputSelection};
use std::borrow::Cow;

#[derive(Clone, Copy)]
pub(super) enum EventSource<'a> {
    Journal(&'a JournalReadView),
    Audit(&'a [EngineEvent]),
}

/// Resolve a source event after canonical or published-index authority checked its claim.
pub(crate) fn materialize_indexed_event<'a>(
    source: &JournalReadView,
    event: &'a EngineEvent,
) -> Result<Cow<'a, EngineEvent>, RecoveryError> {
    materialize(EventSource::Journal(source), event)
}

pub(crate) fn materialize_claimed_event<'a>(
    source: &JournalReadView,
    checked: rw_types::input_claims::InputClaimChecked<'a>,
) -> Result<Cow<'a, EngineEvent>, RecoveryError> {
    materialize_indexed_event(source, checked.into_event())
}

pub(in crate::engine) fn materialize_audit_event<'a>(
    source: &[EngineEvent],
    event: &'a EngineEvent,
) -> Result<Cow<'a, EngineEvent>, RecoveryError> {
    materialize(EventSource::Audit(source), event)
}

fn materialize<'a>(
    source: EventSource<'_>,
    event: &'a EngineEvent,
) -> Result<Cow<'a, EngineEvent>, RecoveryError> {
    let (meta, agent_turn, turn) = match event {
        EngineEvent::ConversationInputCommitted {
            meta,
            agent_turn,
            accepted_source,
            ..
        } => {
            let accepted = read_source(source, *accepted_source, meta)?;
            (meta, *agent_turn, resolve_input(event, &accepted)?)
        }
        EngineEvent::ConversationToolResultsCommitted {
            meta,
            agent_turn,
            results,
            logical,
        } => (
            meta,
            *agent_turn,
            super::tool_results::resolve(source, meta, *agent_turn, results, logical)?,
        ),
        EngineEvent::ConversationContextCommitted {
            meta,
            agent_turn,
            selection,
        } => (meta, *agent_turn, resolve_context(source, meta, selection)?),
        EngineEvent::ConversationTurnCommitted { turn, .. }
            if matches!(turn.role, rw_types::Role::User | rw_types::Role::Tool) =>
        {
            return Err(RecoveryError::Invalid(
                "user/tool conversation requires an explicit source",
            ));
        }
        _ => return Ok(Cow::Borrowed(event)),
    };
    Ok(Cow::Owned(EngineEvent::ConversationTurnCommitted {
        meta: meta.clone(),
        agent_turn,
        turn,
    }))
}

pub(super) fn read_source(
    source: EventSource<'_>,
    selected: rw_types::SequenceId,
    owner: &rw_types::EventMeta,
) -> Result<EngineEvent, RecoveryError> {
    if selected >= owner.sequence_id {
        return Err(RecoveryError::Invalid(
            "conversation source must precede its commit",
        ));
    }
    let limits = SessionEventPageLimits::default();
    let event = match source {
        EventSource::Journal(source) => {
            source
                .page::<EngineEvent>(
                    selected.0.checked_sub(1).map(rw_types::SequenceId),
                    SessionEventPageLimits {
                        max_page_events: 1,
                        max_page_bytes: limits.max_line_bytes as u64 + 1,
                        ..limits
                    },
                )?
                .events
                .into_iter()
                .next()
                .ok_or(RecoveryError::Invalid("missing conversation source"))?
                .event
        }
        EventSource::Audit(events) => events
            .binary_search_by_key(&selected.0, |event| {
                event.meta().map_or(u64::MAX, |meta| meta.sequence_id.0)
            })
            .ok()
            .and_then(|index| events.get(index))
            .cloned()
            .ok_or(RecoveryError::Invalid("missing audit conversation source"))?,
    };
    if event.meta().is_none_or(|meta| {
        meta.sequence_id != selected
            || meta.session_id != owner.session_id
            || meta.protocol_version != owner.protocol_version
    }) {
        return Err(RecoveryError::Invalid("conversation source identity"));
    }
    Ok(event)
}

fn resolve_context(
    source: EventSource<'_>,
    meta: &rw_types::EventMeta,
    selection: &rw_types::conversation_input::ContextSelection,
) -> Result<Turn, RecoveryError> {
    use rw_types::conversation_input::ContextSelection;
    match selection {
        ContextSelection::Continuation {} => Ok(rw_context::auto_continue_turn()),
        ContextSelection::PlanReview { source: review } => {
            if review.0.checked_add(1) != Some(meta.sequence_id.0) {
                return Err(RecoveryError::Invalid(
                    "plan context must follow its review",
                ));
            }
            let EngineEvent::PlanReviewed {
                artifact,
                decision,
                revisions,
                ..
            } = read_source(source, *review, meta)?
            else {
                return Err(RecoveryError::Invalid("plan context source"));
            };
            crate::engine::projection::plan_review_context_turn(
                &artifact,
                decision,
                revisions.as_deref(),
            )
            .ok_or(RecoveryError::Invalid(
                "plan review has no conversation context",
            ))
        }
        ContextSelection::Retained {
            selected_source,
            body_source,
        } => {
            if body_source > selected_source || selected_source >= &meta.sequence_id {
                return Err(RecoveryError::Invalid("retained context source order"));
            }
            let body = read_source(source, *body_source, meta)?;
            if matches!(
                body,
                EngineEvent::ConversationContextCommitted {
                    selection: ContextSelection::Retained { .. },
                    ..
                }
            ) {
                return Err(RecoveryError::Invalid(
                    "retained context must point to a terminal body source",
                ));
            }
            let resolved = materialize(source, &body)?;
            match resolved.into_owned() {
                EngineEvent::ConversationTurnCommitted { turn, .. }
                    if matches!(turn.role, rw_types::Role::User | rw_types::Role::Tool) =>
                {
                    Ok(turn)
                }
                EngineEvent::UserShellStateChanged {
                    command,
                    active: false,
                    status: Some(status),
                    captured_output,
                    ..
                } => Ok(crate::engine::projection::shell_context_turn(
                    command.as_deref().unwrap_or_default(),
                    status,
                    captured_output.as_deref(),
                )),
                _ => Err(RecoveryError::Invalid(
                    "retained context must select source-owned context",
                )),
            }
        }
    }
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
        || input_turn > agent_turn
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
