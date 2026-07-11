use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs as _};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use url::Url;

use crate::{EgressDecision, EgressPolicy, SandboxError};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CLIENT_HELLO_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 64;
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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
    worker: Mutex<Option<JoinHandle<()>>>,
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
        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(SandboxError::Proxy)?;
        listener
            .set_nonblocking(true)
            .map_err(SandboxError::Proxy)?;
        let address = listener.local_addr().map_err(SandboxError::Proxy)?;
        let running = Arc::new(AtomicBool::new(true));
        let active = Arc::new(AtomicUsize::new(0));
        let worker_running = Arc::clone(&running);
        let policy = Arc::new(Mutex::new(policy));
        let worker_policy = Arc::clone(&policy);
        let worker = thread::Builder::new()
            .name("rottweiler-egress-proxy".to_owned())
            .spawn(move || serve(&listener, &worker_policy, &worker_running, &active))
            .map_err(SandboxError::Proxy)?;
        Ok(Self {
            address,
            policy,
            running,
            worker: Mutex::new(Some(worker)),
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
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn serve(
    listener: &TcpListener,
    policy: &Arc<Mutex<EgressPolicy>>,
    running: &Arc<AtomicBool>,
    active: &Arc<AtomicUsize>,
) {
    while running.load(Ordering::Acquire) {
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
                let active_for_connection = Arc::clone(active);
                let spawn = thread::Builder::new()
                    .name("rottweiler-egress-connection".to_owned())
                    .spawn(move || {
                        let _guard = ActiveConnection(active_for_connection);
                        let _ = stream.set_nonblocking(false);
                        let _ = handle_connection(stream, &policy);
                    });
                if spawn.is_err() {
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

struct ActiveConnection(Arc<AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_connection(mut client: TcpStream, policy: &Mutex<EgressPolicy>) -> io::Result<()> {
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
        return forward_plain_http(client, policy, method, authority, version, &header);
    }
    let Some((host, port)) = parse_authority(authority) else {
        write_response(&mut client, 400, "invalid-authority")?;
        return Ok(());
    };
    let Some(addresses) = policy_resolve(&mut client, policy, &host, port)? else {
        return Ok(());
    };
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    let Some((sni, client_hello)) = read_tls_client_hello(&mut client)? else {
        return Ok(());
    };
    if normalize_host(&sni) != normalize_host(&host) {
        return Ok(());
    }
    let Some(mut upstream) = connect_pinned(&addresses) else {
        return Ok(());
    };
    upstream.write_all(&client_hello)?;
    configure_tunnel_timeouts(&client, &upstream)?;
    tunnel(client, upstream)
}

fn policy_resolve(
    client: &mut TcpStream,
    policy: &Mutex<EgressPolicy>,
    host: &str,
    port: u16,
) -> io::Result<Option<Vec<SocketAddr>>> {
    let addresses = (host, port)
        .to_socket_addrs()
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
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
            write_response(client, 403, "approval-required")?;
            return Ok(None);
        }
        EgressDecision::HardDenied => {
            write_response(client, 403, "private-or-unresolved-target")?;
            return Ok(None);
        }
    }
    Ok(Some(addresses))
}

fn forward_plain_http(
    mut client: TcpStream,
    policy: &Mutex<EgressPolicy>,
    method: &str,
    target: &str,
    version: &str,
    header: &[u8],
) -> io::Result<()> {
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
    let Some(addresses) = policy_resolve(&mut client, policy, host, port)? else {
        return Ok(());
    };
    let Some(mut upstream) = connect_pinned(&addresses) else {
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
    let rewritten =
        rewrite_plain_header(header, &format!("{method} {path} {version}"), &authority)?;
    upstream.write_all(&rewritten)?;
    configure_tunnel_timeouts(&client, &upstream)?;
    tunnel(client, upstream)
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

fn rewrite_plain_header(
    header: &[u8],
    request_line: &str,
    canonical_authority: &str,
) -> io::Result<Vec<u8>> {
    let text = std::str::from_utf8(header)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "HTTP header is not UTF-8"))?;
    let mut rewritten = format!("{request_line}\r\n").into_bytes();
    for line in text.lines().skip(1) {
        let normalized = line.trim_end_matches('\r');
        if normalized.is_empty() {
            continue;
        }
        let name = normalized
            .split_once(':')
            .map(|(name, _)| name.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if name == "transfer-encoding" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked proxy requests are unsupported",
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
        rewritten.extend_from_slice(normalized.as_bytes());
        rewritten.extend_from_slice(b"\r\n");
    }
    rewritten.extend_from_slice(format!("Host: {canonical_authority}\r\n").as_bytes());
    rewritten.extend_from_slice(b"\r\n");
    Ok(rewritten)
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

fn tunnel(mut client: TcpStream, mut upstream: TcpStream) -> io::Result<()> {
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
