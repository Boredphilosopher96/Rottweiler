//! A finite JSON result owns admitted events; streaming formats retain only scalars.
use crate::cli_args::OutputFormat;
use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{EngineEvent, TurnStatus, Usage};
use rw_types::allocation::AllocationPlan;
use serde::Serialize;

const MAX_JSON_BYTES: usize = 64 * 1024 * 1024;
const MAX_JSON_EVENTS: usize = 16_384;

pub(super) struct PrintOutput {
    aggregate: Option<PrintAggregate>,
    status: Option<TurnStatus>,
    ends_newline: bool,
}
impl PrintOutput {
    pub(super) fn new(session: &str, format: OutputFormat) -> Self {
        Self {
            aggregate: (format == OutputFormat::Json).then(|| PrintAggregate::new(session)),
            status: None,
            ends_newline: false,
        }
    }
    pub(super) fn push(&mut self, event: EngineEvent) -> Result<()> {
        match &event {
            EngineEvent::TextDelta { text, .. } if !text.is_empty() => {
                self.ends_newline = text.ends_with('\n');
            }
            EngineEvent::CommandFinished { .. } => self.ends_newline = true,
            EngineEvent::TurnFinished { status, .. } => self.status = Some(status.clone()),
            _ => {}
        }
        if let Some(aggregate) = &mut self.aggregate {
            aggregate.push(event)?;
        }
        Ok(())
    }
    pub(super) fn finish(self, format: OutputFormat) -> Result<Option<TurnStatus>> {
        if let Some(aggregate) = self.aggregate {
            serde_json::to_writer(std::io::stdout().lock(), &aggregate).into_diagnostic()?;
            println!();
        } else if format == OutputFormat::Text && !self.ends_newline {
            println!();
        }
        Ok(self.status)
    }
}

#[derive(Serialize)]
struct PrintAggregate {
    session_id: String,
    status: Option<TurnStatus>,
    text: String,
    usage: Usage,
    events: Vec<EngineEvent>,
    #[serde(skip)]
    event_heap: usize,
    #[serde(skip)]
    limit: usize,
}
impl PrintAggregate {
    fn new(session: &str) -> Self {
        Self {
            session_id: session.to_owned(),
            status: None,
            text: String::new(),
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            },
            events: Vec::new(),
            event_heap: 0,
            limit: MAX_JSON_BYTES,
        }
    }
    fn push(&mut self, event: EngineEvent) -> Result<()> {
        if self.events.len() >= MAX_JSON_EVENTS {
            return Err(limit_error());
        }
        let plan =
            AllocationPlan::new(super::public_cli_event(event)).map_err(|_| limit_error())?;
        let (text, newline) = text_fragment(plan.value());
        let text_len = self
            .text
            .len()
            .checked_add(text.len())
            .and_then(|bytes| bytes.checked_add(usize::from(newline)))
            .ok_or_else(limit_error)?;
        let text_capacity = growth(self.text.capacity(), text_len);
        let event_capacity = growth(self.events.capacity(), self.events.len() + 1);
        // Include old and replacement buffers during growth, and both original
        // and prepared event storage while JSON maps normalize. No history scan.
        let buffers = self
            .text
            .capacity()
            .checked_add(text_capacity)
            .and_then(|bytes| {
                self.events
                    .capacity()
                    .checked_add(event_capacity)
                    .and_then(|slots| slots.checked_mul(size_of::<EngineEvent>()))
                    .and_then(|events| bytes.checked_add(events))
            });
        let peak = buffers
            .and_then(|bytes| bytes.checked_add(self.event_heap))
            .and_then(|bytes| bytes.checked_add(self.session_id.capacity()))
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .and_then(|bytes| {
                plan.bytes()
                    .checked_mul(2)
                    .and_then(|event| bytes.checked_add(event))
            });
        if peak.is_none_or(|bytes| bytes > self.limit) {
            return Err(limit_error());
        }
        let event_heap = plan.bytes().saturating_sub(size_of::<EngineEvent>());
        self.text.reserve_exact(text_capacity - self.text.len());
        self.events
            .reserve_exact(event_capacity - self.events.len());
        let event = plan.prepare().into_inner();
        let (text, newline) = text_fragment(&event);
        self.text.push_str(text);
        if newline {
            self.text.push('\n');
        }
        if let EngineEvent::TurnFinished { status, usage, .. } = &event {
            self.status = Some(status.clone());
            self.usage = usage.clone();
        }
        self.event_heap += event_heap;
        self.events.push(event);
        Ok(())
    }
}
fn growth(capacity: usize, needed: usize) -> usize {
    if needed > capacity {
        capacity.saturating_mul(2).max(needed).max(4)
    } else {
        capacity
    }
}
fn text_fragment(event: &EngineEvent) -> (&str, bool) {
    match event {
        EngineEvent::TextDelta { text, .. } => (text, false),
        EngineEvent::CommandFinished { message, .. } => (message, true),
        _ => ("", false),
    }
}
fn limit_error() -> miette::Report {
    miette!(
        "JSON output exceeds its 64 MiB allocation or 16384-event limit; use --output-format stream-json for complete streaming output"
    )
}

#[cfg(test)]
mod tests;
