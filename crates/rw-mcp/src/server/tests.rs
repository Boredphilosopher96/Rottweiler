#![allow(clippy::expect_used)]

use super::*;
use rmcp::{ServiceExt as _, model::CallToolRequestParams};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Bridge {
    messages: AtomicUsize,
}

#[async_trait]
impl EngineMcpBridge for Bridge {
    fn response_limits(&self) -> McpResponseLimits {
        McpResponseLimits::new(1024 * 1024).expect("fixture limit")
    }
    async fn tools(
        &self,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Vec<EngineTool>>, BridgeError> {
        adopt(slot, Vec::new()).await
    }
    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, BridgeError> {
        adopt(slot, json!({"name":name,"arguments":arguments})).await
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
        adopt(
            slot,
            vec![
                SessionSummary {
                    id: "owned".to_owned(),
                    state: "idle".to_owned(),
                },
                SessionSummary {
                    id: "foreign".to_owned(),
                    state: "idle".to_owned(),
                },
            ],
        )
        .await
    }
    async fn send_message(
        &self,
        session_id: &str,
        message: &str,
        slot: McpResponseSlot,
    ) -> Result<McpResponse<Value>, BridgeError> {
        self.messages.fetch_add(1, Ordering::Relaxed);
        adopt(slot, json!({"session":session_id,"message":message})).await
    }
}

fn arguments(value: &Value) -> rmcp::model::JsonObject {
    value.as_object().cloned().expect("object")
}

#[tokio::test]
async fn another_agent_drives_server_fixture_with_scoped_authority() {
    let bridge = Arc::new(Bridge {
        messages: AtomicUsize::new(0),
    });
    let factory = RottweilerMcpServerFactory::new(bridge.clone(), || {
        McpServerAuthority::new(["read".to_owned()], std::iter::empty())
            .map(|authority| authority.with_session_access(true, true, true))
    });
    let server = factory.create().expect("server authority");
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server_service = tokio::spawn(dispatch::serve_io(server, server_io));
    let mut client_service = ().serve(client_io).await.expect("client");
    assert_eq!(
        client_service
            .peer()
            .list_all_tools()
            .await
            .expect("tools")
            .len(),
        4
    );

    let denied = client_service
        .peer()
        .call_tool(
            CallToolRequestParams::new("rottweiler_tools_call")
                .with_arguments(arguments(&json!({"name":"bash","arguments":{}}))),
        )
        .await
        .expect("denied");
    assert_eq!(denied.is_error, Some(true));
    let created = client_service
        .peer()
        .call_tool(
            CallToolRequestParams::new("rottweiler_sessions_create")
                .with_arguments(arguments(&json!({}))),
        )
        .await
        .expect("create");
    assert_eq!(created.is_error, Some(false));
    assert_eq!(
        created
            .structured_content
            .as_ref()
            .expect("structured session result")["id"],
        "owned"
    );
    let sent = client_service
        .peer()
        .call_tool(
            CallToolRequestParams::new("rottweiler_sessions_send")
                .with_arguments(arguments(&json!({"session_id":"owned","message":"hello"}))),
        )
        .await
        .expect("send");
    assert_eq!(sent.is_error, Some(false));
    let foreign = client_service
        .peer()
        .call_tool(
            CallToolRequestParams::new("rottweiler_sessions_send").with_arguments(arguments(
                &json!({"session_id":"foreign","message":"steal"}),
            )),
        )
        .await
        .expect("foreign");
    assert_eq!(foreign.is_error, Some(true));
    assert_eq!(bridge.messages.load(Ordering::Relaxed), 1);
    client_service.close().await.expect("close client");
    server_service
        .await
        .expect("join server")
        .expect("close server");
}

async fn adopt<T: rw_types::allocation::PrepareAllocation + Send + 'static>(
    slot: McpResponseSlot,
    value: T,
) -> Result<McpResponse<T>, BridgeError> {
    slot.adopt(value)
        .await
        .map_err(|_| BridgeError::safe("fixture result exceeded admission"))
}
