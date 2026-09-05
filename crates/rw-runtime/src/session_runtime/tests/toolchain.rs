#![cfg(test)]
use super::Arc;
use super::BashSandboxMode;
use super::CodeIntelligenceProvider;
use super::CommandExecutor;
use super::CommandFixtureMode;
use super::CommandSafetyClassifier;
use super::ExecutionLease;
use super::FixtureCodeIntelligence;
use super::FixtureToolchainExecutor;
use super::HookEvent;
use super::HookFailurePolicy;
use super::RuntimeServiceDescriptor;
use super::RuntimeServiceKind;
use super::SandboxSupport;
use super::ToolOutput;
use super::ToolchainConfig;
use super::ToolchainRuntime;
use super::build_command_executor;
use super::compose_runtime_hooks;
use super::probe_sandbox;
use super::semantic_file_tools;
use super::tempdir;
use super::toolchain_command_identity;

#[test]
fn runtime_service_view_reports_only_live_toolchain_commands() {
    let executor: Arc<dyn CommandExecutor> = Arc::new(FixtureToolchainExecutor::default());
    let runtime = Arc::new(ToolchainRuntime::new(executor, &[]));
    assert!(runtime.active_services().is_empty());

    let formatter = runtime.enter(RuntimeServiceKind::Formatter, "rustfmt".to_owned());
    let duplicate = runtime.enter(RuntimeServiceKind::Formatter, "rustfmt".to_owned());
    let linter = runtime.enter(RuntimeServiceKind::Linter, "clippy-driver".to_owned());
    assert_eq!(
        runtime.active_services(),
        vec![
            RuntimeServiceDescriptor {
                kind: RuntimeServiceKind::Linter,
                name: "clippy-driver".to_owned(),
            },
            RuntimeServiceDescriptor {
                kind: RuntimeServiceKind::Formatter,
                name: "rustfmt".to_owned(),
            },
        ]
    );

    drop(duplicate);
    assert_eq!(runtime.active_services().len(), 2);
    drop(formatter);
    drop(linter);
    assert!(runtime.active_services().is_empty());
}

#[test]
fn toolchain_service_identity_never_exposes_arguments_or_parent_paths() {
    assert_eq!(
        toolchain_command_identity(
            RuntimeServiceKind::Formatter,
            "/opt/tools/bin/rustfmt --edition 2024 src/lib.rs",
        ),
        "rustfmt"
    );
    assert_eq!(
        toolchain_command_identity(RuntimeServiceKind::Linter, "'cargo clippy' --fix"),
        "linter"
    );
    assert_eq!(
        toolchain_command_identity(
            RuntimeServiceKind::Formatter,
            "TOKEN=secret-canary rustfmt src/lib.rs",
        ),
        "formatter"
    );
    assert_eq!(
        toolchain_command_identity(RuntimeServiceKind::Linter, ""),
        "linter"
    );
}

#[tokio::test]
async fn toolchain_post_hook_formats_multi_edit_then_appends_linter_diagnostics() {
    let root = tempdir().expect("workspace");
    let source = root.path().join("src");
    std::fs::create_dir(&source).expect("source directory");
    std::fs::write(source.join("lib.rs"), "fn main(){}\n").expect("source file");
    let executor = Arc::new(FixtureToolchainExecutor::default());
    let runtime = Arc::new(ToolchainRuntime::new(
        executor.clone(),
        &[root.path().to_path_buf()],
    ));
    let hooks = compose_runtime_hooks(
        &ToolchainConfig {
            formatter: Some("fixture-format {file}".to_owned()),
            linters: vec!["fixture-lint {file}".to_owned()],
            test: None,
            rules: Vec::new(),
        },
        runtime,
        semantic_file_tools(),
        None,
    )
    .expect("toolchain hooks");
    let result = hooks
        .dispatch(
            HookEvent::PostTool,
            serde_json::json!({
                "id": "call",
                "name": "multi_edit",
                "arguments": {"path": "src/lib.rs", "edits": []},
                "output": {"type": "text", "text": "multi edit complete"},
                "is_error": false,
            }),
        )
        .await;
    assert!(result.completed());
    assert_eq!(result.payload()["is_error"], true);
    let output: ToolOutput =
        serde_json::from_value(result.payload()["output"].clone()).expect("tool output");
    assert!(matches!(
        output,
        ToolOutput::Text { text }
            if text.contains("fixture diagnostic") && text.contains("linter exit code: 1")
    ));
    let calls = executor
        .calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(calls.len(), 2);
    assert!(calls[0].command.starts_with("fixture-format "));
    assert!(calls[1].command.starts_with("fixture-lint "));
    assert!(calls.iter().all(|call| {
        call.sandbox == BashSandboxMode::Sandboxed && call.network_domains.is_empty()
    }));
}

#[tokio::test]
async fn toolchain_test_runs_only_after_successful_turns_and_blocks_on_failure() {
    let root = tempdir().expect("workspace");
    let executor = Arc::new(FixtureToolchainExecutor::default());
    let runtime = Arc::new(ToolchainRuntime::new(
        executor.clone(),
        &[root.path().to_path_buf()],
    ));
    let hooks = compose_runtime_hooks(
        &ToolchainConfig {
            formatter: None,
            linters: Vec::new(),
            test: Some("fixture-lint suite".to_owned()),
            rules: Vec::new(),
        },
        runtime,
        semantic_file_tools(),
        None,
    )
    .expect("toolchain hooks");

    let skipped = hooks
        .dispatch(
            HookEvent::TurnEnd,
            serde_json::json!({"turn": 1, "status": "Failed"}),
        )
        .await;
    assert!(skipped.completed());
    assert!(
        executor
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );

    let failed = hooks
        .dispatch(
            HookEvent::TurnEnd,
            serde_json::json!({"turn": 2, "status": "Completed"}),
        )
        .await;
    assert!(matches!(
        failed.status(),
        rw_ext::HookDispatchStatus::Blocked { hook_id, message }
            if hook_id == "builtin.toolchain_test"
                && message.contains("test exit code: 1")
                && message.contains("fixture diagnostic")
    ));
    let calls = executor
        .calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].cwd,
        std::fs::canonicalize(root.path()).expect("canonical workspace")
    );
    assert_eq!(calls[0].sandbox, BashSandboxMode::Sandboxed);
    assert!(calls[0].network_domains.is_empty());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn production_toolchain_runs_sandboxed_rustfmt_and_offline_clippy() {
    let rustfmt_available = std::process::Command::new("rustfmt")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    let clippy_available = std::process::Command::new("cargo")
        .args(["clippy", "--version"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !rustfmt_available || !clippy_available {
        assert!(
            std::env::var_os("CI").is_none(),
            "M6 acceptance requires the rustfmt and clippy components in CI"
        );
        eprintln!("skipping real toolchain acceptance: rustfmt or clippy is unavailable");
        return;
    }

    let root = tempdir().expect("workspace");
    let private = tempdir().expect("private runtime state");
    std::fs::create_dir_all(root.path().join("crate/src")).expect("source directory");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crate\"]\nresolver = \"3\"\n",
    )
    .expect("workspace manifest");
    std::fs::write(
        root.path().join("crate/Cargo.toml"),
        "[package]\nname = \"toolchain-acceptance\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("crate manifest");
    std::fs::write(
        root.path().join("crate/src/lib.rs"),
        "pub fn bad(value:&Vec<u8>)->usize{value.len()}\n",
    )
    .expect("unformatted source");
    let roots = vec![root.path().to_path_buf()];
    let lease = Arc::new(
        ExecutionLease::acquire(private.path().join("execution.lock")).expect("execution lease"),
    );
    let safety = Arc::new(CommandSafetyClassifier::default());
    let executor = build_command_executor(
        &roots,
        root.path(),
        CommandFixtureMode::Live,
        &lease,
        &safety,
        None,
    )
    .expect("production sandboxed executor");
    let runtime = Arc::new(ToolchainRuntime::new(executor, &roots));
    let hooks = compose_runtime_hooks(
            &ToolchainConfig {
                formatter: Some("rustfmt {file}".to_owned()),
                linters: vec![
                    "env -u CARGO_TARGET_DIR cargo clippy --offline --workspace --all-targets -- -D warnings".to_owned(),
                ],
                test: None,
                rules: Vec::new(),
            },
            runtime,
            semantic_file_tools(),
            None,
        )
        .expect("production toolchain hooks");
    let result = hooks
            .dispatch(
                HookEvent::PostTool,
                serde_json::json!({
                    "id": "real-toolchain-call",
                    "name": "edit",
                    "arguments": {"path": "crate/src/lib.rs", "old": "value:&Vec<u8>", "new": "value: &[u8]"},
                    "output": {"type": "text", "text": "edit complete"},
                    "is_error": false,
                }),
            )
            .await;

    let sandbox = probe_sandbox();
    if sandbox.support != SandboxSupport::Enforced {
        assert_eq!(
            std::fs::read_to_string(root.path().join("crate/src/lib.rs"))
                .expect("unchanged source"),
            "pub fn bad(value:&Vec<u8>)->usize{value.len()}\n",
            "an unavailable sandbox must fail closed before rustfmt mutates the workspace"
        );
        if result.completed() {
            assert_eq!(result.payload()["is_error"], true);
            let output: ToolOutput = serde_json::from_value(result.payload()["output"].clone())
                .expect("tool output with sandbox diagnostics");
            let ToolOutput::Text { text } = output else {
                panic!("sandbox refusal diagnostics must append to text output")
            };
            assert!(text.contains("Toolchain diagnostics"), "{text}");
            assert!(text.contains("formatter exit code:"), "{text}");
            assert!(text.contains("linter exit code:"), "{text}");
        } else {
            assert_eq!(result.failures().len(), 1, "{:#?}", result.status());
            assert_eq!(
                result.failures()[0].policy(),
                HookFailurePolicy::FailClosed,
                "sandbox launch errors must not be allowed open"
            );
        }
        assert!(
            sandbox.warning.is_some(),
            "an unavailable sandbox capability must explain the degradation"
        );
        return;
    }
    assert!(result.completed(), "{:#?}", result.status());
    assert_eq!(
        std::fs::read_to_string(root.path().join("crate/src/lib.rs")).expect("formatted source"),
        "pub fn bad(value: &Vec<u8>) -> usize {\n    value.len()\n}\n"
    );
    assert_eq!(result.payload()["is_error"], true);
    let output: ToolOutput = serde_json::from_value(result.payload()["output"].clone())
        .expect("tool output with diagnostics");
    let ToolOutput::Text { text } = output else {
        panic!("toolchain diagnostics must append to text output")
    };
    assert!(text.contains("Toolchain diagnostics"));
    assert!(text.contains("ptr_arg") || text.contains("&[_]"), "{text}");
    assert!(text.contains("linter exit code:"), "{text}");
}

#[tokio::test]
async fn post_multi_edit_hook_appends_lsp_diagnostics_without_running_a_build() {
    let root = tempdir().expect("workspace");
    let source = root.path().join("src");
    std::fs::create_dir(&source).expect("source directory");
    std::fs::write(source.join("lib.rs"), "fn broken() {}\n").expect("source file");
    let executor = Arc::new(FixtureToolchainExecutor::default());
    let runtime = Arc::new(ToolchainRuntime::new(
        executor.clone(),
        &[root.path().to_path_buf()],
    ));
    let intelligence: Arc<dyn CodeIntelligenceProvider> = Arc::new(FixtureCodeIntelligence);
    let hooks = compose_runtime_hooks(
        &ToolchainConfig::default(),
        runtime,
        semantic_file_tools(),
        Some(intelligence),
    )
    .expect("runtime hooks");
    let result = hooks
        .dispatch(
            HookEvent::PostTool,
            serde_json::json!({
                "id": "call",
                "name": "multi_edit",
                "arguments": {"path": "src/lib.rs", "edits": []},
                "output": {"type": "text", "text": "multi edit complete"},
                "is_error": false,
            }),
        )
        .await;
    assert!(result.completed());
    let output: ToolOutput =
        serde_json::from_value(result.payload()["output"].clone()).expect("tool output");
    assert!(matches!(
        output,
        ToolOutput::Text { text }
            if text.contains("LSP diagnostics (untrusted)")
                && text.contains("type mismatch")
                && text.contains("&lt;/rottweiler")
    ));
    assert!(
        executor
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "LSP diagnostics must not invoke a formatter, linter, or build"
    );
}
