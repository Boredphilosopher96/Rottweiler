//! Incremental source-qualified search without retaining lifetime transcript bodies.
use super::accounting_projection::{compact_title, session_projection_updated_at};
use miette::{IntoDiagnostic, Result, miette};
use rw_core::EngineEvent;
use rw_store::session::{
    SearchDocumentWriter, SessionEventPageLimits, SessionIndex, SessionProjection,
    SessionStoreError, SessionSummary,
    journal::{JournalPrefixIdentity, JournalReadView},
};
use rw_types::{Block, Role, SequenceId, ToolOutput, ToolOutputPart};
use std::path::Path;

pub(super) fn synchronize(root: &Path, session: &str, source: &JournalReadView) -> Result<()> {
    let index = match SessionIndex::open(root) {
        Ok(index) => index,
        Err(SessionStoreError::UnsupportedSqliteSchema {
            table: "sessions" | "search_documents",
        }) => SessionIndex::reset_derived(root).into_diagnostic()?,
        Err(error) => return Err(error).into_diagnostic(),
    };
    let mut stored = index.projection(session).into_diagnostic()?;
    if let Some(projection) = &stored
        && source.at_prefix(projection.source).is_err()
    {
        index.remove(session).into_diagnostic()?;
        stored = None;
    }
    let mut expected = stored.as_ref().map(|projection| projection.source);
    let mut projection = stored.unwrap_or_else(|| empty(session));
    loop {
        if projection.source == source.prefix_identity() {
            if expected.is_none() {
                index.upsert(&projection).into_diagnostic()?;
            }
            return Ok(());
        }
        let after = projection
            .source
            .next_sequence
            .checked_sub(1)
            .map(SequenceId);
        let page = source
            .page::<EngineEvent>(
                after,
                SessionEventPageLimits {
                    max_page_events: 128,
                    max_page_bytes: 16 * 1024 * 1024,
                    ..SessionEventPageLimits::default()
                },
            )
            .into_diagnostic()?;
        if page.next_cursor == after {
            return Err(miette!("search projection made no source progress"));
        }
        for envelope in &page.events {
            let meta = envelope
                .event
                .meta()
                .ok_or_else(|| miette!("search source contains transient data"))?;
            if meta.session_id.0 != session || meta.sequence_id != envelope.sequence {
                return Err(miette!(
                    "search source event identity differs from its envelope"
                ));
            }
            metadata(&mut projection, &envelope.event);
        }
        projection.complete = !page.has_more;
        projection.source = source
            .prefix_through(page.next_cursor)
            .into_diagnostic()?
            .prefix_identity();
        projection.summary.updated_unix_ms =
            session_projection_updated_at(&root.join("sessions").join(session).join("journal"));
        index
            .apply_page(expected, &projection, |writer| {
                for envelope in &page.events {
                    documents(writer, &envelope.event)?;
                }
                Ok(())
            })
            .into_diagnostic()?;
        expected = Some(projection.source);
    }
}

fn empty(session: &str) -> SessionProjection {
    SessionProjection {
        summary: SessionSummary {
            id: session.into(),
            title: "New session".into(),
            updated_unix_ms: 0,
            cost_micros: 0,
            turn_count: 0,
        },
        explicit_title: false,
        complete: true,
        source: JournalPrefixIdentity::empty(),
    }
}

pub(super) fn metadata(projection: &mut SessionProjection, event: &EngineEvent) {
    match event {
        EngineEvent::SessionTitleUpdated { title, .. } => {
            projection.summary.title.clone_from(title);
            projection.explicit_title = true;
        }
        EngineEvent::UserMessageAccepted {
            content,
            agent_turn,
            ..
        } => {
            if projection.summary.turn_count == 0 && !projection.explicit_title {
                projection.summary.title = compact_title(content);
            }
            projection.summary.turn_count = i64::try_from(*agent_turn).unwrap_or(i64::MAX);
        }
        EngineEvent::ConversationRewound { to_agent_turn, .. } => {
            projection.summary.turn_count = i64::try_from(*to_agent_turn).unwrap_or(i64::MAX);
            if *to_agent_turn == 0 && !projection.explicit_title {
                "New session".clone_into(&mut projection.summary.title);
            }
        }
        _ => {}
    }
}

fn documents(
    writer: &SearchDocumentWriter<'_>,
    event: &EngineEvent,
) -> Result<(), SessionStoreError> {
    let Some(meta) = event.meta() else {
        return Err(SessionStoreError::CorruptEvent("transient search event"));
    };
    let mut part = 0;
    match event {
        EngineEvent::UserMessageAccepted {
            content,
            agent_turn,
            ..
        } => writer.text(*agent_turn, meta.sequence_id, part, content),
        EngineEvent::ConversationTurnCommitted {
            agent_turn, turn, ..
        } if turn.role == Role::Assistant => {
            for block in &turn.blocks {
                if let Block::Text { text } = block {
                    text_field(writer, *agent_turn, meta.sequence_id, &mut part, text)?;
                }
            }
            Ok(())
        }
        EngineEvent::ToolCallFinished {
            turn_id, output, ..
        } => {
            let turn = turn_id
                .0
                .parse::<u64>()
                .map_err(|_| SessionStoreError::CorruptEvent("non-numeric search turn"))?;
            tool_fields(writer, turn, meta.sequence_id, &mut part, output)
        }
        EngineEvent::ConversationRewound { to_agent_turn, .. } => writer.rewind(*to_agent_turn),
        _ => Ok(()),
    }
}

fn text_field(
    writer: &SearchDocumentWriter<'_>,
    turn: u64,
    sequence: SequenceId,
    part: &mut u32,
    text: &str,
) -> Result<(), SessionStoreError> {
    writer.text(turn, sequence, *part, text)?;
    *part = part
        .checked_add(1)
        .ok_or(SessionStoreError::SequenceOverflow)?;
    Ok(())
}
fn tool_fields(
    writer: &SearchDocumentWriter<'_>,
    turn: u64,
    sequence: SequenceId,
    part: &mut u32,
    output: &ToolOutput,
) -> Result<(), SessionStoreError> {
    match output {
        ToolOutput::Text { text } => text_field(writer, turn, sequence, part, text),
        ToolOutput::Structured { value } => json_fields(writer, turn, sequence, part, value),
        ToolOutput::Mixed { parts } => {
            for item in parts {
                match item {
                    ToolOutputPart::Text { text } => {
                        text_field(writer, turn, sequence, part, text)?
                    }
                    ToolOutputPart::Structured { value } => {
                        json_fields(writer, turn, sequence, part, value)?
                    }
                    ToolOutputPart::Image { .. } => {}
                }
            }
            Ok(())
        }
    }
}
fn json_fields(
    writer: &SearchDocumentWriter<'_>,
    turn: u64,
    sequence: SequenceId,
    part: &mut u32,
    value: &serde_json::Value,
) -> Result<(), SessionStoreError> {
    match value {
        serde_json::Value::String(text) => text_field(writer, turn, sequence, part, text),
        serde_json::Value::Array(values) => {
            for value in values {
                json_fields(writer, turn, sequence, part, value)?;
            }
            Ok(())
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                text_field(writer, turn, sequence, part, key)?;
                json_fields(writer, turn, sequence, part, value)?;
            }
            Ok(())
        }
        scalar => text_field(writer, turn, sequence, part, &scalar.to_string()),
    }
}
