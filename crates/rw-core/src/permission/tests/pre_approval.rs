use super::*;

fn shell(command: &str, sandbox: &str, network: &[&str]) -> PermissionRequest {
    PermissionRequest {
        prompt_reason: None,
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: format!("bash-{command}"),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": command,
            "cwd": ".",
            "env": {},
            "network_domains": network,
            "sandbox": sandbox,
        }),
        capabilities: vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
        approval_diff: None,
    }
}

fn pre_approved(gate: &PermissionGate, patterns: &[&str]) -> PermissionGate {
    gate.with_turn_pre_approvals(
        &patterns
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect::<Vec<_>>(),
    )
    .expect("pre-approvals")
}

async fn prompts(gate: &PermissionGate, request: PermissionRequest) -> (PermissionOutcome, usize) {
    let approver = CountingDeny(AtomicUsize::new(0));
    let outcome = authorize_with_behavior(gate, request, ToolBehavior::Shell, &approver).await;
    (outcome, approver.0.load(Ordering::SeqCst))
}

#[tokio::test]
async fn pre_approved_patterns_skip_the_prompt_only_for_matching_commands() {
    let base = PermissionGate::new(PermissionDecision::Ask);
    let gate = pre_approved(&base, &["bash(cargo build*)"]);

    assert_eq!(
        prompts(&gate, shell("cargo build --release", "sandboxed", &[])).await,
        (PermissionOutcome::Allowed, 0)
    );
    assert_eq!(
        prompts(&gate, shell("cargo publish", "sandboxed", &[])).await,
        (PermissionOutcome::Denied, 1),
        "other tools and commands stay available and follow normal policy"
    );
    assert_eq!(
        prompts(
            &gate,
            shell("cargo build && cargo publish", "sandboxed", &[])
        )
        .await,
        (PermissionOutcome::Denied, 1),
        "every command of a compound invocation must be pre-approved"
    );
    assert_eq!(
        prompts(&base, shell("cargo build", "sandboxed", &[])).await,
        (PermissionOutcome::Denied, 1),
        "pre-approval is scoped to the turn gate"
    );
}

#[tokio::test]
async fn deny_rules_sandbox_escape_network_and_hooks_take_precedence() {
    let configured = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![PermissionRule {
            pattern: "bash(git status --porcelain*)".to_owned(),
            action: PermissionDecision::Deny,
        }],
    });
    let gate = pre_approved(&configured, &["bash(git status*)"]);
    assert_eq!(
        prompts(&gate, shell("git status --porcelain", "sandboxed", &[])).await,
        (PermissionOutcome::Denied, 0)
    );
    assert_eq!(
        prompts(&gate, shell("git status", "unsandboxed", &[])).await,
        (PermissionOutcome::Denied, 1),
        "unsandboxed execution still asks"
    );
    assert_eq!(
        prompts(&gate, shell("git status", "sandboxed", &["example.com"])).await,
        (PermissionOutcome::Denied, 1),
        "network domains still ask"
    );
    let approver = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior_in_mode(
            &gate,
            shell("git status", "sandboxed", &[]),
            ToolBehavior::Shell,
            &approver,
            Some(HookPermissionDecision::Ask),
            SessionMode::Execute,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        approver.0.load(Ordering::SeqCst),
        1,
        "a hook ask still prompts"
    );
}

#[tokio::test]
async fn read_only_modes_still_forbid_pre_approved_mutation() {
    let gate = pre_approved(&PermissionGate::new(PermissionDecision::Ask), &["write(*)"]);
    let write = PermissionRequest {
        prompt_reason: None,
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "write".to_owned(),
        tool_name: "write".to_owned(),
        arguments: json!({ "path": "notes.md", "content": "x" }),
        capabilities: vec![ToolCapability::WriteFilesystem],
        approval_diff: None,
    };
    for mode in [SessionMode::Discuss, SessionMode::Plan] {
        assert_eq!(
            authorize_with_behavior_in_mode(
                &gate,
                write.clone(),
                ToolBehavior::Standard,
                &Decision(ApprovalDecision::AllowOnce),
                None,
                mode,
            )
            .await,
            PermissionOutcome::Denied
        );
    }
    assert_eq!(
        authorize_with_behavior(
            &gate,
            write,
            ToolBehavior::Standard,
            &CountingDeny(AtomicUsize::new(0))
        )
        .await,
        PermissionOutcome::Allowed
    );
}

#[cfg(unix)]
#[tokio::test]
async fn session_approvals_granted_during_the_turn_outlive_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = PermissionGate::new(PermissionDecision::Ask).with_workspace_roots([root.path()]);
    let gate = pre_approved(&base, &["bash(cargo build*)"]);
    let invocation = bash_request("/bin/echo stable", root.path());
    assert_eq!(
        authorize_with_behavior(
            &gate,
            invocation.clone(),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::AllowSession),
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(base.snapshot().session_approvals, 1);
    assert_eq!(
        prompts(&base, invocation).await,
        (PermissionOutcome::Allowed, 0)
    );
}

#[test]
fn malformed_pre_approvals_are_rejected() {
    assert!(
        PermissionGate::new(PermissionDecision::Ask)
            .with_turn_pre_approvals(&["bash".to_owned()])
            .is_err()
    );
}
