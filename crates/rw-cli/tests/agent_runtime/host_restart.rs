//! A lost mutation body is retried against a different, fully joined engine process.
use super::*;
use rw_resources::process::BlockingProcess;
use rw_types::{ClientCommand, ClientId, CommandMeta, CommandOutcome, CommandReply, RequestId};
use std::io::Read as _;
use std::os::unix::net::UnixStream;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
const REPLY_LIMIT: usize = 128 * 1024;

#[derive(serde::Deserialize)]
struct Credentials {
    client_id: ClientId,
    token: String,
}

struct NativeHost {
    process: BlockingProcess,
    socket: PathBuf,
    credentials: Credentials,
    reads: u32,
}

fn exchange(
    socket: &Path,
    method: &str,
    path: &str,
    token: &str,
    client: Option<(&ClientId, &str)>,
    body: &[u8],
) -> TestResult<(UnixStream, u16, usize)> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    )?;
    if let Some((client, lane)) = client {
        write!(
            stream,
            "x-rottweiler-client: {}\r\nx-rottweiler-command-lane: {lane}\r\n",
            client.0
        )?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(body)?;
    // Read exactly the header. The mutation test deliberately abandons its body.
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        assert!(header.len() < 8192, "HTTP header exceeded fixture cap");
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    let header = std::str::from_utf8(&header)?;
    let status = header
        .split_whitespace()
        .nth(1)
        .ok_or("HTTP status missing")?
        .parse()?;
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>())
        })
        .ok_or("bounded response length missing")??;
    assert!(length <= REPLY_LIMIT, "HTTP reply exceeded fixture cap");
    Ok((stream, status, length))
}

fn read_body(mut stream: UnixStream, length: usize) -> TestResult<Vec<u8>> {
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

impl NativeHost {
    fn start(
        root: &Path,
        workspace: &Path,
        home: &Path,
        phase: &str,
        provider_delay_ms: u64,
    ) -> TestResult<Self> {
        let runtime = private_test_directory(&root.join(phase));
        let socket = runtime.join("engine.sock");
        let token_file = runtime.join("token");
        let diagnostics = fs::File::create(root.join(format!("{phase}.log")))?;
        let mut command = base_command(workspace, home);
        command
            .args(["serve", "--session", "restart-bootstrap"])
            .arg("--workspace")
            .arg(workspace)
            .arg("--socket")
            .arg(&socket)
            .arg("--token-file")
            .arg(&token_file)
            .arg("--in-memory-replay-script")
            .arg(root.join("provider.json"))
            .arg("--record-script-delay-ms")
            .arg(provider_delay_ms.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(diagnostics);
        let process = BlockingProcess::spawn(&mut command)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let bootstrap = loop {
            assert!(
                process.try_status()?.is_none(),
                "engine exited before readiness: {}",
                fs::read_to_string(root.join(format!("{phase}.log")))?
            );
            if socket.exists()
                && let Ok(token) = fs::read_to_string(&token_file)
                && let Ok((stream, 200, length)) =
                    exchange(&socket, "GET", "/v1/health", token.trim(), None, b"")
            {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&read_body(stream, length)?)?,
                    json!({"ready": true})
                );
                break token;
            }
            assert!(Instant::now() < deadline, "engine did not become ready");
            thread::sleep(Duration::from_millis(10));
        };
        let (stream, status, length) =
            exchange(&socket, "POST", "/v1/connect", bootstrap.trim(), None, b"")?;
        assert_eq!(status, 201);
        let credentials = serde_json::from_slice(&read_body(stream, length)?)?;
        Ok(Self {
            process,
            socket,
            credentials,
            reads: 0,
        })
    }

    fn meta(&self, request: &str) -> CommandMeta {
        CommandMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            client_id: self.credentials.client_id.clone(),
            request_id: RequestId(request.into()),
        }
    }

    fn reconnect(&mut self) -> TestResult {
        let bootstrap = fs::read_to_string(self.socket.with_file_name("token"))?;
        let (stream, status, length) = exchange(
            &self.socket,
            "POST",
            "/v1/connect",
            bootstrap.trim(),
            None,
            b"",
        )?;
        assert_eq!(status, 201);
        self.credentials = serde_json::from_slice(&read_body(stream, length)?)?;
        Ok(())
    }

    fn send(&self, command: &ClientCommand) -> TestResult<(UnixStream, u16, usize)> {
        exchange(
            &self.socket,
            "POST",
            "/v1/command",
            &self.credentials.token,
            Some((
                &self.credentials.client_id,
                if command.is_urgent() {
                    "urgent"
                } else {
                    "normal"
                },
            )),
            &serde_json::to_vec(command)?,
        )
    }

    fn dispatch(&self, command: &ClientCommand) -> TestResult<CommandReply> {
        let (stream, status, length) = self.send(command)?;
        assert_eq!(status, 202);
        Ok(serde_json::from_slice(&read_body(stream, length)?)?)
    }

    fn sessions(&mut self) -> TestResult<Vec<String>> {
        self.reads += 1;
        let reply = self.dispatch(&ClientCommand::ListSessions {
            meta: self.meta(&format!("list-{}", self.reads)),
        })?;
        assert_eq!(reply.outcome(), &CommandOutcome::Accepted {});
        let CommandReply::Read { events, .. } = reply else {
            return Err("read reply missing".into());
        };
        let mut ids = events
            .into_iter()
            .find_map(|event| {
                if let EngineEvent::SessionsListed { sessions, .. } = event {
                    Some(
                        sessions
                            .into_iter()
                            .map(|session| session.session_id.0)
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                }
            })
            .ok_or("session catalog missing")?;
        ids.sort();
        Ok(ids)
    }

    fn bootstrap_catalog(&mut self) -> TestResult<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let ids = self.sessions()?;
            if ids.iter().any(|id| id == "restart-bootstrap") {
                return Ok(ids);
            }
            assert!(
                self.process.try_status()?.is_none(),
                "engine exited before its bootstrap session became discoverable"
            );
            assert!(
                Instant::now() < deadline,
                "bootstrap catalog did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn durable_command_retry_after_native_engine_restart_does_not_create_another_session() -> TestResult
{
    let root = tempfile::Builder::new()
        .prefix("rw-retry-")
        .tempdir_in("/tmp")?
        .keep();
    eprintln!("native restart fixture evidence: {}", root.display());
    let workspace = private_test_directory(&root.join("workspace"));
    let home = private_test_directory(&root.join("home"));
    fs::write(
        root.join("provider.json"),
        r#"[[{"type":"text_delta","text":"unused"},{"type":"finished","reason":"stop"}]]"#,
    )?;
    let mut first = NativeHost::start(&root, &workspace, &home, "first", 0)?;
    let initial = first.bootstrap_catalog()?;
    let command = ClientCommand::CreateSession {
        meta: first.meta("durable-create"),
        cwd: workspace.to_str().ok_or("workspace UTF-8")?.into(),
        model: None,
    };
    let (unread_reply, status, length) = first.send(&command)?;
    assert_eq!(status, 202);
    assert!(length > 0);
    drop(unread_reply); // No application consumed the command result body.
    let created = first.sessions()?;
    assert_eq!(created.len(), initial.len() + 1);
    let old_client = first.credentials.client_id.clone();
    first.process.settle(); // Kill/reap the exact engine before reacquiring its storage.
    let mut restarted = NativeHost::start(&root, &workspace, &home, "restarted", 0)?;
    assert_eq!(restarted.bootstrap_catalog()?, created);
    assert_ne!(restarted.credentials.client_id, old_client);
    let mut retry = command;
    *retry.meta_mut() = restarted.meta("durable-create");
    assert_eq!(
        restarted.dispatch(&retry)?.outcome(),
        &CommandOutcome::Accepted {}
    );
    assert_eq!(restarted.sessions()?, created);
    restarted.reconnect()?; // Changed payload must reach the durable identity, not a client cache.
    *retry.meta_mut() = restarted.meta("durable-create");
    if let ClientCommand::CreateSession { model, .. } = &mut retry {
        *model = Some(rw_types::ModelAlias("changed-identity".into()));
    }
    let rejected = restarted.dispatch(&retry)?;
    let CommandOutcome::Rejected { error } = rejected.outcome() else {
        return Err("changed durable command was accepted".into());
    };
    assert_eq!(error.code, "host_protocol_failure");
    assert!(error.message.contains("operation identity was reused"));
    assert_eq!(restarted.sessions()?, created);
    restarted.process.settle();
    fs::remove_dir_all(root)?;
    Ok(())
}

// Socket clients use the same actor commands and canonical recovery receipts as
// print-mode cancellation/recovery; neither flow interprets terminal input here.
const BOOTSTRAP_SESSION: &str = "restart-bootstrap";

fn bootstrap_session() -> rw_types::SessionId {
    rw_types::SessionId(BOOTSTRAP_SESSION.into())
}

impl NativeHost {
    fn take_driver(&self, request: &str) -> TestResult {
        let reply = self.dispatch(&ClientCommand::TakeDriver {
            meta: self.meta(request),
            session_id: bootstrap_session(),
        })?;
        assert_eq!(reply.outcome(), &CommandOutcome::Accepted {});
        Ok(())
    }

    fn message(&self, content: &str) -> TestResult {
        let reply = self.dispatch(&ClientCommand::SendMessage {
            meta: self.meta(content),
            session_id: bootstrap_session(),
            content: content.into(),
            attachments: Vec::new(),
        })?;
        assert_eq!(reply.outcome(), &CommandOutcome::Accepted {});
        Ok(())
    }

    fn wait_journal<T>(
        &self,
        home: &Path,
        observe: impl Fn(&[EngineEvent]) -> Option<T>,
    ) -> TestResult<T> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(value) = observe(&socket_journal(home)?) {
                return Ok(value);
            }
            assert!(
                self.process.try_status()?.is_none(),
                "socket engine exited during observation"
            );
            assert!(
                Instant::now() < deadline,
                "socket journal observation timed out"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn socket_journal(home: &Path) -> TestResult<Vec<EngineEvent>> {
    const LIMIT: u64 = 1024 * 1024;
    let path = home
        .join("sessions")
        .join(BOOTSTRAP_SESSION)
        .join("journal/active.jsonl");
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    assert!(
        bytes.len() <= usize::try_from(LIMIT)?,
        "fixture journal exceeded byte cap"
    );
    let mut events = Vec::new();
    // A concurrently appended final line is invisible until it is complete.
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        assert!(events.len() < 512, "fixture journal exceeded event cap");
        let envelope: rw_store::session::EventEnvelope<EngineEvent> = serde_json::from_slice(line)?;
        events.push(envelope.event);
    }
    Ok(events)
}

fn started_for(events: &[EngineEvent], content: &str) -> Option<rw_types::TurnId> {
    let turn = events.iter().find_map(|event| match event {
        EngineEvent::UserMessageAccepted {
            agent_turn,
            content: accepted,
            ..
        } if accepted == content => Some(rw_types::TurnId(agent_turn.to_string())),
        _ => None,
    })?;
    events
        .iter()
        .any(|event| matches!(event, EngineEvent::TurnStarted { turn_id, .. } if turn_id == &turn))
        .then_some(turn)
}

fn terminal_status(events: &[EngineEvent], turn: &rw_types::TurnId) -> Option<TurnStatus> {
    let mut statuses = events.iter().filter_map(|event| match event {
        EngineEvent::TurnFinished {
            turn_id, status, ..
        } if turn_id == turn => Some(status.clone()),
        _ => None,
    });
    let status = statuses.next();
    assert!(
        statuses.next().is_none(),
        "turn has duplicate terminal receipts"
    );
    status
}

#[test]
fn socket_interrupt_and_process_recovery_preserve_exact_turn_receipts() -> TestResult {
    let root = tempfile::Builder::new()
        .prefix("rw-socket-recovery-")
        .tempdir_in("/tmp")?
        .keep();
    eprintln!("socket conformance fixture evidence: {}", root.display());
    let workspace = private_test_directory(&root.join("workspace"));
    let home = private_test_directory(&root.join("home"));
    write_script(
        &root.join("provider.json"),
        vec![text_events("delayed"), text_events("delayed")],
    );
    let mut first = NativeHost::start(&root, &workspace, &home, "first", 30_000)?;
    first.bootstrap_catalog()?;
    first.take_driver("initial-driver")?;
    first.message("cancel-over-socket")?;
    let cancelled =
        first.wait_journal(&home, |events| started_for(events, "cancel-over-socket"))?;
    let reply = first.dispatch(&ClientCommand::Interrupt {
        meta: first.meta("interrupt-over-socket"),
        session_id: bootstrap_session(),
    })?;
    assert_eq!(reply.outcome(), &CommandOutcome::Accepted {});
    assert_eq!(
        first.wait_journal(&home, |events| terminal_status(events, &cancelled))?,
        TurnStatus::Interrupted
    );

    first.message("recover-over-socket")?;
    let unfinished =
        first.wait_journal(&home, |events| started_for(events, "recover-over-socket"))?;
    assert_ne!(cancelled, unfinished);
    assert!(terminal_status(&socket_journal(&home)?, &unfinished).is_none());
    first.process.settle();
    assert!(terminal_status(&socket_journal(&home)?, &unfinished).is_none());

    write_script(
        &root.join("provider.json"),
        text_script("socket-recovery-complete"),
    );
    let mut restarted = NativeHost::start(&root, &workspace, &home, "restarted", 0)?;
    restarted.bootstrap_catalog()?;
    restarted.take_driver("recovered-driver")?;
    assert_eq!(
        restarted.wait_journal(&home, |events| terminal_status(events, &unfinished))?,
        TurnStatus::Interrupted
    );
    restarted.message("complete-after-recovery")?;
    let completed = restarted.wait_journal(&home, |events| {
        started_for(events, "complete-after-recovery")
    })?;
    assert_eq!(
        restarted.wait_journal(&home, |events| terminal_status(events, &completed))?,
        TurnStatus::Completed
    );
    restarted.process.settle();
    let events = socket_journal(&home)?;
    assert_eq!(
        terminal_status(&events, &cancelled),
        Some(TurnStatus::Interrupted)
    );
    assert_eq!(
        terminal_status(&events, &unfinished),
        Some(TurnStatus::Interrupted)
    );
    assert_eq!(
        terminal_status(&events, &completed),
        Some(TurnStatus::Completed)
    );
    assert!(events.iter().any(|event| matches!(event, EngineEvent::TextDelta { turn_id, text, .. } if turn_id == &completed && text == "socket-recovery-complete")));
    fs::remove_dir_all(root)?;
    Ok(())
}
