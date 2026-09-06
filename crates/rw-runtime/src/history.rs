//! Read-only session replay, search, and export surfaces.

use std::{
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{EngineEvent, TranscriptFormat};
use rw_providers::FixtureRedactor;
use rw_store::session::{EventEnvelope, SessionIndex, SessionSummary};
use rw_types::json_encoding::JsonWriter;
use serde_json::Value;

mod export;
mod output;
mod redaction;

pub const MAX_HISTORY_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_HISTORY_EVENTS: usize = 250_000;
const MAX_RENDERED_BYTES: usize = 96 * 1024 * 1024;

/// Result of a complete, offline authoritative-journal integrity scan.
#[derive(Debug, serde::Serialize)]
pub struct SessionVerification {
    pub session_id: String,
    pub events: u64,
    pub bytes: u64,
}

/// Verifies every journal segment and every durable event identity.
///
/// # Errors
/// Rejects active sessions, unsafe descriptors, any historical corruption,
/// unknown event payloads, and mismatched session or sequence identities.
pub fn verify_session(storage_root: &Path, session: &str) -> Result<SessionVerification> {
    let view = rw_store::session::journal::JournalReadView::open_existing(storage_root, session)
        .map_err(|error| miette!("session journal could not open for verification: {error}"))?
        .ok_or_else(|| miette!("session journal does not exist"))?;
    let mut cursor = None;
    loop {
        let page = view
            .page::<EngineEvent>(cursor, rw_store::session::SessionEventPageLimits::default())
            .map_err(|error| miette!("session journal integrity verification failed: {error}"))?;
        for envelope in &page.events {
            let meta = envelope
                .event
                .meta()
                .ok_or_else(|| miette!("transient event in session journal"))?;
            if meta.protocol_version != rw_core::PROTOCOL_VERSION
                || meta.session_id.0 != session
                || meta.sequence_id != envelope.sequence
            {
                return Err(miette!(
                    "session journal event identity does not match its envelope"
                ));
            }
        }
        cursor = page.next_cursor;
        if !page.has_more {
            break;
        }
    }
    Ok(SessionVerification {
        session_id: session.to_owned(),
        events: view.prefix_identity().next_sequence,
        bytes: view.total_bytes(),
    })
}

/// Loads one bounded, identity-validated durable session history.
///
/// # Errors
/// Returns an error when storage cannot be read or an event identity is invalid.
pub fn load_events(storage_root: &Path, session: &str) -> Result<Vec<EventEnvelope<EngineEvent>>> {
    load_events_with_size(storage_root, session, MAX_HISTORY_BYTES).map(|(events, _)| events)
}

/// Loads bounded durable events and reports the charged storage bytes.
///
/// # Errors
/// Returns an error when storage cannot be read or an event identity is invalid.
pub fn load_events_with_size(
    storage_root: &Path,
    session: &str,
    max_bytes: u64,
) -> Result<(Vec<EventEnvelope<EngineEvent>>, u64)> {
    let view = rw_store::session::journal::JournalReadView::open_existing(storage_root, session)
        .map_err(|error| miette!("session history could not be read: {error}"))?
        .ok_or_else(|| miette!("session journal does not exist"))?;
    load_events_from_view(&view, session, max_bytes)
}

pub(crate) fn load_events_from_view(
    view: &rw_store::session::journal::JournalReadView,
    session: &str,
    max_bytes: u64,
) -> Result<(Vec<EventEnvelope<EngineEvent>>, u64)> {
    let events = view
        .collect_bounded::<EngineEvent>(max_bytes.min(MAX_HISTORY_BYTES), MAX_HISTORY_EVENTS)
        .map_err(|error| miette!("session history could not be read: {error}"))?;
    let bytes = view.total_bytes();
    for envelope in &events {
        let meta = envelope
            .event
            .meta()
            .ok_or_else(|| miette!("session history contains a non-durable event"))?;
        if meta.protocol_version != rw_core::PROTOCOL_VERSION
            || meta.session_id.0 != session
            || meta.sequence_id != envelope.sequence
        {
            return Err(miette!(
                "session history event identity does not match its durable envelope"
            ));
        }
    }
    Ok((events, bytes))
}

/// Emits exactly the persisted provider-neutral event payloads consumed by clients.
///
/// # Errors
/// Returns an error when an event cannot be serialized or the render cap is exceeded.
pub fn replay_jsonl(events: &[EventEnvelope<EngineEvent>]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    write_replay_jsonl(
        events,
        &mut JsonWriter::buffer(&mut output, MAX_RENDERED_BYTES, 4096).into_diagnostic()?,
    )?;
    Ok(output)
}

fn write_replay_jsonl(
    events: &[EventEnvelope<EngineEvent>],
    output: &mut JsonWriter<'_>,
) -> Result<()> {
    for envelope in events {
        let result = output.serialize(&envelope.event);
        if output.exceeded() {
            return Err(miette!("rendered session history exceeds its output limit"));
        }
        result.into_diagnostic()?;
        output
            .write_all(b"\n")
            .map_err(|_| miette!("rendered session history exceeds its output limit"))?;
    }
    Ok(())
}

/// Renders a redacted transcript in the selected stable export format.
///
/// # Errors
/// Returns an error when serialization, rendering, or size validation fails.
pub fn export_transcript(
    session: &str,
    events: &[EventEnvelope<EngineEvent>],
    format: TranscriptFormat,
    redactor: &FixtureRedactor,
) -> Result<Vec<u8>> {
    export::render(session, events, format, redactor, MAX_RENDERED_BYTES)
}

/// Writes an export beside an already-existing directory entry without following
/// a destination symlink. Forced replacement is limited to regular, single-link files.
///
/// # Errors
/// Returns an error when the destination is unsafe or the durable write fails.
pub fn write_transcript_export(
    storage_root: &Path,
    output: &Path,
    bytes: &[u8],
    force: bool,
) -> Result<PathBuf> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).into_diagnostic()?;
    let filename = output
        .file_name()
        .ok_or_else(|| miette!("export output must name a file"))?;
    if let Ok(canonical_storage) = fs::canonicalize(storage_root)
        && parent.starts_with(canonical_storage)
    {
        return Err(miette!("export output cannot modify Rottweiler storage"));
    }

    #[cfg(unix)]
    write_transcript_export_unix(&parent, filename, bytes, force, || Ok(()))?;

    #[cfg(not(unix))]
    write_transcript_export_portable(storage_root, &parent, filename, bytes, force)?;

    Ok(parent.join(filename))
}

#[cfg(unix)]
fn write_transcript_export_unix(
    parent: &Path,
    filename: &std::ffi::OsStr,
    bytes: &[u8],
    force: bool,
    before_commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};

    let expected = fs::metadata(parent).into_diagnostic()?;
    let directory = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let opened = rustix::fs::fstat(&directory)
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
    {
        use std::os::unix::fs::MetadataExt as _;
        if Some(expected.dev()) != crate::rustix_device_id(opened.st_dev)
            || expected.ino() != opened.st_ino
        {
            return Err(miette!(
                "export output directory changed while it was opened"
            ));
        }
    }
    match rustix::fs::statat(&directory, filename, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            if !force {
                return Err(miette!(
                    "export output already exists; pass --force to replace it"
                ));
            }
            if !FileType::from_raw_mode(stat.st_mode).is_file() {
                return Err(miette!("export output is not a regular file"));
            }
            if stat.st_nlink != 1 {
                return Err(miette!("export output has multiple hard links"));
            }
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(std::io::Error::from(error)).into_diagnostic(),
    }
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).into_diagnostic()?;
    let temporary = format!(
        ".rottweiler-export-{}-{}",
        std::process::id(),
        u64::from_ne_bytes(random)
    );
    let descriptor = rustix::fs::openat(
        &directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)
    .into_diagnostic()?;
    let result = (|| -> Result<()> {
        let mut file = fs::File::from(descriptor);
        file.write_all(bytes).into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
        before_commit()?;
        if force {
            rustix::fs::renameat(&directory, temporary.as_str(), &directory, filename)
        } else {
            rustix::fs::renameat_with(
                &directory,
                temporary.as_str(),
                &directory,
                filename,
                RenameFlags::NOREPLACE,
            )
        }
        .map_err(std::io::Error::from)
        .into_diagnostic()?;
        rustix::fs::fsync(&directory)
            .map_err(std::io::Error::from)
            .into_diagnostic()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&directory, temporary.as_str(), AtFlags::empty());
    }
    result
}

#[cfg(not(unix))]
fn write_transcript_export_portable(
    storage_root: &Path,
    parent: &Path,
    filename: &std::ffi::OsStr,
    bytes: &[u8],
    force: bool,
) -> Result<()> {
    let destination = parent.join(filename);
    if destination.exists() {
        let message = if force {
            "safe --force replacement is unavailable on this platform"
        } else {
            "export output already exists; pass --force to replace it"
        };
        return Err(miette!(message));
    }
    let parent = fs::canonicalize(parent).into_diagnostic()?;
    if let Ok(canonical_storage) = fs::canonicalize(storage_root)
        && parent.starts_with(canonical_storage)
    {
        return Err(miette!("export output cannot modify Rottweiler storage"));
    }
    let destination = parent.join(filename);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()
}

/// Searches the durable session index with a bounded result count.
///
/// # Errors
/// Returns an error when the session index cannot be opened or queried.
pub fn search_sessions(
    storage_root: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<SessionSummary>> {
    if query.trim().is_empty() {
        return Err(miette!("session search query cannot be empty"));
    }
    if !(1..=1_000).contains(&limit) {
        return Err(miette!("session search limit must be between 1 and 1000"));
    }
    SessionIndex::search_read_only(storage_root, query, limit)
        .map_err(|error| miette!("session search failed: {error}"))
}

/// Lists recent durable sessions with a bounded result count.
///
/// # Errors
/// Returns an error when the session index cannot be opened or queried.
pub fn list_sessions(storage_root: &Path, limit: usize) -> Result<Vec<SessionSummary>> {
    if !(1..=1_000).contains(&limit) {
        return Err(miette!("session list limit must be between 1 and 1000"));
    }
    SessionIndex::list_read_only(storage_root, limit)
        .map_err(|error| miette!("session listing failed: {error}"))
}

#[derive(Debug)]
struct TranscriptSection {
    title: String,
    metadata: Vec<(String, String)>,
    body: String,
    merge_key: Option<String>,
}

fn transcript_section(envelope: &Value) -> Result<TranscriptSection> {
    let event = envelope
        .get("event")
        .and_then(Value::as_object)
        .ok_or_else(|| miette!("persisted session event is not an object"))?;
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| miette!("persisted session event has no type"))?;
    let mut metadata = event_metadata(envelope, event);
    let (title, body, merge_key) = match event_type {
        "user_message_accepted" => ("User".to_owned(), string_field(event, "content"), None),
        "message_queued" => (
            "Queued user message".to_owned(),
            string_field(event, "content"),
            None,
        ),
        "text_delta" => {
            let turn = string_field(event, "turn_id");
            (
                "Assistant".to_owned(),
                string_field(event, "text"),
                Some(format!("assistant:{turn}")),
            )
        }
        "thinking_delta" => {
            let turn = string_field(event, "turn_id");
            (
                "Assistant reasoning".to_owned(),
                string_field(event, "text"),
                Some(format!("reasoning:{turn}")),
            )
        }
        "tool_call_started" | "tool_approval_needed" => tool_started_section(event_type, event)?,
        "tool_output_delta" => {
            let call = string_field(event, "tool_call_id");
            (
                "Tool output".to_owned(),
                string_field(event, "chunk"),
                Some(format!("tool-output:{call}")),
            )
        }
        "tool_call_finished" => tool_finished_section(event, &mut metadata)?,
        "plan_submitted" | "plan_reviewed" => plan_section(event, &mut metadata)?,
        "turn_started" => ("Turn started".to_owned(), String::new(), None),
        "turn_finished" => {
            if let Some(status) = event.get("status").and_then(Value::as_str) {
                metadata.push(("Status".to_owned(), status.to_owned()));
            }
            (
                "Turn finished".to_owned(),
                render_turn_accounting(event)?,
                None,
            )
        }
        "ui_notification" => (
            event.get("title").and_then(Value::as_str).map_or_else(
                || "Notification".to_owned(),
                |title| format!("Notification: {title}"),
            ),
            string_field(event, "message"),
            None,
        ),
        "error" => (
            "Error".to_owned(),
            event
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            None,
        ),
        _ => (humanize_event_type(event_type), event_summary(event), None),
    };
    Ok(TranscriptSection {
        title,
        metadata,
        body,
        merge_key,
    })
}

fn tool_started_section(
    event_type: &str,
    event: &serde_json::Map<String, Value>,
) -> Result<(String, String, Option<String>)> {
    let name = string_field(event, "name");
    let title = if event_type == "tool_approval_needed" {
        format!("Tool approval: {name}")
    } else {
        format!("Tool call: {name}")
    };
    let mut body = output::Output::new(MAX_RENDERED_BYTES);
    body.push(&pretty_field(event, "args")?).into_diagnostic()?;
    if let Some(rationale) = event.get("rationale").and_then(Value::as_str) {
        if !body.is_empty() {
            body.push("\n\n").into_diagnostic()?;
        }
        body.push("Rationale: ").into_diagnostic()?;
        body.push(rationale).into_diagnostic()?;
    }
    Ok((title, body.text().into_diagnostic()?, None))
}

fn tool_finished_section(
    event: &serde_json::Map<String, Value>,
    metadata: &mut Vec<(String, String)>,
) -> Result<(String, String, Option<String>)> {
    metadata.push((
        "Status".to_owned(),
        if event.get("is_error").and_then(Value::as_bool) == Some(true) {
            "error"
        } else {
            "completed"
        }
        .to_owned(),
    ));
    Ok((
        "Tool result".to_owned(),
        render_tool_output(event.get("output"))?,
        None,
    ))
}

fn plan_section(
    event: &serde_json::Map<String, Value>,
    metadata: &mut Vec<(String, String)>,
) -> Result<(String, String, Option<String>)> {
    let artifact = event.get("artifact").unwrap_or(&Value::Null);
    if let Some(decision) = event.get("decision").and_then(Value::as_str) {
        metadata.push(("Decision".to_owned(), decision.to_owned()));
    }
    Ok((
        artifact
            .get("title")
            .and_then(Value::as_str)
            .map_or_else(|| "Plan".to_owned(), |title| format!("Plan: {title}")),
        render_plan(artifact)?,
        None,
    ))
}

fn event_metadata(
    envelope: &Value,
    event: &serde_json::Map<String, Value>,
) -> Vec<(String, String)> {
    let mut metadata = Vec::new();
    if let Some(sequence) = envelope.get("sequence").and_then(value_as_display) {
        metadata.push(("Sequence".to_owned(), sequence));
    }
    if let Some(meta) = event.get("meta")
        && let Some(time) = meta.get("emitted_at").and_then(Value::as_str)
    {
        metadata.push(("Time".to_owned(), time.to_owned()));
    }
    if let Some(agent_turn) = event.get("agent_turn").and_then(value_as_display) {
        metadata.push(("Agent turn".to_owned(), agent_turn));
    }
    if let Some(turn_id) = event.get("turn_id").and_then(value_as_display) {
        metadata.push(("Turn".to_owned(), turn_id));
    }
    if let Some(tool_call_id) = event.get("tool_call_id").and_then(value_as_display) {
        metadata.push(("Tool call".to_owned(), tool_call_id));
    }
    metadata
}

fn value_as_display(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn string_field(event: &serde_json::Map<String, Value>, field: &str) -> String {
    event
        .get(field)
        .and_then(value_as_display)
        .unwrap_or_default()
}

fn pretty_field(event: &serde_json::Map<String, Value>, field: &str) -> Result<String> {
    match event.get(field) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(value) => export::pretty(value),
    }
}

fn render_tool_output(output: Option<&Value>) -> Result<String> {
    let Some(output) = output else {
        return Ok(String::new());
    };
    match output.get("type").and_then(Value::as_str) {
        Some("text") => Ok(output
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()),
        Some("structured") => match output.get("value") {
            Some(value) => export::pretty(value),
            None => Ok(String::new()),
        },
        Some("mixed") => {
            let mut rendered = output::Output::new(MAX_RENDERED_BYTES);
            for (index, part) in output
                .get("parts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                if index != 0 {
                    rendered.push("\n").into_diagnostic()?;
                }
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => rendered
                        .push(part.get("text").and_then(Value::as_str).unwrap_or_default())
                        .into_diagnostic()?,
                    Some("structured") => rendered
                        .push(&export::pretty(part.get("value").unwrap_or(&Value::Null))?)
                        .into_diagnostic()?,
                    Some("image") => rendered.push("[image]").into_diagnostic()?,
                    _ => {}
                }
            }
            rendered.text().into_diagnostic()
        }
        _ => export::pretty(output),
    }
}

fn render_plan(artifact: &Value) -> Result<String> {
    let mut output = output::Output::new(MAX_RENDERED_BYTES);
    output
        .push(
            artifact
                .get("summary_md")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .into_diagnostic()?;
    for (index, step) in artifact
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if !output.is_empty() {
            output.push("\n\n").into_diagnostic()?;
        }
        write!(
            &mut output,
            "{}. {}",
            index + 1,
            step.get("description")
                .and_then(Value::as_str)
                .unwrap_or("Unnamed step")
        )
        .into_diagnostic()?;
        if let Some(files) = step.get("files_touched").and_then(Value::as_array) {
            for (index, file) in files.iter().filter_map(Value::as_str).enumerate() {
                output
                    .push(if index == 0 { "\n   Files: " } else { ", " })
                    .into_diagnostic()?;
                output.push(file).into_diagnostic()?;
            }
        }
        if let Some(verification) = step.get("verification").and_then(Value::as_str) {
            write!(&mut output, "\n   Verify: {verification}").into_diagnostic()?;
        }
    }
    let mut has_questions = false;
    for question in artifact
        .get("open_questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !has_questions {
            output.push("\n\nOpen questions:\n").into_diagnostic()?;
        }
        has_questions = true;
        output.push("- ").into_diagnostic()?;
        output.push(question).into_diagnostic()?;
        output.push("\n").into_diagnostic()?;
    }
    if has_questions {
        output.pop_newline();
    }
    output.text().into_diagnostic()
}

fn render_turn_accounting(event: &serde_json::Map<String, Value>) -> Result<String> {
    let mut lines = Vec::new();
    if let Some(usage) = event.get("usage") {
        let fields = [
            ("input", "input_tokens"),
            ("output", "output_tokens"),
            ("cache read", "cache_read_tokens"),
            ("cache write", "cache_write_tokens"),
            ("reasoning", "reasoning_tokens"),
        ]
        .into_iter()
        .filter_map(|(label, field)| {
            usage
                .get(field)
                .and_then(value_as_display)
                .map(|value| format!("{label}: {value}"))
        })
        .collect::<Vec<_>>();
        if !fields.is_empty() {
            lines.push(format!("Tokens — {}", fields.join(", ")));
        }
    }
    if let Some(cost) = event.get("cost") {
        let rendered = match cost.get("kind").and_then(Value::as_str) {
            Some("monetary") => format!(
                "{} micros {}",
                cost.get("amount_micros")
                    .and_then(value_as_display)
                    .unwrap_or_else(|| "0".to_owned()),
                cost.get("currency")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ),
            Some("ai_credits") => format!(
                "{} AI credit micros",
                cost.get("credits_micros")
                    .and_then(value_as_display)
                    .unwrap_or_else(|| "0".to_owned())
            ),
            Some("subscription_quota") => format!(
                "subscription quota: {} {}",
                cost.get("used")
                    .and_then(Value::as_str)
                    .unwrap_or("unreported"),
                cost.get("unit").and_then(Value::as_str).unwrap_or("")
            )
            .trim_end()
            .to_owned(),
            Some("unavailable") => format!(
                "unavailable: {}",
                cost.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified")
            ),
            _ => serde_json::to_string(cost).into_diagnostic()?,
        };
        lines.push(format!("Cost — {rendered}"));
    }
    Ok(lines.join("\n"))
}

fn event_summary(event: &serde_json::Map<String, Value>) -> String {
    for field in ["message", "status", "content", "task"] {
        if let Some(value) = event.get(field).and_then(Value::as_str) {
            return value.to_owned();
        }
    }
    String::new()
}

fn humanize_event_type(event_type: &str) -> String {
    let mut output = String::with_capacity(event_type.len());
    for (index, word) in event_type.split('_').enumerate() {
        if index > 0 {
            output.push(' ');
        }
        if index == 0 {
            let mut characters = word.chars();
            if let Some(first) = characters.next() {
                output.extend(first.to_uppercase());
                output.extend(characters);
            }
        } else {
            output.push_str(word);
        }
    }
    output
}

#[cfg(test)]
mod tests;
