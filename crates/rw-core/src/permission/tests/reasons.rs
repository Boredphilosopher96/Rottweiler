use super::*;

/// Records the request the gate hands to the prompt and denies it.
#[derive(Default)]
struct Capture(Mutex<Option<PermissionRequest>>);

#[async_trait]
impl PermissionApprover for Capture {
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision {
        *self.0.lock().expect("capture") = Some(request);
        ApprovalDecision::Deny
    }
}

impl Capture {
    fn rationale(&self) -> Option<String> {
        self.0
            .lock()
            .expect("capture")
            .as_ref()
            .expect("prompted")
            .prompt_reason
            .clone()
    }
}

fn shell(command: &str, sandbox: &str, network: &[&str]) -> PermissionRequest {
    PermissionRequest {
        prompt_reason: None,
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "reason-bash".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": command,
            "cwd": ".",
            "env": {},
            "network_domains": network,
            "sandbox": sandbox,
        }),
        capabilities: vec![ToolCapability::Execute],
        approval_diff: None,
    }
}

fn write(path: &Path) -> PermissionRequest {
    PermissionRequest {
        prompt_reason: None,
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "reason-write".to_owned(),
        tool_name: "write".to_owned(),
        arguments: json!({ "path": path, "content": "fixture" }),
        capabilities: vec![ToolCapability::WriteFilesystem],
        approval_diff: None,
    }
}

#[tokio::test]
async fn shell_prompts_explain_the_policy_fact_behind_them() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    let approver = Capture::default();
    authorize_with_behavior(
        &gate,
        shell("python --version", "sandboxed", &[]),
        ToolBehavior::Shell,
        &approver,
    )
    .await;
    assert_eq!(
        approver.rationale().as_deref(),
        Some("Not in the safe command list")
    );

    authorize_with_behavior(
        &gate,
        shell("curl example.com", "sandboxed", &["Example.com."]),
        ToolBehavior::Shell,
        &approver,
    )
    .await;
    assert_eq!(
        approver.rationale().as_deref(),
        Some("Network access to example.com")
    );

    authorize_with_behavior(
        &gate,
        shell("python --version", "unsandboxed", &[]),
        ToolBehavior::Shell,
        &approver,
    )
    .await;
    assert_eq!(
        approver.rationale().as_deref(),
        Some("Runs outside the sandbox, without filesystem or network isolation")
    );
}

#[tokio::test]
async fn hook_confirmation_is_named_when_policy_alone_would_allow() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    let approver = Capture::default();
    authorize_with_behavior_in_mode(
        &gate,
        shell("git status", "sandboxed", &[]),
        ToolBehavior::Shell,
        &approver,
        Some(HookPermissionDecision::Ask),
        SessionMode::Execute,
    )
    .await;
    assert_eq!(
        approver.rationale().as_deref(),
        Some("A hook asked to confirm this")
    );
}

#[tokio::test]
async fn edits_explain_only_writes_outside_the_workspace() {
    let root = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let gate = PermissionGate::new(PermissionDecision::Ask).with_workspace_roots([root.path()]);
    let approver = Capture::default();

    authorize_registered_file_mutation(&gate, write(&root.path().join("calc.py")), &approver).await;
    assert_eq!(approver.rationale(), None);

    authorize_registered_file_mutation(&gate, write(&outside.path().join("calc.py")), &approver)
        .await;
    assert_eq!(
        approver.rationale().as_deref(),
        Some("Writes outside the workspace")
    );
}
