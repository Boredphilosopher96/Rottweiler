//! Optimized real EngineHost/store with a compiled TUI; HTTP framing is test-owned.
mod source;
mod transport;
use super::*;
use rw_core::{
    BoundClient, ClientCommand, ClientId, CommandMeta, CommandOutcome, EngineHost,
    EngineHostConfig, RequestId,
};
use rw_providers::{FinishReason, ProviderEvent};
use std::{sync::atomic::Ordering, time::Duration};

const MAX_DURATION: Duration = Duration::from_secs(120);
const STREAM_LINES: usize = 2_000;
const LINE: &str = "joined stream line: bounded native rendering\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "optimized host + separately verified compiled TUI; external owner supplies RW_A19_DIRECTORY"]
async fn joined_streaming_native_client() {
    assert!(
        !std::hint::black_box(cfg!(debug_assertions)),
        "prepare optimized host outside measurement"
    );
    let directory = std::env::var_os("RW_A19_DIRECTORY")
        .map(PathBuf::from)
        .expect("explicit private fixture directory");
    let directory = fs::canonicalize(directory).expect("existing private directory");
    let workspace = private_test_directory(&directory.join("workspace"));
    let storage = private_test_directory(&directory.join("state"));
    let options = options(&storage, &workspace);
    let factory = Arc::new(
        RuntimeSessionFactory::new(options.clone())
            .await
            .expect("factory"),
    );
    let initial = factory
        .create(CreateSessionRequest {
            session_id: SessionId(source::SESSION.into()),
            workspace: workspace.display().to_string(),
            model: None,
        })
        .await
        .expect("initial metadata");
    initial
        .handle()
        .close()
        .await
        .expect("seed preparation session closed");
    drop(initial);
    factory
        .shutdown()
        .await
        .expect("preparation factory settled");
    drop(factory);
    let storage_copy = storage.clone();
    let seeded = rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        source::seed(&storage_copy)
    })
    .await
    .expect("bounded source seeding owner");
    let factory = Arc::new(
        RuntimeSessionFactory::new(options)
            .await
            .expect("measurement factory"),
    );
    let host = EngineHost::new(
        EngineHostConfig::default(),
        factory.clone(),
        factory.clone(),
    )
    .expect("actual EngineHost");
    host.prepare_session(
        CreateSessionRequest {
            session_id: SessionId(source::SESSION.into()),
            workspace: workspace.display().to_string(),
            model: None,
        },
        true,
    )
    .await
    .expect("actual canonical reopen");
    host.prepare_session(
        CreateSessionRequest {
            session_id: SessionId("joined-control".into()),
            workspace: workspace.display().to_string(),
            model: None,
        },
        false,
    )
    .await
    .expect("independent control session");
    let control = host
        .session(&SessionId("joined-control".into()))
        .await
        .expect("control actor")
        .handle();
    assert_eq!(
        control
            .dispatch(ClientCommand::AttachSession {
                meta: metadata("control-attach", "local"),
                session_id: control.session_id().clone(),
                last_seen_sequence: None,
                role: rw_types::ClientRole::Driver
            })
            .await
            .expect("control driver"),
        CommandOutcome::Accepted {}
    );
    let socket = directory.join("joined.sock");
    let token = directory.join("bootstrap.token");
    fs::write(&token, transport::BOOTSTRAP).expect("private token");
    #[cfg(unix)]
    fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("private token mode");
    let mut http = transport::Transport::start(&socket, host.clone()).await;
    publish(
        &directory,
        "joined-input.json",
        &serde_json::json!({"socketPath":socket,"bootstrapTokenFile":token,
        "sessionId":source::SESSION,"history":seeded,"streamLines":STREAM_LINES,"streamLine":LINE,
        "host_kind":"optimized-production-EngineHost","http_kind":"bounded-test-forwarder"}),
    )
    .await;
    let body = async {
        assert_eq!(
            http.controls.recv().await.as_deref(),
            Some("/fixture/stall")
        );
        let pressure = factory
            .journal_service
            .commits
            .measure_storage_pressure_while(&control, 0, async {
                // Polled only after actual commit-worker saturation, not merely on arm.
                publish(
                    &directory,
                    "storage-held.json",
                    &serde_json::json!({"phase":"admitted-native-workers-held"}),
                )
                .await;
                assert_eq!(
                    http.controls.recv().await.as_deref(),
                    Some("/fixture/release")
                );
            })
            .await;
        publish(&directory, "storage-settled.json", &pressure).await;
        assert_eq!(http.controls.recv().await.as_deref(), Some("/fixture/done"));
        pressure
    };
    let result = tokio::time::timeout(MAX_DURATION, body).await;
    // Even failed client/readiness proofs close actual accepted actor effects before transport.
    let shutdown = host
        .dispatch(
            BoundClient {
                client_id: ClientId(transport::CLIENT.into()),
            },
            ClientCommand::ShutdownHost {
                meta: metadata("joined-shutdown", transport::CLIENT),
            },
        )
        .await;
    let commands = http.counts.commands.load(Ordering::Relaxed);
    let events = http.counts.events.load(Ordering::Relaxed);
    let event_bytes = http.counts.event_bytes.load(Ordering::Relaxed);
    http.close().await;
    assert_eq!(
        shutdown.outcome,
        CommandOutcome::Accepted {},
        "physical host shutdown"
    );
    let pressure = result.expect("joined host/client bounded completion");
    assert_eq!(
        fs::read_to_string(workspace.join("joined-approved.txt")).expect("actual approved write"),
        "joined durable approval\n"
    );
    publish(&directory, "joined-host.json", &serde_json::json!({"schemaVersion":1,"pid":std::process::id(),
        "source":seeded,"storage":pressure,"forwarded_commands":commands,"forwarded_events":events,
        "forwarded_event_json_bytes":event_bytes,"streamLines":STREAM_LINES,"streamBytes":STREAM_LINES*LINE.len(),
        "approved_write":true,"host_settled":true,"transport_settled":true})).await;
}

fn options(storage: &Path, workspace: &Path) -> RuntimeHostOptions {
    let mut config = Config::default();
    config.compaction.auto = false;
    let mut stream = (0..STREAM_LINES)
        .map(|_| ProviderEvent::TextDelta { text: LINE.into() })
        .collect::<Vec<_>>();
    stream.extend([
        ProviderEvent::ToolCallStart { id:"child".into(), name:"spawn_agent".into() },
        ProviderEvent::ToolCallEnd { id:"child".into(), arguments:serde_json::json!({"action":"spawn","task":"Produce joined child stream","agent":"general","isolation":"shared"}) },
        ProviderEvent::ToolCallStart { id:"approved".into(), name:"write".into() },
        ProviderEvent::ToolCallEnd { id:"approved".into(), arguments:serde_json::json!({"path":"joined-approved.txt","content":"joined durable approval\n"}) },
        ProviderEvent::Finished { reason:FinishReason::ToolCalls },
    ]);
    RuntimeHostOptions {
        storage_root: storage.into(),
        credentials_path: storage.join("credentials.json"),
        config,
        allowed_workspaces: vec![workspace.into()],
        permission_mode: Some(PermissionMode::Strict),
        max_turns: 4,
        provider_mode: HostedProviderMode::DeterministicReplay {
            provider_name: "joined-fixture".into(),
            scripts: vec![
                stream,
                (0..STREAM_LINES)
                    .map(|_| ProviderEvent::TextDelta {
                        text: "joined child advances\n".into(),
                    })
                    .chain(std::iter::once(ProviderEvent::Finished {
                        reason: FinishReason::Stop,
                    }))
                    .collect(),
                vec![
                    ProviderEvent::TextDelta {
                        text: "joined parent completed".into(),
                    },
                    ProviderEvent::Finished {
                        reason: FinishReason::Stop,
                    },
                ],
            ],
            event_delay_ms: 5,
        },
        dangerously_trust: true,
        wait_for_execution_lease: false,
    }
}
fn metadata(request: &str, client: &str) -> CommandMeta {
    CommandMeta {
        protocol_version: rw_types::PROTOCOL_VERSION,
        client_id: ClientId(client.into()),
        request_id: RequestId(request.into()),
    }
}
async fn publish(directory: &Path, name: &str, value: &serde_json::Value) {
    let bytes = serde_json::to_vec(value).expect("bounded fixture evidence");
    assert!(bytes.len() <= 64 * 1024);
    let temporary = directory.join(format!(".{name}.tmp"));
    tokio::fs::write(&temporary, bytes)
        .await
        .expect("evidence write");
    tokio::fs::rename(temporary, directory.join(name))
        .await
        .expect("atomic evidence publication");
}
