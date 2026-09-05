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
    let upstream =
        UpstreamProxy::new(Url::parse(&format!("http://{corporate_address}")).expect("proxy URL"))
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
    let upstream =
        UpstreamProxy::new(Url::parse(&format!("http://{corporate_address}")).expect("proxy URL"))
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
    sni.extend_from_slice(&(u16::try_from(server_name_length).expect("SNI size")).to_be_bytes());
    sni.push(0);
    sni.extend_from_slice(&(u16::try_from(host.len()).expect("host size")).to_be_bytes());
    sni.extend_from_slice(host);

    let mut extensions = Vec::new();
    extensions.extend_from_slice(&0_u16.to_be_bytes());
    extensions.extend_from_slice(&(u16::try_from(sni.len()).expect("SNI extension")).to_be_bytes());
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
