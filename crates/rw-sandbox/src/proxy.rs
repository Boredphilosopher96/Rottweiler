use base64::Engine as _;
use rustls::pki_types::ServerName;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs as _};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use url::Url;

#[cfg(target_os = "linux")]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use crate::{EgressDecision, EgressPolicy, SandboxError};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_PLAIN_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CLIENT_HELLO_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 64;
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Explicit corporate proxy selected after target-domain and SSRF policy.
/// Authentication is retained only in a redacted, non-serializable boundary.
#[derive(Clone)]
pub struct UpstreamProxy {
    url: Url,
    addresses: Arc<[SocketAddr]>,
    authorization: Option<String>,
}

/// One caller-validated DNS answer set consumed without re-resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressPin {
    host: String,
    port: u16,
    addresses: Vec<SocketAddr>,
}

impl EgressPin {
    /// Creates an exact host/port pin. Policy is still re-evaluated by the
    /// supervisor before this answer set is used.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid host, empty set, zero port, or address
    /// whose port differs from the approved target port.
    pub fn new(host: &str, port: u16, addresses: Vec<SocketAddr>) -> Result<Self, SandboxError> {
        let host = normalize_host(host).ok_or(SandboxError::InvalidEgressPin)?;
        if port == 0
            || addresses.is_empty()
            || addresses.iter().any(|address| address.port() != port)
        {
            return Err(SandboxError::InvalidEgressPin);
        }
        Ok(Self {
            host,
            port,
            addresses,
        })
    }
}

impl std::fmt::Debug for UpstreamProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamProxy")
            .field("url", &sanitized_proxy_url(&self.url))
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "[REDACTED]"),
            )
            .finish_non_exhaustive()
    }
}

impl UpstreamProxy {
    /// Validates an HTTP(S) proxy URL without inline credentials.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported schemes, credentials, or missing hosts.
    pub fn new(url: Url) -> Result<Self, SandboxError> {
        Self::new_with_resolver(url, |host, port| {
            (host, port)
                .to_socket_addrs()
                .map(Iterator::collect::<Vec<_>>)
                .map_err(|_| SandboxError::InvalidProxy)
        })
    }

    fn new_with_resolver(
        url: Url,
        resolver: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, SandboxError>,
    ) -> Result<Self, SandboxError> {
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(SandboxError::InvalidProxy);
        }
        let host = url.host_str().ok_or(SandboxError::InvalidProxy)?;
        let port = url
            .port_or_known_default()
            .ok_or(SandboxError::InvalidProxy)?;
        let addresses = resolver(host, port)?;
        if addresses.is_empty() {
            return Err(SandboxError::InvalidProxy);
        }
        Ok(Self {
            url,
            addresses: addresses.into(),
            authorization: None,
        })
    }

    /// Adds HTTP Basic credentials without embedding them in the proxy URL.
    #[must_use]
    pub fn with_basic_auth(mut self, username: &str, password: &str) -> Self {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{username}:{password}").as_bytes());
        self.authorization = Some(format!("Basic {encoded}"));
        self
    }
}

fn sanitized_proxy_url(url: &Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or("[invalid]"),
        url.port_or_known_default().unwrap_or_default()
    )
}

/// A host-side HTTP CONNECT proxy whose listener lifetime is supervised by the
/// owning engine.  Every upstream socket passes through [`EgressPolicy`]
/// immediately after DNS resolution and connects to one of those pinned
/// addresses. CONNECT waits for a bounded TLS `ClientHello` and requires its
/// plaintext SNI to equal the approved authority before opening the upstream
/// socket. Missing, mismatched, malformed, or ECH-only names fail closed.
pub struct SupervisedEgressProxy {
    address: SocketAddr,
    policy: Arc<Mutex<EgressPolicy>>,
    running: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    denials: Arc<AtomicUsize>,
    worker: Mutex<Option<JoinHandle<()>>>,
    connection_workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    clients: Arc<Mutex<BTreeMap<usize, TcpStream>>>,
    #[cfg(target_os = "linux")]
    relay_clients: Arc<Mutex<BTreeMap<usize, UnixStream>>>,
    #[cfg(target_os = "linux")]
    relay_path: PathBuf,
    #[cfg(target_os = "linux")]
    relay_worker: Mutex<Option<JoinHandle<()>>>,
    #[cfg(target_os = "linux")]
    _relay_directory: tempfile::TempDir,
}

/// Instance-scoped shutdown observation that cannot be confused by operating
/// system port reuse after the listener closes.
#[derive(Clone, Debug)]
pub struct ProxyLifecycle {
    stopped: Arc<AtomicBool>,
}

/// Cloneable monotonic observation of policy-denied egress attempts.
#[derive(Clone, Debug)]
pub struct ProxyDenials {
    denials: Arc<AtomicUsize>,
}

impl ProxyDenials {
    /// Number of requests rejected by the domain/private-address policy.
    #[must_use]
    pub fn count(&self) -> usize {
        self.denials.load(Ordering::Acquire)
    }
}

impl ProxyLifecycle {
    /// True only after every listener and accepted connection worker has
    /// exited and joined.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for SupervisedEgressProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SupervisedEgressProxy")
            .field("address", &self.address)
            .field("running", &self.running.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl SupervisedEgressProxy {
    /// Binds a loopback-only proxy and starts its supervisor thread.
    ///
    /// # Errors
    ///
    /// Returns an error if a private loopback listener cannot be created.
    pub fn start(policy: EgressPolicy) -> Result<Self, SandboxError> {
        Self::start_with_upstream(policy, None)
    }

    /// Starts a policy proxy that chains allowed traffic through an explicit
    /// corporate proxy after target validation.
    ///
    /// # Errors
    ///
    /// Returns an error if a private listener cannot be created.
    pub fn start_with_upstream(
        policy: EgressPolicy,
        upstream_proxy: Option<UpstreamProxy>,
    ) -> Result<Self, SandboxError> {
        Self::start_with_upstream_and_pins(policy, upstream_proxy, Vec::new())
    }

    /// Starts a chained proxy with exact caller-validated DNS answer sets.
    ///
    /// # Errors
    ///
    /// Returns an error if a private listener cannot be created.
    #[allow(clippy::too_many_lines)]
    pub fn start_with_upstream_and_pins(
        policy: EgressPolicy,
        upstream_proxy: Option<UpstreamProxy>,
        pins: Vec<EgressPin>,
    ) -> Result<Self, SandboxError> {
        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(SandboxError::Proxy)?;
        listener
            .set_nonblocking(true)
            .map_err(SandboxError::Proxy)?;
        let address = listener.local_addr().map_err(SandboxError::Proxy)?;
        #[cfg(target_os = "linux")]
        let (relay_directory, relay_path, relay_listener) = {
            let directory = tempfile::Builder::new()
                .prefix("rottweiler-egress-")
                .tempdir()
                .map_err(SandboxError::Proxy)?;
            let path = directory.path().join("relay.sock");
            let listener = UnixListener::bind(&path).map_err(SandboxError::Proxy)?;
            listener
                .set_nonblocking(true)
                .map_err(SandboxError::Proxy)?;
            (directory, path, listener)
        };
        let running = Arc::new(AtomicBool::new(true));
        let stopped = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let denials = Arc::new(AtomicUsize::new(0));
        let next_connection = Arc::new(AtomicUsize::new(0));
        let connection_workers = Arc::new(Mutex::new(Vec::new()));
        let clients = Arc::new(Mutex::new(BTreeMap::new()));
        #[cfg(target_os = "linux")]
        let relay_clients = Arc::new(Mutex::new(BTreeMap::new()));
        let worker_running = Arc::clone(&running);
        let policy = Arc::new(Mutex::new(policy));
        let worker_policy = Arc::clone(&policy);
        let upstream_proxy = upstream_proxy.map(Arc::new);
        let worker_upstream_proxy = upstream_proxy.clone();
        let pins = Arc::new(
            pins.into_iter()
                .map(|pin| ((pin.host, pin.port), pin.addresses))
                .collect::<BTreeMap<_, _>>(),
        );
        let worker_pins = Arc::clone(&pins);
        let worker_connections = Arc::clone(&connection_workers);
        let worker_clients = Arc::clone(&clients);
        let worker_next_connection = Arc::clone(&next_connection);
        let worker_denials = Arc::clone(&denials);
        let worker = thread::Builder::new()
            .name("rottweiler-egress-proxy".to_owned())
            .spawn(move || {
                serve(
                    &listener,
                    &worker_policy,
                    worker_upstream_proxy.as_deref(),
                    &worker_pins,
                    &worker_running,
                    &active,
                    &worker_next_connection,
                    &worker_denials,
                    &worker_connections,
                    &worker_clients,
                );
            })
            .map_err(SandboxError::Proxy)?;
        #[cfg(target_os = "linux")]
        let relay_worker = {
            let relay_running = Arc::clone(&running);
            match thread::Builder::new()
                .name("rottweiler-egress-host-relay".to_owned())
                .spawn({
                    let connection_workers = Arc::clone(&connection_workers);
                    let relay_clients = Arc::clone(&relay_clients);
                    let next_connection = Arc::clone(&next_connection);
                    move || {
                        serve_unix_relay(
                            &relay_listener,
                            address,
                            &relay_running,
                            &connection_workers,
                            &next_connection,
                            &relay_clients,
                        );
                    }
                }) {
                Ok(worker) => worker,
                Err(error) => {
                    running.store(false, Ordering::Release);
                    let _ = TcpStream::connect_timeout(&address, Duration::from_millis(100));
                    let _ = worker.join();
                    return Err(SandboxError::Proxy(error));
                }
            }
        };
        Ok(Self {
            address,
            policy,
            running,
            stopped,
            denials,
            worker: Mutex::new(Some(worker)),
            connection_workers,
            clients,
            #[cfg(target_os = "linux")]
            relay_clients,
            #[cfg(target_os = "linux")]
            relay_path,
            #[cfg(target_os = "linux")]
            relay_worker: Mutex::new(Some(relay_worker)),
            #[cfg(target_os = "linux")]
            _relay_directory: relay_directory,
        })
    }

    /// Loopback endpoint injected as `HTTP_PROXY`/`HTTPS_PROXY` into a granted
    /// sandboxed command.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Canonical proxy URL without credentials.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Private pathname socket used by the Linux network-namespace relay.
    /// Other platforms route directly to the exact loopback endpoint.
    #[must_use]
    pub fn relay_path(&self) -> Option<&Path> {
        #[cfg(target_os = "linux")]
        {
            Some(&self.relay_path)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Returns an instance-scoped lifecycle observer for deterministic tests
    /// and supervisors. It remains valid after the proxy object is dropped.
    #[must_use]
    pub fn lifecycle(&self) -> ProxyLifecycle {
        ProxyLifecycle {
            stopped: Arc::clone(&self.stopped),
        }
    }

    /// Returns an observer suitable for a process supervisor. A denial is a
    /// terminal capability violation when the owning manifest omitted network.
    #[must_use]
    pub fn denials(&self) -> ProxyDenials {
        ProxyDenials {
            denials: Arc::clone(&self.denials),
        }
    }

    /// Hot-adds a user-approved domain for once/session/always recovery flows.
    /// Persistence scope is owned by the permission engine; this updates the
    /// live connection gate immediately.
    pub fn allow_domain(&self, domain: &str) -> bool {
        self.policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .allow_domain(domain)
    }
}

impl Drop for SupervisedEgressProxy {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        for stream in self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
        #[cfg(target_os = "linux")]
        for stream in self
            .relay_clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        #[cfg(target_os = "linux")]
        let _ = UnixStream::connect(&self.relay_path);
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
        #[cfg(target_os = "linux")]
        if let Some(worker) = self
            .relay_worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
        let workers = std::mem::take(
            &mut *self
                .connection_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for worker in workers {
            let _ = worker.join();
        }
        self.stopped.store(true, Ordering::Release);
    }
}

#[cfg(target_os = "linux")]
fn serve_unix_relay(
    listener: &UnixListener,
    proxy: SocketAddr,
    running: &Arc<AtomicBool>,
    workers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
    next_connection: &AtomicUsize,
    clients: &Arc<Mutex<BTreeMap<usize, UnixStream>>>,
) {
    while running.load(Ordering::Acquire) {
        reap_finished_workers(workers);
        match listener.accept() {
            Ok((client, _)) => {
                let connection = next_connection.fetch_add(1, Ordering::Relaxed);
                if let Ok(control) = client.try_clone() {
                    clients
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(connection, control);
                }
                let clients_for_connection = Arc::clone(clients);
                if let Ok(worker) = thread::Builder::new()
                    .name("rottweiler-egress-host-relay-connection".to_owned())
                    .spawn(move || {
                        if let Ok(upstream) =
                            TcpStream::connect_timeout(&proxy, Duration::from_secs(2))
                        {
                            let _ = tunnel_unix_to_tcp(client, upstream);
                        }
                        clients_for_connection
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&connection);
                    })
                {
                    workers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(worker);
                } else {
                    clients
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&connection);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

#[cfg(target_os = "linux")]
fn tunnel_unix_to_tcp(mut client: UnixStream, mut upstream: TcpStream) -> io::Result<()> {
    let mut client_read = client.try_clone()?;
    let mut upstream_write = upstream.try_clone()?;
    let forward = thread::spawn(move || {
        let result = io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
        result
    });
    let reverse = io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = forward.join();
    reverse.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn serve(
    listener: &TcpListener,
    policy: &Arc<Mutex<EgressPolicy>>,
    upstream_proxy: Option<&UpstreamProxy>,
    pins: &Arc<BTreeMap<(String, u16), Vec<SocketAddr>>>,
    running: &Arc<AtomicBool>,
    active: &Arc<AtomicUsize>,
    next_connection: &Arc<AtomicUsize>,
    denials: &Arc<AtomicUsize>,
    workers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
    clients: &Arc<Mutex<BTreeMap<usize, TcpStream>>>,
) {
    while running.load(Ordering::Acquire) {
        reap_finished_workers(workers);
        match listener.accept() {
            Ok((stream, _)) => {
                if active
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        (count < MAX_ACTIVE_CONNECTIONS).then_some(count + 1)
                    })
                    .is_err()
                {
                    let mut stream = stream;
                    let _ = stream.set_nonblocking(false);
                    let _ = write_response(&mut stream, 503, "connection-limit");
                    continue;
                }
                let policy = Arc::clone(policy);
                let upstream_proxy = upstream_proxy.cloned();
                let pins = Arc::clone(pins);
                let active_for_connection = Arc::clone(active);
                let running = Arc::clone(running);
                let clients_for_connection = Arc::clone(clients);
                let denials = Arc::clone(denials);
                let connection = next_connection.fetch_add(1, Ordering::Relaxed);
                if let Ok(control) = stream.try_clone() {
                    clients
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(connection, control);
                }
                let spawn = thread::Builder::new()
                    .name("rottweiler-egress-connection".to_owned())
                    .spawn(move || {
                        let _guard = ActiveConnection(active_for_connection);
                        let _ = stream.set_nonblocking(false);
                        let _ = handle_connection(
                            stream,
                            &policy,
                            upstream_proxy.as_ref(),
                            &pins,
                            &running,
                            &denials,
                        );
                        clients_for_connection
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&connection);
                    });
                if let Ok(worker) = spawn {
                    workers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(worker);
                } else {
                    clients
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&connection);
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

fn reap_finished_workers(workers: &Mutex<Vec<JoinHandle<()>>>) {
    let finished = {
        let mut workers = workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut finished = Vec::new();
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                finished.push(workers.swap_remove(index));
            } else {
                index += 1;
            }
        }
        finished
    };
    for worker in finished {
        let _ = worker.join();
    }
}

struct ActiveConnection(Arc<AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_connection(
    mut client: TcpStream,
    policy: &Mutex<EgressPolicy>,
    upstream_proxy: Option<&UpstreamProxy>,
    pins: &BTreeMap<(String, u16), Vec<SocketAddr>>,
    running: &AtomicBool,
    denials: &AtomicUsize,
) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(5)))?;
    client.set_write_timeout(Some(Duration::from_secs(5)))?;
    let Some((request_line, header)) = read_request_header(&mut client)? else {
        return Ok(());
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let authority = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        write_response(&mut client, 400, "invalid-request-line")?;
        return Ok(());
    }
    if method != "CONNECT" {
        return forward_plain_http(
            client,
            policy,
            upstream_proxy,
            pins,
            denials,
            method,
            authority,
            version,
            &header,
        );
    }
    let Some((host, port)) = parse_authority(authority) else {
        write_response(&mut client, 400, "invalid-authority")?;
        return Ok(());
    };
    let Some(addresses) = policy_resolve(&mut client, policy, pins, &host, port, denials)? else {
        return Ok(());
    };
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    let Some((sni, client_hello)) = read_tls_client_hello(&mut client)? else {
        return Ok(());
    };
    if normalize_host(&sni) != normalize_host(&host) {
        return Ok(());
    }
    let Some(mut upstream) = connect_target(&host, port, &addresses, upstream_proxy)? else {
        return Ok(());
    };
    upstream.write_all(&client_hello)?;
    configure_tunnel_timeouts(&client, upstream.socket())?;
    tunnel_connection(client, upstream, running)
}

fn policy_resolve(
    client: &mut TcpStream,
    policy: &Mutex<EgressPolicy>,
    pins: &BTreeMap<(String, u16), Vec<SocketAddr>>,
    host: &str,
    port: u16,
    denials: &AtomicUsize,
) -> io::Result<Option<Vec<SocketAddr>>> {
    let addresses = normalize_host(host)
        .and_then(|host| pins.get(&(host, port)).cloned())
        .unwrap_or_else(|| {
            (host, port)
                .to_socket_addrs()
                .map(Iterator::collect::<Vec<_>>)
                .unwrap_or_default()
        });
    let ips = addresses
        .iter()
        .map(SocketAddr::ip)
        .collect::<Vec<IpAddr>>();
    let decision = policy
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .evaluate(host, &ips);
    match decision {
        EgressDecision::Allowed => {}
        EgressDecision::ApprovalRequired => {
            denials.fetch_add(1, Ordering::AcqRel);
            write_response(client, 403, "approval-required")?;
            return Ok(None);
        }
        EgressDecision::HardDenied => {
            denials.fetch_add(1, Ordering::AcqRel);
            write_response(client, 403, "private-or-unresolved-target")?;
            return Ok(None);
        }
    }
    Ok(Some(addresses))
}

#[allow(clippy::too_many_arguments)]
fn forward_plain_http(
    mut client: TcpStream,
    policy: &Mutex<EgressPolicy>,
    upstream_proxy: Option<&UpstreamProxy>,
    pins: &BTreeMap<(String, u16), Vec<SocketAddr>>,
    denials: &AtomicUsize,
    method: &str,
    target: &str,
    version: &str,
    header: &[u8],
) -> io::Result<()> {
    if !matches!(method, "GET" | "HEAD") {
        write_response(&mut client, 405, "unsupported-http-method")?;
        return Ok(());
    }
    let Ok(url) = Url::parse(target) else {
        write_response(&mut client, 400, "absolute-http-url-required")?;
        return Ok(());
    };
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        write_response(&mut client, 400, "invalid-http-target")?;
        return Ok(());
    }
    let Some(host) = url.host_str() else {
        write_response(&mut client, 400, "missing-http-host")?;
        return Ok(());
    };
    let port = url.port_or_known_default().unwrap_or(80);
    let Some(addresses) = policy_resolve(&mut client, policy, pins, host, port, denials)? else {
        return Ok(());
    };
    let Some(mut upstream) = connect_plain_target(&addresses, upstream_proxy)? else {
        write_response(&mut client, 502, "upstream-connect-failed")?;
        return Ok(());
    };
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let request_target = if upstream_proxy.is_some() {
        let mut absolute = url.clone();
        absolute.set_fragment(None);
        let pinned_ip = addresses
            .first()
            .map(SocketAddr::ip)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "target pin is missing"))?;
        absolute
            .set_ip_host(pinned_ip)
            .map_err(|()| io::Error::new(io::ErrorKind::InvalidInput, "target pin is invalid"))?;
        absolute.to_string()
    } else {
        path
    };
    let rewritten = rewrite_plain_header(
        header,
        &format!("{method} {request_target} {version}"),
        &authority,
        upstream_proxy.and_then(|proxy| proxy.authorization.as_deref()),
    )?;
    upstream.write_all(&rewritten)?;
    upstream.flush()?;
    configure_tunnel_timeouts(&client, upstream.socket())?;
    relay_one_http_response(&mut client, &mut upstream, method == "HEAD")
}

fn relay_one_http_response(
    client: &mut TcpStream,
    upstream: &mut UpstreamConnection,
    head_request: bool,
) -> io::Result<()> {
    let Some(header) = read_stream_header(upstream)? else {
        return Ok(());
    };
    let (content_length, body_forbidden) = response_framing(&header, head_request)?;
    client.write_all(&header)?;
    if body_forbidden {
        return Ok(());
    }
    let mut transferred = 0_usize;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let remaining = content_length.map_or(buffer.len(), |length| {
            length.saturating_sub(transferred).min(buffer.len())
        });
        if remaining == 0 {
            break;
        }
        let length = upstream.read(&mut buffer[..remaining])?;
        if length == 0 {
            if content_length.is_some_and(|expected| transferred != expected) {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated upstream HTTP response",
                ));
            }
            break;
        }
        transferred = transferred.saturating_add(length);
        if transferred > MAX_PLAIN_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upstream HTTP response exceeded the proxy bound",
            ));
        }
        client.write_all(&buffer[..length])?;
    }
    Ok(())
}

fn response_framing(header: &[u8], head_request: bool) -> io::Result<(Option<usize>, bool)> {
    let text = strict_header_text(header)?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP response"))?;
    let mut content_length = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = strict_header_line(line)?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transfer-encoded proxy responses are unsupported",
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() || value.contains(',') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ambiguous HTTP response length",
                ));
            }
            let length = value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP response length")
            })?;
            if length > MAX_PLAIN_RESPONSE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "upstream HTTP response exceeded the proxy bound",
                ));
            }
            content_length = Some(length);
        }
    }
    Ok((
        content_length,
        head_request || (100..200).contains(&status) || matches!(status, 204 | 304),
    ))
}

fn normalize_host(host: &str) -> Option<String> {
    crate::normalize_egress_domain(host)
}

fn read_tls_client_hello(client: &mut TcpStream) -> io::Result<Option<(String, Vec<u8>)>> {
    let mut captured = Vec::new();
    let mut handshake = Vec::new();
    loop {
        let mut header = [0_u8; 5];
        if client.read_exact(&mut header).is_err() || header[0] != 22 {
            return Ok(None);
        }
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if length == 0
            || length > 18_432
            || captured.len().saturating_add(5 + length) > MAX_CLIENT_HELLO_BYTES
        {
            return Ok(None);
        }
        let mut payload = vec![0_u8; length];
        client.read_exact(&mut payload)?;
        captured.extend_from_slice(&header);
        captured.extend_from_slice(&payload);
        handshake.extend_from_slice(&payload);
        if handshake.len() < 4 {
            continue;
        }
        if handshake[0] != 1 {
            return Ok(None);
        }
        let expected = 4
            + ((usize::from(handshake[1]) << 16)
                | (usize::from(handshake[2]) << 8)
                | usize::from(handshake[3]));
        if expected > MAX_CLIENT_HELLO_BYTES {
            return Ok(None);
        }
        if handshake.len() >= expected {
            return Ok(parse_client_hello_sni(&handshake[..expected]).map(|sni| (sni, captured)));
        }
    }
}

fn parse_client_hello_sni(handshake: &[u8]) -> Option<String> {
    if handshake.first().copied() != Some(1) || handshake.len() < 39 {
        return None;
    }
    let mut offset = 38;
    let session_length = usize::from(*handshake.get(offset)?);
    offset = offset.checked_add(1 + session_length)?;
    let cipher_length = read_u16(handshake, offset)?;
    offset = offset.checked_add(2 + cipher_length)?;
    let compression_length = usize::from(*handshake.get(offset)?);
    offset = offset.checked_add(1 + compression_length)?;
    let extensions_length = read_u16(handshake, offset)?;
    offset = offset.checked_add(2)?;
    let extensions_end = offset.checked_add(extensions_length)?;
    if extensions_end > handshake.len() {
        return None;
    }
    let mut server_name = None;
    let mut saw_ech = false;
    while offset < extensions_end {
        if offset + 4 > extensions_end {
            return None;
        }
        let kind = read_u16(handshake, offset)?;
        let length = read_u16(handshake, offset + 2)?;
        offset += 4;
        let end = offset.checked_add(length)?;
        if end > extensions_end {
            return None;
        }
        match kind {
            0 => {
                if server_name.is_some() {
                    return None;
                }
                server_name = Some(parse_server_name_extension(&handshake[offset..end])?);
            }
            0xfe0d => saw_ech = true,
            _ => {}
        }
        offset = end;
    }
    (!saw_ech).then_some(server_name).flatten()
}

fn parse_server_name_extension(extension: &[u8]) -> Option<String> {
    let list_length = read_u16(extension, 0)?;
    if list_length + 2 != extension.len() {
        return None;
    }
    let mut offset = 2;
    let mut server_name = None;
    while offset < extension.len() {
        if offset + 3 > extension.len() {
            return None;
        }
        let name_type = *extension.get(offset)?;
        let length = read_u16(extension, offset + 1)?;
        offset += 3;
        let end = offset.checked_add(length)?;
        if end > extension.len() {
            return None;
        }
        if name_type == 0 {
            if server_name.is_some() {
                return None;
            }
            server_name = Some(
                std::str::from_utf8(&extension[offset..end])
                    .ok()
                    .and_then(normalize_host)?,
            );
        }
        offset = end;
    }
    server_name
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ])))
}

enum UpstreamConnection {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl UpstreamConnection {
    fn socket(&self) -> &TcpStream {
        match self {
            Self::Plain(stream) => stream,
            Self::Tls(stream) => &stream.sock,
        }
    }
}

impl Read for UpstreamConnection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for UpstreamConnection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

fn connect_target(
    host: &str,
    port: u16,
    addresses: &[SocketAddr],
    upstream_proxy: Option<&UpstreamProxy>,
) -> io::Result<Option<UpstreamConnection>> {
    let Some(proxy) = upstream_proxy else {
        return Ok(connect_pinned(addresses).map(UpstreamConnection::Plain));
    };
    let Some(pinned) = addresses.first() else {
        return Ok(None);
    };
    let pinned_authority = pinned.to_string();
    let mut stream = connect_proxy_transport(proxy)?;
    write!(
        stream,
        "CONNECT {pinned_authority} HTTP/1.1\r\nHost: {host}:{port}\r\n"
    )?;
    if let Some(authorization) = &proxy.authorization {
        write!(stream, "Proxy-Authorization: {authorization}\r\n")?;
    }
    stream.write_all(b"Proxy-Connection: keep-alive\r\n\r\n")?;
    stream.flush()?;
    let Some(header) = read_stream_header(&mut stream)? else {
        return Ok(None);
    };
    let status = std::str::from_utf8(&header)
        .ok()
        .and_then(|header| header.lines().next())
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok());
    Ok((status == Some(200)).then_some(stream))
}

fn connect_plain_target(
    addresses: &[SocketAddr],
    upstream_proxy: Option<&UpstreamProxy>,
) -> io::Result<Option<UpstreamConnection>> {
    match upstream_proxy {
        Some(proxy) => connect_proxy_transport(proxy).map(Some),
        None => Ok(connect_pinned(addresses).map(UpstreamConnection::Plain)),
    }
}

fn connect_proxy_transport(proxy: &UpstreamProxy) -> io::Result<UpstreamConnection> {
    let host = proxy
        .url
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "proxy host is missing"))?;
    let socket = connect_pinned(&proxy.addresses)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "proxy connection failed"))?;
    socket.set_read_timeout(Some(Duration::from_secs(15)))?;
    socket.set_write_timeout(Some(Duration::from_secs(15)))?;
    if proxy.url.scheme() == "http" {
        return Ok(UpstreamConnection::Plain(socket));
    }
    let roots = webpki_roots::TLS_SERVER_ROOTS
        .iter()
        .cloned()
        .collect::<rustls::RootCertStore>();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "proxy host is invalid"))?;
    let connection = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut stream = rustls::StreamOwned::new(connection, socket);
    stream
        .conn
        .complete_io(&mut stream.sock)
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(UpstreamConnection::Tls(Box::new(stream)))
}

fn read_stream_header(stream: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while bytes.len() < MAX_HEADER_BYTES {
        if stream.read(&mut byte)? == 0 {
            return Ok(None);
        }
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\n\n") {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

fn rewrite_plain_header(
    header: &[u8],
    request_line: &str,
    canonical_authority: &str,
    proxy_authorization: Option<&str>,
) -> io::Result<Vec<u8>> {
    let text = strict_header_text(header)?;
    let mut rewritten = format!("{request_line}\r\n").into_bytes();
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            continue;
        }
        let (name, _) = strict_header_line(line)?;
        let name = name.to_ascii_lowercase();
        if matches!(name.as_str(), "content-length" | "transfer-encoding") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "body-bearing proxy requests are unsupported",
            ));
        }
        if matches!(
            name.as_str(),
            "host"
                | "proxy-authorization"
                | "proxy-connection"
                | "connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
        ) {
            continue;
        }
        rewritten.extend_from_slice(line.as_bytes());
        rewritten.extend_from_slice(b"\r\n");
    }
    rewritten.extend_from_slice(format!("Host: {canonical_authority}\r\n").as_bytes());
    if let Some(authorization) = proxy_authorization {
        rewritten.extend_from_slice(format!("Proxy-Authorization: {authorization}\r\n").as_bytes());
    }
    rewritten.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(rewritten)
}

fn strict_header_text(header: &[u8]) -> io::Result<&str> {
    let text = std::str::from_utf8(header)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "HTTP header is not UTF-8"))?;
    text.strip_suffix("\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "HTTP header is ambiguous"))
}

fn strict_header_line(line: &str) -> io::Result<(&str, &str)> {
    if line.starts_with([' ', '\t']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "obsolete folded HTTP headers are unsupported",
        ));
    }
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP header"))?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed HTTP header",
        ));
    }
    Ok((name, value))
}

fn configure_tunnel_timeouts(client: &TcpStream, upstream: &TcpStream) -> io::Result<()> {
    client.set_read_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    client.set_write_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    upstream.set_read_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    upstream.set_write_timeout(Some(TUNNEL_IDLE_TIMEOUT))?;
    Ok(())
}

fn read_request_header(client: &mut TcpStream) -> io::Result<Option<(String, Vec<u8>)>> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while bytes.len() < MAX_HEADER_BYTES {
        if client.read(&mut byte)? == 0 {
            return Ok(None);
        }
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") || bytes.ends_with(b"\n\n") {
            let line_end = bytes
                .windows(2)
                .position(|window| window == b"\r\n")
                .or_else(|| bytes.iter().position(|value| *value == b'\n'))
                .unwrap_or(bytes.len());
            let line = std::str::from_utf8(&bytes[..line_end])
                .ok()
                .map(ToOwned::to_owned);
            return Ok(line.map(|line| (line, bytes)));
        }
    }
    Ok(None)
}

fn parse_authority(authority: &str) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        return port.parse().ok().map(|port| (host.to_owned(), port));
    }
    let (host, port) = authority.rsplit_once(':')?;
    (!host.is_empty())
        .then(|| port.parse().ok().map(|port| (host.to_owned(), port)))
        .flatten()
}

fn connect_pinned(addresses: &[SocketAddr]) -> Option<TcpStream> {
    addresses
        .iter()
        .find_map(|address| TcpStream::connect_timeout(address, Duration::from_secs(5)).ok())
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str) -> io::Result<()> {
    let body = reason.as_bytes();
    write!(
        stream,
        "HTTP/1.1 {status} Forbidden\r\ncontent-length: {}\r\nx-proxy-error: {reason}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn tunnel(mut client: TcpStream, mut upstream: TcpStream, running: &AtomicBool) -> io::Result<()> {
    let quantum = Duration::from_millis(25);
    client.set_read_timeout(Some(quantum))?;
    upstream.set_read_timeout(Some(quantum))?;
    let mut client_open = true;
    let mut upstream_open = true;
    let mut last_activity = Instant::now();
    let mut buffer = [0_u8; 16 * 1024];
    while running.load(Ordering::Acquire)
        && (client_open || upstream_open)
        && last_activity.elapsed() < TUNNEL_IDLE_TIMEOUT
    {
        let forward =
            copy_tunnel_quantum(&mut client, &mut upstream, &mut client_open, &mut buffer)?;
        let reverse =
            copy_tunnel_quantum(&mut upstream, &mut client, &mut upstream_open, &mut buffer)?;
        if forward || reverse {
            last_activity = Instant::now();
        }
    }
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    Ok(())
}

fn copy_tunnel_quantum<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    open: &mut bool,
    buffer: &mut [u8],
) -> io::Result<bool> {
    if !*open {
        return Ok(false);
    }
    let activity = match reader.read(buffer) {
        Ok(0) => {
            *open = false;
            false
        }
        Ok(length) => {
            writer.write_all(&buffer[..length])?;
            true
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            false
        }
        Err(error) => return Err(error),
    };
    Ok(activity)
}

fn tunnel_connection(
    client: TcpStream,
    upstream: UpstreamConnection,
    running: &AtomicBool,
) -> io::Result<()> {
    match upstream {
        UpstreamConnection::Plain(upstream) => tunnel(client, upstream, running),
        UpstreamConnection::Tls(upstream) => tunnel_tls_proxy(client, *upstream, running),
    }
}

fn tunnel_tls_proxy(
    mut client: TcpStream,
    mut upstream: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    running: &AtomicBool,
) -> io::Result<()> {
    let quantum = Duration::from_millis(25);
    client.set_read_timeout(Some(quantum))?;
    upstream.sock.set_read_timeout(Some(quantum))?;
    let mut client_open = true;
    let mut upstream_open = true;
    let mut last_activity = Instant::now();
    let mut buffer = [0_u8; 16 * 1024];
    while running.load(Ordering::Acquire)
        && (client_open || upstream_open)
        && last_activity.elapsed() < TUNNEL_IDLE_TIMEOUT
    {
        if client_open {
            match client.read(&mut buffer) {
                Ok(0) => {
                    client_open = false;
                    upstream.conn.send_close_notify();
                    let _ = upstream.flush();
                }
                Ok(length) => {
                    upstream.write_all(&buffer[..length])?;
                    upstream.flush()?;
                    last_activity = Instant::now();
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        if upstream_open {
            match upstream.read(&mut buffer) {
                Ok(0) => {
                    upstream_open = false;
                    let _ = client.shutdown(Shutdown::Write);
                }
                Ok(length) => {
                    client.write_all(&buffer[..length])?;
                    last_activity = Instant::now();
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.sock.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use std::io::{BufRead as _, BufReader};

    #[test]
    fn proxy_blocks_private_targets_and_tunnels_only_after_global_opt_out() {
        let denied =
            SupervisedEgressProxy::start(EgressPolicy::new(["127.0.0.1"])).expect("denied proxy");
        let response = connect_request(denied.address(), "127.0.0.1:9");
        assert!(response.contains("403 Forbidden"));
        assert!(response.contains("private-or-unresolved-target"));

        let upstream =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("upstream listener");
        let upstream_address = upstream.local_addr().expect("upstream address");
        let hello = tls_client_hello("localhost");
        let expected_hello = hello.clone();
        let upstream_worker = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().expect("upstream accept");
            let mut received = vec![0_u8; expected_hello.len()];
            stream.read_exact(&mut received).expect("upstream read");
            assert_eq!(received, expected_hello);
            stream.write_all(&[42]).expect("upstream write");
        });
        let allowed = SupervisedEgressProxy::start(
            EgressPolicy::new(Vec::<String>::new()).with_private_destinations(true),
        )
        .expect("allowed proxy");
        let authority = format!("localhost:{}", upstream_address.port());
        let approval = connect_request(allowed.address(), &authority);
        assert!(approval.contains("approval-required"));
        assert!(allowed.allow_domain("localhost"));
        let mut stream = TcpStream::connect(allowed.address()).expect("proxy connect");
        write!(
            stream,
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
        )
        .expect("connect request");
        let mut reader = BufReader::new(stream.try_clone().expect("client clone"));
        let mut status = String::new();
        reader.read_line(&mut status).expect("status");
        assert!(status.contains("200 Connection Established"));
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("header");
            if header == "\r\n" {
                break;
            }
        }
        stream.write_all(&hello).expect("TLS ClientHello");
        stream
            .shutdown(Shutdown::Write)
            .expect("finish tunnel request");
        let mut reply = [0_u8; 1];
        reader.read_exact(&mut reply).expect("tunnel read");
        assert_eq!(reply, [42]);
        upstream_worker.join().expect("upstream worker");
    }

    #[test]
    fn connect_sni_mismatch_never_reaches_shared_ip_upstream() {
        let upstream = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("upstream");
        upstream
            .set_nonblocking(true)
            .expect("nonblocking upstream");
        let upstream_address = upstream.local_addr().expect("upstream address");
        let proxy = SupervisedEgressProxy::start(
            EgressPolicy::new(["localhost"]).with_private_destinations(true),
        )
        .expect("proxy");
        let authority = format!("localhost:{}", upstream_address.port());
        let mut stream = TcpStream::connect(proxy.address()).expect("proxy connect");
        write!(
            stream,
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
        )
        .expect("CONNECT");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut status = String::new();
        reader.read_line(&mut status).expect("status");
        assert!(status.contains("200 Connection Established"));
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("header");
            if header == "\r\n" {
                break;
            }
        }
        stream
            .write_all(&tls_client_hello("attacker.example"))
            .expect("mismatched hello");
        stream.shutdown(Shutdown::Write).expect("finish hello");
        thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            upstream.accept(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn client_hello_parser_fails_closed_for_absent_ech_only_and_malformed_sni() {
        let hello = tls_client_hello("example.com");
        assert_eq!(
            parse_client_hello_sni(&hello[5..]),
            Some("example.com".to_owned())
        );
        let mut ech_only = hello.clone();
        // One-extension fixture: replace server_name (0x0000) with ECH
        // (0xfe0d). Without a plaintext server_name, routing must stop.
        ech_only[52..54].copy_from_slice(&0xfe0d_u16.to_be_bytes());
        assert_eq!(parse_client_hello_sni(&ech_only[5..]), None);
        let sni_and_ech = tls_client_hello_with_ech("example.com");
        assert_eq!(parse_client_hello_sni(&sni_and_ech[5..]), None);
        assert_eq!(parse_client_hello_sni(&hello[5..hello.len() - 1]), None);
        assert_eq!(parse_client_hello_sni(&[1, 0, 0, 0]), None);
    }

    #[test]
    fn proxy_forwards_plain_http_absolute_form_without_proxy_credentials() {
        let upstream =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("upstream listener");
        let upstream_address = upstream.local_addr().expect("upstream address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().expect("upstream accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(request.starts_with("GET /path?q=1 HTTP/1.1\r\n"));
            assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
            assert!(request.contains(&format!("Host: {upstream_address}\r\n")));
            assert!(!request.contains("attacker.invalid"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .expect("response");
        });
        let proxy = SupervisedEgressProxy::start(
            EgressPolicy::new(["127.0.0.1"]).with_private_destinations(true),
        )
        .expect("proxy");
        let mut stream = TcpStream::connect(proxy.address()).expect("proxy connect");
        write!(
            stream,
            "GET http://{upstream_address}/path?q=1 HTTP/1.1\r\nHost: attacker.invalid\r\nProxy-Authorization: secret\r\nConnection: keep-alive\r\n\r\n"
        )
        .expect("request");
        stream.shutdown(Shutdown::Write).expect("request complete");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        assert!(response.ends_with("ok"), "{response:?}");
        worker.join().expect("upstream worker");
    }

    #[test]
    fn plain_http_closes_after_one_message_and_rejects_smuggling_framing() {
        for header in [
            b"GET http://localhost/ HTTP/1.1\r\nContent-Length: 1\r\n\r\n".as_slice(),
            b"GET http://localhost/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"GET http://localhost/ HTTP/1.1\r\nX-Test: one\r\n folded\r\n\r\n",
            b"GET http://localhost/ HTTP/1.1\nHost: localhost\n\n",
        ] {
            assert!(rewrite_plain_header(header, "GET / HTTP/1.1", "localhost", None).is_err());
        }

        let upstream = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("upstream");
        let upstream_address = upstream.local_addr().expect("upstream address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().expect("upstream accept");
            stream
                .set_read_timeout(Some(Duration::from_millis(250)))
                .expect("timeout");
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).expect("request header");
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).expect("request UTF-8");
            assert_eq!(request.matches("GET ").count(), 1, "{request:?}");
            assert!(request.contains("Connection: close\r\n"), "{request:?}");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .expect("response");
        });
        let proxy = SupervisedEgressProxy::start(
            EgressPolicy::new(["127.0.0.1"]).with_private_destinations(true),
        )
        .expect("proxy");
        let mut client = TcpStream::connect(proxy.address()).expect("proxy connect");
        write!(
            client,
            "GET http://{upstream_address}/first HTTP/1.1\r\nHost: ignored\r\n\r\nGET http://169.254.169.254/latest HTTP/1.1\r\nHost: metadata\r\n\r\n"
        )
        .expect("pipelined request");
        let mut response = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            match client.read(&mut buffer) {
                Ok(0) => break,
                Ok(length) => response.extend_from_slice(&buffer[..length]),
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => break,
                Err(error) => panic!("response: {error}"),
            }
        }
        let response = String::from_utf8(response).expect("response UTF-8");
        assert_eq!(response.matches("HTTP/1.1").count(), 1, "{response:?}");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response:?}");
        worker.join().expect("worker");
    }

    #[test]
    fn dropping_proxy_closes_active_tunnel_and_joins_connection_worker() {
        let upstream = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("upstream");
        let upstream_address = upstream.local_addr().expect("upstream address");
        let hello = tls_client_hello("localhost");
        let worker = thread::spawn(move || {
            let (mut stream, _) = upstream.accept().expect("upstream accept");
            let mut received = vec![0_u8; hello.len()];
            stream.read_exact(&mut received).expect("client hello");
            let mut eof = [0_u8; 1];
            assert_eq!(stream.read(&mut eof).expect("upstream EOF"), 0);
        });
        let proxy = SupervisedEgressProxy::start(
            EgressPolicy::new(["localhost"]).with_private_destinations(true),
        )
        .expect("proxy");
        let lifecycle = proxy.lifecycle();
        let authority = format!("localhost:{}", upstream_address.port());
        let mut client = TcpStream::connect(proxy.address()).expect("proxy connect");
        write!(
            client,
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
        )
        .expect("CONNECT");
        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("CONNECT response");
            if line == "\r\n" {
                break;
            }
        }
        client
            .write_all(&tls_client_hello("localhost"))
            .expect("client hello");
        thread::sleep(Duration::from_millis(25));
        drop(proxy);
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("timeout");
        let mut eof = [0_u8; 1];
        assert_eq!(client.read(&mut eof).expect("client EOF"), 0);
        assert!(lifecycle.is_stopped());
        worker.join().expect("upstream worker");
    }

    #[test]
    fn corporate_proxy_receives_only_pinned_target_and_injected_auth() {
        let corporate = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("corporate");
        let corporate_address = corporate.local_addr().expect("corporate address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = corporate.accept().expect("corporate accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(request.starts_with("GET http://"), "{request:?}");
            assert!(!request.starts_with("GET http://localhost"), "{request:?}");
            assert!(request.contains("\r\nHost: localhost:9\r\n"), "{request:?}");
            assert!(
                request.contains("\r\nProxy-Authorization: Basic dXNlcjpzZWNyZXQtY2FuYXJ5\r\n"),
                "{request:?}"
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .expect("response");
        });
        let upstream = UpstreamProxy::new(
            Url::parse(&format!("http://{corporate_address}")).expect("proxy URL"),
        )
        .expect("upstream")
        .with_basic_auth("user", "secret-canary");
        let debug = format!("{upstream:?}");
        assert!(!debug.contains("user"));
        assert!(!debug.contains("secret-canary"));
        let proxy = SupervisedEgressProxy::start_with_upstream(
            EgressPolicy::new(["localhost"]).with_private_destinations(true),
            Some(upstream),
        )
        .expect("policy proxy");
        let mut stream = TcpStream::connect(proxy.address()).expect("proxy connect");
        stream
            .write_all(
                b"GET http://localhost:9/path HTTP/1.1\r\nHost: attacker.invalid\r\nProxy-Authorization: attacker-secret\r\n\r\n",
            )
            .expect("request");
        stream.shutdown(Shutdown::Write).expect("request complete");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        assert!(response.ends_with("ok"), "{response:?}");
        worker.join().expect("corporate worker");
    }

    #[test]
    fn corporate_connect_pins_ip_while_preserving_host_and_client_sni() {
        let corporate = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("corporate");
        let corporate_address = corporate.local_addr().expect("corporate address");
        let hello = tls_client_hello("localhost");
        let expected_hello = hello.clone();
        let worker = thread::spawn(move || {
            let (mut stream, _) = corporate.accept().expect("corporate accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("header");
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(request.starts_with("CONNECT "), "{request:?}");
            assert!(!request.starts_with("CONNECT localhost:"), "{request:?}");
            assert!(request.contains("\r\nHost: localhost:9\r\n"), "{request:?}");
            assert!(
                request.contains("\r\nProxy-Authorization: Basic dXNlcjpzZWNyZXQtY2FuYXJ5\r\n"),
                "{request:?}"
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .expect("CONNECT response");
            let mut received = vec![0_u8; expected_hello.len()];
            reader.read_exact(&mut received).expect("client hello");
            assert_eq!(received, expected_hello);
            stream.write_all(&[42]).expect("tunnel response");
        });
        let upstream = UpstreamProxy::new(
            Url::parse(&format!("http://{corporate_address}")).expect("proxy URL"),
        )
        .expect("upstream")
        .with_basic_auth("user", "secret-canary");
        let proxy = SupervisedEgressProxy::start_with_upstream(
            EgressPolicy::new(["localhost"]).with_private_destinations(true),
            Some(upstream),
        )
        .expect("policy proxy");
        let mut stream = TcpStream::connect(proxy.address()).expect("proxy connect");
        stream
            .write_all(b"CONNECT localhost:9 HTTP/1.1\r\nHost: localhost:9\r\n\r\n")
            .expect("CONNECT request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut status = String::new();
        reader.read_line(&mut status).expect("status");
        assert!(status.contains("200 Connection Established"));
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("header");
            if line == "\r\n" {
                break;
            }
        }
        stream.write_all(&hello).expect("client hello");
        let mut response = [0_u8; 1];
        reader.read_exact(&mut response).expect("tunnel response");
        assert_eq!(response, [42]);
        worker.join().expect("corporate worker");
    }

    #[test]
    fn corporate_proxy_url_rejects_paths_and_inline_credentials() {
        for value in [
            "http://proxy.example/path",
            "http://user:password@proxy.example/",
            "socks5://proxy.example/",
        ] {
            assert!(UpstreamProxy::new(Url::parse(value).expect("URL")).is_err());
        }
    }

    #[test]
    fn corporate_proxy_resolution_is_captured_once_before_credentials() {
        let resolutions = AtomicUsize::new(0);
        let first: SocketAddr = "127.0.0.1:3128".parse().expect("first address");
        let proxy = UpstreamProxy::new_with_resolver(
            Url::parse("http://proxy.invalid:3128").expect("proxy URL"),
            |host, port| {
                assert_eq!((host, port), ("proxy.invalid", 3128));
                resolutions.fetch_add(1, Ordering::Relaxed);
                Ok(vec![first])
            },
        )
        .expect("upstream proxy")
        .with_basic_auth("user", "dns-rebind-secret");
        assert_eq!(resolutions.load(Ordering::Relaxed), 1);
        assert_eq!(&*proxy.addresses, &[first]);
        assert!(!format!("{proxy:?}").contains("dns-rebind-secret"));
        assert_eq!(resolutions.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn validated_pin_prevents_a_second_dns_resolution() {
        let target = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("target");
        let target_address = target.local_addr().expect("target address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = target.accept().expect("target accept");
            let mut request = [0_u8; 1024];
            let length = stream.read(&mut request).expect("request");
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with("GET /pinned HTTP/1.1\r\n"));
            assert!(request.contains(&format!(
                "\r\nHost: rebind.invalid:{}\r\n",
                target_address.port()
            )));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .expect("response");
        });
        let pin = EgressPin::new(
            "rebind.invalid",
            target_address.port(),
            vec![target_address],
        )
        .expect("pin");
        let proxy = SupervisedEgressProxy::start_with_upstream_and_pins(
            EgressPolicy::new(["rebind.invalid"]).with_private_destinations(true),
            None,
            vec![pin],
        )
        .expect("proxy");
        let mut stream = TcpStream::connect(proxy.address()).expect("proxy connect");
        write!(
            stream,
            "GET http://rebind.invalid:{}/pinned HTTP/1.1\r\nHost: ignored.invalid\r\n\r\n",
            target_address.port()
        )
        .expect("request");
        stream.shutdown(Shutdown::Write).expect("request complete");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        assert!(response.ends_with("ok"));
        worker.join().expect("target worker");
    }

    fn connect_request(proxy: SocketAddr, authority: &str) -> String {
        let mut stream = TcpStream::connect(proxy).expect("proxy connect");
        write!(
            stream,
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
        )
        .expect("request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        response
    }

    fn tls_client_hello(host: &str) -> Vec<u8> {
        tls_client_hello_fixture(host, false)
    }

    fn tls_client_hello_with_ech(host: &str) -> Vec<u8> {
        tls_client_hello_fixture(host, true)
    }

    fn tls_client_hello_fixture(host: &str, include_ech: bool) -> Vec<u8> {
        let host = host.as_bytes();
        let mut sni = Vec::new();
        let server_name_length = 1 + 2 + host.len();
        sni.extend_from_slice(
            &(u16::try_from(server_name_length).expect("SNI size")).to_be_bytes(),
        );
        sni.push(0);
        sni.extend_from_slice(&(u16::try_from(host.len()).expect("host size")).to_be_bytes());
        sni.extend_from_slice(host);

        let mut extensions = Vec::new();
        extensions.extend_from_slice(&0_u16.to_be_bytes());
        extensions
            .extend_from_slice(&(u16::try_from(sni.len()).expect("SNI extension")).to_be_bytes());
        extensions.extend_from_slice(&sni);
        if include_ech {
            extensions.extend_from_slice(&0xfe0d_u16.to_be_bytes());
            extensions.extend_from_slice(&0_u16.to_be_bytes());
        }

        let mut body = Vec::new();
        body.extend_from_slice(&0x0303_u16.to_be_bytes());
        body.extend_from_slice(&[7_u8; 32]);
        body.push(0);
        body.extend_from_slice(&2_u16.to_be_bytes());
        body.extend_from_slice(&0x1301_u16.to_be_bytes());
        body.push(1);
        body.push(0);
        body.extend_from_slice(
            &(u16::try_from(extensions.len()).expect("extensions size")).to_be_bytes(),
        );
        body.extend_from_slice(&extensions);

        let mut handshake = vec![1];
        let length = body.len();
        handshake.extend_from_slice(&[
            u8::try_from((length >> 16) & 0xff).expect("length"),
            u8::try_from((length >> 8) & 0xff).expect("length"),
            u8::try_from(length & 0xff).expect("length"),
        ]);
        handshake.extend_from_slice(&body);

        let mut record = vec![22, 0x03, 0x01];
        record.extend_from_slice(
            &(u16::try_from(handshake.len()).expect("handshake size")).to_be_bytes(),
        );
        record.extend_from_slice(&handshake);
        record
    }
}
