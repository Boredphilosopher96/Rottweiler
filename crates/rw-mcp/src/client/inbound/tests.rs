#![allow(clippy::expect_used)]
use super::*;
use rmcp::ServiceExt as _;
use serde_json::json;

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
    let client = router
        .clone()
        .serve(client_io)
        .await
        .expect("client handshake");
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
    use crate::McpClient as _;
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
    let service = McpInboundRouter::default()
        .serve(client_io)
        .await
        .expect("client handshake");
    let server = server.await.expect("server task");
    let client = super::super::RmcpClient::new(
        rw_types::McpServerId::new("unary").expect("id"),
        service,
        None,
    );
    assert!(client.list_tools().await.expect("no tools").is_empty());
    assert!(
        client
            .list_resources()
            .await
            .expect("no resources")
            .is_empty()
    );
    assert!(client.list_prompts().await.expect("no prompts").is_empty());
    server.cancel().await.expect("server shutdown");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while client.catalog_valid() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnection revokes catalog");
    assert!(client.call_tool("unavailable", json!({})).await.is_err());
    client
        .close(std::time::Duration::from_secs(1))
        .await
        .expect("client cleanup");
}
