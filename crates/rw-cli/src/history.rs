//! Read-only session replay, search, and export surfaces.

use std::{
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{EngineEvent, TranscriptFormat, runtime_support::FixtureRedactor};
use rw_store::session::{EventEnvelope, SessionEventLog, SessionIndex, SessionSummary};
use serde_json::Value;

pub(crate) const MAX_HISTORY_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_HISTORY_EVENTS: usize = 250_000;
const MAX_RENDERED_BYTES: usize = 96 * 1024 * 1024;

pub(crate) fn load_events(
    storage_root: &Path,
    session: &str,
) -> Result<Vec<EventEnvelope<EngineEvent>>> {
    load_events_with_size(storage_root, session, MAX_HISTORY_BYTES).map(|(events, _)| events)
}

pub(crate) fn load_events_with_size(
    storage_root: &Path,
    session: &str,
    max_bytes: u64,
) -> Result<(Vec<EventEnvelope<EngineEvent>>, u64)> {
    let (events, bytes) = SessionEventLog::load_existing_bounded_with_size::<EngineEvent>(
        storage_root,
        session,
        max_bytes.min(MAX_HISTORY_BYTES),
        MAX_HISTORY_EVENTS,
    )
    .map_err(|error| miette!("session history could not be read: {error}"))?;
    for envelope in &events {
        let meta = envelope
            .event
            .meta()
            .ok_or_else(|| miette!("session history contains a non-durable event"))?;
        if meta.session_id.0 != session || meta.sequence_id != envelope.sequence {
            return Err(miette!(
                "session history event identity does not match its durable envelope"
            ));
        }
    }
    Ok((events, bytes))
}

/// Emits exactly the persisted provider-neutral event payloads consumed by clients.
pub(crate) fn replay_jsonl(events: &[EventEnvelope<EngineEvent>]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for envelope in events {
        serde_json::to_writer(&mut output, &envelope.event).into_diagnostic()?;
        output.push(b'\n');
        enforce_render_limit(&output)?;
    }
    Ok(output)
}

pub(crate) fn export_transcript(
    session: &str,
    events: &[EventEnvelope<EngineEvent>],
    format: TranscriptFormat,
    redactor: &FixtureRedactor,
) -> Result<Vec<u8>> {
    let sanitized = events
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<Vec<_>, _>>()
        .into_diagnostic()?
        .into_iter()
        .map(|value| redact_export_value(value, redactor))
        .collect::<Vec<_>>();
    let mut output = match format {
        TranscriptFormat::Json => serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "session_id": session,
            "events": sanitized,
        }))
        .into_diagnostic()?,
        TranscriptFormat::Markdown => markdown_export(session, &sanitized)?,
        TranscriptFormat::Html => html_export(session, &sanitized)?,
    };
    if output.last() != Some(&b'\n') {
        output.push(b'\n');
    }
    enforce_render_limit(&output)?;
    Ok(output)
}

/// Writes an export beside an already-existing directory entry without following
/// a destination symlink. Forced replacement is limited to regular, single-link files.
pub(crate) fn write_transcript_export(
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

pub(crate) fn search_sessions(
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

pub(crate) fn list_sessions(storage_root: &Path, limit: usize) -> Result<Vec<SessionSummary>> {
    if !(1..=1_000).contains(&limit) {
        return Err(miette!("session list limit must be between 1 and 1000"));
    }
    SessionIndex::list_read_only(storage_root, limit)
        .map_err(|error| miette!("session listing failed: {error}"))
}

fn markdown_export(session: &str, events: &[Value]) -> Result<Vec<u8>> {
    let mut output =
        format!("# Rottweiler transcript: {}\n\n", escape_markdown(session)).into_bytes();
    for section in transcript_sections(events)? {
        output.extend_from_slice(format!("## {}\n\n", escape_markdown(&section.title)).as_bytes());
        if !section.metadata.is_empty() {
            let metadata = section
                .metadata
                .iter()
                .map(|(label, value)| {
                    format!("{}: {}", escape_markdown(label), escape_markdown(value))
                })
                .collect::<Vec<_>>()
                .join(" · ");
            output.extend_from_slice(format!("*{metadata}*\n\n").as_bytes());
        }
        append_markdown_quote(&mut output, &section.body);
        enforce_render_limit(&output)?;
    }
    Ok(output)
}

fn html_export(session: &str, events: &[Value]) -> Result<Vec<u8>> {
    let mut output = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Rottweiler transcript {}</title><style>body{{font:16px/1.5 system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem;color:#202124}}section{{border-top:1px solid #ddd;padding:1rem 0}}h1,h2{{line-height:1.2}}dl{{display:flex;gap:1rem;flex-wrap:wrap;color:#5f6368;font-size:.875rem}}dt{{font-weight:600}}dd{{margin:0}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;background:#f6f8fa;padding:1rem;border-radius:.5rem}}</style></head><body><main><h1>Rottweiler transcript {}</h1>\n",
        escape_html(session),
        escape_html(session)
    );
    for section in transcript_sections(events)? {
        output.push_str("<section><h2>");
        output.push_str(&escape_html(&section.title));
        output.push_str("</h2>");
        if !section.metadata.is_empty() {
            output.push_str("<dl>");
            for (label, value) in section.metadata {
                output.push_str("<div><dt>");
                output.push_str(&escape_html(&label));
                output.push_str("</dt><dd>");
                output.push_str(&escape_html(&value));
                output.push_str("</dd></div>");
            }
            output.push_str("</dl>");
        }
        if !section.body.is_empty() {
            output.push_str("<pre>");
            output.push_str(&escape_html(&section.body));
            output.push_str("</pre>");
        }
        output.push_str("</section>\n");
        enforce_render_limit(output.as_bytes())?;
    }
    output.push_str("</main></body></html>\n");
    Ok(output.into_bytes())
}

#[derive(Debug)]
struct TranscriptSection {
    title: String,
    metadata: Vec<(String, String)>,
    body: String,
    merge_key: Option<String>,
}

fn transcript_sections(events: &[Value]) -> Result<Vec<TranscriptSection>> {
    let mut sections: Vec<TranscriptSection> = Vec::new();
    for envelope in events {
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
            "tool_call_started" | "tool_approval_needed" => {
                tool_started_section(event_type, event)?
            }
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
        if let Some(key) = merge_key.as_ref()
            && let Some(previous) = sections.last_mut()
            && previous.merge_key.as_ref() == Some(key)
        {
            previous.body.push_str(&body);
            continue;
        }
        sections.push(TranscriptSection {
            title,
            metadata,
            body,
            merge_key,
        });
    }
    Ok(sections)
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
    let mut body = pretty_field(event, "args")?;
    if let Some(rationale) = event.get("rationale").and_then(Value::as_str) {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str("Rationale: ");
        body.push_str(rationale);
    }
    Ok((title, body, None))
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
        Some(value) => serde_json::to_string_pretty(value).into_diagnostic(),
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
            Some(value) => serde_json::to_string_pretty(value).into_diagnostic(),
            None => Ok(String::new()),
        },
        Some("mixed") => {
            let mut rendered = Vec::new();
            for part in output
                .get("parts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                rendered.push(match part.get("type").and_then(Value::as_str) {
                    Some("text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    Some("structured") => {
                        serde_json::to_string_pretty(part.get("value").unwrap_or(&Value::Null))
                            .into_diagnostic()?
                    }
                    Some("image") => "[image]".to_owned(),
                    _ => String::new(),
                });
            }
            Ok(rendered.join("\n"))
        }
        _ => serde_json::to_string_pretty(output).into_diagnostic(),
    }
}

fn render_plan(artifact: &Value) -> Result<String> {
    let mut output = artifact
        .get("summary_md")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    for (index, step) in artifact
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if !output.is_empty() {
            output.push_str("\n\n");
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
            let files = files.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            if !files.is_empty() {
                write!(&mut output, "\n   Files: {}", files.join(", ")).into_diagnostic()?;
            }
        }
        if let Some(verification) = step.get("verification").and_then(Value::as_str) {
            write!(&mut output, "\n   Verify: {verification}").into_diagnostic()?;
        }
    }
    let questions = artifact
        .get("open_questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    if !questions.is_empty() {
        output.push_str("\n\nOpen questions:\n");
        for question in questions {
            output.push_str("- ");
            output.push_str(question);
            output.push('\n');
        }
        output.pop();
    }
    Ok(output)
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

fn append_markdown_quote(output: &mut Vec<u8>, body: &str) {
    if body.is_empty() {
        return;
    }
    for line in body.lines() {
        output.extend_from_slice(b"> ");
        output.extend_from_slice(line.as_bytes());
        output.push(b'\n');
    }
    output.push(b'\n');
}

fn escape_markdown(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\r', '\n'], " ")
}

fn redact_export_value(value: Value, redactor: &FixtureRedactor) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| {
                    let lowered = key.to_ascii_lowercase();
                    let sensitive = [
                        "token",
                        "password",
                        "secret",
                        "api_key",
                        "authorization",
                        "credential",
                        "signature",
                    ]
                    .iter()
                    .any(|marker| lowered.contains(marker));
                    if sensitive {
                        (key, Value::String("[REDACTED]".to_owned()))
                    } else {
                        (key, redact_export_value(value, redactor))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_export_value(value, redactor))
                .collect(),
        ),
        Value::String(value) => Value::String(redact_export_string(&value, redactor)),
        other => other,
    }
}

fn redact_export_string(value: &str, redactor: &FixtureRedactor) -> String {
    let value = redactor.redact_text(value);
    let value = redact_embedded_paths(&value);
    let value = redact_embedded_known_secrets(&value);
    value
        .split_inclusive(char::is_whitespace)
        .map(|part| {
            let token = part.trim_end_matches(char::is_whitespace);
            let suffix = &part[token.len()..];
            if looks_like_secret(token) {
                format!("[REDACTED]{suffix}")
            } else {
                part.to_owned()
            }
        })
        .collect()
}

fn redact_embedded_paths(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let Some(start) = next_absolute_path_start(value, cursor) else {
            output.push_str(&value[cursor..]);
            break;
        };
        output.push_str(&value[cursor..start]);
        output.push_str("[REDACTED_PATH]");
        cursor = absolute_path_end(value, start);
    }
    output
}

fn next_absolute_path_start(value: &str, cursor: usize) -> Option<usize> {
    value[cursor..]
        .char_indices()
        .find_map(|(offset, character)| {
            let index = cursor + offset;
            if !path_has_left_boundary(value, index) {
                return None;
            }
            let tail = &value[index..];
            if character == '/' && is_known_slash_command(tail) {
                return None;
            }
            let file_uri = tail.starts_with("file:///") || tail.starts_with("file://localhost/");
            let html_closing_tag = character == '/'
                && value[..index].ends_with('<')
                && tail
                    .strip_prefix('/')
                    .and_then(|tag| tag.strip_suffix('>'))
                    .is_some_and(|tag| {
                        !tag.is_empty()
                            && tag.chars().all(|character| {
                                character.is_ascii_alphanumeric() || character == '-'
                            })
                    });
            let unix = character == '/'
                && !html_closing_tag
                && tail.as_bytes().get(1).is_some_and(|next| *next != b'/');
            let windows_drive = character.is_ascii_alphabetic()
                && tail.as_bytes().get(1) == Some(&b':')
                && tail
                    .as_bytes()
                    .get(2)
                    .is_some_and(|separator| matches!(separator, b'/' | b'\\'));
            let windows_unc = tail.starts_with("\\\\")
                && tail.as_bytes().get(2).is_some_and(|next| *next != b'\\');
            (file_uri || unix || windows_drive || windows_unc).then_some(index)
        })
}

fn is_known_slash_command(value: &str) -> bool {
    let token = value
        .strip_prefix('/')
        .and_then(|value| {
            let end = value
                .find(|character: char| {
                    character.is_whitespace()
                        || matches!(character, '"' | '\'' | '<' | '>' | ')' | ']' | '}' | ',')
                })
                .unwrap_or(value.len());
            value.get(..end)
        })
        .unwrap_or_default();
    matches!(
        token,
        "help"
            | "status"
            | "mode"
            | "permissions"
            | "plan"
            | "rewind"
            | "fork"
            | "review"
            | "interrupt"
            | "context"
            | "cost"
            | "compact"
            | "trust"
            | "add-dir"
            | "init"
            | "deep-init"
            | "memory"
            | "models"
            | "providers"
            | "mcp"
            | "mcp.prompt"
            | "project"
            | "workspace"
            | "session"
            | "plugin"
    )
}

fn path_has_left_boundary(value: &str, index: usize) -> bool {
    index == 0
        || value[..index].chars().next_back().is_some_and(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '=' | '"' | '\'' | '<' | '>' | '(' | '[' | '{' | ',' | ';'
                )
        })
}

fn absolute_path_end(value: &str, start: usize) -> usize {
    value[start..]
        .char_indices()
        .skip(1)
        .find(|(_, character)| {
            character.is_whitespace()
                || matches!(
                    character,
                    '"' | '\'' | '<' | '>' | ')' | ']' | '}' | ',' | ';'
                )
        })
        .map_or(value.len(), |(offset, _)| start + offset)
}

fn redact_embedded_known_secrets(value: &str) -> String {
    redact_matching_spans(
        value,
        &["github_pat_", "ghp_", "sk-", "AKIA"],
        "[REDACTED]",
        looks_like_secret,
    )
}

fn redact_matching_spans(
    value: &str,
    markers: &[&str],
    replacement: &str,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let next = markers
            .iter()
            .filter_map(|marker| {
                value[cursor..]
                    .find(marker)
                    .map(|offset| (cursor + offset, marker))
            })
            .min_by_key(|(offset, _)| *offset);
        let Some((start, marker)) = next else {
            output.push_str(&value[cursor..]);
            break;
        };
        output.push_str(&value[cursor..start]);
        let end = span_end(value, start + marker.len());
        let candidate = &value[start..end];
        if predicate(candidate) {
            output.push_str(replacement);
        } else {
            output.push_str(candidate);
        }
        cursor = end;
    }
    output
}

fn span_end(value: &str, after_marker: usize) -> usize {
    value[after_marker..]
        .char_indices()
        .find(|(_, character)| {
            character.is_whitespace()
                || matches!(
                    character,
                    '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                )
        })
        .map_or(value.len(), |(offset, _)| after_marker + offset)
}

fn looks_like_secret(value: &str) -> bool {
    let trimmed = value.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.')
    });
    if trimmed.starts_with("sk-")
        || trimmed.starts_with("ghp_")
        || trimmed.starts_with("github_pat_")
        || (trimmed.starts_with("AKIA") && trimmed.len() == 20)
    {
        return true;
    }
    if looks_like_utc_timestamp(trimmed) {
        return false;
    }
    if !(24..=4_096).contains(&trimmed.len())
        || trimmed.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return false;
    }
    let classes = [
        trimmed.bytes().any(|byte| byte.is_ascii_lowercase()),
        trimmed.bytes().any(|byte| byte.is_ascii_uppercase()),
        trimmed.bytes().any(|byte| byte.is_ascii_digit()),
        trimmed
            .bytes()
            .any(|byte| matches!(byte, b'-' | b'_' | b'+' | b'/' | b'=')),
    ];
    if classes.into_iter().filter(|present| *present).count() < 3 {
        return false;
    }
    shannon_entropy(trimmed.as_bytes()) >= 3.5
}

fn looks_like_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    (20..=35).contains(&bytes.len())
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
        && bytes.get(16) == Some(&b':')
        && bytes.last() == Some(&b'Z')
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16)
                || *byte == b'.'
                || byte.is_ascii_digit()
                || (index + 1 == bytes.len() && *byte == b'Z')
        })
}

fn shannon_entropy(bytes: &[u8]) -> f64 {
    let mut counts = [0_u32; 256];
    for byte in bytes {
        counts[usize::from(*byte)] += 1;
    }
    let length = u32::try_from(bytes.len()).map_or(f64::from(u32::MAX), f64::from);
    counts
        .into_iter()
        .filter(|count| *count != 0)
        .map(|count| {
            let probability = f64::from(count) / length;
            -probability * probability.log2()
        })
        .sum()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn enforce_render_limit(output: &[u8]) -> Result<()> {
    if output.len() > MAX_RENDERED_BYTES {
        return Err(miette!("rendered session history exceeds its output limit"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use rw_core::{EventMeta, PROTOCOL_VERSION, SequenceId, SessionId};
    use rw_store::session::{SessionProjection, SessionSummary};

    fn fixture() -> Vec<EventEnvelope<EngineEvent>> {
        vec![EventEnvelope {
            schema_version: 1,
            sequence: SequenceId(0),
            event: EngineEvent::UiNotification {
                meta: EventMeta {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: SessionId("golden".to_owned()),
                    sequence_id: SequenceId(0),
                    emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                    caused_by: None,
                },
                plugin_id: "fixture".to_owned(),
                title: "<script>alert(1)</script>".to_owned(),
                message: "key sk-AbCdEf0123456789GhIjKlMn at /Users/alice/private".to_owned(),
            },
        }]
    }

    #[test]
    fn export_formats_match_injection_safe_redacted_goldens() {
        let redactor = FixtureRedactor::default();
        for (format, expected) in [
            (
                TranscriptFormat::Markdown,
                include_bytes!("../tests/golden/history.md").as_slice(),
            ),
            (
                TranscriptFormat::Html,
                include_bytes!("../tests/golden/history.html").as_slice(),
            ),
            (
                TranscriptFormat::Json,
                include_bytes!("../tests/golden/history.json").as_slice(),
            ),
        ] {
            let actual =
                export_transcript("golden", &fixture(), format, &redactor).expect("export");
            assert_eq!(actual, expected);
            assert!(!String::from_utf8_lossy(&actual).contains("sk-AbCd"));
            assert!(!String::from_utf8_lossy(&actual).contains("/Users/alice"));
        }
    }

    #[test]
    fn replay_is_the_exact_engine_event_jsonl_seam() {
        let replay = replay_jsonl(&fixture()).expect("replay");
        let decoded: EngineEvent = serde_json::from_slice(&replay).expect("event JSON");
        assert_eq!(decoded, fixture()[0].event);
    }

    #[test]
    fn read_only_session_listing_is_newest_first_and_bounded() {
        let storage = tempfile::tempdir().expect("storage");
        let index = SessionIndex::open(storage.path()).expect("index");
        for (id, updated) in [("older", 1), ("newer-b", 2), ("newer-a", 2)] {
            index
                .upsert(&SessionProjection {
                    summary: SessionSummary {
                        id: id.to_owned(),
                        title: id.to_owned(),
                        updated_unix_ms: updated,
                        cost_micros: 0,
                    },
                    transcript: String::new(),
                    projected_through: None,
                })
                .expect("projection");
        }
        let listed = list_sessions(storage.path(), 2).expect("read-only list");
        assert_eq!(
            listed
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["newer-a", "newer-b"]
        );
        assert!(list_sessions(storage.path(), 0).is_err());
        assert!(list_sessions(storage.path(), 1_001).is_err());
    }

    #[test]
    fn replay_rejects_event_identity_outside_its_durable_envelope() {
        let storage = tempfile::tempdir().expect("storage");
        let mut log = SessionEventLog::open(storage.path(), "history").expect("event log");
        let mut event = fixture()[0].event.clone();
        event.meta_mut().expect("durable meta").session_id = SessionId("other".to_owned());
        log.append(event).expect("mismatched event fixture");
        drop(log);

        let error = load_events(storage.path(), "history").expect_err("identity must fail closed");
        assert!(error.to_string().contains("identity"));
    }

    #[test]
    fn export_redaction_handles_delimiter_attached_paths_and_secrets() {
        let input = "cwd=/Users/alice/repo (file:///home/bob/private) token=sk-AbCdEf0123456789GhIjKlMn <b>unsafe</b>";
        let redacted = redact_export_string(input, &FixtureRedactor::default());
        assert_eq!(
            redacted,
            "cwd=[REDACTED_PATH] ([REDACTED_PATH]) token=[REDACTED] <b>unsafe</b>"
        );
        let html = escape_html(&redacted);
        assert!(!html.contains("<b>"));
        assert!(html.contains("&lt;b&gt;"));
    }

    #[test]
    fn export_redaction_combines_known_environment_values_and_arbitrary_absolute_paths() {
        let redactor = FixtureRedactor::default();
        redactor.register_known_value("correct-horse-battery-staple");
        let input = concat!(
            "token=correct-horse-battery-staple ",
            "unix=/private/tmp/rottweiler/repo ",
            "windows=D:\\work\\private\\repo ",
            "unc=\\\\server\\share\\repo ",
            "url=https://example.invalid/public/path relative=src/main.rs"
        );
        let redacted = redact_export_string(input, &redactor);
        assert!(!redacted.contains("correct-horse"));
        assert!(!redacted.contains("/private/tmp"));
        assert!(!redacted.contains("D:\\work"));
        assert!(!redacted.contains("\\\\server"));
        assert!(redacted.contains("https://example.invalid/public/path"));
        assert!(redacted.contains("relative=src/main.rs"));
        assert_eq!(redacted.matches("[REDACTED_PATH]").count(), 3);
    }

    #[test]
    fn export_redaction_preserves_timestamps_and_slash_command_help() {
        let input = "at 2026-07-12T14:23:45.123Z use /add-dir <path> then /models";
        let redacted = redact_export_string(input, &FixtureRedactor::default());
        assert_eq!(redacted, input);
        assert_eq!(
            redact_export_string("read /Users/alice/private", &FixtureRedactor::default()),
            "read [REDACTED_PATH]"
        );
    }

    #[test]
    fn export_json_redacts_opaque_reasoning_signatures_by_field_name() {
        let redacted = redact_export_value(
            serde_json::json!({
                "type": "thinking_delta",
                "text": "summary",
                "signature": "provider-opaque-ciphertext",
            }),
            &FixtureRedactor::default(),
        );
        assert_eq!(redacted["text"], "summary");
        assert_eq!(redacted["signature"], "[REDACTED]");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn readable_transcript_groups_conversation_tools_plans_and_accounting() {
        let event = |sequence: u64, event: Value| {
            serde_json::json!({
                "schema_version": 1,
                "sequence": sequence.to_string(),
                "event": event,
            })
        };
        let events = vec![
            event(
                0,
                serde_json::json!({
                    "type": "user_message_accepted",
                    "meta": {"emitted_at": "2026-01-01T00:00:00Z"},
                    "agent_turn": "1",
                    "content": "Build the feature",
                }),
            ),
            event(
                1,
                serde_json::json!({
                    "type": "text_delta",
                    "meta": {"emitted_at": "2026-01-01T00:00:01Z"},
                    "turn_id": "turn-1",
                    "text": "I will ",
                }),
            ),
            event(
                2,
                serde_json::json!({
                    "type": "text_delta",
                    "meta": {"emitted_at": "2026-01-01T00:00:02Z"},
                    "turn_id": "turn-1",
                    "text": "do that.",
                }),
            ),
            event(
                3,
                serde_json::json!({
                    "type": "tool_call_started",
                    "meta": {"emitted_at": "2026-01-01T00:00:03Z"},
                    "turn_id": "turn-1",
                    "tool_call_id": "tool-1",
                    "name": "read",
                    "args": {"path": "README.md"},
                }),
            ),
            event(
                4,
                serde_json::json!({
                    "type": "tool_call_finished",
                    "meta": {"emitted_at": "2026-01-01T00:00:04Z"},
                    "turn_id": "turn-1",
                    "tool_call_id": "tool-1",
                    "output": {"type": "text", "text": "contents"},
                    "is_error": false,
                }),
            ),
            event(
                5,
                serde_json::json!({
                    "type": "plan_submitted",
                    "meta": {"emitted_at": "2026-01-01T00:00:05Z"},
                    "artifact": {
                        "title": "Implementation",
                        "summary_md": "Make the change safely.",
                        "steps": [{
                            "description": "Edit the code",
                            "files_touched": ["src/main.rs"],
                            "verification": "cargo test",
                        }],
                        "open_questions": [],
                    },
                }),
            ),
            event(
                6,
                serde_json::json!({
                    "type": "turn_finished",
                    "meta": {"emitted_at": "2026-01-01T00:00:06Z"},
                    "turn_id": "turn-1",
                    "status": "completed",
                    "usage": {
                        "input_tokens": "10",
                        "output_tokens": "5",
                        "cache_read_tokens": "2",
                        "cache_write_tokens": "0",
                        "reasoning_tokens": "1",
                    },
                    "cost": {"kind": "monetary", "amount_micros": "42", "currency": "USD"},
                }),
            ),
        ];

        let sections = transcript_sections(&events).expect("readable transcript");
        assert_eq!(
            sections
                .iter()
                .map(|section| section.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "User",
                "Assistant",
                "Tool call: read",
                "Tool result",
                "Plan: Implementation",
                "Turn finished",
            ]
        );
        assert_eq!(sections[1].body, "I will do that.");
        assert!(sections[4].body.contains("Verify: cargo test"));
        assert!(sections[5].body.contains("input: 10"));
        assert!(sections[5].body.contains("42 micros USD"));
    }

    #[cfg(unix)]
    #[test]
    fn export_refuses_symlink_or_storage_targets_without_mutating_events() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().expect("storage");
        let session = storage.path().join("sessions/history");
        fs::create_dir_all(&session).expect("session directory");
        let events = session.join("events.jsonl");
        fs::write(&events, b"canary").expect("events");
        let output = tempfile::tempdir().expect("output");
        let planted = output.path().join("transcript.md");
        symlink(&events, &planted).expect("planted symlink");
        assert!(write_transcript_export(storage.path(), &planted, b"replacement", true).is_err());
        assert_eq!(fs::read(&events).expect("events unchanged"), b"canary");
        assert!(
            write_transcript_export(storage.path(), &session.join("export.md"), b"x", false)
                .is_err()
        );
        assert_eq!(
            fs::read(&events).expect("events still unchanged"),
            b"canary"
        );
    }

    #[cfg(unix)]
    #[test]
    fn export_parent_swap_stays_bound_to_the_opened_directory_descriptor() {
        use std::os::unix::fs::symlink;

        let storage = tempfile::tempdir().expect("storage");
        let session = storage.path().join("sessions/history");
        fs::create_dir_all(&session).expect("session directory");
        let events = session.join("events.jsonl");
        fs::write(&events, b"event-canary").expect("events");

        let output = tempfile::tempdir().expect("output");
        let parent = output.path().join("safe");
        let moved = output.path().join("moved");
        fs::create_dir(&parent).expect("safe parent");
        let canonical_parent = fs::canonicalize(&parent).expect("canonical parent");
        let parent_for_swap = parent.clone();
        let moved_for_swap = moved.clone();
        let session_for_swap = session.clone();
        write_transcript_export_unix(
            &canonical_parent,
            std::ffi::OsStr::new("transcript.md"),
            b"safe export",
            false,
            move || {
                fs::rename(&parent_for_swap, &moved_for_swap).into_diagnostic()?;
                symlink(&session_for_swap, &parent_for_swap).into_diagnostic()?;
                Ok(())
            },
        )
        .expect("descriptor-bound export");

        assert_eq!(
            fs::read(moved.join("transcript.md")).expect("export"),
            b"safe export"
        );
        assert_eq!(
            fs::read(&events).expect("events unchanged"),
            b"event-canary"
        );
        assert!(!session.join("transcript.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn export_no_clobber_is_atomic_against_a_destination_creation_race() {
        let output = tempfile::tempdir().expect("output");
        let parent = fs::canonicalize(output.path()).expect("canonical output");
        let destination = parent.join("transcript.md");
        let destination_for_race = destination.clone();
        let result = write_transcript_export_unix(
            &parent,
            std::ffi::OsStr::new("transcript.md"),
            b"replacement",
            false,
            move || {
                fs::write(&destination_for_race, b"planted").into_diagnostic()?;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(destination).expect("planted output"), b"planted");
        assert!(fs::read_dir(&parent).expect("output entries").all(|entry| {
            !entry
                .expect("output entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".rottweiler-export-")
        }));
    }
}
