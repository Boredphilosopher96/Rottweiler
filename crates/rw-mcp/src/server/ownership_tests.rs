#![allow(clippy::expect_used)]
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, ReadHalf, WriteHalf},
    sync::Semaphore,
};

pub(super) struct Bridge {
    pub(super) entered: Semaphore,
    pub(super) release: Semaphore,
    completed: AtomicUsize,
}
#[async_trait]
impl EngineMcpBridge for Bridge {
    fn response_limits(&self) -> McpResponseLimits {
        McpResponseLimits::new(1024 * 1024).expect("limits")
    }
    async fn tools(
        &self,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<EngineTool>>, BridgeError> {
        adopt(slot, Vec::new()).await
    }
    async fn call_tool(
        &self,
        _name: &str,
        arguments: Value,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, BridgeError> {
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .map_err(|_| BridgeError::safe("fixture closed"))?
            .forget();
        self.completed.fetch_add(1, Ordering::Release);
        adopt(slot, arguments).await
    }
    async fn create_session(
        &self,
        _title: Option<String>,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<SessionSummary>, BridgeError> {
        adopt(
            slot,
            SessionSummary {
                id: "owned".to_owned(),
                state: "idle".to_owned(),
            },
        )
        .await
    }
    async fn list_sessions(
        &self,
        _authorized: Vec<String>,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<SessionSummary>>, BridgeError> {
        adopt(slot, Vec::new()).await
    }
    async fn send_message(
        &self,
        _session_id: &str,
        _message: &str,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, BridgeError> {
        adopt(slot, json!({})).await
    }
}
async fn adopt<T: rw_types::allocation::PrepareAllocation + Send + 'static>(
    slot: McpResponseSlot,
    value: T,
) -> Result<McpResponse<T>, BridgeError> {
    slot.adopt(value)
        .await
        .map_err(|_| BridgeError::safe("fixture allocation"))
}
pub(super) fn server(timeout: Duration) -> (RottweilerMcpServer, Arc<Bridge>) {
    let bridge = Arc::new(Bridge {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
        completed: AtomicUsize::new(0),
    });
    (
        RottweilerMcpServer::new(
            bridge.clone(),
            McpServerAuthority::new(["read".to_owned()], []).expect("authority"),
        )
        .with_request_timeout(timeout),
        bridge,
    )
}
struct Client {
    read: BufReader<ReadHalf<DuplexStream>>,
    write: WriteHalf<DuplexStream>,
}
impl Client {
    async fn send(&mut self, value: Value) {
        let bytes = serde_json::to_vec(&value).expect("JSON");
        self.write.write_all(&bytes).await.expect("send");
        self.write.write_all(b"\n").await.expect("newline");
    }
    async fn receive(&mut self) -> Value {
        let mut bytes = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            self.read.read_until(b'\n', &mut bytes),
        )
        .await
        .expect("reply deadline")
        .expect("read");
        serde_json::from_slice(&bytes).expect("reply JSON")
    }
    async fn initialize(&mut self) {
        self.send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
        assert!(self.receive().await.get("result").is_some());
    }
    async fn call(&mut self, id: i64) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"rottweiler_tools_call","arguments":{"name":"read","arguments":{"value":id}}}})).await;
    }
}
fn connect(server: RottweilerMcpServer) -> (Client, tokio::task::JoinHandle<std::io::Result<()>>) {
    let (client, server_io) = tokio::io::duplex(16 * 1024);
    let (read, write) = tokio::io::split(client);
    (
        Client {
            read: BufReader::new(read),
            write,
        },
        tokio::spawn(dispatch::serve_io(server, server_io)),
    )
}

#[tokio::test]
async fn preinitialize_ping_inline_negotiation_and_unsupported_methods_remain_usable() {
    let (server, _) = server(Duration::from_secs(30));
    let (mut client, service) = connect(server);
    for id in 0..70 {
        client
            .send(json!({"jsonrpc":"2.0","id":id,"method":"ping"}))
            .await;
        assert_eq!(client.receive().await["result"], json!({}));
    }
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    client
        .send(json!({"jsonrpc":"2.0","id":71,"method":"server/discover","params":{"_meta":meta}}))
        .await;
    assert!(client.receive().await.get("result").is_some());
    client
        .send(json!({"jsonrpc":"2.0","id":72,"method":"tools/list"}))
        .await;
    assert_eq!(client.receive().await["error"]["code"], -32602);
    client
        .send(json!({"jsonrpc":"2.0","id":73,"method":"unknown/method","params":{"_meta":meta}}))
        .await;
    assert_eq!(client.receive().await["error"]["code"], -32601);
    client
        .send(json!({"jsonrpc":"2.0","id":74,"method":"tools/list","params":{"_meta":meta}}))
        .await;
    assert_eq!(
        client.receive().await["result"]["tools"]
            .as_array()
            .expect("tools")
            .len(),
        4
    );
    drop(client);
    service.await.expect("join").expect("close");
}

#[tokio::test]
async fn cancelled_requests_settle_and_reuse_admission_beyond_the_request_limit() {
    let (server, bridge) = server(Duration::from_secs(30));
    let (mut client, service) = connect(server);
    client.initialize().await;
    for id in 1..=70 {
        client.call(id).await;
        bridge.entered.acquire().await.expect("entered").forget();
        client.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id,"reason":"cancel"}})).await;
        // Ping acknowledgement fences processing of the cancellation before the
        // blocked bridge returns. The cancelled result must never enter the wire.
        client
            .send(json!({"jsonrpc":"2.0","id":1000+id,"method":"ping"}))
            .await;
        assert_eq!(client.receive().await["id"], 1000 + id);
        bridge.release.add_permits(1);
    }
    drop(client);
    service.await.expect("join").expect("close");
    assert_eq!(bridge.completed.load(Ordering::Acquire), 70);
}

#[tokio::test]
async fn response_deadline_and_caller_loss_keep_the_physical_bridge_operation_owned() {
    let (server, bridge) = server(Duration::from_millis(20));
    let (mut client, service) = connect(server);
    client.initialize().await;
    client.call(1).await;
    bridge.entered.acquire().await.expect("entered").forget();
    assert!(client.receive().await.get("error").is_some());
    assert_eq!(bridge.completed.load(Ordering::Acquire), 0);
    drop(client);
    let mut service = service;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut service)
            .await
            .is_err()
    );
    bridge.release.add_permits(1);
    service.await.expect("join").expect("close");
    assert_eq!(bridge.completed.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn missing_owned_tool_arguments_are_rejected_before_bridge_admission() {
    let (server, bridge) = server(Duration::from_secs(30));
    let (mut client, service) = connect(server);
    client.initialize().await;
    client.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"rottweiler_tools_call","arguments":{"name":"read"}}})).await;
    assert_eq!(client.receive().await["error"]["code"], -32602);
    assert_eq!(bridge.entered.available_permits(), 0);
    drop(client);
    service.await.expect("join").expect("close");
}

#[tokio::test]
async fn unterminated_oversized_frame_is_refused_before_json_decode() {
    let (server, _) = server(Duration::from_secs(30));
    let (client, service) = connect(server);
    let mut write = client.write;
    let sender = tokio::spawn(async move {
        write
            .write_all(&vec![b' '; crate::ingress::frame::STDIO_FRAME_BYTES + 1])
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_secs(5), service)
            .await
            .expect("deadline")
            .expect("join")
            .is_err()
    );
    let _ = sender.await.expect("sender join");
}

#[tokio::test]
async fn losing_the_connection_caller_does_not_drop_accepted_bridge_work() {
    let (server, bridge) = server(Duration::from_secs(30));
    let retired = Arc::downgrade(&server.authority);
    let (mut client, service) = connect(server);
    client.initialize().await;
    client.call(1).await;
    bridge.entered.acquire().await.expect("entered").forget();
    service.abort();
    assert!(service.await.expect_err("caller cancelled").is_cancelled());
    drop(client);
    assert!(retired.upgrade().is_some());
    assert_eq!(bridge.completed.load(Ordering::Acquire), 0);
    bridge.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while retired.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual connection retirement");
    assert_eq!(bridge.completed.load(Ordering::Acquire), 1);
}

#[test]
fn authority_rejects_overflow_and_invalid_owned_ids_without_truncation() {
    assert!(
        McpServerAuthority::new([], (0..=MAX_SERVER_SESSIONS).map(|id| format!("s-{id}"))).is_err()
    );
    assert!(McpServerAuthority::new([], ["../outside".to_owned()]).is_err());
    assert!(McpServerAuthority::new((0..65).map(|id| format!("tool-{id}")), []).is_err());
    assert!(McpServerAuthority::new(["x".repeat(257)], []).is_err());
    assert_eq!(
        McpServerAuthority::new([], (0..MAX_SERVER_SESSIONS).map(|id| format!("s-{id}")))
            .expect("legal bound")
            .sessions
            .into_inner()
            .len(),
        MAX_SERVER_SESSIONS
    );
}
