use super::*;
use rw_mcp::{CompactJsonEncoder, FilesystemSpool, McpStdioSandboxPolicy, PayloadSource as _};
use std::os::unix::fs::PermissionsExt as _;

#[tokio::test]
async fn echoed_vault_credential_is_redacted_before_canonical_payload_publication() {
    const TOKEN: &str = "mcp-vault-echo-only-canary";
    let root = tempfile::tempdir().expect("root");
    let credentials = root.path().join("credentials.toml");
    fs::write(
        &credentials,
        format!("version = 1\n[credentials]\nfixture-token = \"{TOKEN}\"\n"),
    )
    .expect("vault fixture");
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).expect("credential mode");
    let redactor = Arc::new(SharedPluginRedactor::new(
        rw_providers::FixtureRedactor::default(),
    ));
    let secret = crate::extension_runtime::mcp_service::resolve_mcp_credential(
        &CredentialManager::system(credentials),
        &*redactor,
        "fixture-token",
    )
    .expect("resolve classified credential");
    assert_eq!(secret, TOKEN);
    assert!(
        !rw_mcp::PayloadRedactor::redact(&*redactor, &secret, 1024, &mut |_| Ok(()))
            .expect("registered before exposure")
            .contains(TOKEN)
    );
    let journals = crate::journal_service::JournalService::new(root.path()).expect("journals");
    let source = journals.payload_source("session").expect("lazy source");
    let spool = Arc::new(FilesystemSpool::new(source.clone(), redactor));
    let manager = McpManager::new(
        Arc::new(CatalogConnector),
        spool,
        Arc::new(CompactJsonEncoder),
        McpLimits {
            response_bytes: 256,
            ..McpLimits::default()
        },
    );
    let server = McpServerId::new("fixture").expect("server");
    manager
        .register(McpServerConfig {
            id: server.clone(),
            enabled: true,
            defer_tools: true,
            tool_capabilities: rw_mcp::McpToolCapabilityOverrides::default(),
            transport: McpTransportConfig::Stdio {
                executable: "fixture".into(),
                args: vec![],
                working_directory: None,
                environment: vec![],
                sandbox: McpStdioSandboxPolicy::default(),
            },
        })
        .await
        .expect("register");
    assert!(
        manager
            .connect_all()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
    let response = manager
        .call_tool(&server, "echo", json!({"echo":secret.repeat(100)}))
        .await
        .expect("echo response");
    let reference = response.overflow.as_ref().expect("durable overflow");
    assert_eq!(response.payloads.references(), vec![reference.clone()]);
    let window = source
        .open()
        .expect("source")
        .window(reference, 0, None, &|| false)
        .expect("authenticated body");
    assert!(!window.content.contains(TOKEN));
    for entry in
        fs::read_dir(root.path().join("sessions/session/payloads")).expect("payload namespace")
    {
        let path = entry.expect("entry").path();
        if path
            .extension()
            .is_some_and(|extension| extension == "payload")
        {
            let bytes = fs::read(path).expect("at-rest bytes");
            assert!(
                !bytes
                    .windows(TOKEN.len())
                    .any(|window| window == TOKEN.as_bytes())
            );
        }
    }
    assert!(
        manager
            .shutdown()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
}
