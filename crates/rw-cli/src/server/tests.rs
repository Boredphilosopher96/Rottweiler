use std::sync::atomic::{AtomicUsize, Ordering};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use hyper::{Uri, client::conn::http1 as client_http1};
use rw_core::{
    Attachment, AttachmentData, CommandAckMeta, CommandMeta, EngineError, EngineErrorCategory,
    EventMeta, PROTOCOL_VERSION, RequestId, SessionDescriptor, TurnId,
};
use rw_store::session::SessionEventLog;
use tempfile::tempdir;
use tokio::net::UnixStream;

use super::*;

#[derive(Default)]
struct StubEngine {
    dispatches: AtomicUsize,
    received: Mutex<Vec<(ClientId, ClientCommand)>>,
    completions: Mutex<Vec<ShellCompletionFixture>>,
    provider_keys: Mutex<Vec<(ClientId, SessionId, String, String)>>,
    subscription_error: Mutex<Option<EventSubscriptionError>>,
}

type ShellCompletionFixture = (SessionId, ShellId, i32, Option<String>);

#[async_trait]
impl ServerEngine for StubEngine {
    async fn dispatch(
        &self,
        bound_client: ClientId,
        command: ClientCommand,
    ) -> std::result::Result<rw_core::HostReply, String> {
        self.dispatches.fetch_add(1, Ordering::Relaxed);
        self.received
            .lock()
            .expect("received commands")
            .push((bound_client, command.clone()));
        if matches!(command, ClientCommand::ShutdownHost { .. }) {
            return Ok(rw_core::HostReply::command(CommandOutcome::Accepted));
        }
        Ok(rw_core::HostReply::command(CommandOutcome::Rejected {
            error: EngineError {
                category: EngineErrorCategory::Protocol,
                code: "fixture".to_owned(),
                message: "fixture".to_owned(),
                retryable: false,
                details: None,
            },
        }))
    }

    async fn subscribe(
        &self,
        _bound_client: ClientId,
        _session_id: Option<SessionId>,
        _last_seen: Option<SequenceId>,
    ) -> std::result::Result<
        mpsc::Receiver<std::result::Result<EngineEvent, String>>,
        EventSubscriptionError,
    > {
        if let Some(error) = self
            .subscription_error
            .lock()
            .expect("subscription error")
            .clone()
        {
            return Err(error);
        }
        let (send, receive) = mpsc::channel(1);
        send.send(Ok(EngineEvent::SessionsListed {
            meta: CommandAckMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("fixture".to_owned()),
                request_id: RequestId("fixture".to_owned()),
                emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
            },
            sessions: Vec::<SessionDescriptor>::new(),
        }))
        .await
        .map_err(|_| "fixture receiver closed".to_owned())?;
        Ok(receive)
    }

    async fn complete_shell(
        &self,
        session_id: SessionId,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> std::result::Result<(), String> {
        self.completions.lock().expect("shell completions").push((
            session_id,
            shell_id,
            status,
            captured_output,
        ));
        Ok(())
    }

    async fn submit_provider_api_key(
        &self,
        bound_client: ClientId,
        session_id: SessionId,
        provider: String,
        api_key: ProviderApiKey,
    ) -> std::result::Result<ProviderApiKeySubmission, String> {
        self.provider_keys.lock().expect("provider keys").push((
            bound_client,
            session_id,
            provider,
            api_key.expose_secret().to_owned(),
        ));
        Ok(ProviderApiKeySubmission {
            stored: true,
            activated: true,
            warnings: vec!["fixture credential-store warning".to_owned()],
        })
    }

    async fn activate_provider(
        &self,
        _bound_client: ClientId,
        _session_id: SessionId,
        _provider: String,
    ) -> std::result::Result<(), String> {
        Ok(())
    }
}

async fn unix_request(socket: &Path, request: Request<Full<Bytes>>) -> Response<Incoming> {
    let stream = UnixStream::connect(socket).await.expect("connect test UDS");
    let (mut sender, connection) = client_http1::handshake(TokioIo::new(stream))
        .await
        .expect("HTTP handshake");
    tokio::spawn(async move {
        connection.await.expect("test HTTP connection");
    });
    sender.send_request(request).await.expect("send request")
}

fn request_builder(method: Method, path: &str) -> hyper::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(path.parse::<Uri>().expect("fixture URI"))
        .header(hyper::header::HOST, "localhost")
}

#[test]
fn runtime_files_are_private_and_token_debug_is_redacted() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir().expect("runtime root");
    let (runtime, _listener) = ServerRuntime::create(root.path()).expect("runtime");
    assert_eq!(
        fs::metadata(&runtime.paths.directory)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for path in [
        &runtime.paths.socket,
        &runtime.paths.token,
        &runtime.paths.descriptor,
    ] {
        assert_eq!(
            fs::metadata(path)
                .expect("private metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let debug = format!("{runtime:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains(&runtime.bootstrap.encode()));
    let descriptor = fs::read_to_string(&runtime.paths.descriptor).expect("descriptor");
    assert!(!descriptor.contains(&runtime.bootstrap.encode()));
}

#[test]
fn plugin_development_is_a_distinct_narrow_transport_capability() {
    let request = Request::builder()
        .header(CAPABILITY_HEADER, "plugin_development")
        .body(())
        .expect("capability request");
    assert_eq!(
        requested_capability(&request).expect("plugin development capability"),
        ClientCapability::PluginDevelopment
    );
}

#[test]
fn supervisor_selected_runtime_paths_rotate_only_expected_stale_artifacts() {
    let root = tempdir().expect("runtime root");
    let directory = root.path().join("selected");
    fs::create_dir(&directory).expect("selected directory");
    set_mode(&directory, 0o700).expect("private selected directory");
    let paths = ServerRuntimePaths {
        socket: directory.join("engine.sock"),
        token: directory.join("auth.token"),
        descriptor: directory.join("runtime.json"),
        directory,
    };
    let (first, first_listener) =
        ServerRuntime::create_for_session(paths.clone(), Some("session-selected"))
            .expect("first selected runtime");
    let first_token = fs::read_to_string(&first.paths.token).expect("first token");
    let first_descriptor = fs::read_to_string(&first.paths.descriptor).expect("descriptor");
    assert!(first_descriptor.contains(r#""session_id":"session-selected""#));
    assert!(ServerRuntime::create_at(paths.clone()).is_err());
    assert_eq!(
        fs::read_to_string(&first.paths.token).expect("live token remains"),
        first_token
    );
    drop(first_listener);
    let (second, _second_listener) =
        ServerRuntime::create_at(paths).expect("restarted selected runtime");
    let second_token = fs::read_to_string(&second.paths.token).expect("second token");
    assert_ne!(first_token, second_token);
}

#[test]
fn selected_runtime_refuses_symlink_artifacts() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("runtime root");
    let directory = root.path().join("selected");
    fs::create_dir(&directory).expect("selected directory");
    set_mode(&directory, 0o700).expect("private selected directory");
    let outside = root.path().join("outside");
    fs::write(&outside, b"do not replace").expect("outside fixture");
    let token = directory.join("auth.token");
    symlink(&outside, &token).expect("token symlink");
    let paths = ServerRuntimePaths {
        socket: directory.join("engine.sock"),
        token,
        descriptor: directory.join("runtime.json"),
        directory,
    };
    assert!(ServerRuntime::create_at(paths).is_err());
    assert_eq!(
        fs::read(&outside).expect("outside remains"),
        b"do not replace"
    );
}

#[test]
fn client_tokens_are_bound_to_server_minted_ids() {
    let registry = ClientRegistry::new();
    let first = registry
        .mint(ClientCapability::Interactive)
        .expect("first client");
    let second = registry
        .mint(ClientCapability::ShellBroker)
        .expect("second client");
    assert_eq!(
        registry.authenticate(&first.client_id.0, &first.token),
        Some(AuthenticatedClient {
            client_id: first.client_id.clone(),
            capability: ClientCapability::Interactive,
        })
    );
    assert_eq!(
        registry.authenticate(&first.client_id.0, &second.token),
        None
    );
    assert_eq!(
        registry.authenticate(&second.client_id.0, &first.token),
        None
    );
}

#[test]
fn protocol_fixture_command_has_transport_overwritable_meta() {
    let mut command = ClientCommand::ListSessions {
        meta: CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("spoofed".to_owned()),
            request_id: RequestId("request".to_owned()),
        },
    };
    command.meta_mut().client_id = ClientId("bound".to_owned());
    assert_eq!(command.meta().client_id.0, "bound");
}

#[test]
fn provider_api_key_request_debug_is_redacted() {
    let canary = "rw-secret-canary-never-debug";
    let request = ProviderApiKeyRequest {
        session_id: "session".to_owned(),
        provider: "openai".to_owned(),
        api_key: canary.to_owned(),
    };
    let debug = format!("{request:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains(canary));
}

#[test]
fn provider_api_key_attempt_guard_is_bounded_and_drop_cleans_reservation() {
    let attempts = Arc::new(Mutex::new(HashSet::new()));
    let first =
        ProviderApiKeyAttemptGuard::reserve(Arc::clone(&attempts), "company-openai".to_owned())
            .expect("first reservation");
    assert!(
        ProviderApiKeyAttemptGuard::reserve(Arc::clone(&attempts), "company-openai".to_owned(),)
            .is_none()
    );
    drop(first);
    assert!(ProviderApiKeyAttemptGuard::reserve(attempts, "company-openai".to_owned(),).is_some());
}

#[tokio::test]
async fn provider_api_key_uses_authenticated_non_protocol_channel_and_sanitized_response() {
    let root = tempdir().expect("runtime root");
    let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
    let engine = Arc::new(StubEngine::default());
    let state = ServerState::new(engine.clone(), &runtime);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve(listener, state, shutdown_rx));
    let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");
    let connected = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/connect")
            .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
            .body(Full::new(Bytes::new()))
            .expect("connect request"),
    )
    .await;
    let credentials: ClientCredentials = serde_json::from_slice(
        &connected
            .into_body()
            .collect()
            .await
            .expect("connect body")
            .to_bytes(),
    )
    .expect("credentials");
    let canary = "rw-secret-canary-never-wire-back";
    let body = serde_json::to_vec(&serde_json::json!({
        "session_id": "session-secret",
        "provider": "company-openai",
        "api_key": canary,
    }))
    .expect("secret body");
    let response = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/provider-api-key")
            .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
            .header(CLIENT_HEADER, &credentials.client_id.0)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .expect("credential request"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_bytes = response
        .into_body()
        .collect()
        .await
        .expect("response")
        .to_bytes();
    assert!(
        !response_bytes
            .windows(canary.len())
            .any(|window| window == canary.as_bytes())
    );
    assert!(String::from_utf8_lossy(&response_bytes).contains("fixture credential-store warning"));
    {
        let keys = engine.provider_keys.lock().expect("provider keys");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, credentials.client_id);
        assert_eq!(keys[0].1.0, "session-secret");
        assert_eq!(keys[0].2, "company-openai");
        assert_eq!(keys[0].3, canary);
    }
    assert!(
        !serde_json::to_string(&ClientCommand::ListSessions {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("client".to_owned()),
                request_id: RequestId("request".to_owned()),
            },
        })
        .expect("protocol JSON")
        .contains(canary)
    );
    shutdown.send(true).expect("shutdown");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn command_transport_accepts_the_maximum_legal_image_attachment_envelope() {
    let root = tempdir().expect("runtime root");
    let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
    let engine = Arc::new(StubEngine::default());
    let state = ServerState::new(engine.clone(), &runtime);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve(listener, state, shutdown_rx));
    let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");
    let connected = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/connect")
            .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
            .body(Full::new(Bytes::new()))
            .expect("connect request"),
    )
    .await;
    let credentials: ClientCredentials = serde_json::from_slice(
        &connected
            .into_body()
            .collect()
            .await
            .expect("connect body")
            .to_bytes(),
    )
    .expect("credentials");
    let image = BASE64_STANDARD.encode(vec![0_u8; 5 * 1024 * 1024]);
    let command = ClientCommand::SendMessage {
        meta: CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("spoofed".to_owned()),
            request_id: RequestId("maximum-attachments".to_owned()),
        },
        session_id: SessionId("session-attachments".to_owned()),
        content: "inspect both screenshots".to_owned(),
        attachments: ["first.png", "second.png"]
            .map(|name| Attachment {
                name: name.to_owned(),
                source_path: None,
                media_type: "image/png".to_owned(),
                data: AttachmentData::InlineBase64 {
                    data: image.clone(),
                },
            })
            .to_vec(),
    };
    let body = serde_json::to_vec(&command).expect("command JSON");
    assert!(
        body.len() < COMMAND_BODY_LIMIT,
        "legal envelope must fit transport"
    );
    assert!(
        body.len() > 2 * 1024 * 1024,
        "fixture must cover the old failure"
    );
    let response = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/command")
            .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
            .header(CLIENT_HEADER, &credentials.client_id.0)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .expect("attachment command"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(engine.dispatches.load(Ordering::Relaxed), 1);
    shutdown.send(true).expect("shutdown");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn uds_auth_mints_bound_identity_and_overwrites_spoofed_meta() {
    let root = tempdir().expect("runtime root");
    let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
    let engine = Arc::new(StubEngine::default());
    let state = ServerState::new(engine.clone(), &runtime);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve(listener, state, shutdown_rx));

    let missing = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/connect")
            .body(Full::new(Bytes::new()))
            .expect("missing-auth request"),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");
    let connected = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/connect")
            .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
            .body(Full::new(Bytes::new()))
            .expect("connect request"),
    )
    .await;
    assert_eq!(connected.status(), StatusCode::CREATED);
    let credentials: ClientCredentials = serde_json::from_slice(
        &connected
            .into_body()
            .collect()
            .await
            .expect("connect body")
            .to_bytes(),
    )
    .expect("client credentials");

    let command = ClientCommand::ListSessions {
        meta: CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("spoofed-driver".to_owned()),
            request_id: RequestId("list".to_owned()),
        },
    };
    let command_bytes = serde_json::to_vec(&command).expect("command JSON");
    let wrong_token = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/command")
            .header(AUTHORIZATION, format!("Bearer {bootstrap}"))
            .header(CLIENT_HEADER, &credentials.client_id.0)
            .body(Full::new(Bytes::from(command_bytes.clone())))
            .expect("wrong-token command"),
    )
    .await;
    assert_eq!(wrong_token.status(), StatusCode::UNAUTHORIZED);

    let accepted = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/command")
            .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
            .header(CLIENT_HEADER, &credentials.client_id.0)
            .body(Full::new(Bytes::from(command_bytes)))
            .expect("bound command"),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    {
        let received = engine.received.lock().expect("received command");
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].0, credentials.client_id);
        assert_eq!(received[0].1.meta().client_id, credentials.client_id);
    }

    let broker_connected = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/connect")
            .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
            .header(CAPABILITY_HEADER, "shell_broker")
            .body(Full::new(Bytes::new()))
            .expect("broker connect"),
    )
    .await;
    assert_eq!(broker_connected.status(), StatusCode::CREATED);
    let broker: ClientCredentials = serde_json::from_slice(
        &broker_connected
            .into_body()
            .collect()
            .await
            .expect("broker connect body")
            .to_bytes(),
    )
    .expect("broker credentials");
    let completion = ClientCommand::UserShellEnded {
        meta: CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("spoofed-broker".to_owned()),
            request_id: RequestId("complete-shell".to_owned()),
        },
        session_id: SessionId("session-tty".to_owned()),
        shell_id: ShellId("shell-9".to_owned()),
        status: 130,
        captured_output: Some("interrupted".to_owned()),
    };
    let trusted_completion = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/command")
            .header(AUTHORIZATION, format!("Bearer {}", broker.token))
            .header(CLIENT_HEADER, &broker.client_id.0)
            .body(Full::new(Bytes::from(
                serde_json::to_vec(&completion).expect("completion JSON"),
            )))
            .expect("trusted shell completion"),
    )
    .await;
    assert_eq!(trusted_completion.status(), StatusCode::ACCEPTED);
    assert_eq!(
        engine
            .completions
            .lock()
            .expect("shell completions")
            .as_slice(),
        [(
            SessionId("session-tty".to_owned()),
            ShellId("shell-9".to_owned()),
            130,
            Some("interrupted".to_owned()),
        )]
    );

    let events = unix_request(
        &runtime.paths.socket,
        request_builder(Method::GET, "/v1/events")
            .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
            .header(CLIENT_HEADER, &credentials.client_id.0)
            .header(ACCEPT, "text/event-stream")
            .body(Full::new(Bytes::new()))
            .expect("events request"),
    )
    .await;
    assert_eq!(events.status(), StatusCode::OK);
    let event_body = events
        .into_body()
        .collect()
        .await
        .expect("event body")
        .to_bytes();
    let event_body = String::from_utf8(event_body.to_vec()).expect("UTF-8 SSE");
    assert!(event_body.contains("event: engine"));
    assert!(event_body.contains("\"type\":\"sessions_listed\""));

    shutdown.send(true).expect("stop server");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn accepted_shutdown_host_stops_the_transport_listener() {
    let root = tempdir().expect("runtime root");
    let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
    let engine = Arc::new(StubEngine::default());
    let state = ServerState::new(engine.clone(), &runtime);
    let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve(listener, state, shutdown_rx));
    let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");

    crate::remote::shutdown_authenticated_host(
        &runtime.paths.socket,
        bootstrap.trim(),
        std::time::Duration::from_secs(1),
    )
    .await
    .expect("attached remote shutdown");
    tokio::time::timeout(std::time::Duration::from_secs(1), server)
        .await
        .expect("server must stop after accepted host shutdown")
        .expect("server task")
        .expect("server result");
    assert!(matches!(
        engine
            .received
            .lock()
            .expect("received commands")
            .last()
            .map(|(_, command)| command),
        Some(ClientCommand::ShutdownHost { .. })
    ));
}

#[tokio::test]
async fn replay_cursor_ahead_is_a_typed_rejection_before_sse_success() {
    let root = tempdir().expect("runtime root");
    let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
    let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");
    let engine = Arc::new(StubEngine::default());
    *engine
        .subscription_error
        .lock()
        .expect("subscription error") = Some(EventSubscriptionError::ReplayCursorAhead);
    let state = ServerState::new(engine, &runtime);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve(listener, state, shutdown_rx));

    let connected = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/connect")
            .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
            .body(Full::new(Bytes::new()))
            .expect("connect request"),
    )
    .await;
    let credentials: ClientCredentials = serde_json::from_slice(
        &connected
            .into_body()
            .collect()
            .await
            .expect("connect body")
            .to_bytes(),
    )
    .expect("client credentials");

    let events = unix_request(
        &runtime.paths.socket,
        request_builder(
            Method::GET,
            "/v1/events?session_id=session-ahead&last_seen_sequence=9",
        )
        .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
        .header(CLIENT_HEADER, &credentials.client_id.0)
        .header(ACCEPT, "text/event-stream")
        .body(Full::new(Bytes::new()))
        .expect("events request"),
    )
    .await;
    assert_eq!(events.status(), StatusCode::CONFLICT);
    let body: serde_json::Value = serde_json::from_slice(
        &events
            .into_body()
            .collect()
            .await
            .expect("cursor rejection body")
            .to_bytes(),
    )
    .expect("cursor rejection JSON");
    assert_eq!(
        body,
        serde_json::json!({
            "error": {
                "code": "replay_cursor_ahead",
                "message": "last seen sequence is ahead of the durable log"
            }
        })
    );

    shutdown.send(true).expect("stop server");
    server.await.expect("server join").expect("server result");
}

#[tokio::test]
async fn remote_auth_canaries_never_enter_events_persistence_or_diagnostics() {
    let root = tempdir().expect("runtime root");
    let (runtime, listener) = ServerRuntime::create(root.path()).expect("runtime");
    let bootstrap = fs::read_to_string(&runtime.paths.token).expect("bootstrap token");
    let engine = Arc::new(StubEngine::default());
    let state = ServerState::new(engine, &runtime);
    let initial_diagnostics = format!("{runtime:?} {state:?}");
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(serve(listener, state, shutdown_rx));

    let connected = unix_request(
        &runtime.paths.socket,
        request_builder(Method::POST, "/v1/connect")
            .header(AUTHORIZATION, format!("Bearer {}", bootstrap.trim()))
            .body(Full::new(Bytes::new()))
            .expect("connect request"),
    )
    .await;
    assert_eq!(connected.status(), StatusCode::CREATED);
    let credentials: ClientCredentials = serde_json::from_slice(
        &connected
            .into_body()
            .collect()
            .await
            .expect("connect body")
            .to_bytes(),
    )
    .expect("client credentials");

    let events = unix_request(
        &runtime.paths.socket,
        request_builder(Method::GET, "/v1/events")
            .header(AUTHORIZATION, format!("Bearer {}", credentials.token))
            .header(CLIENT_HEADER, &credentials.client_id.0)
            .header(ACCEPT, "text/event-stream")
            .body(Full::new(Bytes::new()))
            .expect("events request"),
    )
    .await;
    assert_eq!(events.status(), StatusCode::OK);
    let event_body = String::from_utf8(
        events
            .into_body()
            .collect()
            .await
            .expect("event body")
            .to_bytes()
            .to_vec(),
    )
    .expect("UTF-8 SSE");
    let streamed_json = event_body
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("serialized SSE event");
    let streamed_event: EngineEvent =
        serde_json::from_str(streamed_json).expect("streamed EngineEvent");
    let reserialized_streamed =
        serde_json::to_string(&streamed_event).expect("streamed event serializes");

    let durable_event = EngineEvent::TurnStarted {
        meta: EventMeta {
            protocol_version: PROTOCOL_VERSION,
            session_id: SessionId("remote-canary-session".to_owned()),
            sequence_id: SequenceId(0),
            emitted_at: "2026-01-01T00:00:00.000Z".to_owned(),
            caused_by: None,
        },
        turn_id: TurnId("1".to_owned()),
    };
    let serialized_durable =
        serde_json::to_string(&durable_event).expect("durable event serializes");
    let mut log =
        SessionEventLog::open(root.path(), "remote-canary-session").expect("session event log");
    log.append_expected(SequenceId(0), durable_event.clone())
        .expect("persist durable event");
    let persisted = fs::read_to_string(log.path().join("active.jsonl")).expect("active journal");
    let diagnostics = format!("{initial_diagnostics} {streamed_event:?} {durable_event:?}");

    for canary in [bootstrap.trim(), credentials.token.as_str()] {
        for artifact in [
            event_body.as_str(),
            reserialized_streamed.as_str(),
            serialized_durable.as_str(),
            persisted.as_str(),
            diagnostics.as_str(),
        ] {
            assert!(
                !artifact.contains(canary),
                "remote authentication canary entered an event or diagnostic"
            );
        }
    }

    shutdown.send(true).expect("stop server");
    server.await.expect("server join").expect("server result");
}
