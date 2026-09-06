//! One admitted event and one merged section accompany the bounded export buffer.
use super::{
    EngineEvent, EventEnvelope, FixtureRedactor, MAX_HISTORY_BYTES, MAX_HISTORY_EVENTS,
    TranscriptFormat, TranscriptSection, output::Output, transcript_section,
};
use miette::{IntoDiagnostic as _, Result, miette};
use rw_types::json_encoding::JsonWriter;
use rw_types::json_structure::{JsonStructureLimits, preflight_json};
use serde::{Serialize, Serializer, ser::SerializeSeq};
use serde_json::Value;
use std::io::Write as _;

const VALUE_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn render(
    session: &str,
    events: &[EventEnvelope<EngineEvent>],
    format: TranscriptFormat,
    redactor: &FixtureRedactor,
    limit: usize,
) -> Result<Vec<u8>> {
    if events.len() > MAX_HISTORY_EVENTS {
        return Err(miette!("history event admission exceeded"));
    }
    let mut output = Output::new(limit);
    match format {
        TranscriptFormat::Json => {
            #[derive(Serialize)]
            struct Document<'a> {
                schema_version: u32,
                session_id: &'a str,
                events: JsonEvents<'a>,
            }
            serde_json::to_writer_pretty(
                &mut output.writer().into_diagnostic()?,
                &Document {
                    schema_version: 1,
                    session_id: session,
                    events: JsonEvents { events, redactor },
                },
            )
            .into_diagnostic()?;
        }
        TranscriptFormat::Markdown | TranscriptFormat::Html => {
            begin(&mut output, session, format)?;
            visit_sections(Sanitized::new(events, redactor), limit, |section| {
                write_section(&mut output, &section, format)
            })?;
            if format == TranscriptFormat::Html {
                output.push("</main></body></html>\n").into_diagnostic()?;
            }
        }
    }
    if !output.ends_in_newline() {
        output.push("\n").into_diagnostic()?;
    }
    Ok(output.finish())
}

struct JsonEvents<'a> {
    events: &'a [EventEnvelope<EngineEvent>],
    redactor: &'a FixtureRedactor,
}
impl Serialize for JsonEvents<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.events.len()))?;
        for event in Sanitized::new(self.events, self.redactor) {
            let event = event.map_err(serde::ser::Error::custom)?;
            sequence.serialize_element(&event)?;
        }
        sequence.end()
    }
}

struct Sanitized<'a> {
    events: std::slice::Iter<'a, EventEnvelope<EngineEvent>>,
    redactor: &'a FixtureRedactor,
    remaining: u64,
}
impl<'a> Sanitized<'a> {
    fn new(events: &'a [EventEnvelope<EngineEvent>], redactor: &'a FixtureRedactor) -> Self {
        Self {
            events: events.iter(),
            redactor,
            remaining: MAX_HISTORY_BYTES,
        }
    }
    fn prepare(&mut self, event: &EventEnvelope<EngineEvent>) -> Result<Value> {
        let limit = rw_store::session::SessionEventPageLimits::default().max_line_bytes;
        let mut bytes = Vec::new();
        JsonWriter::buffer(&mut bytes, limit, 4096)
            .into_diagnostic()?
            .serialize(event)
            .into_diagnostic()?;
        self.remaining = self
            .remaining
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| miette!("history encoded admission exceeded"))?;
        let shape = preflight_json(
            &bytes,
            JsonStructureLimits {
                max_encoded_bytes: limit,
                max_nodes: 65_536,
                max_string_bytes: limit,
                max_depth: 64,
            },
        )
        .into_diagnostic()?;
        if shape
            .direct_value_decode_bytes()
            .is_none_or(|bytes| bytes > VALUE_BYTES)
        {
            return Err(miette!("history value allocation admission exceeded"));
        }
        let value: Value = serde_json::from_slice(&bytes).into_diagnostic()?;
        drop(bytes);
        super::redaction::redact_export_value(value, self.redactor, VALUE_BYTES).into_diagnostic()
    }
}
impl Iterator for Sanitized<'_> {
    type Item = Result<Value>;
    fn next(&mut self) -> Option<Self::Item> {
        self.events.next().map(|event| self.prepare(event))
    }
}

pub(super) fn visit_sections(
    events: impl IntoIterator<Item = Result<Value>>,
    limit: usize,
    mut visit: impl FnMut(TranscriptSection) -> Result<()>,
) -> Result<()> {
    let mut pending: Option<TranscriptSection> = None;
    for event in events {
        let section = transcript_section(&event?)?;
        if let Some(previous) = &mut pending
            && section.merge_key.is_some()
            && section.merge_key == previous.merge_key
        {
            let mut bytes = std::mem::take(&mut previous.body).into_bytes();
            JsonWriter::buffer(&mut bytes, limit, 4096)
                .into_diagnostic()?
                .write_all(section.body.as_bytes())
                .into_diagnostic()?;
            previous.body = String::from_utf8(bytes).into_diagnostic()?;
        } else {
            if let Some(previous) = pending.take() {
                visit(previous)?;
            }
            pending = Some(section);
        }
    }
    if let Some(section) = pending {
        visit(section)?;
    }
    Ok(())
}

fn begin(output: &mut Output, session: &str, format: TranscriptFormat) -> Result<()> {
    if format == TranscriptFormat::Markdown {
        output.push("# Rottweiler transcript: ").into_diagnostic()?;
        output.markdown(session).into_diagnostic()?;
        output.push("\n\n").into_diagnostic()?;
    } else {
        output.push("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Rottweiler transcript ").into_diagnostic()?;
        output.html(session).into_diagnostic()?;
        output.push("</title><style>body{font:16px/1.5 system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem;color:#202124}section{border-top:1px solid #ddd;padding:1rem 0}h1,h2{line-height:1.2}dl{display:flex;gap:1rem;flex-wrap:wrap;color:#5f6368;font-size:.875rem}dt{font-weight:600}dd{margin:0}pre{white-space:pre-wrap;overflow-wrap:anywhere;background:#f6f8fa;padding:1rem;border-radius:.5rem}</style></head><body><main><h1>Rottweiler transcript ").into_diagnostic()?;
        output.html(session).into_diagnostic()?;
        output.push("</h1>\n").into_diagnostic()?;
    }
    Ok(())
}
fn write_section(
    output: &mut Output,
    section: &TranscriptSection,
    format: TranscriptFormat,
) -> Result<()> {
    if format == TranscriptFormat::Markdown {
        markdown_section(output, section)
    } else {
        html_section(output, section)
    }
}
fn markdown_section(output: &mut Output, section: &TranscriptSection) -> Result<()> {
    output.push("## ").into_diagnostic()?;
    output.markdown(&section.title).into_diagnostic()?;
    output.push("\n\n").into_diagnostic()?;
    if !section.metadata.is_empty() {
        output.push("*").into_diagnostic()?;
        for (index, (label, value)) in section.metadata.iter().enumerate() {
            if index != 0 {
                output.push(" · ").into_diagnostic()?;
            }
            output.markdown(label).into_diagnostic()?;
            output.push(": ").into_diagnostic()?;
            output.markdown(value).into_diagnostic()?;
        }
        output.push("*\n\n").into_diagnostic()?;
    }
    if !section.body.is_empty() {
        for line in section.body.lines() {
            output.push("> ").into_diagnostic()?;
            output.push(line).into_diagnostic()?;
            output.push("\n").into_diagnostic()?;
        }
        output.push("\n").into_diagnostic()?;
    }
    Ok(())
}
fn html_section(output: &mut Output, section: &TranscriptSection) -> Result<()> {
    output.push("<section><h2>").into_diagnostic()?;
    output.html(&section.title).into_diagnostic()?;
    output.push("</h2>").into_diagnostic()?;
    if !section.metadata.is_empty() {
        output.push("<dl>").into_diagnostic()?;
        for (label, value) in &section.metadata {
            output.push("<div><dt>").into_diagnostic()?;
            output.html(label).into_diagnostic()?;
            output.push("</dt><dd>").into_diagnostic()?;
            output.html(value).into_diagnostic()?;
            output.push("</dd></div>").into_diagnostic()?;
        }
        output.push("</dl>").into_diagnostic()?;
    }
    if !section.body.is_empty() {
        output.push("<pre>").into_diagnostic()?;
        output.html(&section.body).into_diagnostic()?;
        output.push("</pre>").into_diagnostic()?;
    }
    output.push("</section>\n").into_diagnostic()?;
    Ok(())
}

pub(super) fn pretty(value: &Value) -> Result<String> {
    let mut output = Output::new(super::MAX_RENDERED_BYTES);
    serde_json::to_writer_pretty(&mut output.writer().into_diagnostic()?, value)
        .into_diagnostic()?;
    output.text().into_diagnostic()
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
