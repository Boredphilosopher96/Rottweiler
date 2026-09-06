//! Human output borrows only displayed fields; JSON admits its public event copy.
use super::{EngineEvent, MAX_REPL_OUTPUT_BYTES, OutputFormat, public_event::PublicEventPlan};
use miette::{IntoDiagnostic as _, Result, miette};
use rw_types::json_encoding::JsonWriter;
use serde::Serialize;
use std::{fmt, io::Write as _};

pub(super) fn message(event: &EngineEvent, format: OutputFormat) -> Result<Option<String>> {
    encode(event, format, MAX_REPL_OUTPUT_BYTES)
}
fn encode(event: &EngineEvent, format: OutputFormat, limit: usize) -> Result<Option<String>> {
    if format != OutputFormat::StreamJson {
        return text(event, limit);
    }
    let plan = PublicEventPlan::new(event).ok_or_else(exhausted)?;
    let output_limit = plan
        .bytes()
        .checked_mul(2)
        .and_then(|bytes| limit.checked_sub(bytes))
        .ok_or_else(exhausted)?;
    let event = plan.prepare().ok_or_else(exhausted)?;
    let mut count = JsonWriter::count(output_limit);
    count.serialize(event.value()).into_diagnostic()?;
    count.write_all(b"\n").into_diagnostic()?;
    let length = count.written();
    let mut bytes = Vec::with_capacity(length);
    let mut output = JsonWriter::buffer(&mut bytes, length, 0).into_diagnostic()?;
    output.serialize(event.value()).into_diagnostic()?;
    output.write_all(b"\n").into_diagnostic()?;
    String::from_utf8(bytes).map(Some).into_diagnostic()
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
pub(super) fn pretty(value: &impl Serialize, limit: usize) -> Result<String> {
    let mut bytes = Vec::new();
    let mut output = JsonWriter::buffer(&mut bytes, limit, 0).into_diagnostic()?;
    serde_json::to_writer_pretty(&mut output, value).into_diagnostic()?;
    output.write_all(b"\n").into_diagnostic()?;
    String::from_utf8(bytes).into_diagnostic()
}
fn text(event: &EngineEvent, limit: usize) -> Result<Option<String>> {
    Ok(match event {
        EngineEvent::TextDelta { text, .. } | EngineEvent::ToolOutputDelta { chunk: text, .. } => {
            Some(formatted(format_args!("{text}"), limit)?)
        }
        EngineEvent::ContextSnapshotReady { snapshot, .. } => Some(pretty(snapshot, limit)?),
        EngineEvent::CostSnapshotReady { snapshot, .. } => Some(pretty(snapshot, limit)?),
        EngineEvent::PromptDumpReady { dump, .. } => Some(pretty(dump, limit)?),
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
        EngineEvent, EventMeta, PROTOCOL_VERSION, SequenceId, SessionId, ToolCallId,
        ToolInvocationId, ToolOutput, TurnId, allocation::PrepareAllocation as _,
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
    fn message_admits_public_copy_before_preparation_and_encoding() {
        let projected = event(None);
        let mut expected = serde_json::to_string(&projected).expect("public bytes");
        expected.push('\n');
        let limit = projected.prepared_bytes().expect("prepared size") * 2 + expected.len();
        let source = event(Some("opaque".repeat(4096)));
        let actual = encode(&source, OutputFormat::StreamJson, limit)
            .expect("private signature is never copied")
            .expect("message");
        assert_eq!(actual, expected);
        assert!(matches!(
            source,
            EngineEvent::ThinkingDelta {
                signature: Some(_),
                ..
            }
        ));
        assert_eq!(actual.capacity(), actual.len());
        assert!(encode(&event(None), OutputFormat::StreamJson, limit - 1).is_err());
    }
    #[test]
    fn unhandled_tool_result_needs_no_projection_or_output_credit() {
        let EngineEvent::ThinkingDelta { meta, turn_id, .. } = event(None) else {
            unreachable!("fixture variant");
        };
        let ignored = EngineEvent::ToolCallFinished {
            meta,
            turn_id,
            tool_call_id: ToolCallId("call".into()),
            invocation_id: ToolInvocationId("invocation".into()),
            output: ToolOutput::Structured {
                value: serde_json::json!({"body": "x".repeat(1024 * 1024)}),
            },
            presentation: None,
            is_error: false,
            call_index: 0,
        };
        assert!(
            encode(&ignored, OutputFormat::Text, 0)
                .expect("ignored without copy")
                .is_none()
        );
        assert!(encode(&ignored, OutputFormat::StreamJson, 0).is_err());
    }
    #[test]
    fn human_delta_admits_only_displayed_bytes_at_the_limit() {
        let EngineEvent::ThinkingDelta { meta, turn_id, .. } = event(None) else {
            unreachable!("fixture variant");
        };
        let expected = "🦀".repeat(1024);
        let event = EngineEvent::TextDelta {
            meta,
            turn_id,
            text: expected.clone(),
        };
        let actual = encode(&event, OutputFormat::Text, expected.len())
            .expect("exact displayed-byte credit")
            .expect("text");
        assert_eq!(actual, expected);
        assert!(actual.capacity() <= expected.len());
        assert!(encode(&event, OutputFormat::Text, expected.len() - 1).is_err());
    }
    #[test]
    fn human_text_formatting_admits_prefix_and_newline() {
        let exact = formatted(format_args!("error: {}\n", "🦀"), 12).expect("exact text");
        assert_eq!(exact, "error: 🦀\n");
        assert!(exact.capacity() <= 12);
        assert!(formatted(format_args!("error: {}\n", "🦀"), 11).is_err());
    }
}
