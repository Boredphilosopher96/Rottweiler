use super::*;
use rw_store::session::SessionEventPageLimits;
use rw_types::{
    EventMeta, SessionPayloadReference, ToolCallId, ToolInvocationId, ToolOutput, TurnId,
};

fn completion(sequence: u64, reference: SessionPayloadReference) -> EngineEvent {
    EngineEvent::ToolCallFinished {
        payloads: vec![reference],
        presentation: None,
        meta: EventMeta {
            protocol_version: rw_core::SESSION_EVENT_VERSION,
            session_id: SessionId("parent".into()),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-09-08T00:00:00Z".into(),
            caused_by: None,
        },
        turn_id: TurnId("1".into()),
        tool_call_id: ToolCallId(format!("tool-{sequence}")),
        invocation_id: ToolInvocationId(format!("invocation-{sequence}")),
        output: ToolOutput::Text {
            text: "bounded reference".into(),
        },
        is_error: false,
        call_index: 0,
    }
}

#[test]
fn selected_fork_copies_only_attached_prefix_and_survives_parent_deletion()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let service =
        JournalService::new(root.path()).map_err(|error| io::Error::other(error.to_string()))?;
    let source = service
        .payload_source("parent")
        .map_err(|error| io::Error::other(error.to_string()))?;
    let store = rw_mcp::PayloadSource::open(&*source)?;
    let retained = store.write(b"retained durable body", &|| false)?;
    let later = store.write(b"outside the selected prefix", &|| false)?;
    let mut parent = SessionEventLog::open(root.path(), "parent")?;
    parent.append(&completion(0, retained.clone()))?;
    parent.append(&completion(1, later.clone()))?;
    let copier = ForkPayloads {
        source,
        target: service
            .payload_source("child")
            .map_err(|error| io::Error::other(error.to_string()))?,
    };
    let child = SessionEventLog::fork_mapped_view::<EngineEvent, _>(
        root.path(),
        "parent",
        "child",
        &parent.read_view(),
        Some(SequenceId(0)),
        move |mut event| {
            copier.copy(&event)?;
            event
                .meta_mut()
                .ok_or(SessionStoreError::CorruptEvent("missing meta"))?
                .session_id = SessionId("child".into());
            Ok(event)
        },
    )?;
    let page = child
        .read_view()
        .page::<EngineEvent>(None, SessionEventPageLimits::default())?;
    assert_eq!(page.events.len(), 1);
    drop(child);
    drop(parent);
    drop(store);
    drop(service);
    std::fs::remove_dir_all(root.path().join("sessions/parent"))?;
    let reopened =
        JournalService::new(root.path()).map_err(|error| io::Error::other(error.to_string()))?;
    let source = reopened
        .payload_source("child")
        .map_err(|error| io::Error::other(error.to_string()))?;
    let store = rw_mcp::PayloadSource::open(&*source)?;
    assert_eq!(
        store.window(&retained, 0, None, &|| false)?.content,
        "retained durable body"
    );
    assert!(store.window(&later, 0, None, &|| false).is_err());
    Ok(())
}
