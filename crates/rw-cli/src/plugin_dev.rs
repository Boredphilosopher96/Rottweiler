//! Actor-owned live TypeScript plugin attachment.

use std::{
    fs,
    io::Read as _,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{
    Method, Request, StatusCode,
    body::{Bytes, Incoming},
    client::conn::http1 as client_http1,
    header::{AUTHORIZATION, CONTENT_TYPE, HOST},
};
use hyper_util::rt::TokioIo;
use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{ClientCommand, CommandMeta, CommandOutcome, PROTOCOL_VERSION, RequestId, SessionId};
use serde::Deserialize;
use tokio::net::UnixStream;

use crate::server::{CAPABILITY_HEADER, CLIENT_HEADER, ClientCredentials};

const CONTROL_BODY_LIMIT: usize = 64 * 1024;
const MAX_WATCH_BYTES: u64 = 16 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeDescriptor {
    version: u16,
    pid: u32,
    session_id: Option<String>,
    socket: PathBuf,
    token_file: PathBuf,
}

struct DevelopmentClient {
    socket: PathBuf,
    credentials: ClientCredentials,
    session_id: SessionId,
    request: u64,
}

impl DevelopmentClient {
    async fn connect(runtime_root: &Path, selector: &str) -> Result<Self> {
        let descriptor = select_runtime(runtime_root, selector)?;
        let bootstrap = read_private_token(&descriptor.token_file)?;
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/connect")
            .header(HOST, "localhost")
            .header(AUTHORIZATION, format!("Bearer {bootstrap}"))
            .header(CAPABILITY_HEADER, "plugin_development")
            .body(Full::new(Bytes::new()))
            .map_err(|_| miette!("could not build plugin development authentication"))?;
        let response = unix_request(&descriptor.socket, request).await?;
        if response.status() != StatusCode::CREATED {
            return Err(miette!(
                "the local engine rejected plugin development authentication"
            ));
        }
        let credentials = collect_json(response.into_body()).await?;
        Ok(Self {
            socket: descriptor.socket,
            credentials,
            session_id: SessionId(
                descriptor
                    .session_id
                    .ok_or_else(|| miette!("selected engine has no session binding"))?,
            ),
            request: 0,
        })
    }

    async fn attach(&mut self, source: &Path) -> Result<()> {
        self.request = self.request.saturating_add(1);
        self.dispatch(ClientCommand::AttachDevelopmentPlugin {
            meta: self.meta(),
            session_id: self.session_id.clone(),
            source: source.to_string_lossy().into_owned(),
        })
        .await
    }

    async fn detach(&mut self) -> Result<()> {
        self.request = self.request.saturating_add(1);
        self.dispatch(ClientCommand::DetachDevelopmentPlugin {
            meta: self.meta(),
            session_id: self.session_id.clone(),
        })
        .await
    }

    fn meta(&self) -> CommandMeta {
        CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: self.credentials.client_id.clone(),
            request_id: RequestId(format!("plugin-dev-{}", self.request)),
        }
    }

    async fn dispatch(&self, command: ClientCommand) -> Result<()> {
        let body = serde_json::to_vec(&command).into_diagnostic()?;
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/command")
            .header(HOST, "localhost")
            .header(AUTHORIZATION, format!("Bearer {}", self.credentials.token))
            .header(CLIENT_HEADER, &self.credentials.client_id.0)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| miette!("could not build plugin development command"))?;
        let response = unix_request(&self.socket, request).await?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(miette!(
                "the local engine rejected the plugin development command"
            ));
        }
        match collect_json::<rw_core::CommandReply>(response.into_body())
            .await?
            .outcome()
        {
            CommandOutcome::Accepted {} => Ok(()),
            CommandOutcome::Rejected { error } => Err(miette!(
                "plugin development was rejected ({}): {}",
                error.code,
                error.message
            )),
        }
    }
}

/// Attaches one source package, reloads stable edits, and detaches on Ctrl-C.
pub async fn run(path: &Path, session: &str, runtime_root: &Path) -> Result<()> {
    let source = canonical_source(path)?;
    let mut client = DevelopmentClient::connect(runtime_root, session).await?;
    client.attach(&source).await?;
    eprintln!("plugin development attached; press Ctrl-C to detach");
    let mut fingerprint = source_fingerprint(&source)?;
    let mut pending: Option<(blake3::Hash, Instant)> = None;
    let result = loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                break signal.into_diagnostic();
            }
            () = tokio::time::sleep(POLL_INTERVAL) => {
                let current = source_fingerprint(&source)?;
                if current == fingerprint {
                    pending = None;
                    continue;
                }
                match pending {
                    Some((candidate, since)) if candidate == current && since.elapsed() >= DEBOUNCE => {
                        match client.attach(&source).await {
                            Ok(()) => {
                                fingerprint = current;
                                pending = None;
                                eprintln!("plugin development reloaded");
                            }
                            Err(error) => {
                                pending = None;
                                eprintln!("plugin development reload rejected; retaining last good generation: {error}");
                            }
                        }
                    }
                    Some((candidate, _)) if candidate == current => {}
                    _ => pending = Some((current, Instant::now())),
                }
            }
        }
    };
    let detach = client.detach().await;
    result?;
    detach?;
    eprintln!("plugin development detached");
    Ok(())
}

fn select_runtime(root: &Path, selector: &str) -> Result<RuntimeDescriptor> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let descriptor_path = entry.path().join("runtime.json");
        let Ok(bytes) = read_private_file(&descriptor_path, CONTROL_BODY_LIMIT as u64) else {
            continue;
        };
        let Ok(descriptor) = serde_json::from_slice::<RuntimeDescriptor>(&bytes) else {
            continue;
        };
        if descriptor.version != 1
            || descriptor.pid == 0
            || !descriptor.socket.is_absolute()
            || !descriptor.token_file.is_absolute()
            || descriptor
                .session_id
                .as_deref()
                .is_none_or(|session| selector != "current" && selector != session)
            || std::os::unix::net::UnixStream::connect(&descriptor.socket).is_err()
        {
            continue;
        }
        candidates.push(descriptor);
    }
    if candidates.len() != 1 {
        return Err(miette!(
            "plugin development requires exactly one live engine matching --session {selector:?}"
        ));
    }
    candidates
        .pop()
        .ok_or_else(|| miette!("live engine selection failed"))
}

fn canonical_source(path: &Path) -> Result<PathBuf> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(miette!("plugin development source cannot be a symlink"));
    }
    let path = fs::canonicalize(path).into_diagnostic()?;
    if !path.is_dir() {
        return Err(miette!(
            "plugin development source must be a package directory"
        ));
    }
    Ok(path)
}

fn source_fingerprint(root: &Path) -> Result<blake3::Hash> {
    let mut files = vec![
        root.join("manifest.json"),
        root.join("package.json"),
        root.join("bun.lock"),
    ];
    collect_source_files(&root.join("src"), &mut files, 0)?;
    files.sort();
    files.dedup();
    let mut total = 0_u64;
    let mut hasher = blake3::Hasher::new();
    for path in files {
        let bytes = read_private_file(&path, MAX_WATCH_BYTES)?;
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| miette!("plugin development source size overflowed"))?;
        if total > MAX_WATCH_BYTES {
            return Err(miette!("plugin development source exceeds 16 MiB"));
        }
        hasher.update(
            path.strip_prefix(root)
                .into_diagnostic()?
                .as_os_str()
                .as_encoded_bytes(),
        );
        hasher.update(b"\0");
        hasher.update(&bytes);
    }
    Ok(hasher.finalize())
}

fn collect_source_files(current: &Path, files: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err(miette!(
            "plugin development source exceeds 64 directory levels"
        ));
    }
    for entry in fs::read_dir(current).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let metadata = fs::symlink_metadata(entry.path()).into_diagnostic()?;
        if metadata.file_type().is_symlink() {
            return Err(miette!("plugin development source contains a symlink"));
        }
        if metadata.is_dir() {
            collect_source_files(&entry.path(), files, depth + 1)?;
        } else if metadata.is_file() {
            files.push(entry.path());
        } else {
            return Err(miette!("plugin development source contains a special file"));
        }
    }
    Ok(())
}

fn read_private_token(path: &Path) -> Result<String> {
    let bytes = read_private_file(path, 128)?;
    let token = String::from_utf8(bytes).into_diagnostic()?;
    let token = token.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(miette!("engine bootstrap token is invalid"));
    }
    Ok(token.to_owned())
}

fn read_private_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).into_diagnostic()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.len() > limit
    {
        return Err(miette!(
            "plugin development input is not an owner-controlled regular file"
        ));
    }
    let file = {
        use rustix::fs::{Mode, OFlags};
        let descriptor = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| miette!("plugin development input could not open safely: {error}"))?;
        fs::File::from(descriptor)
    };
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    if bytes.len() as u64 > limit {
        return Err(miette!("plugin development input exceeds its byte limit"));
    }
    Ok(bytes)
}

async fn unix_request(
    socket: &Path,
    request: Request<Full<Bytes>>,
) -> Result<hyper::Response<Incoming>> {
    let stream = UnixStream::connect(socket).await.into_diagnostic()?;
    let (mut sender, connection) = client_http1::handshake(TokioIo::new(stream))
        .await
        .into_diagnostic()?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender.send_request(request).await.into_diagnostic()
}

async fn collect_json<T: serde::de::DeserializeOwned>(body: Incoming) -> Result<T> {
    let bytes = Limited::new(body, CONTROL_BODY_LIMIT)
        .collect()
        .await
        .map_err(|_| miette!("plugin development response exceeded its byte limit"))?
        .to_bytes();
    serde_json::from_slice(&bytes)
        .map_err(|_| miette!("plugin development response contained invalid JSON"))
}
