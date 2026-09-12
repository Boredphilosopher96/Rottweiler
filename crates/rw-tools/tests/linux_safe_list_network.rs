#![allow(clippy::expect_used)]

#[cfg(target_os = "linux")]
fn main() {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    use rw_sandbox::{NetworkPolicy, SandboxPolicy, SandboxSupport};
    use rw_tools::{
        BashSandboxMode, CancellationToken, CommandExecutor, CommandRequest, CommandSafety,
        CommandSafetyClassifier, TokioCommandExecutor, maybe_run_sandbox_helper,
    };
    use rw_types::ToolOutputStream;

    use output::Capture;

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
        let scratch_owner = rw_tools::CommandScratch::create("fixture").expect("scratch owner");
        let scratch = scratch_owner.path().to_path_buf();
        std::fs::create_dir(&workspace).expect("workspace");
        let shadow_bin = workspace.join("shadow-bin");
        std::fs::create_dir(&shadow_bin).expect("shadow bin");
        let shadow_python = shadow_bin.join("python3");
        std::fs::write(
            &shadow_python,
            "#!/bin/sh\nprintf 'PATH python3 was selected' >&2\nexit 91\n",
        )
        .expect("shadow python");
        std::fs::set_permissions(&shadow_python, std::fs::Permissions::from_mode(0o700))
            .expect("shadow python permissions");
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
        let command = format!(
            "/usr/bin/python3 -I {}",
            shell_words::quote(&probe.to_string_lossy())
        );
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
            .sandboxed(
                policy,
                rw_sandbox::SandboxHelper::from_running(
                    &std::env::current_exe().expect("native driver"),
                )
                .expect("running helper"),
                scratch_owner,
            )
            .with_command_safety(classifier)
            .with_policy_egress(true);
        let capture = Arc::new(Capture::default());
        let outcome = executor
            .run(
                CommandRequest {
                    command,
                    cwd: workspace,
                    env: BTreeMap::from([(
                        "PATH".to_owned(),
                        shadow_bin.to_string_lossy().into_owned(),
                    )]),
                    network_domains: Vec::new(),
                    sandbox: BashSandboxMode::Sandboxed,
                },
                CancellationToken::default(),
                capture.clone(),
            )
            .await
            .expect("sandboxed safe-list command");
        let stdout = capture.stream(&ToolOutputStream::Stdout);
        let stderr = capture.stream(&ToolOutputStream::Stderr);
        assert_eq!(
            outcome.exit_code, 0,
            "network probe must observe EPERM\nstdout: {stdout:?}\nstderr: {stderr:?}"
        );
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod output {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use rw_tools::{ToolError, ToolOutputChunk, ToolOutputSink};
    use rw_types::ToolOutputStream;

    #[derive(Default)]
    pub(super) struct Capture(Mutex<Vec<ToolOutputChunk>>);

    #[async_trait]
    impl ToolOutputSink for Capture {
        async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
            self.0.lock().expect("capture lock").push(chunk);
            Ok(())
        }
    }

    impl Capture {
        pub(super) fn stream(&self, stream: &ToolOutputStream) -> String {
            self.0
                .lock()
                .expect("capture lock")
                .iter()
                .filter(|chunk| &chunk.stream == stream)
                .map(|chunk| chunk.content.as_str())
                .collect()
        }
    }
}
