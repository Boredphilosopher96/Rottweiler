#![allow(clippy::expect_used)]
use crate::{
    McpClient, McpError, McpHttpBody, McpHttpClient, McpHttpMethod, McpHttpResponse,
    McpResponseSlot, SecretToken, client::connect_http,
};
use async_trait::async_trait;
use futures_util::{StreamExt as _, stream};
use rmcp::model::ProtocolVersion;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

#[derive(Clone, Copy)]
enum Case {
    AcceptedGet,
    EmptyNotification,
    ErrorBody,
    Priming,
}
struct Seen {
    method: McpHttpMethod,
    headers: Vec<(String, String)>,
    body: Value,
}
struct Fixture {
    case: Case,
    seen: Mutex<Vec<Seen>>,
    events: mpsc::Sender<Vec<u8>>,
    receive: Arc<AsyncMutex<mpsc::Receiver<Vec<u8>>>>,
}
impl Fixture {
    fn new(case: Case) -> Arc<Self> {
        let (events, receive) = mpsc::channel(4);
        Arc::new(Self {
            case,
            seen: Mutex::new(Vec::new()),
            events,
            receive: Arc::new(AsyncMutex::new(receive)),
        })
    }
    async fn connect(self: &Arc<Self>) -> Arc<dyn McpClient> {
        tokio::time::timeout(
            Duration::from_secs(3),
            connect_http(
                rw_types::McpServerId::new("raw-http-fixture").expect("id"),
                "https://fixture.invalid/mcp".into(),
                Some(SecretToken::new("fixture-token")),
                Arc::clone(self) as Arc<dyn McpHttpClient>,
                8,
            ),
        )
        .await
        .expect("bounded connection")
        .expect("negotiated connection")
    }
    fn post(&self, request: &Value) -> McpHttpResponse {
        let id = &request["id"];
        match request["method"].as_str().expect("request method") {
            "server/discover" => json_response(
                200,
                &json!({"jsonrpc":"2.0", "id":id,
                "error":{"code":-32601,"message":"initialize required"}}),
            ),
            "initialize" => {
                let result = json!({"jsonrpc":"2.0", "id":id, "result":{
                    "protocolVersion":ProtocolVersion::STANDARD_HEADERS,
                    "capabilities":{"tools":{}}, "serverInfo":{"name":"raw-fixture","version":"1"}
                }});
                let mut response = if matches!(self.case, Case::Priming) {
                    let wire = format!("id: 0\nretry: 1\ndata:\n\ndata: \t \n\ndata: {result}\n\n");
                    McpHttpResponse {
                        status: 200,
                        headers: vec![("content-type".into(), "text/event-stream".into())],
                        body: stream::iter(
                            wire.into_bytes().into_iter().map(|byte| Ok(vec![byte])),
                        )
                        .boxed(),
                    }
                } else {
                    json_response(200, &result)
                };
                response
                    .headers
                    .push(("mcp-session-id".into(), "fixture-session".into()));
                response
            }
            "notifications/initialized" => {
                empty_response(if matches!(self.case, Case::EmptyNotification) {
                    200
                } else {
                    202
                })
            }
            "tools/list" => {
                let body = json!({"jsonrpc":"2.0", "id":id, "result":{"tools":[{
                    "name":"echo", "description":"fixture", "inputSchema":{"type":"object","properties":{
                        "locale":{"type":"string", "x-mcp-header":"Locale"}
                    }}
                }]}});
                if matches!(self.case, Case::AcceptedGet) {
                    self.events
                        .try_send(
                            format!(
                                "data: {}\n\n",
                                serde_json::to_string(&body).expect("SSE JSON")
                            )
                            .into_bytes(),
                        )
                        .expect("one pending response");
                    empty_response(202)
                } else {
                    json_response(200, &body)
                }
            }
            "tools/call" if request["params"]["name"] == "fail" => json_response(
                400,
                &json!({
                    "jsonrpc":"2.0", "id":id, "error":{"code":-32602,"message":"remote private detail"}
                }),
            ),
            "tools/call" => json_response(
                200,
                &json!({"jsonrpc":"2.0", "id":id,
                "result":{"content":[{"type":"text","text":"exact reply"}],"isError":false}}),
            ),
            method => panic!("unexpected fixture method {method}"),
        }
    }
}
#[async_trait]
impl McpHttpClient for Fixture {
    async fn request(
        &self,
        method: McpHttpMethod,
        uri: &str,
        headers: Vec<(String, String)>,
        body: McpHttpBody,
    ) -> Result<McpHttpResponse, McpError> {
        assert_eq!(uri, "https://fixture.invalid/mcp");
        assert!(body.as_ref().len() <= 8192);
        let body = if body.as_ref().is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(body.as_ref()).expect("outbound JSON")
        };
        {
            let mut seen = self.seen.lock().expect("fixture history");
            assert!(seen.len() < 32);
            seen.push(Seen {
                method,
                headers,
                body: body.clone(),
            });
        }
        Ok(match method {
            McpHttpMethod::Post => self.post(&body),
            McpHttpMethod::Delete => empty_response(204),
            McpHttpMethod::Get => McpHttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: stream::unfold(Arc::clone(&self.receive), |receiver| async move {
                    let bytes = receiver.lock().await.recv().await?;
                    Some((Ok(bytes), receiver))
                })
                .boxed(),
            },
        })
    }
}
fn empty_response(status: u16) -> McpHttpResponse {
    McpHttpResponse {
        status,
        headers: vec![("content-length".into(), "0".into())],
        body: stream::empty().boxed(),
    }
}
fn json_response(status: u16, value: &Value) -> McpHttpResponse {
    McpHttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: stream::iter([Ok(serde_json::to_vec(&value).expect("response JSON"))]).boxed(),
    }
}
fn slot(client: &dyn McpClient) -> McpResponseSlot {
    McpResponseSlot::new(client.response_limits()).expect("response slot")
}
async fn close(client: &dyn McpClient) {
    tokio::time::timeout(Duration::from_secs(3), client.close(Duration::from_secs(2)))
        .await
        .expect("bounded close")
        .expect("physical HTTP cleanup");
}

#[tokio::test]
async fn accepted_request_is_resolved_by_the_session_get_stream() {
    let fixture = Fixture::new(Case::AcceptedGet);
    let client = fixture.connect().await;
    let tools = tokio::time::timeout(Duration::from_secs(3), client.list_tools(slot(&*client)))
        .await
        .expect("GET delivers accepted request")
        .expect("tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "echo");
    assert!(
        fixture
            .seen
            .lock()
            .expect("history")
            .iter()
            .any(|event| event.method == McpHttpMethod::Get)
    );
    close(&*client).await;
}

#[tokio::test]
async fn empty_notification_success_preserves_session_standard_headers_and_delete() {
    let fixture = Fixture::new(Case::EmptyNotification);
    let client = fixture.connect().await;
    assert_eq!(
        client
            .list_tools(slot(&*client))
            .await
            .expect("tools")
            .len(),
        1
    );
    let result = client
        .call_tool("echo", json!({"locale":"en-US"}), slot(&*client))
        .await
        .expect("tool");
    assert_eq!(result["content"][0]["text"], "exact reply");
    close(&*client).await;
    let seen = fixture.seen.lock().expect("history");
    let call = seen
        .iter()
        .find(|event| event.body["method"] == "tools/call")
        .expect("call");
    for (name, value) in [
        ("authorization", "Bearer fixture-token"),
        ("mcp-session-id", "fixture-session"),
        (
            "mcp-protocol-version",
            ProtocolVersion::STANDARD_HEADERS.as_str(),
        ),
        ("mcp-method", "tools/call"),
        ("mcp-name", "echo"),
        ("mcp-param-locale", "en-US"),
    ] {
        assert!(
            call.headers
                .iter()
                .any(|pair| pair.0 == name && pair.1 == value),
            "missing {name}"
        );
    }
    assert_eq!(
        seen.iter()
            .filter(|event| event.method == McpHttpMethod::Delete)
            .count(),
        1
    );
}

#[tokio::test]
async fn non_success_json_rpc_error_settles_only_its_request() {
    let fixture = Fixture::new(Case::ErrorBody);
    let client = fixture.connect().await;
    let error = client
        .call_tool("fail", json!({}), slot(&*client))
        .await
        .err()
        .expect("remote error");
    assert!(matches!(error, McpError::Protocol(_)));
    assert!(!error.to_string().contains("remote private detail"));
    let result = client
        .call_tool("echo", json!({}), slot(&*client))
        .await
        .expect("connection remains usable");
    assert_eq!(result["content"][0]["text"], "exact reply");
    close(&*client).await;
}

#[tokio::test]
async fn empty_and_whitespace_priming_events_preserve_initialization_and_reuse() {
    let fixture = Fixture::new(Case::Priming);
    let client = fixture.connect().await;
    assert_eq!(
        client
            .list_tools(slot(&*client))
            .await
            .expect("tools after priming")
            .len(),
        1
    );
    let result = client
        .call_tool("echo", json!({}), slot(&*client))
        .await
        .expect("call after priming");
    assert_eq!(result["content"][0]["text"], "exact reply");
    close(&*client).await;
    assert_eq!(
        fixture
            .seen
            .lock()
            .expect("history")
            .iter()
            .filter(|request| request.method == McpHttpMethod::Delete)
            .count(),
        1
    );
}
