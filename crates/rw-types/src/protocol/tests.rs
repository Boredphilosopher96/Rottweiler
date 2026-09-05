use super::{
    ClientCommand, ClientId, CommandAckMeta, CommandMeta, Cost, EngineEvent, EngineEventDelivery,
    EventMeta, McpEnvironmentEntry, ModeId, RequestId, SequenceId, SessionId, SubagentId,
    SubscriptionTokenAccounting, TranscriptFormat,
};

#[test]
fn cost_owns_subscription_token_interpretation() {
    let metered = Cost::SubscriptionQuota {
        used: Some("736".to_owned()),
        unit: Some("TOKENS".to_owned()),
    };
    let missing = Cost::SubscriptionQuota {
        used: None,
        unit: Some("tokens".to_owned()),
    };
    let other = Cost::AiCredits {
        credits_micros: 1,
        nominal_amount_micros: None,
        currency: None,
    };

    assert_eq!(
        metered.subscription_token_accounting(),
        SubscriptionTokenAccounting::Metered(736)
    );
    assert_eq!(
        missing.subscription_token_accounting(),
        SubscriptionTokenAccounting::Unavailable
    );
    assert_eq!(
        other.subscription_token_accounting(),
        SubscriptionTokenAccounting::NotApplicable
    );
}

#[test]
fn transport_can_replace_untrusted_wire_client_identity() {
    let mut command = ClientCommand::ShutdownHost {
        meta: CommandMeta {
            protocol_version: 1,
            client_id: ClientId("spoofed-on-wire".to_owned()),
            request_id: RequestId("request-1".to_owned()),
        },
    };
    command.meta_mut().client_id = ClientId("bound-connection".to_owned());
    assert_eq!(command.meta().client_id.0, "bound-connection");
}

#[test]
fn session_id_parser_owns_the_path_component_grammar() {
    for value in ["session", "session-1", "session_1", "session.1"] {
        assert_eq!(SessionId::parse(value), Ok(SessionId(value.to_owned())));
    }
    for value in ["", ".", "..", "../escape", "has/slash", "has space"] {
        assert!(SessionId::parse(value).is_err(), "accepted {value:?}");
    }
    assert!(SessionId::parse("a".repeat(super::MAX_SESSION_ID_BYTES)).is_ok());
    assert!(SessionId::parse("a".repeat(super::MAX_SESSION_ID_BYTES + 1)).is_err());
}

#[test]
fn command_session_accessor_distinguishes_host_and_session_commands() {
    let meta = CommandMeta {
        protocol_version: 1,
        client_id: ClientId("client".to_owned()),
        request_id: RequestId("request".to_owned()),
    };
    let session = SessionId("session".to_owned());
    let scoped = ClientCommand::AttachSession {
        meta: meta.clone(),
        session_id: session.clone(),
        last_seen_sequence: None,
        role: super::ClientRole::Driver,
    };
    let host = ClientCommand::ListSessions { meta };

    assert_eq!(scoped.session_id(), Some(&session));
    assert_eq!(host.session_id(), None);
}

#[test]
fn session_export_command_and_result_have_stable_wire_shapes()
-> Result<(), Box<dyn std::error::Error>> {
    let command = ClientCommand::ExportSession {
        meta: CommandMeta {
            protocol_version: 1,
            client_id: ClientId("driver".to_owned()),
            request_id: RequestId("export".to_owned()),
        },
        session_id: SessionId("session".to_owned()),
        format: TranscriptFormat::Markdown,
        output_path: "/tmp/transcript.md".to_owned(),
        force: true,
    };
    let command = serde_json::to_value(command)?;
    assert_eq!(command["type"], "export_session");
    assert_eq!(command["format"], "markdown");
    assert_eq!(command["output_path"], "/tmp/transcript.md");
    assert_eq!(command["force"], true);

    let event = EngineEvent::SessionExported {
        meta: CommandAckMeta {
            protocol_version: 1,
            client_id: ClientId("driver".to_owned()),
            request_id: RequestId("export".to_owned()),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
        },
        session_id: SessionId("session".to_owned()),
        output_path: "/private/tmp/transcript.md".to_owned(),
    };
    let event = serde_json::to_value(event)?;
    assert_eq!(event["type"], "session_exported");
    assert_eq!(event["output_path"], "/private/tmp/transcript.md");
    Ok(())
}

#[test]
fn session_rename_command_has_a_stable_wire_shape() -> Result<(), Box<dyn std::error::Error>> {
    let command = ClientCommand::RenameSession {
        meta: CommandMeta {
            protocol_version: 1,
            client_id: ClientId("picker".to_owned()),
            request_id: RequestId("rename".to_owned()),
        },
        session_id: SessionId("session".to_owned()),
        title: "Auth refactor".to_owned(),
    };
    let command = serde_json::to_value(command)?;
    assert_eq!(command["type"], "rename_session");
    assert_eq!(command["session_id"], "session");
    assert_eq!(command["title"], "Auth refactor");
    Ok(())
}

#[test]
fn mcp_stdio_management_commands_have_stable_redacted_wire_shapes()
-> Result<(), Box<dyn std::error::Error>> {
    let meta = CommandMeta {
        protocol_version: 1,
        client_id: ClientId("picker".to_owned()),
        request_id: RequestId("mcp-stdio".to_owned()),
    };
    let secret = "wire-secret-canary";
    let command = ClientCommand::AddMcpStdioServer {
        meta: meta.clone(),
        session_id: SessionId("session".to_owned()),
        name: "docs".to_owned(),
        executable: "/usr/local/bin/docs-mcp".to_owned(),
        args: vec!["--stdio".to_owned()],
        environment: vec![McpEnvironmentEntry {
            key: "DOCS_TOKEN".to_owned(),
            value: secret.to_owned(),
        }],
    };
    let debug = format!("{command:?}");
    assert!(debug.contains("DOCS_TOKEN"));
    assert!(!debug.contains(secret));
    let wire = serde_json::to_value(command)?;
    assert_eq!(wire["type"], "add_mcp_stdio_server");
    assert_eq!(wire["session_id"], "session");
    assert_eq!(wire["name"], "docs");
    assert_eq!(wire["executable"], "/usr/local/bin/docs-mcp");
    assert_eq!(wire["args"], serde_json::json!(["--stdio"]));
    assert_eq!(
        wire["environment"],
        serde_json::json!([{"key":"DOCS_TOKEN","value":secret}])
    );

    let remove = serde_json::to_value(ClientCommand::RemoveMcpServer {
        meta,
        session_id: SessionId("session".to_owned()),
        name: "docs".to_owned(),
    })?;
    assert_eq!(remove["type"], "remove_mcp_server");
    assert_eq!(remove["session_id"], "session");
    assert_eq!(remove["name"], "docs");
    Ok(())
}

#[test]
fn subagent_replay_completion_exposes_page_and_tail_state() -> Result<(), Box<dyn std::error::Error>>
{
    let event = EngineEvent::SubagentReplayCompleted {
        meta: CommandAckMeta {
            protocol_version: 1,
            client_id: ClientId("driver".to_owned()),
            request_id: RequestId("replay".to_owned()),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
        },
        session_id: SessionId("parent".to_owned()),
        subagent_id: SubagentId("child".to_owned()),
        through_sequence: Some(SequenceId(15)),
        next_cursor: Some(SequenceId(15)),
        tail_sequence: Some(SequenceId(30)),
        has_more: true,
        events_before_page: 8,
        truncated: true,
    };
    let wire = serde_json::to_value(event)?;
    assert_eq!(wire["through_sequence"], "15");
    assert_eq!(wire["next_cursor"], "15");
    assert_eq!(wire["tail_sequence"], "30");
    assert_eq!(wire["has_more"], true);
    assert_eq!(wire["events_before_page"], "8");
    assert_eq!(wire["truncated"], true);
    Ok(())
}

#[test]
fn event_delivery_is_owned_by_the_protocol_variant() {
    let connection = EngineEvent::SessionExported {
        meta: CommandAckMeta {
            protocol_version: 1,
            client_id: ClientId("driver".to_owned()),
            request_id: RequestId("export".to_owned()),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
        },
        session_id: SessionId("session".to_owned()),
        output_path: "/tmp/export.md".to_owned(),
    };
    let transient = EngineEvent::SubagentProgress {
        parent_session_id: SessionId("session".to_owned()),
        subagent_id: SubagentId("child".to_owned()),
        child_session_id: SessionId("child-session".to_owned()),
        child_sequence: None,
        event: serde_json::json!({"type": "progress"}),
    };
    let durable = EngineEvent::ModeChanged {
        meta: EventMeta {
            protocol_version: 1,
            session_id: SessionId("session".to_owned()),
            sequence_id: SequenceId(1),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        },
        mode: ModeId("execute".to_owned()),
        definition_fingerprint: "fixture".to_owned(),
    };

    assert_eq!(connection.delivery(), EngineEventDelivery::Connection);
    assert_eq!(transient.delivery(), EngineEventDelivery::Transient);
    assert_eq!(durable.delivery(), EngineEventDelivery::Durable);
}
