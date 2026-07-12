#![allow(clippy::expect_used)]

#[cfg(target_os = "linux")]
#[allow(clippy::items_after_statements, clippy::too_many_lines)]
fn main() {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use rw_sandbox::{NetworkPolicy, SandboxPolicy, SandboxSupport};
    use rw_tools::{
        BashSandboxMode, CancellationToken, CommandExecutor, CommandOutcome, CommandRequest,
        ExecutionLease, RecordingCommandExecutor, ReplayCommandExecutor, TokioCommandExecutor,
        ToolError, ToolOutputChunk, ToolOutputSink, maybe_run_sandbox_helper,
    };
    use rw_types::ToolOutputStream;

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
            "command recording sandbox is required, but the host reported: {warning}"
        );
        eprintln!("skipping command recording sandbox: {warning}");
        return;
    }

    #[derive(Default)]
    struct Capture(Mutex<Vec<ToolOutputChunk>>);

    #[async_trait]
    impl ToolOutputSink for Capture {
        async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
            self.0.lock().expect("capture lock").push(chunk);
            Ok(())
        }
    }

    fn captured(capture: &Capture, stream: &ToolOutputStream) -> String {
        capture
            .0
            .lock()
            .expect("capture lock")
            .iter()
            .filter(|chunk| &chunk.stream == stream)
            .map(|chunk| chunk.content.as_str())
            .collect()
    }

    fn assert_success(
        outcome: CommandOutcome,
        capture: &Capture,
        expected_stdout: &str,
        expected_stderr: &str,
    ) {
        assert_eq!(outcome.exit_code, 0, "guarded command did not execute");
        assert_eq!(
            captured(capture, &ToolOutputStream::Stdout),
            expected_stdout
        );
        assert_eq!(
            captured(capture, &ToolOutputStream::Stderr),
            expected_stderr
        );
    }

    let runtime = tokio::runtime::Runtime::new().expect("Tokio runtime");
    runtime.block_on(async {
        let root = tempfile::tempdir().expect("temporary directory");
        let workspace = root.path().join("workspace");
        let hook_scratch = root.path().join("hook-scratch");
        let private = root.path().join("private");
        let recordings = root.path().join("recordings");
        let hook_recordings = recordings.join("read-only-hooks");
        for directory in [&workspace, &hook_scratch, &private] {
            std::fs::create_dir(directory).expect("test directory");
        }
        let workspace = std::fs::canonicalize(workspace).expect("canonical workspace");
        let hook_scratch = std::fs::canonicalize(hook_scratch).expect("canonical hook scratch");
        let lease = Arc::new(
            ExecutionLease::acquire(private.join("execution.lock")).expect("execution lease"),
        );

        let ordinary_policy = Arc::new(
            SandboxPolicy::new([&workspace], NetworkPolicy::Deny).expect("ordinary policy"),
        );
        let hook_policy = Arc::new(
            SandboxPolicy::new([&hook_scratch], NetworkPolicy::Deny).expect("hook policy"),
        );
        let ordinary_live: Arc<dyn CommandExecutor> = Arc::new(
            TokioCommandExecutor::with_execution_lease(Arc::clone(&lease))
                .sandboxed(ordinary_policy),
        );
        let hook_live: Arc<dyn CommandExecutor> = Arc::new(
            TokioCommandExecutor::with_execution_lease(Arc::clone(&lease)).sandboxed(hook_policy),
        );
        let ordinary = RecordingCommandExecutor::new(ordinary_live, &recordings, &workspace)
            .expect("ordinary recorder");
        let hook = RecordingCommandExecutor::new(hook_live, &hook_recordings, &hook_scratch)
            .expect("hook recorder");
        let ordinary_request = CommandRequest {
            command: "printf ordinary-out; printf ordinary-err >&2".to_owned(),
            cwd: workspace.clone(),
            env: BTreeMap::new(),
            network_domains: Vec::new(),
            sandbox: BashSandboxMode::Sandboxed,
        };
        let hook_request = CommandRequest {
            command: "printf hook-out; printf hook-err >&2".to_owned(),
            cwd: hook_scratch.clone(),
            env: BTreeMap::from([
                (
                    "HOME".to_owned(),
                    hook_scratch.to_string_lossy().into_owned(),
                ),
                (
                    "TMPDIR".to_owned(),
                    hook_scratch.to_string_lossy().into_owned(),
                ),
            ]),
            network_domains: Vec::new(),
            sandbox: BashSandboxMode::Sandboxed,
        };

        let ordinary_capture = Arc::new(Capture::default());
        let ordinary_outcome = ordinary
            .run(
                ordinary_request.clone(),
                CancellationToken::default(),
                ordinary_capture.clone(),
            )
            .await
            .expect("record ordinary command");
        assert_success(
            ordinary_outcome,
            &ordinary_capture,
            "ordinary-out",
            "ordinary-err",
        );

        let hook_capture = Arc::new(Capture::default());
        let hook_outcome = hook
            .run(
                hook_request.clone(),
                CancellationToken::default(),
                hook_capture.clone(),
            )
            .await
            .expect("record hook command");
        assert_success(hook_outcome, &hook_capture, "hook-out", "hook-err");

        for path in [
            recordings.join("commands.json"),
            hook_recordings.join("commands.json"),
        ] {
            let occurrences: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).expect("persisted command fixture"))
                    .expect("valid command fixture");
            assert_eq!(occurrences.as_array().map(Vec::len), Some(1));
        }
        drop(ordinary);
        drop(hook);

        let ordinary =
            ReplayCommandExecutor::load(&recordings, &workspace).expect("ordinary command replay");
        let hook = ReplayCommandExecutor::load(&hook_recordings, &hook_scratch)
            .expect("hook command replay");

        let ordinary_capture = Arc::new(Capture::default());
        let ordinary_outcome = ordinary
            .run(
                ordinary_request.clone(),
                CancellationToken::default(),
                ordinary_capture.clone(),
            )
            .await
            .expect("replay ordinary command");
        assert_success(
            ordinary_outcome,
            &ordinary_capture,
            "ordinary-out",
            "ordinary-err",
        );

        let hook_capture = Arc::new(Capture::default());
        let hook_outcome = hook
            .run(
                hook_request.clone(),
                CancellationToken::default(),
                hook_capture.clone(),
            )
            .await
            .expect("replay hook command");
        assert_success(hook_outcome, &hook_capture, "hook-out", "hook-err");

        for (executor, request) in [
            (&ordinary as &dyn CommandExecutor, ordinary_request),
            (&hook as &dyn CommandExecutor, hook_request),
        ] {
            let error = executor
                .run(
                    request,
                    CancellationToken::default(),
                    Arc::new(Capture::default()),
                )
                .await
                .expect_err("each namespaced occurrence is consumed exactly once");
            assert!(error.to_string().contains("exhausted"));
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {}
