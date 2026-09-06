//! A REPL message owns its event and admits preparation plus final encoded bytes.
use super::{EngineEvent, MAX_REPL_OUTPUT_BYTES, OutputFormat, public_cli_event};
use miette::{IntoDiagnostic as _, Result, miette};
use rw_types::{allocation::AllocationPlan, json_encoding::JsonWriter};
use serde::Serialize;
use std::{fmt, io::Write as _};

pub(super) fn message(event: EngineEvent, format: OutputFormat) -> Result<Option<String>> {
    encode(event, format, MAX_REPL_OUTPUT_BYTES)
}
fn encode(event: EngineEvent, format: OutputFormat, limit: usize) -> Result<Option<String>> {
    let plan = AllocationPlan::new(public_cli_event(event)).map_err(|_| exhausted())?;
    let output_limit = plan
        .bytes()
        .checked_mul(2)
        .and_then(|bytes| limit.checked_sub(bytes))
        .ok_or_else(exhausted)?;
    if format == OutputFormat::StreamJson {
        let mut count = JsonWriter::count(output_limit);
        count.serialize(plan.value()).into_diagnostic()?;
        count.write_all(b"\n").into_diagnostic()?;
        let length = count.written();
        let event = plan.prepare();
        let mut bytes = Vec::with_capacity(length);
        let mut output = JsonWriter::buffer(&mut bytes, length, 0).into_diagnostic()?;
        output.serialize(event.value()).into_diagnostic()?;
        output.write_all(b"\n").into_diagnostic()?;
        return String::from_utf8(bytes).map(Some).into_diagnostic();
    }
    text(plan.prepare().into_inner(), output_limit)
}
fn exhausted() -> miette::Report {
    miette!("REPL output allocation admission exceeded")
}

fn formatted(args: fmt::Arguments<'_>, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    JsonWriter::buffer(&mut bytes, limit, 0)
        .into_diagnostic()?
        .write_fmt(args)
        .into_diagnostic()?;
    String::from_utf8(bytes).into_diagnostic()
}
fn pretty(value: &impl Serialize, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    let mut output = JsonWriter::buffer(&mut bytes, limit, 0).into_diagnostic()?;
    serde_json::to_writer_pretty(&mut output, value).into_diagnostic()?;
    output.write_all(b"\n").into_diagnostic()?;
    String::from_utf8(bytes).into_diagnostic()
}
fn text(event: EngineEvent, limit: usize) -> Result<Option<String>> {
    Ok(match event {
        EngineEvent::TextDelta { text, .. } | EngineEvent::ToolOutputDelta { chunk: text, .. } => {
            Some(text)
        }
        EngineEvent::ContextSnapshotReady { snapshot, .. } => Some(pretty(&snapshot, limit)?),
        EngineEvent::CostSnapshotReady { snapshot, .. } => Some(pretty(&snapshot, limit)?),
        EngineEvent::ContextItemPinned { item_id, .. } => Some(formatted(
            format_args!("pinned context item {}\n", item_id.0),
            limit,
        )?),
        EngineEvent::ContextItemEvicted { item_id, .. } => Some(formatted(
            format_args!("evicted context item {}\n", item_id.0),
            limit,
        )?),
        EngineEvent::CompactionStarted { reason, .. } => Some(formatted(
            format_args!("compaction started ({reason:?})\n"),
            limit,
        )?),
        EngineEvent::CompactionAttemptFinished { cost, .. } => Some(formatted(
            format_args!("compaction attempt accounted ({cost:?})\n"),
            limit,
        )?),
        EngineEvent::CompactionFinished {
            reclaimed_tokens, ..
        } => Some(formatted(
            format_args!("compaction finished; reclaimed {reclaimed_tokens} estimated tokens\n"),
            limit,
        )?),
        EngineEvent::BudgetStatusChanged {
            level,
            scope,
            current,
            limit: bound,
            ..
        } => Some(formatted(
            format_args!("budget {level:?} ({scope:?}): {current}/{bound}\n"),
            limit,
        )?),
        EngineEvent::CommandFinished { message, .. } => {
            Some(formatted(format_args!("{message}\n"), limit)?)
        }
        EngineEvent::GuardTriggered { message, .. } => {
            Some(formatted(format_args!("error: {message}\n"), limit)?)
        }
        EngineEvent::Error { error, .. } => Some(formatted(
            format_args!("error: {}\n", error.message),
            limit,
        )?),
        _ => None,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{encode, formatted};
    use crate::cli_args::OutputFormat;
    use rw_types::{
        EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, TurnId,
        allocation::PrepareAllocation as _,
    };
    fn event(signature: Option<String>) -> EngineEvent {
        EngineEvent::ThinkingDelta {
            meta: EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: SessionId("repl".into()),
                sequence_id: SequenceId(0),
                emitted_at: "2026-01-01T00:00:00Z".into(),
                caused_by: None,
            },
            turn_id: TurnId("turn".into()),
            text: "Unicode 🦀\n\0".into(),
            signature,
        }
    }
    #[test]
    fn message_counts_public_projection_before_preparation_and_encoding() {
        let projected = event(None);
        let mut expected = serde_json::to_string(&projected).expect("public bytes");
        expected.push('\n');
        let limit = projected.prepared_bytes().expect("prepared size") * 2 + expected.len();
        let actual = encode(
            event(Some("opaque".repeat(4096))),
            OutputFormat::StreamJson,
            limit,
        )
        .expect("private signature retired before admission")
        .expect("message");
        assert_eq!(actual, expected);
        assert_eq!(actual.capacity(), actual.len());
        assert!(encode(event(None), OutputFormat::StreamJson, limit - 1).is_err());
    }
    #[test]
    fn human_text_formatting_admits_prefix_and_newline() {
        let exact = formatted(format_args!("error: {}\n", "🦀"), 12).expect("exact text");
        assert_eq!(exact, "error: 🦀\n");
        assert!(exact.capacity() <= 12);
        assert!(formatted(format_args!("error: {}\n", "🦀"), 11).is_err());
    }
}
