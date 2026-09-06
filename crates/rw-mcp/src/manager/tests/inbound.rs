use super::*;

#[tokio::test]
async fn revoked_catalog_is_hidden_and_reconnect_requires_schema_approval() {
    let id = McpServerId::new("changing").expect("id");
    let first = Arc::new(MockClient {
        schema_version: Arc::new(Mutex::new(1)),
        closed: AtomicBool::new(false),
        fail_close: false,
        invalidated: AtomicBool::new(false),
    });
    let connector = Arc::new(MockConnector {
        clients: Mutex::new(BTreeMap::from([(id.clone(), first.clone())])),
    });
    let manager = McpManager::new(
        connector.clone(),
        Arc::new(MemorySpool::default()),
        Arc::new(CompactJsonEncoder),
        McpLimits::default(),
    );
    manager
        .register(McpServerConfig {
            id: id.clone(),
            transport: McpTransportConfig::Stdio {
                executable: "fixture".into(),
                args: vec![],
                working_directory: None,
                environment: vec![],
                sandbox: McpStdioSandboxPolicy::default(),
            },
            enabled: true,
            defer_tools: true,
            tool_capabilities: crate::McpToolCapabilityOverrides::default(),
        })
        .await
        .expect("register");
    manager.set_enabled(&id, true).await.expect("connect");
    assert_eq!(manager.tool_search("lookup", Some(&id)).await.len(), 1);
    first.invalidated.store(true, Ordering::Release);
    assert!(matches!(
        manager.statuses().await[0].state,
        ServerState::Failed { .. }
    ));
    assert!(manager.tool_search("lookup", Some(&id)).await.is_empty());
    assert!(manager.resources().await.is_empty());
    assert!(manager.prompts().await.is_empty());
    assert!(manager.call_tool(&id, "lookup", json!({})).await.is_err());
    assert!(manager.approve_pending_tools(&id).await.is_err());
    let second = Arc::new(MockClient {
        schema_version: Arc::new(Mutex::new(2)),
        closed: AtomicBool::new(false),
        fail_close: false,
        invalidated: AtomicBool::new(false),
    });
    connector.clients.lock().await.insert(id.clone(), second);
    manager.set_enabled(&id, true).await.expect("reconnect");
    assert!(
        first.closed.load(Ordering::Acquire),
        "prior connection must be settled"
    );
    assert!(matches!(
        manager.statuses().await[0].state,
        ServerState::ApprovalRequired
    ));
    assert!(manager.call_tool(&id, "lookup", json!({})).await.is_err());
    manager
        .approve_pending_tools(&id)
        .await
        .expect("approve schema");
    assert!(manager.call_tool(&id, "lookup", json!({})).await.is_ok());
    assert!(
        manager
            .shutdown()
            .await
            .iter()
            .all(|(_, result)| result.is_ok())
    );
}
