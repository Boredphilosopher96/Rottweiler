#![allow(clippy::expect_used)]
use super::{McpApprovalStore, request};
use rw_tools::{ProtocolChildLauncher as _, SandboxedProtocolLauncher};
use std::{fs, os::unix::fs::PermissionsExt as _, path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn approved_mcp_bytes_survive_replacement_without_losing_workspace_authority() {
    let _admission = crate::native_fixture::admit().await;
    let fixture = Fixture::new();
    let catalog = crate::extension_config::discover_executable_configs(
        &fixture.user,
        &fixture.workspace,
        false,
    )
    .expect("discover exact MCP command");
    let discovered = &catalog.mcp_servers[0];
    let mut config = discovered
        .runtime_config(|_| unreachable!("no credentials"))
        .expect("runtime request");
    let store =
        McpApprovalStore::open(&fixture.private, &catalog.mcp_servers).expect("approval owner");
    let roots = [fixture.workspace.clone()];
    assert!(!config.enabled);
    config.enabled = true; // The manager may explicitly enable an approved server.
    assert!(
        store
            .capture_stdio(&config, &roots, &fixture.workspace)
            .await
            .is_err()
    );
    store
        .approve_server(&config.id)
        .expect("approve exact fingerprint");
    let mut changed = config.clone();
    if let rw_mcp::McpTransportConfig::Stdio { environment, .. } = &mut changed.transport {
        environment.push(("UNAPPROVED".into(), "canary".into()));
    }
    assert!(
        store
            .capture_stdio(&changed, &roots, &fixture.workspace)
            .await
            .is_err()
    );
    let captured = store
        .capture_stdio(&config, &roots, &fixture.workspace)
        .await
        .expect("pin approved bytes");
    fs::write(&fixture.executable, b"replaced executable").expect("replace installation");
    fs::write(
        fixture.user.join("entry.js"),
        b"throw new Error('replaced')",
    )
    .expect("replace entry");
    fs::write(fixture.user.join("payload.txt"), b"replaced").expect("replace input");
    assert!(
        store
            .capture_stdio(&config, &roots, &fixture.workspace)
            .await
            .is_err()
    );
    let helper = crate::native_fixture::sandbox_helper().expect("explicit helper prerequisite");
    let launcher =
        SandboxedProtocolLauncher::new(&roots, &fixture.scratch, &helper, Vec::<String>::new())
            .expect("sandbox authority")
            .with_approved_command(captured);
    let mut child = launcher
        .spawn(&request(&config).expect("stdio request"))
        .await
        .expect("launch pinned command");
    let mut ready = [0; 5];
    tokio::time::timeout(Duration::from_secs(3), child.stdout.read_exact(&mut ready))
        .await
        .expect("readiness deadline")
        .expect("actual child readiness");
    assert_eq!(&ready, b"ready");
    drop(launcher);
    child
        .stdin
        .write_all(b"continue\n")
        .await
        .expect("continue after launcher retirement");
    child.stdin.shutdown().await.expect("close input");
    let mut output = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        (&mut child.stdout).take(1024).read_to_end(&mut output),
    )
    .await
    .expect("output deadline")
    .expect("bounded output");
    assert_eq!(output, b"approved:workspace");
    assert_eq!(
        fs::read(fixture.workspace.join("result.txt")).expect("workspace effect"),
        b"approved:workspace"
    );
    child
        .handle
        .terminate_and_reap(Duration::from_secs(3))
        .await
        .expect("physical group settlement");
}

struct Fixture {
    _directory: tempfile::TempDir,
    user: PathBuf,
    workspace: PathBuf,
    executable: PathBuf,
    private: PathBuf,
    scratch: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("fixture");
        let root = directory.path().canonicalize().expect("canonical fixture");
        let user = root.join("user");
        let workspace = root.join("workspace");
        let private = root.join("private");
        let scratch = root.join("scratch");
        for path in [&user, &workspace, &private, &scratch] {
            fs::create_dir(path).expect("directory");
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private mode");
        }
        fs::create_dir(user.join(".rottweiler")).expect("user configuration");
        let executable = user.join("runtime");
        let path = std::env::var_os("PATH").expect("PATH");
        let bun = std::env::split_paths(&path)
            .map(|root| root.join("bun"))
            .find(|path| path.is_file())
            .expect("Bun is required for native MCP conformance");
        fs::copy(bun, &executable).expect("movable native runtime");
        fs::write(
            user.join("entry.js"),
            r"import { readFileSync, writeFileSync } from 'node:fs';
process.stdout.write('ready');
for await (const _ of Bun.stdin.stream()) {}
const result = readFileSync(Bun.argv[2], 'utf8') + ':' + readFileSync('workspace.txt', 'utf8');
writeFileSync('result.txt', result);
process.stdout.write(result);",
        )
        .expect("entry");
        fs::write(user.join("payload.txt"), b"approved").expect("approved input");
        fs::write(workspace.join("workspace.txt"), b"workspace").expect("workspace input");
        fs::write(user.join(".rottweiler/mcp.toml"), format!(
            "[servers.pinned]\nenabled=false\nargv=['{}','{}','{}']\ncwd='{}'\nread_roots=['{}']\nwrite_roots=['{}']\n",
            executable.display(), user.join("entry.js").display(), user.join("payload.txt").display(),
            workspace.display(), workspace.display(), workspace.display(),
        )).expect("MCP configuration");
        Self {
            _directory: directory,
            user,
            workspace,
            executable,
            private,
            scratch,
        }
    }
}
