use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn export_session_requires_an_absolute_path_and_current_driver() {
    let factory = Arc::new(StubFactory::new());
    let queries = Arc::new(StubQueries::default());
    let host = EngineHost::new(
        EngineHostConfig {
            max_sessions: 1,
            max_deduplicated_requests: 32,
        },
        factory,
        queries.clone(),
    )
    .expect("host");
    let session_id = SessionId("export-session".to_owned());
    let driver = BoundClient {
        client_id: ClientId("export-driver".to_owned()),
    };
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "export-resume"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );

    let mut events = host
        .subscribe(driver.clone(), None, None)
        .await
        .expect("export events");
    let output = tempfile::tempdir()
        .expect("output")
        .path()
        .join("transcript.md");
    assert_eq!(
        host.dispatch(
            driver.clone(),
            ClientCommand::ExportSession {
                meta: meta("spoofed", "export-success"),
                session_id: session_id.clone(),
                format: TranscriptFormat::Markdown,
                output_path: output.display().to_string(),
                force: false,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let exported_path = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::SessionExported { output_path, .. } = decode_host_event(
                events
                    .recv()
                    .await
                    .expect("export event")
                    .expect("export result"),
            ) {
                break output_path;
            }
        }
    })
    .await
    .expect("typed export result");
    assert_eq!(exported_path, output.display().to_string());
    assert_eq!(
        *queries
            .exports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![(
            session_id.clone(),
            TranscriptFormat::Markdown,
            output.display().to_string(),
            false,
        )]
    );

    assert!(matches!(
        host.dispatch(
            driver,
            ClientCommand::ExportSession {
                meta: meta("spoofed", "export-relative"),
                session_id: session_id.clone(),
                format: TranscriptFormat::Json,
                output_path: "transcript.json".to_owned(),
                force: false,
            },
        )
        .await.outcome,
        CommandOutcome::Rejected { error }
            if error.code == "host_protocol_failure"
                && error.message.contains("absolute")
    ));
    assert!(matches!(
        host.dispatch(
            BoundClient {
                client_id: ClientId("other-client".to_owned()),
            },
            ClientCommand::ExportSession {
                meta: meta("spoofed", "export-unowned"),
                session_id,
                format: TranscriptFormat::Html,
                output_path: output.display().to_string(),
                force: true,
            },
        )
        .await.outcome,
        CommandOutcome::Rejected { error }
            if error.code == "host_protocol_failure"
                && error.message.contains("current driver")
    ));
    assert_eq!(
        queries
            .exports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn rename_persists_and_lists_a_session_without_its_driver_lease() {
    let (host, _factory) = host(2);
    let picker = BoundClient {
        client_id: ClientId("picker-client".to_owned()),
    };
    let active = SessionId("active-session".to_owned());
    let past = SessionId("past-session".to_owned());
    assert_eq!(
        host.dispatch(
            picker.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "resume-active"),
                session_id: active,
                last_seen_sequence: None,
                role: ClientRole::Driver,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    assert_eq!(
        host.dispatch(
            picker.clone(),
            ClientCommand::ResumeSession {
                meta: meta("spoofed", "resume-past"),
                session_id: past.clone(),
                last_seen_sequence: None,
                role: ClientRole::Observer,
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let past_session = host.ready_session(&past).await.expect("past session");
    assert_eq!(
        past_session
            .handle()
            .snapshot()
            .await
            .expect("past snapshot")
            .driver_client_id,
        None,
        "the renamed session must not be driven by the caller"
    );
    let mut host_events = host
        .subscribe(picker.clone(), None, None)
        .await
        .expect("picker events");

    assert_eq!(
        host.dispatch(
            picker.clone(),
            ClientCommand::RenameSession {
                meta: meta("spoofed", "rename-past"),
                session_id: past.clone(),
                title: "Past auth refactor".to_owned(),
            },
        )
        .await
        .outcome,
        CommandOutcome::Accepted {}
    );
    let title_event = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = decode_host_event(
                host_events
                    .recv()
                    .await
                    .expect("host event")
                    .expect("host result"),
            );
            if matches!(
                &event,
                EngineEvent::SessionTitleUpdated { meta, title, .. }
                    if meta.session_id == past && title == "Past auth refactor"
            ) {
                break event;
            }
        }
    })
    .await
    .expect("forwarded title update");
    assert!(matches!(
        title_event,
        EngineEvent::SessionTitleUpdated { .. }
    ));

    let mut durable = past_session
        .handle()
        .subscribe_client(ClientId("durable-reader".to_owned()), None)
        .expect("subscription");
    let persisted_title = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EngineEvent::SessionTitleUpdated { title, .. } = durable
                .recv()
                .await
                .expect("durable event")
                .as_ref()
                .clone()
            {
                break title;
            }
        }
    })
    .await
    .expect("persisted title event");
    assert_eq!(persisted_title, "Past auth refactor");

    let reply = host
        .dispatch(
            picker,
            ClientCommand::ListSessions {
                meta: meta("spoofed", "list-after-rename"),
            },
        )
        .await;
    let rw_types::CommandReply::Read {
        outcome: CommandOutcome::Accepted {},
        events,
    } = serde_json::from_slice(&reply.bytes).expect("typed read reply")
    else {
        panic!("accepted read")
    };
    let listed = events
        .into_iter()
        .find_map(|event| match event {
            EngineEvent::SessionsListed { sessions, .. } => Some(sessions),
            _ => None,
        })
        .expect("listed renamed session");
    assert!(
        listed
            .iter()
            .any(|session| { session.session_id == past && session.title == "Past auth refactor" })
    );
}
