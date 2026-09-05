use super::*;

#[tokio::test]
async fn list_commands_routes_to_the_explicit_sessions_assembled_registry() {
    let (host, _factory) = host(3);
    let bound = BoundClient {
        client_id: ClientId("palette-driver".to_owned()),
    };
    for name in ["palette-first", "palette-second"] {
        assert_eq!(
            host.dispatch(
                bound.clone(),
                ClientCommand::ResumeSession {
                    meta: meta("spoofed", &format!("resume-{name}")),
                    session_id: SessionId(name.to_owned()),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                },
            )
            .await
            .outcome,
            CommandOutcome::Accepted
        );
    }
    for name in ["palette-first", "palette-second"] {
        let reply = host
            .dispatch(
                bound.clone(),
                ClientCommand::ListCommands {
                    meta: meta("spoofed", &format!("list-{name}")),
                    session_id: SessionId(name.to_owned()),
                },
            )
            .await;
        let rw_types::CommandReply::Read {
            outcome: CommandOutcome::Accepted,
            events,
        } = serde_json::from_slice(&reply.bytes).expect("typed read reply")
        else {
            panic!("accepted read")
        };
        let (session_id, commands, truncated) = events
            .into_iter()
            .find_map(|event| match event {
                EngineEvent::CommandDescriptorsListed {
                    session_id,
                    commands,
                    truncated,
                    ..
                } => Some((session_id, commands, truncated)),
                _ => None,
            })
            .expect("command catalog");
        assert_eq!(session_id, SessionId(name.to_owned()));
        assert!(!truncated);
        assert!(
            commands
                .iter()
                .any(|command| command.name == format!("only.{name}"))
        );
        let other = if name == "palette-first" {
            "palette-second"
        } else {
            "palette-first"
        };
        assert!(
            commands
                .iter()
                .all(|command| command.name != format!("only.{other}"))
        );
        assert!(commands.iter().any(|command| command.name == "permissions"));
        assert!(commands.iter().any(|command| command.name == "add-dir"));
    }
}

#[tokio::test]
async fn list_modes_returns_a_bounded_connection_scoped_live_catalog() {
    let (host, _factory) = host(2);
    let bound = BoundClient {
        client_id: ClientId("mode-driver".to_owned()),
    };
    let session_id = SessionId("mode-catalog".to_owned());
    assert_eq!(
        host.dispatch(
            bound.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "resume-mode-catalog"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted
    );
    let reply = host
        .dispatch(
            bound,
            ClientCommand::ListModes {
                meta: meta("spoofed", "list-mode-catalog"),
                session_id: session_id.clone(),
            },
        )
        .await;
    let rw_types::CommandReply::Read {
        outcome: CommandOutcome::Accepted,
        events,
    } = serde_json::from_slice(&reply.bytes).expect("typed read reply")
    else {
        panic!("accepted read")
    };
    let listed = events
        .into_iter()
        .find_map(|event| match event {
            EngineEvent::ModesListed {
                session_id: listed_session,
                modes,
                truncated,
                ..
            } if listed_session == session_id => Some((modes, truncated)),
            _ => None,
        })
        .expect("mode catalog");
    assert!(!listed.1);
    assert_eq!(listed.0.len(), 3);
    assert!(
        listed
            .0
            .iter()
            .any(|mode| mode.id.0 == "execute" && mode.current)
    );
    assert!(listed.0.iter().all(|mode| !mode.description.is_empty()));
}

#[test]
fn wire_command_catalog_is_bounded_below_the_sse_line_limit() {
    let descriptors = (0..600).map(|index| {
        ExtensionCommandDescriptor::new(
            format!("catalog-{index}"),
            format!("{}-{index}", "description".repeat(80)),
        )
        .with_argument_hint("<value>".repeat(20))
    });
    let (commands, truncated) = wire_command_catalog(descriptors);
    assert!(truncated);
    assert!(commands.len() <= MAX_WIRE_COMMANDS);
    assert!(
        serde_json::to_vec(&commands)
            .expect("bounded command catalog JSON")
            .len()
            <= MAX_WIRE_COMMAND_CATALOG_BYTES
    );
}

#[test]
fn wire_mode_catalog_is_count_and_byte_bounded() {
    let active = ModeDescriptor {
        id: rw_types::ModeId("zzzz-active".to_owned()),
        description: "active mode beyond every stable-order cutoff".to_owned(),
        current: true,
    };
    let descriptors = (0..200).map(|index| ModeDescriptor {
        id: rw_types::ModeId(format!("mode-{index}")),
        description: format!("{}-{index}", "description".repeat(80)),
        current: false,
    });
    let (modes, truncated) = wire_mode_catalog(active.clone(), descriptors);
    assert!(truncated);
    assert!(modes.len() <= MAX_WIRE_MODES);
    assert_eq!(modes.first(), Some(&active));
    assert_eq!(modes.iter().filter(|mode| mode.current).count(), 1);
    assert!(
        serde_json::to_vec(&modes)
            .expect("bounded mode catalog JSON")
            .len()
            <= MAX_WIRE_MODE_CATALOG_BYTES
    );
}

#[test]
fn wire_command_catalog_preserves_each_runtime_source() {
    let sources = [
        rw_types::CommandSource::Builtin,
        rw_types::CommandSource::Project,
        rw_types::CommandSource::User,
        rw_types::CommandSource::Plugin,
        rw_types::CommandSource::Skill,
        rw_types::CommandSource::Workflow,
        rw_types::CommandSource::Mcp,
    ];
    let descriptors = sources.iter().enumerate().map(|(index, source)| {
        ExtensionCommandDescriptor::new(format!("source-{index}"), "source test")
            .with_source(*source)
    });

    let (commands, truncated) = wire_command_catalog(descriptors);

    assert!(!truncated);
    assert_eq!(commands.len(), sources.len());
    for (command, expected) in commands.iter().zip(sources) {
        assert_eq!(command.source, expected);
    }
}
