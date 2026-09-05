use super::BTreeMap;
use super::CancellationToken;
use super::EgressPolicy;
use super::FetchRequest;
use super::IpAddr;
use super::PolicyWebFetcher;
use super::ResolvedToolProxy;
use super::ToolError;
use super::UpstreamProxy;
use super::Url;
use super::WebFetcher;
use super::Write;
use super::cross_origin_webfetch_header_is_safe;
use super::is_public_ip;
use super::validate_egress_decision;
use std::io::Read;

#[test]
fn rejects_private_and_reserved_network_targets() {
    for address in [
        "127.0.0.1",
        "10.0.0.1",
        "169.254.1.1",
        "192.0.2.1",
        "100.64.0.1",
        "::1",
        "fc00::1",
        "2001:db8::1",
        "64:ff9b::a9fe:a9fe",
        "64:ff9b::a00:1",
        "64:ff9b:1::1",
        "2002:a9fe:a9fe::1",
        "2001:0000::1",
        "2001:4860:4860:0:0200:5efe:a9fe:a9fe",
    ] {
        let address: IpAddr = address.parse().expect("fixture address");
        assert!(!is_public_ip(address), "{address} must be rejected");
    }
    assert!(is_public_ip("1.1.1.1".parse().expect("public address")));
    assert!(is_public_ip(
        "2606:4700:4700::1111".parse().expect("public address")
    ));
    assert!(is_public_ip(
        "64:ff9b::101:101".parse().expect("public NAT64 address")
    ));
}

#[test]
fn webfetch_egress_requires_declared_domain_and_keeps_ssrf_hard_denied() {
    let public = "1.1.1.1".parse().expect("public address");
    let private = "169.254.169.254".parse().expect("metadata address");
    let mut policy = EgressPolicy::default();
    assert!(policy.allow_domain("example.com"));
    assert!(validate_egress_decision(&policy, "example.com", &[public]).is_ok());
    assert!(matches!(
        validate_egress_decision(&policy, "other.example", &[public]),
        Err(ToolError::Network(message)) if message.contains("not declared")
    ));
    assert!(matches!(
        validate_egress_decision(&policy, "example.com", &[private]),
        Err(ToolError::Network(message)) if message.contains("private")
    ));
}

#[test]
fn cross_origin_webfetch_redirects_drop_custom_credentials() {
    for credential in [
        "authorization",
        "cookie",
        "x-api-key",
        "x-auth-token",
        "proxy-authorization",
    ] {
        assert!(!cross_origin_webfetch_header_is_safe(credential));
    }
    for safe in ["accept", "accept-language", "user-agent"] {
        assert!(cross_origin_webfetch_header_is_safe(safe));
    }
}

#[tokio::test]
async fn webfetch_chains_through_authenticated_proxy_after_target_pin() {
    use std::net::TcpListener;
    use std::thread;

    let corporate = TcpListener::bind("127.0.0.1:0").expect("corporate proxy");
    let address = corporate.local_addr().expect("corporate address");
    let worker =
        thread::spawn(move || {
            let (mut stream, _) = corporate.accept().expect("accept");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let length = stream.read(&mut buffer).expect("request");
                if length == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..length]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).expect("request UTF-8");
            assert!(request.starts_with("GET http://127.0.0.1:8/target HTTP/1.1\r\n"));
            assert!(request.contains(
                "\r\nProxy-Authorization: Basic dXNlcjp3ZWJmZXRjaC1zZWNyZXQtY2FuYXJ5\r\n"
            ));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("response");
        });
    let url = Url::parse(&format!("http://{address}")).expect("proxy URL");
    let upstream = UpstreamProxy::new(url.clone())
        .expect("upstream")
        .with_basic_auth("user", "webfetch-secret-canary");
    let fetcher = PolicyWebFetcher::new(true, Some(ResolvedToolProxy { url, upstream }));
    let response = fetcher
        .fetch(
            FetchRequest {
                url: Url::parse("http://127.0.0.1:8/target").expect("target URL"),
                headers: BTreeMap::new(),
                max_bytes: 64,
            },
            CancellationToken::default(),
        )
        .await
        .expect("proxy webfetch");
    worker.join().expect("proxy worker");
    assert_eq!(response.body, b"ok");
}
