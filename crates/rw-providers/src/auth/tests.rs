#![cfg(test)]
use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;

use crate::{ProviderError, ProviderErrorKind};

use super::{
    AuthMaterial, AuthProvider, OAuthAuthorizationCode, OAuthAuthorizationCodeConfig, OAuthEntropy,
    OAuthRefreshConfig, ProxyAuthentication, RefreshTokenSink, RefreshingOAuth, Secret,
};

#[derive(Debug)]
struct FixedEntropy(StdMutex<VecDeque<[u8; 32]>>);

impl FixedEntropy {
    fn new(values: impl IntoIterator<Item = [u8; 32]>) -> Self {
        Self(StdMutex::new(values.into_iter().collect()))
    }
}

impl OAuthEntropy for FixedEntropy {
    fn fill(&self, destination: &mut [u8]) -> Result<(), ProviderError> {
        let value = self
            .0
            .lock()
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "fixture entropy lock failed",
                )
            })?
            .pop_front()
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "fixture entropy was exhausted",
                )
            })?;
        if destination.len() != value.len() {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "fixture entropy length did not match",
            ));
        }
        destination.copy_from_slice(&value);
        Ok(())
    }
}

struct RecordingRefreshSink {
    values: StdMutex<Vec<String>>,
    fail: bool,
}

impl fmt::Debug for RecordingRefreshSink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordingRefreshSink")
            .field("values", &"[REDACTED]")
            .field("fail", &self.fail)
            .finish()
    }
}

#[async_trait]
impl RefreshTokenSink for RecordingRefreshSink {
    async fn persist(&self, refresh_token: &Secret) -> Result<(), ProviderError> {
        if self.fail {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                format!("fixture rejected {}", refresh_token.expose_secret()),
            ));
        }
        self.values
            .lock()
            .unwrap_or_else(|error| panic!("refresh sink lock must not be poisoned: {error}"))
            .push(refresh_token.expose_secret().to_owned());
        Ok(())
    }
}

#[test]
fn secrets_never_debug_as_plaintext() {
    let secret = Secret::new("credential-canary");
    let rendered = format!("{secret:?}");
    assert!(!rendered.contains("credential-canary"));
    assert!(rendered.contains("REDACTED"));
}

#[test]
fn proxy_authentication_debug_redacts_both_fields() {
    let authentication =
        ProxyAuthentication::new("proxy-user-canary", Secret::new("proxy-password-canary"));
    let rendered = format!("{authentication:?}");
    assert!(!rendered.contains("proxy-user-canary"));
    assert!(!rendered.contains("proxy-password-canary"));
    assert!(rendered.contains("REDACTED"));
}

#[tokio::test]
async fn refresh_rotation_is_persisted_before_access_token_is_returned() {
    const INITIAL_REFRESH: &str = "initial-refresh-canary";
    const ROTATED_REFRESH: &str = "rotated-refresh-canary";
    const ACCESS_TOKEN: &str = "refreshed-access-canary";
    let (endpoint, request_task) = spawn_http_response(&format!(
        r#"{{"access_token":"{ACCESS_TOKEN}","refresh_token":"{ROTATED_REFRESH}","expires_in":3600,"token_type":"Bearer"}}"#
    ))
    .await;
    let sink = Arc::new(RecordingRefreshSink {
        values: StdMutex::new(Vec::new()),
        fail: false,
    });
    let auth = RefreshingOAuth::new_with_sink(
        refresh_config(endpoint, INITIAL_REFRESH),
        reqwest::Client::new(),
        sink.clone(),
    );

    let material = auth
        .material()
        .await
        .unwrap_or_else(|error| panic!("refresh must succeed: {error}"));
    let AuthMaterial::Bearer(access) = material else {
        panic!("refresh must return bearer material");
    };
    assert_eq!(access.expose_secret(), ACCESS_TOKEN);
    assert_eq!(
        sink.values
            .lock()
            .unwrap_or_else(|error| panic!("refresh sink lock must not be poisoned: {error}"))
            .as_slice(),
        [ROTATED_REFRESH]
    );
    let request = request_task
        .await
        .unwrap_or_else(|error| panic!("refresh fixture must join: {error}"));
    assert!(request.contains("grant_type=refresh_token"));
    assert!(request.contains(&format!("refresh_token={INITIAL_REFRESH}")));
    let debug = format!("{auth:?} {sink:?} {access:?}");
    for secret in [INITIAL_REFRESH, ROTATED_REFRESH, ACCESS_TOKEN] {
        assert!(!debug.contains(secret));
    }
}

#[tokio::test]
async fn refresh_rotation_storage_failure_is_sanitized_and_fails_closed() {
    const ROTATED_REFRESH: &str = "unstored-rotation-canary";
    const ACCESS_TOKEN: &str = "must-not-be-exposed-canary";
    let (endpoint, request_task) = spawn_http_response(&format!(
        r#"{{"access_token":"{ACCESS_TOKEN}","refresh_token":"{ROTATED_REFRESH}","expires_in":3600,"token_type":"Bearer"}}"#
    ))
    .await;
    let auth = RefreshingOAuth::new_with_sink(
        refresh_config(endpoint, "initial-refresh"),
        reqwest::Client::new(),
        Arc::new(RecordingRefreshSink {
            values: StdMutex::new(Vec::new()),
            fail: true,
        }),
    );

    let Err(error) = auth.material().await else {
        panic!("failed rotation persistence must suppress access material");
    };
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(
        error.message,
        "could not persist the rotated OAuth refresh token"
    );
    assert!(!error.to_string().contains(ROTATED_REFRESH));
    assert!(!error.to_string().contains(ACCESS_TOKEN));
    request_task
        .await
        .unwrap_or_else(|error| panic!("refresh fixture must join: {error}"));
}

#[tokio::test]
async fn authorization_code_pkce_happy_path_uses_authenticated_proxy() {
    const ACCESS_TOKEN: &str = "access-token-canary";
    const REFRESH_TOKEN: &str = "refresh-token-canary";
    const PROXY_PASSWORD: &str = "proxy-password-canary";
    let (proxy, request_task) = spawn_http_response(&format!(
        r#"{{"access_token":"{ACCESS_TOKEN}","refresh_token":"{REFRESH_TOKEN}","expires_in":3600,"token_type":"Bearer"}}"#
    ))
    .await;
    let token_endpoint = url("http://127.0.0.1:1/oauth/token");
    let proxy_authentication = ProxyAuthentication::new("proxy-user", Secret::new(PROXY_PASSWORD));
    let client =
        crate::http::build_client_with_proxy_auth(Some(&proxy), Some(&proxy_authentication))
            .unwrap_or_else(|error| panic!("authenticated proxy client must build: {error}"));
    let state_bytes = [7_u8; 32];
    let verifier_bytes = [9_u8; 32];
    let flow = OAuthAuthorizationCode::with_client_and_entropy(
        oauth_config(token_endpoint, Duration::from_secs(5)),
        client,
        Arc::new(FixedEntropy::new([state_bytes, verifier_bytes])),
    )
    .with_authorization_parameters([
        ("resource".to_owned(), "https://mcp.example/mcp".to_owned()),
        ("audience".to_owned(), "mcp.example".to_owned()),
    ])
    .with_token_parameters([
        ("resource".to_owned(), "https://mcp.example/mcp".to_owned()),
        ("audience".to_owned(), "mcp.example".to_owned()),
    ]);
    let session = flow
        .begin()
        .await
        .unwrap_or_else(|error| panic!("OAuth session must begin: {error}"));

    let authorization_url = session.authorization_url().clone();
    let redirect_uri = session.redirect_uri().clone();
    let state = query_value(&authorization_url, "state");
    assert_eq!(state, URL_SAFE_NO_PAD.encode(state_bytes));
    assert_eq!(query_value(&authorization_url, "response_type"), "code");
    assert_eq!(
        query_value(&authorization_url, "client_id"),
        "fixture-client"
    );
    assert_eq!(query_value(&authorization_url, "scope"), "models tools");
    assert_eq!(
        query_value(&authorization_url, "resource"),
        "https://mcp.example/mcp"
    );
    assert_eq!(query_value(&authorization_url, "audience"), "mcp.example");
    assert_eq!(
        query_value(&authorization_url, "code_challenge_method"),
        "S256"
    );
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    assert_eq!(
        query_value(&authorization_url, "code_challenge"),
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    );
    assert_eq!(
        query_value(&authorization_url, "redirect_uri"),
        redirect_uri.as_str()
    );

    let completion = tokio::spawn(session.complete());
    send_callback(&redirect_uri, "authorization-code-canary", &state).await;
    let tokens = completion
        .await
        .unwrap_or_else(|error| panic!("OAuth completion task must join: {error}"))
        .unwrap_or_else(|error| panic!("OAuth token exchange must succeed: {error}"));
    assert_eq!(tokens.access_token().expose_secret(), ACCESS_TOKEN);
    assert_eq!(
        tokens.refresh_token().map(Secret::expose_secret),
        Some(REFRESH_TOKEN)
    );
    assert_eq!(tokens.expires_in(), Some(3600));

    let request = request_task
        .await
        .unwrap_or_else(|error| panic!("proxy fixture must join: {error}"));
    let lower = request.to_ascii_lowercase();
    assert!(request.starts_with("POST http://127.0.0.1:1/oauth/token HTTP/1.1"));
    let proxy_authorization = STANDARD.encode(format!("proxy-user:{PROXY_PASSWORD}"));
    assert!(lower.contains(&format!(
        "proxy-authorization: basic {}",
        proxy_authorization.to_ascii_lowercase()
    )));
    assert!(request.contains("grant_type=authorization_code"));
    assert!(request.contains("code=authorization-code-canary"));
    assert!(request.contains(&format!("code_verifier={verifier}")));
    assert!(request.contains("resource=https%3A%2F%2Fmcp.example%2Fmcp"));
    assert!(request.contains("audience=mcp.example"));
    assert!(!request.contains(PROXY_PASSWORD));

    let debug = format!("{tokens:?} {proxy_authentication:?}");
    for secret in [
        ACCESS_TOKEN,
        REFRESH_TOKEN,
        PROXY_PASSWORD,
        &state,
        &verifier,
    ] {
        assert!(!debug.contains(secret));
    }
}

#[tokio::test]
async fn callback_state_mismatch_fails_before_token_exchange_without_leaking_state() {
    let unused_token_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("unused token listener must bind: {error}"));
    let token_address = unused_token_listener
        .local_addr()
        .unwrap_or_else(|error| panic!("unused token address must resolve: {error}"));
    let flow = OAuthAuthorizationCode::with_client_and_entropy(
        oauth_config(
            url(&format!("http://{token_address}/oauth/token")),
            Duration::from_secs(5),
        ),
        reqwest::Client::new(),
        Arc::new(FixedEntropy::new([[1_u8; 32], [2_u8; 32]])),
    );
    let session = flow
        .begin()
        .await
        .unwrap_or_else(|error| panic!("OAuth session must begin: {error}"));
    let redirect_uri = session.redirect_uri().clone();
    let expected_state = query_value(session.authorization_url(), "state");
    let completion = tokio::spawn(session.complete());
    send_callback(
        &redirect_uri,
        "authorization-code-should-not-be-used",
        "attacker-state-canary",
    )
    .await;
    let result = completion
        .await
        .unwrap_or_else(|join_error| panic!("OAuth completion must join: {join_error}"));
    let Err(error) = result else {
        panic!("mismatched state must fail");
    };
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("state validation failed"));
    assert!(!diagnostic.contains("attacker-state-canary"));
    assert!(!diagnostic.contains(&expected_state));
}

#[tokio::test]
async fn callback_timeout_is_sanitized() {
    let flow = OAuthAuthorizationCode::with_client_and_entropy(
        oauth_config(
            url("http://127.0.0.1:1/oauth/token"),
            Duration::from_millis(10),
        ),
        reqwest::Client::new(),
        Arc::new(FixedEntropy::new([[3_u8; 32], [4_u8; 32]])),
    );
    let session = flow
        .begin()
        .await
        .unwrap_or_else(|error| panic!("OAuth session must begin: {error}"));
    let Err(error) = session.complete().await else {
        panic!("missing callback must time out");
    };
    assert_eq!(error.kind, ProviderErrorKind::Timeout);
    assert_eq!(
        error.message,
        "timed out waiting for the OAuth loopback callback"
    );
}

fn oauth_config(token_endpoint: Url, callback_timeout: Duration) -> OAuthAuthorizationCodeConfig {
    OAuthAuthorizationCodeConfig {
        authorization_endpoint: url("https://authorization.example/authorize"),
        token_endpoint,
        client_id: "fixture-client".to_owned(),
        scopes: vec!["models".to_owned(), "tools".to_owned()],
        callback_timeout,
    }
}

fn refresh_config(token_endpoint: Url, refresh_token: &str) -> OAuthRefreshConfig {
    OAuthRefreshConfig {
        token_endpoint,
        client_id: "fixture-client".to_owned(),
        client_secret: None,
        refresh_token: Secret::new(refresh_token),
        scope: Some("models tools".to_owned()),
    }
}

fn url(value: &str) -> Url {
    Url::parse(value).unwrap_or_else(|error| panic!("fixture URL must parse: {error}"))
}

fn query_value(url: &Url, name: &str) -> String {
    url.query_pairs()
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
        .unwrap_or_else(|| panic!("query parameter {name} must exist"))
}

async fn send_callback(redirect_uri: &Url, code: &str, state: &str) {
    let mut callback = redirect_uri.clone();
    callback
        .query_pairs_mut()
        .append_pair("code", code)
        .append_pair("state", state);
    let address = format!(
        "{}:{}",
        callback
            .host_str()
            .unwrap_or_else(|| panic!("redirect host must exist")),
        callback
            .port()
            .unwrap_or_else(|| panic!("redirect port must exist"))
    );
    let mut stream = TcpStream::connect(&address)
        .await
        .unwrap_or_else(|error| panic!("browser callback must connect: {error}"));
    let target = format!(
        "{}?{}",
        callback.path(),
        callback
            .query()
            .unwrap_or_else(|| panic!("callback query must exist"))
    );
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .await
        .unwrap_or_else(|error| panic!("browser callback must write: {error}"));
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .unwrap_or_else(|error| panic!("browser callback response must read: {error}"));
}

async fn spawn_http_response(body: &str) -> (Url, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("HTTP fixture must bind: {error}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("HTTP fixture address must resolve: {error}"));
    let body = body.to_owned();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .unwrap_or_else(|error| panic!("HTTP fixture must accept: {error}"));
        let request = read_http_request(&mut stream).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .unwrap_or_else(|error| panic!("HTTP fixture response must write: {error}"));
        request
    });
    (url(&format!("http://{address}")), task)
}

async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .unwrap_or_else(|error| panic!("HTTP fixture request must read: {error}"));
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_length = header_end + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_length]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or_default();
        if bytes.len() >= header_length.saturating_add(content_length) {
            break;
        }
    }
    String::from_utf8(bytes)
        .unwrap_or_else(|error| panic!("HTTP fixture request must be UTF-8: {error}"))
}
