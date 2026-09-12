#![allow(clippy::expect_used)]

use super::*;

#[tokio::test]
async fn session_listing_reads_only_exact_authorized_live_sessions() {
    let root = tempfile::tempdir().expect("root");
    let workspace = std::fs::canonicalize(root.path()).expect("workspace");
    let storage = workspace.join("state");
    std::fs::create_dir(&storage).expect("state");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o700))
            .expect("private state");
    }
    let factory = Arc::new(
        RuntimeSessionFactory::new(RuntimeHostOptions {
            credentials_path: storage.join("credentials.json"),
            storage_root: storage.clone(),
            config: rw_core::Config::default(),
            allowed_workspaces: vec![workspace.clone()],
            permission_mode: Some(PermissionModeDescriptor::Strict),
            max_turns: 1,
            provider_mode: HostedProviderMode::DeterministicReplay {
                provider_name: "offline-mcp-bridge".into(),
                scripts: Vec::new(),
                event_delay_ms: 0,
            },
            dangerously_trust: false,
            wait_for_execution_lease: false,
        })
        .await
        .expect("factory"),
    );
    let host = rw_runtime::HeadlessRuntimeBuilder::new(factory)
        .with_config(EngineHostConfig {
            max_sessions: MAX_SERVER_SESSIONS,
            ..EngineHostConfig::default()
        })
        .build()
        .expect("host");
    let bridge = CliMcpBridge {
        response_limits: McpResponseLimits::new(WORKING_BYTES).expect("response limit"),
        host,
        registry: read_only_tools().expect("tools"),
        tool_context: ToolContext::from_workspace_roots(std::slice::from_ref(&workspace))
            .expect("context"),
        permissions: PermissionGate::for_headless_mode(PermissionModeDescriptor::AutoSafe)
            .with_workspace_roots(std::slice::from_ref(&workspace)),
        bound: BoundClient {
            client_id: ClientId("mcp-list-test".into()),
        },
        workspace: workspace.to_string_lossy().into_owned(),
        request_sequence: AtomicU64::new(1),
        request_namespace: "exact-live-list".into(),
    };
    let slot = || McpResponseSlot::new(bridge.response_limits()).expect("slot");
    let first = bridge.create_session(None, slot()).await.expect("first");
    let second = bridge.create_session(None, slot()).await.expect("second");
    assert_ne!(first.id, second.id);
    // A malformed foreign persisted entry must never be consulted or reopened.
    let foreign = storage.join("sessions/foreign");
    std::fs::create_dir_all(&foreign).expect("foreign directory");
    std::fs::write(foreign.join("metadata.json"), b"invalid metadata").expect("foreign fixture");
    let listed = bridge
        .list_sessions(vec![first.id.clone(), "not-open".into()], slot())
        .await
        .expect("authorized list");
    assert_eq!(
        listed.as_slice(),
        &[SessionSummary {
            id: first.id.clone(),
            state: "driver".into()
        }]
    );
    assert!(
        bridge
            .host
            .session(&rw_core::SessionId("foreign".into()))
            .await
            .is_none()
    );
    let empty = bridge
        .list_sessions(Vec::new(), slot())
        .await
        .expect("empty authority");
    assert!(empty.is_empty());
    bridge.shutdown().await;
}
