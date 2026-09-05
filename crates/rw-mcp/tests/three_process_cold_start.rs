#![allow(clippy::expect_used)]

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use rw_mcp::{
    CompactJsonEncoder, FilesystemSpool, McpConnectionApprovalPolicy, McpError, McpLimits,
    McpManager, McpServerConfig, McpTransportConfig, TestOnlyUnsandboxedStdioConnector,
};
use rw_types::McpServerId;

struct ApprovedFixture(PathBuf);

#[async_trait]
impl McpConnectionApprovalPolicy for ApprovedFixture {
    async fn approve(&self, config: &McpServerConfig) -> Result<(), McpError> {
        match &config.transport {
            McpTransportConfig::Stdio { executable, .. } if executable == &self.0 => Ok(()),
            _ => Err(McpError::Policy("fixture executable mismatch".to_owned())),
        }
    }
}

#[tokio::test]
async fn three_real_stdio_processes_reach_prompt_ready_under_release_budget() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_rw-mcp-fixture"));
    let connector = Arc::new(TestOnlyUnsandboxedStdioConnector::new(Arc::new(
        ApprovedFixture(executable.clone()),
    )));
    let directory = tempfile::tempdir().expect("temp");
    let spool = Arc::new(
        FilesystemSpool::new(directory.path().to_path_buf())
            .await
            .expect("spool"),
    );
    let manager = McpManager::new(
        connector,
        spool,
        Arc::new(CompactJsonEncoder),
        McpLimits::default(),
    );
    for index in 0..3 {
        let pid_file = directory.path().join(format!("pid-{index}"));
        manager
            .register(McpServerConfig {
                id: McpServerId::new(format!("real-{index}")).expect("id"),
                transport: McpTransportConfig::Stdio {
                    executable: executable.clone(),
                    args: Vec::new(),
                    working_directory: None,
                    environment: vec![(
                        "RW_MCP_PID_FILE".to_owned(),
                        pid_file.to_string_lossy().into_owned(),
                    )],
                    sandbox: rw_mcp::McpStdioSandboxPolicy::default(),
                },
                enabled: true,
                defer_tools: true,
                tool_capabilities: rw_mcp::McpToolCapabilityOverrides::default(),
            })
            .await
            .expect("register");
    }
    let cold_start = Instant::now();
    let connected = manager.connect_all().await;
    assert_eq!(connected.len(), 3);
    assert!(
        connected.iter().all(|(_, result)| result.is_ok()),
        "{connected:?}"
    );
    let prompt = manager.deferred_prompt().await.expect("prompt");
    let prompt_ready = cold_start.elapsed();
    if cfg!(debug_assertions) {
        assert!(
            prompt_ready < Duration::from_secs(2),
            "debug cold-start sanity budget exceeded: {prompt_ready:?}"
        );
    } else {
        assert!(
            prompt_ready < Duration::from_millis(250),
            "release cold-start to prompt-ready budget exceeded: {prompt_ready:?}"
        );
    }
    let tokenizer = tiktoken_rs::cl100k_base().expect("tokenizer");
    assert!(tokenizer.encode_with_special_tokens(&prompt).len() < 2_000);
    let pids = (0..3)
        .map(|index| {
            std::fs::read_to_string(directory.path().join(format!("pid-{index}"))).expect("pid")
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        pids.len(),
        3,
        "each MCP connection must be a distinct real process"
    );
    assert_eq!(manager.tool_search("echo", None).await.len(), 3);
    assert_eq!(manager.resources().await.len(), 3);
    assert_eq!(manager.prompts().await.len(), 3);
    assert!(
        manager
            .shutdown()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
}
