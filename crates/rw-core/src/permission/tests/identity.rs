use super::super::rules::canonical_shell_commands;
use super::*;

#[test]
fn canonical_shell_requires_every_simple_command_and_normalizes_rm_flags() {
    assert_eq!(
        canonical_shell_commands("/usr/bin/git status && rm -fr build"),
        Some(vec!["git status".to_owned(), "rm -rf build".to_owned()])
    );
    assert!(canonical_shell_commands("bash -c 'git status'").is_none());
    assert!(canonical_shell_commands("echo $(cat secret)").is_none());
    for command in [
        "LD_PRELOAD=/tmp/injected.so cat file",
        "cat $FILE",
        "cat ${FILE}",
        "cat *.rs",
        "cat file?.rs",
        "cat [ab].rs",
        "printf {a,b}",
        "cat ~/secret",
    ] {
        assert!(
            canonical_shell_commands(command).is_none(),
            "runtime-expanded command matched an allow-rule target: {command}"
        );
    }
}

#[test]
fn exact_shell_identity_preserves_assignments_argv_boundaries_order_and_operators() {
    let cwd = tempfile::tempdir().expect("cwd");
    let identity = |command| {
        canonical_key_arguments_for(&bash_request(command, cwd.path()), ToolBehavior::Shell)
    };
    assert_ne!(
        identity("FLAG=a /bin/echo x"),
        identity("FLAG=b /bin/echo x")
    );
    assert_ne!(identity("/bin/echo 'a b'"), identity("/bin/echo a b"));
    assert_ne!(
        identity("/bin/echo a && /bin/echo b"),
        identity("/bin/echo b && /bin/echo a")
    );
    assert_ne!(
        identity("/bin/echo a && /bin/echo b"),
        identity("/bin/echo a ; /bin/echo b")
    );
}

#[tokio::test]
async fn nonrememberable_approval_is_denied_when_workspace_changes_while_prompting() {
    let roots = tempfile::tempdir().expect("roots");
    let initial = roots.path().join("initial");
    let replacement = roots.path().join("replacement");
    fs::create_dir(&initial).expect("initial root");
    fs::create_dir(&replacement).expect("replacement root");

    for decision in [
        ApprovalDecision::AllowSession,
        ApprovalDecision::AllowProject,
    ] {
        let gate =
            Arc::new(PermissionGate::new(PermissionDecision::Ask).with_workspace_roots([&initial]));
        let approver = ChangeWorkspaceThenApprove {
            gate: Arc::clone(&gate),
            replacement: replacement.clone(),
            decision,
        };
        assert_eq!(
            authorize_with_behavior(
                &gate,
                bash_request("/bin/echo approved > output", &initial),
                ToolBehavior::Shell,
                &approver,
            )
            .await,
            PermissionOutcome::Denied,
            "an approval from the old workspace generation must never execute"
        );
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn explicitly_typed_unsandboxed_patterns_are_rememberable_without_generic_escalation() {
    let root = tempfile::tempdir().expect("root");
    let request = |command: &str| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "unsandboxed-pattern".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": command,
            "cwd": root.path(),
            "env": {},
            "network_domains": [],
            "sandbox": "unsandboxed",
        }),
        capabilities: vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
        approval_diff: None,
    };
    let explicit_rule = PermissionRule {
        pattern: "bash_unsandboxed(echo *)".to_owned(),
        action: PermissionDecision::Allow,
    };
    let gate = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![explicit_rule.clone()],
    });
    let no_prompt = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("/bin/echo first"),
            ToolBehavior::Shell,
            &no_prompt,
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("/bin/printf first"),
            ToolBehavior::Shell,
            &no_prompt,
        )
        .await,
        PermissionOutcome::Denied
    );

    let denied = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![
            explicit_rule.clone(),
            PermissionRule {
                pattern: "bash(echo*)".to_owned(),
                action: PermissionDecision::Deny,
            },
        ],
    });
    assert_eq!(
        authorize_with_behavior(
            &denied,
            request("/bin/echo first"),
            ToolBehavior::Shell,
            &no_prompt,
        )
        .await,
        PermissionOutcome::Denied
    );

    let strict = PermissionGate::for_headless_mode(PermissionModeDescriptor::Strict);
    strict
        .add_session_rule(explicit_rule.clone())
        .expect("typed session rule");
    assert_eq!(
        authorize_with_behavior(
            &strict,
            request("/bin/echo session"),
            ToolBehavior::Shell,
            &no_prompt,
        )
        .await,
        PermissionOutcome::Allowed
    );
    let auto_safe = PermissionGate::for_headless_mode(PermissionModeDescriptor::AutoSafe);
    auto_safe
        .add_session_rule(explicit_rule)
        .expect("typed session rule");
    assert_eq!(
        authorize_with_behavior(
            &auto_safe,
            request("/bin/echo session"),
            ToolBehavior::Shell,
            &no_prompt,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        authorize_with_behavior_in_mode(
            &gate,
            request("/bin/echo plan"),
            ToolBehavior::Shell,
            &no_prompt,
            None,
            SessionMode::Plan,
        )
        .await,
        PermissionOutcome::Denied
    );
}

#[tokio::test]
async fn remembered_mutations_bind_full_arguments_diff_and_bash_execution_context() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    let write = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "write".to_owned(),
        tool_name: "write".to_owned(),
        arguments: json!({"path": "same.txt", "content": "approved"}),
        capabilities: vec![ToolCapability::WriteFilesystem],
        approval_diff: Some(UnifiedDiff {
            proposal_id: "proposal".to_owned(),
            path: "same.txt".to_owned(),
            unified_diff: "diff".to_owned(),
            arguments_hash: "args".to_owned(),
            base_hash: "base-a".to_owned(),
            diff_hash: "diff-a".to_owned(),
            truncated: false,
        }),
    };
    assert_eq!(
        gate.authorize(write.clone(), &Decision(ApprovalDecision::AllowSession))
            .await,
        PermissionOutcome::Allowed
    );
    let deny = CountingDeny(AtomicUsize::new(0));
    let mut same_proposal = write.clone();
    same_proposal
        .approval_diff
        .as_mut()
        .expect("approval diff")
        .proposal_id = "different-call-instance".to_owned();
    assert_eq!(
        gate.authorize(same_proposal, &deny).await,
        PermissionOutcome::Allowed
    );
    let mut changed_content = write.clone();
    changed_content.arguments = json!({"path": "same.txt", "content": "different"});
    assert_eq!(
        gate.authorize(changed_content, &deny).await,
        PermissionOutcome::Denied
    );
    let mut changed_base = write;
    changed_base
        .approval_diff
        .as_mut()
        .expect("approval diff")
        .base_hash = "base-b".to_owned();
    assert_eq!(
        gate.authorize(changed_base, &deny).await,
        PermissionOutcome::Denied
    );

    let bash_gate = PermissionGate::new(PermissionDecision::Ask);
    let bash = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "bash".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": "/bin/echo test",
            "cwd": "crate-a",
            "env": {"PATH": "/trusted/bin", "GIT_CONFIG_COUNT": "0"},
            "network_domains": []
        }),
        capabilities: vec![ToolCapability::Execute],
        approval_diff: None,
    };
    assert_eq!(
        authorize_with_behavior(
            &bash_gate,
            bash.clone(),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::AllowSession),
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        authorize_with_behavior(&bash_gate, bash.clone(), ToolBehavior::Shell, &deny).await,
        PermissionOutcome::Allowed
    );
    for arguments in [
        json!({"command": "/bin/echo test", "cwd": "crate-b", "env": {"PATH": "/trusted/bin"}, "network_domains": []}),
        json!({"command": "/bin/echo test", "cwd": "crate-a", "env": {"PATH": "/attacker/bin"}, "network_domains": []}),
    ] {
        let mut changed = bash.clone();
        changed.arguments = arguments;
        assert_eq!(
            authorize_with_behavior(&bash_gate, changed, ToolBehavior::Shell, &deny).await,
            PermissionOutcome::Denied
        );
    }
    assert_eq!(deny.0.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn remembered_network_domains_are_normalized_exact_and_invalid_fail_closed() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    let invocation = |domains: Vec<&str>| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "network-domains".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": "/bin/echo network",
            "cwd": ".",
            "env": {},
            "network_domains": domains,
        }),
        capabilities: vec![ToolCapability::Execute, ToolCapability::Network],
        approval_diff: None,
    };
    assert_eq!(
        authorize_with_behavior(
            &gate,
            invocation(vec!["Example.COM.", "api.example.com"]),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::AllowSession),
        )
        .await,
        PermissionOutcome::Allowed
    );
    let deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(
            &gate,
            invocation(vec!["api.example.com", "example.com", "EXAMPLE.COM"]),
            ToolBehavior::Shell,
            &deny,
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        authorize_with_behavior(
            &gate,
            invocation(vec!["other.example.com"]),
            ToolBehavior::Shell,
            &deny,
        )
        .await,
        PermissionOutcome::Denied
    );
    let yolo = PermissionGate::for_headless_mode(PermissionModeDescriptor::Yolo);
    assert_eq!(
        authorize_with_behavior(
            &yolo,
            invocation(vec!["https://invalid.example"]),
            ToolBehavior::Shell,
            &deny,
        )
        .await,
        PermissionOutcome::Denied
    );
}

#[tokio::test]
async fn webfetch_is_no_prompt_for_every_valid_public_origin() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    let request = |url: &str| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "webfetch".to_owned(),
        tool_name: "webfetch".to_owned(),
        arguments: json!({"url": url, "headers": {}}),
        capabilities: vec![ToolCapability::Network],
        approval_diff: None,
    };
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("https://Example.com/path/a?query=one"),
            ToolBehavior::WebFetch,
            &Decision(ApprovalDecision::AllowSession),
        )
        .await,
        PermissionOutcome::Allowed
    );
    let deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("https://example.com/other/path"),
            ToolBehavior::WebFetch,
            &deny,
        )
        .await,
        PermissionOutcome::Allowed
    );
    for url in [
        "https://sub.example.com/path/a",
        "https://example.com:8443/path/a",
        "http://example.com/path/a",
    ] {
        assert_eq!(
            authorize_with_behavior(&gate, request(url), ToolBehavior::WebFetch, &deny).await,
            PermissionOutcome::Allowed
        );
    }
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("file:///private/etc/passwd"),
            ToolBehavior::WebFetch,
            &deny,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 0);
}
