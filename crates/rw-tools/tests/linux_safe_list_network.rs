#![allow(clippy::expect_used)]

#[cfg(target_os = "linux")]
fn main() {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rw_sandbox::{NetworkPolicy, SandboxPolicy, SandboxSupport};
    use rw_tools::{
        BashSandboxMode, CancellationToken, CommandExecutor, CommandRequest, CommandSafety,
        CommandSafetyClassifier, NoopOutputSink, TokioCommandExecutor, maybe_run_sandbox_helper,
    };

    if maybe_run_sandbox_helper(std::env::args_os()).expect("sandbox helper dispatch") {
        unreachable!("sandbox helper replaces the process");
    }

    let capability = rw_sandbox::probe();
    if capability.support != SandboxSupport::Enforced {
        let warning = capability
            .warning
            .as_deref()
            .unwrap_or("Linux sandbox capability unavailable");
        assert!(
            std::env::var_os("ROTTWEILER_REQUIRE_LINUX_SANDBOX").is_none(),
            "safe-list network isolation is required, but the host reported: {warning}"
        );
        eprintln!("skipping safe-list network isolation: {warning}");
        return;
    }

    let runtime = tokio::runtime::Runtime::new().expect("Tokio runtime");
    runtime.block_on(async {
        let root = tempfile::tempdir().expect("temporary directory");
        let workspace = root.path().join("workspace");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::create_dir(&scratch).expect("scratch");
        let probe = workspace.join("network-denial-probe.py");
        std::fs::write(
            &probe,
            r#"import errno, os, socket, sys
if any(os.environ.get(k) for k in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy")):
    sys.exit(94)
try:
    socket.socket(socket.AF_INET, socket.SOCK_STREAM)
except OSError as error:
    sys.exit(0 if error.errno in (errno.EPERM, errno.EACCES) else 93)
sys.exit(92)
"#,
        )
        .expect("network denial probe");
        let command = format!("python3 {}", shell_words::quote(&probe.to_string_lossy()));
        let classifier = Arc::new(
            CommandSafetyClassifier::new(&[globset::escape(&command)])
                .expect("safe-list classifier"),
        );
        assert_eq!(classifier.classify(&command), CommandSafety::SafeListed);

        let policy = Arc::new(
            SandboxPolicy::new([&workspace, &scratch], NetworkPolicy::Deny)
                .expect("sandbox policy"),
        );
        let executor = TokioCommandExecutor::default()
            .sandboxed(policy)
            .with_command_safety(classifier)
            .with_policy_egress(true);
        let outcome = executor
            .run(
                CommandRequest {
                    command,
                    cwd: workspace,
                    env: BTreeMap::new(),
                    network_domains: Vec::new(),
                    sandbox: BashSandboxMode::Sandboxed,
                },
                CancellationToken::default(),
                Arc::new(NoopOutputSink),
            )
            .await
            .expect("sandboxed safe-list command");
        assert_eq!(outcome.exit_code, 0, "network probe must observe EPERM");
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {}
