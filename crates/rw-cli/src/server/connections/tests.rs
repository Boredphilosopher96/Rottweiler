#![allow(clippy::expect_used)]
use super::*;
use std::{future::Future, io, time::Duration};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::UnixStream,
    sync::Semaphore,
};

struct EffectEngine {
    entered: Notify,
    release: Semaphore,
    completed: AtomicBool,
    panic_on_release: AtomicBool,
    retired: Arc<AtomicBool>,
}
impl Default for EffectEngine {
    fn default() -> Self {
        Self {
            entered: Notify::new(),
            release: Semaphore::new(0),
            completed: AtomicBool::new(false),
            panic_on_release: AtomicBool::new(false),
            retired: Arc::new(AtomicBool::new(false)),
        }
    }
}
struct EffectRetirement(Arc<AtomicBool>);
impl Drop for EffectRetirement {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl EffectEngine {
    async fn execute(&self) {
        let _retirement = EffectRetirement(Arc::clone(&self.retired));
        self.entered.notify_one();
        self.release
            .acquire()
            .await
            .expect("effect release")
            .forget();
        assert!(
            !self.panic_on_release.load(Ordering::SeqCst),
            "fixture handler panic"
        );
        self.completed.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl ServerEngine for EffectEngine {
    async fn dispatch(
        &self,
        _client: ClientId,
        _command: ClientCommand,
    ) -> std::result::Result<rw_core::HostReply, String> {
        self.execute().await;
        Ok(rw_core::HostReply::command(CommandOutcome::Accepted {}))
    }
    async fn submit_provider_api_key(
        &self,
        _client: ClientId,
        _session: SessionId,
        _provider: String,
        _key: ProviderApiKey,
    ) -> std::result::Result<ProviderApiKeySubmission, String> {
        self.execute().await;
        Ok(ProviderApiKeySubmission {
            stored: true,
            activated: true,
            warnings: vec![],
        })
    }
    async fn subscribe(
        &self,
        _client: ClientId,
        _session: Option<SessionId>,
        _sequence: Option<SequenceId>,
    ) -> std::result::Result<
        mpsc::Receiver<std::result::Result<rw_core::HostEvent, String>>,
        EventSubscriptionError,
    > {
        Err(EventSubscriptionError::Other("unused subscription".into()))
    }
    async fn complete_shell(
        &self,
        _session: SessionId,
        _shell: ShellId,
        _status: i32,
        _output: Option<String>,
    ) -> std::result::Result<(), String> {
        Err("unused shell".into())
    }
    async fn activate_provider(
        &self,
        _client: ClientId,
        _session: SessionId,
        _provider: String,
    ) -> std::result::Result<(), String> {
        Err("unused activation".into())
    }
}

#[derive(Clone, Copy)]
enum Effect {
    Credential,
    Command,
}
struct Fixture {
    _root: tempfile::TempDir,
    runtime: ServerRuntime,
    state: ServerState,
    engine: Arc<EffectEngine>,
    shutdown: tokio::sync::watch::Sender<bool>,
    server: tokio::task::JoinHandle<Result<()>>,
}
impl Fixture {
    fn start() -> Self {
        let root = tempfile::tempdir().expect("runtime");
        let (runtime, listener) = ServerRuntime::create(root.path()).expect("listener");
        let engine = Arc::new(EffectEngine::default());
        let mut state = ServerState::new(engine.clone(), &runtime);
        state.connections = Arc::new(Semaphore::new(1));
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(listener, state.clone(), receiver));
        Self {
            _root: root,
            runtime,
            state,
            engine,
            shutdown,
            server,
        }
    }
    async fn request(&self, effect: Effect) -> UnixStream {
        let credentials = self
            .state
            .clients
            .mint(ClientCapability::Interactive)
            .expect("bound authority");
        let (path, body) = match effect {
            Effect::Credential => (
                "/v1/provider-api-key",
                serde_json::to_vec(&serde_json::json!({
                    "session_id":"session", "provider":"fixture", "api_key":"test-credential"
                }))
                .expect("credential request"),
            ),
            Effect::Command => (
                "/v1/command",
                serde_json::to_vec(&ClientCommand::ListSessions {
                    meta: rw_types::CommandMeta {
                        protocol_version: rw_core::PROTOCOL_VERSION,
                        client_id: credentials.client_id.clone(),
                        request_id: rw_types::RequestId("blocked".into()),
                    },
                })
                .expect("command"),
            ),
        };
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\n{CLIENT_HEADER}: {}\r\n{COMMAND_LANE_HEADER}: normal\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            credentials.token,
            credentials.client_id.0,
            body.len()
        );
        let mut stream = UnixStream::connect(&self.runtime.paths.socket)
            .await
            .expect("transport");
        stream.write_all(request.as_bytes()).await.expect("headers");
        stream.write_all(&body).await.expect("body");
        bounded(self.engine.entered.notified()).await;
        stream
    }
    fn assert_effect_retained(&self, effect: Effect) {
        assert!(!self.engine.completed.load(Ordering::SeqCst));
        assert!(!self.engine.retired.load(Ordering::SeqCst));
        assert_eq!(self.state.connections.available_permits(), 0);
        if matches!(effect, Effect::Credential) {
            assert!(
                self.state
                    .provider_api_key_attempts
                    .lock()
                    .expect("attempts")
                    .contains("fixture")
            );
        }
    }
    async fn release(&self) {
        self.engine.release.add_permits(1);
        bounded(async {
            while self.state.connections.available_permits() != 1
                || Arc::strong_count(&self.engine) != 2
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(self.engine.completed.load(Ordering::SeqCst));
        assert!(self.engine.retired.load(Ordering::SeqCst));
        assert!(
            self.state
                .provider_api_key_attempts
                .lock()
                .expect("attempts")
                .is_empty()
        );
    }
}
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("lifecycle deadline")
}
async fn transport_closed(stream: &mut UnixStream) {
    let mut byte = [0];
    match bounded(stream.read(&mut byte)).await {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
            ) => {}
        result => panic!("transport was not closed: {result:?}"),
    }
}

#[tokio::test]
async fn shutdown_and_waiter_loss_close_transport_but_settle_credential_and_command_effects() {
    for caller_loss in [false, true] {
        for effect in [Effect::Credential, Effect::Command] {
            let fixture = Fixture::start();
            let mut stream = fixture.request(effect).await;
            if caller_loss {
                fixture.server.abort();
            } else {
                fixture.shutdown.send(true).expect("stop");
            }
            transport_closed(&mut stream).await;
            fixture.assert_effect_retained(effect);
            if !caller_loss {
                assert!(!fixture.server.is_finished());
            }
            fixture.release().await;
            let joined = bounded(fixture.server).await;
            if caller_loss {
                assert!(joined.expect_err("waiter cancelled").is_cancelled());
            } else {
                joined.expect("server owner").expect("effects settled");
            }
        }
    }
}

#[tokio::test]
async fn panicked_handler_retires_admission_and_reports_shutdown_failure() {
    let fixture = Fixture::start();
    fixture
        .engine
        .panic_on_release
        .store(true, Ordering::SeqCst);
    let mut stream = fixture.request(Effect::Credential).await;
    fixture.shutdown.send(true).expect("stop");
    transport_closed(&mut stream).await;
    fixture.assert_effect_retained(Effect::Credential);
    fixture.engine.release.add_permits(1);
    let error = bounded(fixture.server)
        .await
        .expect("server owner")
        .expect_err("handler panic is reported");
    assert!(error.to_string().contains("engine request task failed"));
    assert!(fixture.engine.retired.load(Ordering::SeqCst));
    assert!(!fixture.engine.completed.load(Ordering::SeqCst));
    assert_eq!(fixture.state.connections.available_permits(), 1);
    assert!(
        fixture
            .state
            .provider_api_key_attempts
            .lock()
            .expect("attempts")
            .is_empty()
    );
}

#[tokio::test]
async fn disconnected_blocked_effect_keeps_connection_admission_across_reconnects() {
    let fixture = Fixture::start();
    let stream = fixture.request(Effect::Command).await;
    drop(stream);
    for _ in 0..4 {
        let mut replacement = UnixStream::connect(&fixture.runtime.paths.socket)
            .await
            .expect("replacement transport");
        transport_closed(&mut replacement).await;
        fixture.assert_effect_retained(Effect::Command);
    }
    fixture.shutdown.send(true).expect("stop");
    fixture.release().await;
    bounded(fixture.server)
        .await
        .expect("server owner")
        .expect("settled");
}

#[tokio::test]
async fn dropping_idle_sse_receiver_releases_source_without_another_event() {
    let (source, receive) = mpsc::channel(1);
    let forwarded = forward_events(receive);
    drop(forwarded);
    bounded(source.closed()).await;
}

#[tokio::test]
async fn dropping_full_sse_receiver_retires_a_forwarder_waiting_to_send() {
    let (source, receive) = mpsc::channel(1);
    let forwarded = forward_events(receive);
    for _ in 0..HOST_EVENT_FORWARD_CAPACITY + 2 {
        bounded(source.send(Err(rw_core::HostError::Protocol("bounded event".into()))))
            .await
            .expect("forwarded or queued");
    }
    assert_eq!(forwarded.len(), HOST_EVENT_FORWARD_CAPACITY);
    drop(forwarded);
    bounded(source.closed()).await;
}
