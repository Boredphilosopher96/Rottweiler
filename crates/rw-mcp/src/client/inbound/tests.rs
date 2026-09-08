#![allow(clippy::expect_used)]
use super::*;
use crate::client::ingress::{Ingress, stdio::StdioTransport};
use crate::{McpClient, McpResponseSlot};
use rmcp::ServiceExt as _;
use serde_json::json;

async fn admitted_service(
    router: McpInboundRouter,
    stream: tokio::io::DuplexStream,
) -> (
    rmcp::service::RunningService<RoleClient, McpInboundRouter>,
    Arc<Ingress>,
) {
    let ingress = Ingress::new(router.clone()).expect("ingress admission");
    let (reader, writer) = tokio::io::split(stream);
    let transport = StdioTransport::new(Box::pin(reader), Box::pin(writer), Arc::clone(&ingress))
        .expect("transport admission");
    let transport = crate::client::transport::ClientTransport::Stdio(transport);
    let service = Box::pin(router.serve(transport))
        .await
        .expect("client handshake");
    (service, ingress)
}

fn slot(client: &impl McpClient) -> McpResponseSlot {
    McpResponseSlot::new(client.response_limits()).expect("response admission")
}

#[test]
fn capability_advertisement_contains_no_unowned_host_authority() {
    let info = McpInboundRouter::default().get_info();
    assert_eq!(
        serde_json::to_value(info.capabilities).expect("capabilities"),
        json!({})
    );
    assert_eq!(info.client_info.name, "rottweiler");
}

#[test]
fn inbound_host_requests_are_rejected_without_selecting_a_user_answer() {
    for request in [
        json!({"method":"roots/list"}),
        json!({"method":"sampling/createMessage","params":{"messages":[],"maxTokens":1}}),
        json!({"method":"elicitation/create","params":{"message":"Confirm?","requestedSchema":{"type":"object","properties":{}}}}),
        json!({"method":"tasks/get","params":{"taskId":"foreign"}}),
        json!({"method":"extension/custom","params":{"secret":"never reflect this"}}),
    ] {
        let request = serde_json::from_value(request).expect("server request");
        let error = McpInboundRouter::request(&request).expect_err("unsupported authority");
        assert_eq!(error.code, ErrorCode::METHOD_NOT_FOUND);
        assert!(!error.message.contains("never reflect this"));
    }
    let ping = serde_json::from_value(json!({"method":"ping"})).expect("ping");
    assert!(McpInboundRouter::request(&ping).is_ok());
}

#[test]
fn catalog_notifications_revoke_the_shared_snapshot_with_constant_storage() {
    for method in [
        "notifications/tools/list_changed",
        "notifications/resources/list_changed",
        "notifications/prompts/list_changed",
    ] {
        let router = McpInboundRouter::default();
        let consumer = router.clone();
        assert!(consumer.catalog_valid());
        for _ in 0..1_000 {
            router.notification(
                &serde_json::from_value(json!({"method":method})).expect("notification"),
            );
        }
        assert!(!consumer.catalog_valid());
    }
}

#[tokio::test]
async fn actual_connection_negotiates_and_routes_unsolicited_requests() {
    struct FixtureServer;
    impl rmcp::ServerHandler for FixtureServer {}
    let (client_io, server_io) = tokio::io::duplex(8 * 1024);
    let router = McpInboundRouter::default();
    let server = tokio::spawn(async move {
        FixtureServer
            .serve(server_io)
            .await
            .expect("server handshake")
    });
    let (client, _ingress) = admitted_service(router.clone(), client_io).await;
    let server = server.await.expect("server task");
    assert_eq!(
        serde_json::to_value(&server.peer_info().expect("negotiated client").capabilities)
            .expect("capabilities"),
        json!({})
    );
    let request = serde_json::from_value(json!({"method":"roots/list"})).expect("roots request");
    assert!(server.peer().send_request(request).await.is_err());
    server
        .peer()
        .notify_tool_list_changed()
        .await
        .expect("catalog change");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while router.catalog_valid() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("routed notification");
    client.cancel().await.expect("client cleanup");
    server.cancel().await.expect("server cleanup");
}

#[tokio::test]
async fn absent_server_capabilities_do_not_trigger_catalog_requests() {
    struct NoCatalogs;
    impl rmcp::ServerHandler for NoCatalogs {
        async fn list_tools(
            &self,
            _: Option<rmcp::model::PaginatedRequestParams>,
            _: RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ListToolsResult, ErrorData> {
            panic!("unadvertised tool listing");
        }
        async fn list_resources(
            &self,
            _: Option<rmcp::model::PaginatedRequestParams>,
            _: RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ListResourcesResult, ErrorData> {
            panic!("unadvertised resource listing");
        }
        async fn list_prompts(
            &self,
            _: Option<rmcp::model::PaginatedRequestParams>,
            _: RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ListPromptsResult, ErrorData> {
            panic!("unadvertised prompt listing");
        }
    }
    let (client_io, server_io) = tokio::io::duplex(8 * 1024);
    let server =
        tokio::spawn(async move { NoCatalogs.serve(server_io).await.expect("server handshake") });
    let (service, ingress) = admitted_service(McpInboundRouter::default(), client_io).await;
    let server = server.await.expect("server task");
    let client = super::super::RmcpClient::new(
        rw_types::McpServerId::new("unary").expect("id"),
        service,
        None,
        ingress,
    )
    .await;
    assert!(!client.peer.response_cache_config().await.enabled);
    assert!(
        client
            .list_tools(slot(&client))
            .await
            .expect("no tools")
            .is_empty()
    );
    assert!(
        client
            .list_resources(slot(&client))
            .await
            .expect("no resources")
            .is_empty()
    );
    assert!(
        client
            .list_prompts(slot(&client))
            .await
            .expect("no prompts")
            .is_empty()
    );
    server.cancel().await.expect("server shutdown");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while client.catalog_valid() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnection revokes catalog");
    assert!(
        client
            .call_tool("unavailable", json!({}), slot(&client))
            .await
            .is_err()
    );
    client
        .close(std::time::Duration::from_secs(1))
        .await
        .expect("client cleanup");
}

#[tokio::test]
async fn prompt_reads_do_not_reuse_or_fall_back_to_an_uncharged_peer_cache() {
    use std::sync::atomic::AtomicUsize;
    struct ChangingPrompt(Arc<AtomicUsize>);
    impl rmcp::ServerHandler for ChangingPrompt {
        fn get_info(&self) -> rmcp::model::ServerInfo {
            serde_json::from_value(json!({
                "protocolVersion": rmcp::model::ProtocolVersion::default(),
                "capabilities": {"prompts": {}},
                "serverInfo": {"name": "uncached", "version": "1"}
            }))
            .expect("server info")
        }
        async fn get_prompt(
            &self,
            _: rmcp::model::GetPromptRequestParams,
            _: RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::GetPromptResponse, ErrorData> {
            let call = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            if call > 2 {
                return Err(ErrorData::internal_error("remote failure", None));
            }
            Ok(rmcp::model::GetPromptResponse::Complete(
                serde_json::from_value(json!({
                    "description": call.to_string(), "messages": [],
                    "ttlMs": 60_000, "cacheScope": "public"
                }))
                .expect("prompt response"),
            ))
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = ChangingPrompt(Arc::clone(&calls));
    let (client_io, server_io) = tokio::io::duplex(8192);
    let server = tokio::spawn(async move { handler.serve(server_io).await.expect("server") });
    let (service, ingress) = admitted_service(McpInboundRouter::default(), client_io).await;
    let server = server.await.expect("server task");
    let client = super::super::RmcpClient::new(
        rw_types::McpServerId::new("uncached").expect("id"),
        service,
        None,
        ingress,
    )
    .await;
    for expected in ["1", "2"] {
        assert_eq!(
            client
                .get_prompt("same", json!({}), slot(&client))
                .await
                .expect("fresh prompt")["description"],
            expected
        );
    }
    assert!(
        client
            .get_prompt("same", json!({}), slot(&client))
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    client
        .close(std::time::Duration::from_secs(1))
        .await
        .expect("client close");
    server.cancel().await.expect("server close");
}
