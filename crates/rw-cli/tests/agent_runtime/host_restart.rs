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
    client: Option<&ClientId>,
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
    if let Some(client) = client {
        write!(
            stream,
            "x-rottweiler-client: {}\r\nx-rottweiler-command-lane: normal\r\n",
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
    fn start(root: &Path, workspace: &Path, home: &Path, phase: &str) -> TestResult<Self> {
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
            Some(&self.credentials.client_id),
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
    let mut first = NativeHost::start(&root, &workspace, &home, "first")?;
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
    let mut restarted = NativeHost::start(&root, &workspace, &home, "restarted")?;
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
