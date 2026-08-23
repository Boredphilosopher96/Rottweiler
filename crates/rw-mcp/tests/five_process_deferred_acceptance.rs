#![allow(clippy::expect_used)]

use std::{
    collections::BTreeSet,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use rw_mcp::{
    CompactJsonEncoder, FilesystemSpool, McpConnectionApprovalPolicy, McpError, McpLimits,
    McpManager, McpServerConfig, McpStdioSandboxPolicy, McpTransportConfig,
    SandboxedStdioConnector, ServerState,
};
use rw_tools::SandboxedProtocolLauncher;
use rw_types::McpServerId;
use serde_json::json;

const PROFILES: [&str; 5] = ["repository", "issues", "notes", "database", "isolated"];

struct ApprovedProfiles {
    executable: PathBuf,
    workspace: PathBuf,
}

#[async_trait]
impl McpConnectionApprovalPolicy for ApprovedProfiles {
    async fn approve(&self, config: &McpServerConfig) -> Result<(), McpError> {
        let McpTransportConfig::Stdio {
            executable,
            working_directory: Some(cwd),
            sandbox,
            ..
        } = &config.transport
        else {
            return Err(McpError::Policy("fixture transport mismatch".to_owned()));
        };
        let roots_are_bounded = sandbox
            .read_roots
            .iter()
            .chain(&sandbox.write_roots)
            .all(|root| root.starts_with(&self.workspace));
        if executable == &self.executable && cwd == &self.workspace && roots_are_bounded {
            Ok(())
        } else {
            Err(McpError::Policy(
                "fixture executable or authority mismatch".to_owned(),
            ))
        }
    }
}

fn fixture_config(
    executable: &Path,
    workspace: &Path,
    profile: &str,
    network_probe: Option<std::net::SocketAddr>,
) -> McpServerConfig {
    let authority = workspace.join("authority").join(profile);
    let denied = workspace.join("denied").join(format!("{profile}.txt"));
    let pid_file = authority.join("pid");
    let filesystem_result = authority.join("filesystem-result");
    let network_result = authority.join("network-result");
    let mut environment = vec![
        ("RW_MCP_PROFILE".to_owned(), profile.to_owned()),
        (
            "RW_MCP_PID_FILE".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        ),
        (
            "RW_MCP_ALLOWED_WRITE".to_owned(),
            authority.join("allowed.txt").to_string_lossy().into_owned(),
        ),
        (
            "RW_MCP_DENIED_WRITE".to_owned(),
            denied.to_string_lossy().into_owned(),
        ),
        (
            "RW_MCP_FILESYSTEM_RESULT".to_owned(),
            filesystem_result.to_string_lossy().into_owned(),
        ),
    ];
    if let Some(address) = network_probe {
        environment.extend([
            ("RW_MCP_NETWORK_PROBE".to_owned(), address.to_string()),
            (
                "RW_MCP_NETWORK_RESULT".to_owned(),
                network_result.to_string_lossy().into_owned(),
            ),
        ]);
    }
    let read_roots = match profile {
        "repository" | "database" => vec![workspace.join("datasets").join(profile)],
        "notes" => vec![authority.clone()],
        _ => Vec::new(),
    };
    let allowed_domains = if profile == "issues" {
        vec!["api.example.com".to_owned()]
    } else {
        Vec::new()
    };
    McpServerConfig {
        id: McpServerId::new(profile).expect("id"),
        transport: McpTransportConfig::Stdio {
            executable: executable.to_path_buf(),
            args: Vec::new(),
            working_directory: Some(workspace.to_path_buf()),
            environment,
            sandbox: McpStdioSandboxPolicy {
                read_roots,
                write_roots: vec![authority],
                allowed_domains,
            },
        },
        enabled: true,
        defer_tools: true,
        tool_capabilities: rw_mcp::McpToolCapabilityOverrides::default(),
    }
}

async fn assert_reaped(pids: &BTreeSet<i32>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = pids
            .iter()
            .copied()
            .filter(|raw| {
                rustix::process::Pid::from_raw(*raw)
                    .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
            })
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture processes were not reaped: {remaining:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct AcceptanceHarness {
    manager: McpManager,
    listener: TcpListener,
    workspace: PathBuf,
    _scratch: tempfile::TempDir,
}

async fn acceptance_harness(executable: &Path, directory: &Path) -> AcceptanceHarness {
    let workspace = directory.join("workspace");
    std::fs::create_dir_all(workspace.join("denied")).expect("denied root");
    for profile in PROFILES {
        std::fs::create_dir_all(workspace.join("authority").join(profile)).expect("authority");
        if matches!(profile, "repository" | "database") {
            std::fs::create_dir_all(workspace.join("datasets").join(profile)).expect("dataset");
        }
    }
    let workspace = std::fs::canonicalize(workspace).expect("workspace");
    let allowed_environment = [
        "RW_MCP_PROFILE",
        "RW_MCP_PID_FILE",
        "RW_MCP_ALLOWED_WRITE",
        "RW_MCP_DENIED_WRITE",
        "RW_MCP_FILESYSTEM_RESULT",
        "RW_MCP_NETWORK_PROBE",
        "RW_MCP_NETWORK_RESULT",
    ]
    .into_iter()
    .map(str::to_owned);
    let scratch = tempfile::tempdir().expect("scratch");
    let launcher = SandboxedProtocolLauncher::new(
        std::slice::from_ref(&workspace),
        scratch.path(),
        std::env::current_exe().expect("sandbox helper identity"),
        allowed_environment,
    )
    .expect("production launcher");
    let connector = Arc::new(SandboxedStdioConnector::new(
        launcher,
        Arc::new(ApprovedProfiles {
            executable: executable.to_path_buf(),
            workspace: workspace.clone(),
        }),
    ));
    std::fs::create_dir(directory.join("spool")).expect("spool directory");
    let spool = Arc::new(
        FilesystemSpool::new(directory.join("spool"))
            .await
            .expect("spool"),
    );
    let manager = McpManager::new(
        connector,
        spool,
        Arc::new(CompactJsonEncoder),
        McpLimits::default(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback canary");
    listener.set_nonblocking(true).expect("nonblocking");
    for profile in PROFILES {
        manager
            .register(fixture_config(
                executable,
                &workspace,
                profile,
                (profile == "issues").then(|| listener.local_addr().expect("address")),
            ))
            .await
            .expect("register");
    }
    AcceptanceHarness {
        manager,
        listener,
        workspace,
        _scratch: scratch,
    }
}

fn assert_policy_probes(harness: &AcceptanceHarness) {
    for profile in PROFILES {
        let authority = harness.workspace.join("authority").join(profile);
        assert_eq!(
            std::fs::read_to_string(authority.join("filesystem-result"))
                .expect("filesystem result"),
            "denied"
        );
        assert_eq!(
            std::fs::read_to_string(authority.join("allowed.txt")).expect("allowed write"),
            "allowed"
        );
        assert!(
            !harness
                .workspace
                .join("denied")
                .join(format!("{profile}.txt"))
                .exists()
        );
    }
    assert_eq!(
        std::fs::read_to_string(harness.workspace.join("authority/issues/network-result"))
            .expect("network result"),
        "denied"
    );
    assert!(
        harness.listener.accept().is_err(),
        "local SSRF canary was reached"
    );
}

#[tokio::test]
async fn five_distinct_production_sandboxed_servers_remain_deferred_and_bounded() {
    if rw_tools::probe_sandbox().support != rw_tools::SandboxSupport::Enforced {
        return;
    }
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_rw-mcp-fixture"));
    let directory = tempfile::tempdir().expect("temp");
    let harness = acceptance_harness(&executable, directory.path()).await;
    let manager = &harness.manager;

    let connected = manager.connect_all().await;
    assert_eq!(connected.len(), PROFILES.len());
    assert!(
        connected.iter().all(|(_, result)| result.is_ok()),
        "{connected:?}"
    );
    let statuses = manager.statuses().await;
    assert_eq!(statuses.len(), PROFILES.len());
    assert!(statuses.iter().all(|status| {
        matches!(status.state, ServerState::Ready)
            && status.tool_count == 3
            && status.resource_count == 1
            && status.prompt_count == 1
    }));

    let deferred_prompt = manager.deferred_prompt().await.expect("deferred prompt");
    let tokenizer = tiktoken_rs::cl100k_base().expect("tokenizer");
    assert!(tokenizer.encode_with_special_tokens(&deferred_prompt).len() < 2_000);
    assert!(!deferred_prompt.contains("inputSchema"));
    assert_eq!(
        manager.deferred_tool_index().await.len(),
        PROFILES.len() * 3
    );

    let target = McpServerId::new("repository").expect("target id");
    let selected = manager.tool_search("echo_repository", Some(&target)).await;
    assert_eq!(selected.len(), 1);
    let called = manager
        .call_tool(
            &target,
            "echo_repository",
            json!({"value":"five-process-canary"}),
        )
        .await
        .expect("tool call");
    assert!(called.encoded.contains("five-process-canary"));

    assert_policy_probes(&harness);

    let pids = PROFILES
        .iter()
        .map(|profile| {
            std::fs::read_to_string(
                harness
                    .workspace
                    .join("authority")
                    .join(profile)
                    .join("pid"),
            )
            .expect("pid file")
            .parse::<i32>()
            .expect("pid")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(pids.len(), PROFILES.len());
    let shutdown = manager.shutdown().await;
    assert!(shutdown.iter().all(|(_, result)| result.is_ok()));
    assert_reaped(&pids).await;
}
