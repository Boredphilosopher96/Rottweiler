#![allow(clippy::expect_used)]
use rw_providers::{
    GuardedHttpFetchError, GuardedHttpMethod, GuardedHttpRequest, ProviderErrorKind,
    guarded_http_request,
};
use rw_resources::{ResourceClass, ResourceLease};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn available_network() -> Vec<ResourceLease> {
    let mut leases = Vec::new();
    while let Ok(lease) = rw_resources::try_acquire(ResourceClass::Network) {
        leases.push(lease);
        assert!(leases.len() <= 64);
    }
    leases
}

fn request(address: std::net::SocketAddr) -> GuardedHttpRequest {
    GuardedHttpRequest {
        method: GuardedHttpMethod::Get,
        url: format!("http://{address}/owned-body").parse().expect("URL"),
        headers: Vec::new(),
        body: Vec::new(),
        proxy: None,
        proxy_authentication: None,
        dns_pin: None,
        allow_private_destinations: true,
        response_deadline: Duration::from_secs(2),
        frame_deadline: Duration::from_secs(1),
        max_frame_bytes: 64,
        max_body_bytes: 64,
    }
}

#[tokio::test]
async fn returned_http_body_owns_capacity_and_exhaustion_never_authorizes_failover() {
    let baseline = available_network();
    assert_eq!(baseline.len(), 64);
    drop(baseline);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut header = Vec::new();
        let mut bytes = [0_u8; 1024];
        while !header.ends_with(b"\r\n\r\n") {
            let count = socket.read(&mut bytes).await.expect("request");
            assert!(count > 0 && header.len() + count <= 4096);
            header.extend_from_slice(&bytes[..count]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\n")
            .await
            .expect("headers");
        let _ = released.await;
        let _ = socket.write_all(b"abc").await;
    });
    let response = guarded_http_request(request(address))
        .await
        .expect("headers returned");
    let remaining = available_network();
    assert_eq!(
        remaining.len(),
        63,
        "unread body still owns its physical request"
    );
    let rejected = guarded_http_request(request(address)).await;
    let Err(GuardedHttpFetchError::Provider(error)) = rejected else {
        panic!("expected local admission rejection")
    };
    assert_eq!(error.kind, ProviderErrorKind::ResourceExhausted);
    assert!(!error.is_retryable());
    drop(remaining);
    drop(response);
    assert_eq!(available_network().len(), 64);
    release.send(()).expect("finish server");
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("bounded server exit")
        .expect("server");
}
