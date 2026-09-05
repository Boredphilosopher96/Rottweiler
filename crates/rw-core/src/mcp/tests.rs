#![cfg(test)]
#![allow(clippy::expect_used)]

use super::*;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, Ordering},
};

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rw_mcp::{
    BridgeError, EngineMcpBridge, EngineTool, McpServerAuthority, RottweilerMcpServer,
    SessionSummary,
};
use rw_store::credentials::{
    CredentialError, CredentialStore, CredentialStoreUnavailable, Secret as StoredSecret,
};

const HTTP_BEARER_CANARY: &str = "mcp-http-bearer-canary-never-log";

#[derive(Clone, Copy)]
struct EmptyCredentialEnvironment;

impl CredentialEnvironment for EmptyCredentialEnvironment {
    fn get(&self, _name: &str) -> Result<Option<String>, CredentialError> {
        Ok(None)
    }
}

#[derive(Clone, Default)]
struct MemoryCredentialStore(Arc<StdMutex<BTreeMap<String, String>>>);

impl CredentialStore for MemoryCredentialStore {
    fn get(
        &self,
        identifier: &str,
    ) -> Result<Option<StoredSecret<String>>, CredentialStoreUnavailable> {
        Ok(self
            .0
            .lock()
            .map_err(|_| CredentialStoreUnavailable)?
            .get(identifier)
            .cloned()
            .map(StoredSecret::new))
    }

    fn set(
        &self,
        identifier: &str,
        secret: &StoredSecret<String>,
    ) -> Result<(), CredentialStoreUnavailable> {
        self.0
            .lock()
            .map_err(|_| CredentialStoreUnavailable)?
            .insert(identifier.to_owned(), secret.expose_secret().clone());
        Ok(())
    }
}

struct PolicyClient {
    server: String,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl rw_mcp::McpClient for PolicyClient {
    async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let names = if self.server == "github" {
            vec!["get_issue", "delete_issue"]
        } else {
            vec!["search_messages"]
        };
        Ok(names
            .into_iter()
            .map(|name| {
                json!({
                    "name": name,
                    "description": format!("fixture {name}"),
                    "inputSchema": {"type": "object"}
                })
            })
            .collect())
    }

    async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
        Ok(Vec::new())
    }

    async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
        Ok(Vec::new())
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"server": self.server, "name": name, "arguments": arguments}))
    }

    async fn read_resource(&self, _uri: &str) -> Result<Value, McpError> {
        unreachable!("policy fixture has no resources")
    }

    async fn get_prompt(&self, _name: &str, _arguments: Value) -> Result<Value, McpError> {
        unreachable!("policy fixture has no prompts")
    }

    async fn close(&self, _timeout: Duration) -> Result<(), McpError> {
        Ok(())
    }
}

struct PolicyConnector {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl McpConnector for PolicyConnector {
    async fn connect(
        &self,
        config: &McpServerConfig,
    ) -> Result<Arc<dyn rw_mcp::McpClient>, McpError> {
        Ok(Arc::new(PolicyClient {
            server: config.id.as_str().to_owned(),
            calls: Arc::clone(&self.calls),
        }))
    }
}

#[derive(Default)]
struct PolicySpool;

#[async_trait]
impl OverflowSpool for PolicySpool {
    async fn write(
        &self,
        _server: &McpServerId,
        _operation: &str,
        _bytes: &[u8],
    ) -> Result<OverflowReference, McpError> {
        unreachable!("policy fixture responses remain below the overflow limit")
    }

    async fn read(&self, _reference: &OverflowReference) -> Result<Vec<u8>, McpError> {
        unreachable!("policy fixture never creates overflow references")
    }

    async fn remove(&self, _reference: &OverflowReference) -> Result<(), McpError> {
        Ok(())
    }
}

struct EchoBridge;

#[async_trait]
impl EngineMcpBridge for EchoBridge {
    async fn tools(&self) -> Result<Vec<EngineTool>, BridgeError> {
        Ok(vec![EngineTool {
            name: "echo".to_owned(),
            description: "Echo one bounded test message".to_owned(),
            input_schema: json!({"type":"object"}),
        }])
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, BridgeError> {
        if name != "echo" {
            return Err(BridgeError::safe("unknown test tool"));
        }
        Ok(arguments)
    }

    async fn create_session(&self, _title: Option<String>) -> Result<SessionSummary, BridgeError> {
        Err(BridgeError::safe("not used by test"))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, BridgeError> {
        Ok(Vec::new())
    }

    async fn send_message(&self, _session_id: &str, _message: &str) -> Result<Value, BridgeError> {
        Err(BridgeError::safe("not used by test"))
    }
}

struct AllowConnection;

#[async_trait]
impl McpConnectionApprovalPolicy for AllowConnection {
    async fn approve(&self, _config: &McpServerConfig) -> Result<(), McpError> {
        Ok(())
    }
}

struct CanaryAuthorization;

#[async_trait]
impl McpAuthorizationProvider for CanaryAuthorization {
    async fn token(
        &self,
        _server: &McpServerId,
        _resource: &str,
    ) -> Result<Option<SecretToken>, McpError> {
        Ok(Some(SecretToken::new(HTTP_BEARER_CANARY)))
    }
}

fn oauth_login_config(token_endpoint: Url) -> McpOAuthLoginConfig {
    McpOAuthLoginConfig {
        server: McpServerId::new("oauth-fixture").expect("server id"),
        authorization_endpoint: Url::parse("https://auth.example/authorize")
            .expect("authorization URL"),
        token_endpoint,
        client_id: "public-client".to_owned(),
        scopes: vec!["mcp:tools".to_owned()],
        proxy: None,
        credential_reference: CredentialReference::new("mcp.oauth-fixture.oauth"),
        resource: "https://mcp.example/mcp".to_owned(),
        audience: "mcp.example".to_owned(),
        credentials_path: std::env::temp_dir().join("unused-oauth-credentials.toml"),
    }
}

#[test]
fn toon_encoder_is_structured_and_deterministic() {
    let encoded = ToonMcpEncoder
        .encode(&json!({"items":[{"name":"alpha"}]}))
        .expect("TOON");
    let encoded = String::from_utf8(encoded).expect("UTF-8");
    assert!(encoded.contains("items[1]"));
    assert_eq!(ToonMcpEncoder.format(), "toon");
}

#[test]
fn protected_mcp_framing_and_utf8_truncation_are_stable() {
    let result = untrusted_result("remote instructions", json!({}));
    assert!(result.content.starts_with(UNTRUSTED_OPEN));
    assert!(result.content.ends_with(UNTRUSTED_CLOSE));
    assert_eq!(truncate_utf8("🐕🐕", 5), "🐕");
}

#[tokio::test]
async fn expired_mcp_oauth_refreshes_once_and_persists_rotation() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const INITIAL_REFRESH: &str = "mcp-initial-refresh-canary";
    const ROTATED_REFRESH: &str = "mcp-rotated-refresh-canary";
    const REFRESHED_ACCESS: &str = "mcp-refreshed-access-canary";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("token listener");
    let token_endpoint = Url::parse(&format!(
        "http://{}/token",
        listener.local_addr().expect("token address")
    ))
    .expect("token endpoint");
    let reference = CredentialReference::new("mcp.oauth-refresh.oauth");
    let credential_store = MemoryCredentialStore::default();
    let manager = Arc::new(CredentialManager::with_backends(
        EmptyCredentialEnvironment,
        credential_store,
        std::env::temp_dir().join("unused-mcp-refresh-fixture.toml"),
    ));
    let stored = StoredMcpOAuthCredential {
        version: 2,
        access_token: "expired-access-canary".to_owned(),
        refresh_token: Some(INITIAL_REFRESH.to_owned()),
        expires_at_unix_seconds: Some(0),
        resource: "https://mcp.example/mcp".to_owned(),
        audience: "mcp.example".to_owned(),
        token_endpoint: Some(token_endpoint.as_str().to_owned()),
        client_id: Some("public-client".to_owned()),
        scopes: vec!["mcp:tools".to_owned()],
        proxy: None,
    };
    manager
        .store(
            &reference,
            &StoredSecret::new(serde_json::to_string(&stored).expect("stored JSON")),
        )
        .expect("seed credential");
    let server = McpServerId::new("oauth-refresh-fixture").expect("server id");
    let provider = VaultMcpTokenProvider::new(
        manager.clone(),
        BTreeMap::from([(
            server.clone(),
            McpOAuthBinding {
                token_reference: reference.clone(),
                resource: stored.resource.clone(),
                audience: stored.audience.clone(),
                refresh: Some(McpOAuthRefreshBinding {
                    token_endpoint: token_endpoint.clone(),
                    client_id: "public-client".to_owned(),
                    scopes: vec!["mcp:tools".to_owned()],
                    proxy: None,
                }),
            },
        )]),
    );
    let responder = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("refresh request");
        let mut request = vec![0_u8; 16 * 1024];
        let count = stream.read(&mut request).await.expect("read refresh");
        let request = String::from_utf8_lossy(&request[..count]);
        assert!(request.contains("grant_type=refresh_token"));
        assert!(request.contains(INITIAL_REFRESH));
        let body = format!(
            r#"{{"access_token":"{REFRESHED_ACCESS}","refresh_token":"{ROTATED_REFRESH}","expires_in":3600,"token_type":"Bearer"}}"#
        );
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("write refresh");
    });
    let first = provider
        .token(&server, "https://mcp.example/mcp")
        .await
        .expect("refresh token")
        .expect("bearer");
    assert_eq!(first.expose(), REFRESHED_ACCESS);
    responder.await.expect("refresh responder");
    let second = provider
        .token(&server, "https://mcp.example/mcp")
        .await
        .expect("cached token")
        .expect("cached bearer");
    assert_eq!(second.expose(), REFRESHED_ACCESS);
    let resolved = manager.resolve(&reference).expect("rotated credential");
    let rotated: StoredMcpOAuthCredential =
        serde_json::from_str(resolved.secret().expose_secret()).expect("rotated JSON");
    assert_eq!(rotated.refresh_token.as_deref(), Some(ROTATED_REFRESH));
    let debug = format!("{provider:?}");
    assert!(!debug.contains(INITIAL_REFRESH));
    assert!(!debug.contains(ROTATED_REFRESH));
    assert!(!debug.contains(REFRESHED_ACCESS));
}

#[test]
fn mcp_http_headers_reject_oversized_and_control_ids() {
    assert!(mcp_http_headers(None, Some("bad id"), None, HashMap::new(), false).is_err());
    assert!(
        mcp_http_headers(
            None,
            Some("ok-session"),
            Some("x".repeat(MCP_HTTP_MAX_EVENT_ID_BYTES + 1)),
            HashMap::new(),
            false,
        )
        .is_err()
    );
}

#[test]
fn loopback_authority_is_scoped_to_one_origin() {
    let endpoint = Url::parse("http://127.0.0.1:8123/mcp").expect("URL");
    let authority = LoopbackMcpAuthority::for_endpoint(&endpoint).expect("authority");
    assert_eq!(authority.origin, "http://127.0.0.1:8123");
    assert!(
        LoopbackMcpAuthority::for_endpoint(&Url::parse("https://example.com/mcp").expect("URL"))
            .is_err()
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn agent_mcp_policy_hides_schemas_and_denies_direct_calls_without_narrowing_main() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let manager = Arc::new(McpManager::new(
        Arc::new(PolicyConnector {
            calls: Arc::clone(&calls),
        }),
        Arc::new(PolicySpool),
        Arc::new(rw_mcp::CompactJsonEncoder),
        rw_mcp::McpLimits {
            response_bytes: 64 * 1024,
            request_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
        },
    ));
    for server in ["github", "slack"] {
        let tool_capabilities = if server == "github" {
            rw_mcp::McpToolCapabilityOverrides {
                server_default: Some(CapabilityManifest::new([ToolCapability::ReadFilesystem])),
                tools: BTreeMap::from([("delete_issue".to_owned(), CapabilityManifest::default())]),
            }
        } else {
            rw_mcp::McpToolCapabilityOverrides::default()
        };
        manager
            .register(McpServerConfig {
                id: McpServerId::new(server).expect("server id"),
                transport: McpTransportConfig::Stdio {
                    executable: "fixture".into(),
                    args: Vec::new(),
                    working_directory: None,
                    environment: Vec::new(),
                    sandbox: rw_mcp::McpStdioSandboxPolicy::default(),
                },
                enabled: true,
                defer_tools: true,
                tool_capabilities,
            })
            .await
            .expect("register");
    }
    assert!(
        manager
            .connect_all()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );

    let workspace = tempfile::tempdir().expect("workspace");
    let restricted = ToolContext::new(workspace.path())
        .expect("context")
        .with_mcp_tool_policy(
            rw_tools::McpToolPolicy::restricted(["mcp:github/get_issue".to_owned()])
                .expect("policy"),
        );
    let search = ToolSearchTool {
        manager: Arc::clone(&manager),
    };
    let result = search
        .execute(&restricted, json!({"query": ""}))
        .await
        .expect("restricted search");
    let encoded = result.data.to_string();
    assert!(encoded.contains("get_issue"));
    assert!(!encoded.contains("delete_issue"));
    assert!(!encoded.contains("search_messages"));
    assert!(!result.content.contains("delete_issue"));
    assert!(!result.content.contains("search_messages"));

    let call = McpCallTool {
        manager: Arc::clone(&manager),
    };
    assert_eq!(
        call.invocation_capabilities(
            &json!({"server":"github", "name":"get_issue", "arguments":{}})
        )
        .expect("server classification"),
        CapabilityManifest::new([ToolCapability::ReadFilesystem])
    );
    assert_eq!(
        call.invocation_capabilities(
            &json!({"server":"github", "name":"delete_issue", "arguments":{}})
        )
        .expect("tool classification"),
        CapabilityManifest::default()
    );
    assert_eq!(
        call.invocation_capabilities(
            &json!({"server":"slack", "name":"search_messages", "arguments":{}})
        )
        .expect("restrictive default"),
        CapabilityManifest::new([ToolCapability::Network, ToolCapability::Execute])
    );
    call.execute(
        &restricted,
        json!({"server":"github", "name":"get_issue", "arguments":{}}),
    )
    .await
    .expect("permitted call");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let denied = call
        .execute(
            &restricted,
            json!({"server":"github", "name":"delete_issue", "arguments":{}}),
        )
        .await
        .expect_err("direct ungranted call must fail before the manager");
    assert!(
        denied
            .to_string()
            .contains("not allowed for the active agent")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let main = ToolContext::new(workspace.path()).expect("main context");
    let result = search
        .execute(&main, json!({"query": ""}))
        .await
        .expect("main search remains unrestricted");
    let encoded = result.data.to_string();
    assert!(encoded.contains("get_issue"));
    assert!(encoded.contains("delete_issue"));
    assert!(encoded.contains("search_messages"));
    call.execute(
        &main,
        json!({"server":"github", "name":"delete_issue", "arguments":{}}),
    )
    .await
    .expect("main approved MCP config remains callable");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_oauth_sends_resource_and_audience_at_both_protocol_boundaries() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let token_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("token listener");
    let token_endpoint = Url::parse(&format!(
        "http://{}/token",
        token_listener.local_addr().expect("token address")
    ))
    .expect("token URL");
    let login = begin_mcp_oauth_login(oauth_login_config(token_endpoint))
        .await
        .expect("login begins");
    let authorization_url = Url::parse(login.authorization_url()).expect("authorization URL");
    let query = authorization_url.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(
        query.get("resource").map(AsRef::as_ref),
        Some("https://mcp.example/mcp")
    );
    assert_eq!(
        query.get("audience").map(AsRef::as_ref),
        Some("mcp.example")
    );
    assert_eq!(
        query.get("code_challenge_method").map(AsRef::as_ref),
        Some("S256")
    );
    let state = query.get("state").expect("state").to_string();
    let redirect = Url::parse(login.redirect_uri()).expect("redirect URL");
    let debug = format!("{login:?}");
    assert!(!debug.contains(&state));

    let completion = tokio::spawn(login.complete());
    let mut callback = tokio::net::TcpStream::connect((
        redirect.host_str().expect("redirect host"),
        redirect.port().expect("redirect port"),
    ))
    .await
    .expect("callback connection");
    callback
        .write_all(
            format!(
                "GET {}?code=fixture-code&state={state} HTTP/1.1\r\nHost: {}:{}\r\n\r\n",
                redirect.path(),
                redirect.host_str().expect("redirect host"),
                redirect.port().expect("redirect port")
            )
            .as_bytes(),
        )
        .await
        .expect("callback write");
    let mut callback_response = Vec::new();
    callback
        .read_to_end(&mut callback_response)
        .await
        .expect("callback response");

    let (mut token_stream, _) = token_listener.accept().await.expect("token request");
    let mut request = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let mut chunk = [0_u8; 4096];
            let count = token_stream.read(&mut chunk).await.expect("token read");
            assert!(count > 0, "token request ended before its body");
            request.extend_from_slice(&chunk[..count]);
            assert!(
                request.len() <= 16 * 1024,
                "token request exceeded test cap"
            );
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("content length");
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
    })
    .await
    .expect("bounded token request");
    let request = String::from_utf8_lossy(&request);
    assert!(request.contains("resource=https%3A%2F%2Fmcp.example%2Fmcp"));
    assert!(request.contains("audience=mcp.example"));
    token_stream
        .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .expect("token rejection");
    drop(token_stream);
    let error = completion
        .await
        .expect("completion joins")
        .expect_err("rejected token exchange fails");
    let diagnostic = error.to_string();
    assert!(!diagnostic.contains("fixture-code"));
    assert!(!diagnostic.contains(&state));
}

#[tokio::test]
async fn dropping_mcp_oauth_login_releases_the_loopback_listener() {
    let login = begin_mcp_oauth_login(oauth_login_config(
        Url::parse("http://127.0.0.1:1/token").expect("token URL"),
    ))
    .await
    .expect("login begins");
    let redirect = Url::parse(login.redirect_uri()).expect("redirect URL");
    let address = (
        redirect.host_str().expect("redirect host"),
        redirect.port().expect("redirect port"),
    );
    drop(login);
    // A connect-after-drop assertion is racy under the parallel test suite:
    // the OS may immediately recycle this ephemeral port for another OAuth
    // fixture. Rebinding the exact address proves that this session released
    // its listener without sending traffic to an unrelated recycled port.
    let rebound = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match tokio::net::TcpListener::bind(address).await {
                Ok(listener) => break listener,
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("loopback address could not rebind: {error}"),
            }
        }
    })
    .await
    .expect("dropped login must release its callback address");
    drop(rebound);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn production_connector_drives_real_rmcp_http_with_bearer_canary() {
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tower_service::Service as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("HTTP listener");
    let address = listener.local_addr().expect("HTTP address");
    let endpoint = Url::parse(&format!("http://{address}/mcp")).expect("MCP URL");
    let bearer_seen = Arc::new(AtomicBool::new(false));
    let bearer_rejected = Arc::new(AtomicBool::new(false));
    let service: StreamableHttpService<RottweilerMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            || {
                Ok(RottweilerMcpServer::new(
                    Arc::new(EchoBridge),
                    McpServerAuthority::new(["echo".to_owned()], []),
                ))
            },
            Arc::default(),
            StreamableHttpServerConfig::default().with_sse_keep_alive(None),
        );
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn({
        let bearer_seen = bearer_seen.clone();
        let bearer_rejected = bearer_rejected.clone();
        async move {
            loop {
                let (stream, _) = tokio::select! {
                    accepted = listener.accept() => accepted.expect("HTTP accept"),
                    changed = shutdown_rx.changed() => {
                        let _ = changed;
                        break;
                    }
                };
                let mcp = service.clone();
                let bearer_seen = bearer_seen.clone();
                let bearer_rejected = bearer_rejected.clone();
                tokio::spawn(async move {
                    let guarded =
                        service_fn(move |request: http::Request<hyper::body::Incoming>| {
                            let mut mcp = mcp.clone();
                            let bearer_seen = bearer_seen.clone();
                            let bearer_rejected = bearer_rejected.clone();
                            async move {
                                let authorization = request
                                    .headers()
                                    .get(http::header::AUTHORIZATION)
                                    .and_then(|value| value.to_str().ok());
                                if authorization.is_some_and(|value| {
                                    value == format!("Bearer {HTTP_BEARER_CANARY}")
                                }) {
                                    bearer_seen.store(true, Ordering::SeqCst);
                                } else {
                                    bearer_rejected.store(true, Ordering::SeqCst);
                                }
                                mcp.call(request).await
                            }
                        });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), guarded)
                        .await;
                });
            }
        }
    });

    let http_client = ProductionMcpHttpClient::new().with_loopback_authority(
        LoopbackMcpAuthority::for_endpoint(&endpoint).expect("loopback authority"),
    );
    let connector = ProductionMcpHttpConnector::new(
        http_client,
        Arc::new(CanaryAuthorization),
        Arc::new(AllowConnection),
    );
    let config = McpServerConfig {
        id: McpServerId::new("http-canary").expect("server id"),
        transport: McpTransportConfig::StreamableHttp {
            endpoint: endpoint.to_string(),
            oauth: true,
        },
        enabled: true,
        defer_tools: true,
        tool_capabilities: rw_mcp::McpToolCapabilityOverrides::default(),
    };
    let client = connector.connect(&config).await.expect("MCP initialize");
    let catalog = client.list_tools().await.expect("MCP tool catalog");
    assert!(
        catalog
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("rottweiler_tools_call")))
    );
    let result = client
        .call_tool(
            "rottweiler_tools_call",
            json!({"name":"echo","arguments":{"message":"hello over guarded HTTP"}}),
        )
        .await
        .expect("MCP tool call");
    assert!(result.to_string().contains("hello over guarded HTTP"));
    client
        .close(Duration::from_secs(2))
        .await
        .expect("MCP shutdown");
    assert!(bearer_seen.load(Ordering::SeqCst));
    assert!(!bearer_rejected.load(Ordering::SeqCst));
    let diagnostics = format!("{config:?} {result:?}");
    assert!(!diagnostics.contains(HTTP_BEARER_CANARY));

    let _ = shutdown_tx.send(true);
    server.await.expect("HTTP server joins");
}
