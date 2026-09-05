use super::*;

#[tokio::test]
async fn per_turn_qualified_tool_restriction_denies_broader_bash_invocations() {
    let gate = PermissionGate::new(PermissionDecision::Allow)
        .restricted_to_patterns(&["bash(git status)".to_owned()])
        .expect("qualified restriction");
    let request = |command: &str| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: format!("bash-{command}"),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": command,
            "cwd": ".",
            "env": {},
            "network_domains": [],
            "sandbox": "sandboxed",
        }),
        capabilities: vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
        approval_diff: None,
    };
    let deny = Decision(ApprovalDecision::Deny);
    assert_eq!(
        authorize_with_behavior(&gate, request("git status"), ToolBehavior::Shell, &deny,).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        authorize_with_behavior(&gate, request("git push"), ToolBehavior::Shell, &deny).await,
        PermissionOutcome::Denied
    );
}

#[cfg(unix)]
#[tokio::test]
async fn complex_or_mutable_bash_approval_executes_once_and_is_never_remembered() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("tempdir");
    let script = root.path().join("script");
    fs::write(&script, "#!/bin/sh\nprintf mutable\n").expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("executable");
    for command in [
        "/bin/echo ok > output",
        "/bin/echo $(/bin/echo nested)",
        "/bin/rm *.tmp",
        "/bin/rm file?.tmp",
        "/bin/rm [ab].tmp",
        "/bin/echo {first,second}",
        "/bin/echo ~/secret",
        "/bin/echo background &",
        "/bin/sh -c '/bin/echo nested'",
        "eval /bin/echo unsafe",
        "cd /tmp",
        "export PATH=/tmp",
        "PATH=/tmp /bin/echo changed",
        "./script safe",
    ] {
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(root.path().join(format!(
                "complex-{}.json",
                blake3::hash(command.as_bytes()).to_hex()
            )));
        let invocation = bash_request(command, root.path());
        assert_eq!(
            authorize_with_behavior(
                &gate,
                invocation.clone(),
                ToolBehavior::Shell,
                &Decision(ApprovalDecision::AllowProject),
            )
            .await,
            PermissionOutcome::Allowed,
            "an accepted approval must execute the displayed invocation for {command}"
        );
        assert_eq!(gate.snapshot().project_approvals, 0);
        assert_eq!(gate.snapshot().session_approvals, 0);
        assert_eq!(
            authorize_with_behavior(
                &gate,
                invocation.clone(),
                ToolBehavior::Shell,
                &Decision(ApprovalDecision::AllowOnce),
            )
            .await,
            PermissionOutcome::Allowed,
            "one-time approval should remain usable for {command}"
        );
        assert_eq!(
            authorize_with_behavior(
                &gate,
                invocation,
                ToolBehavior::Shell,
                &Decision(ApprovalDecision::Deny),
            )
            .await,
            PermissionOutcome::Denied,
            "non-rememberable command was recalled: {command}"
        );
    }
}

#[test]
fn session_approval_id_failure_degrades_to_allow_once() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    assert_eq!(
        gate.accept_for_generation(0, None),
        PermissionOutcome::Allowed,
        "failure to allocate remember-only metadata must not reject the accepted invocation"
    );
    assert_eq!(gate.snapshot().session_approvals, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn audited_root_owned_simple_executable_can_be_remembered() {
    let root = tempfile::tempdir().expect("tempdir");
    for decision in [
        ApprovalDecision::AllowSession,
        ApprovalDecision::AllowProject,
    ] {
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(root.path().join(format!("{decision:?}.json")));
        let invocation = bash_request("/bin/echo stable", root.path());
        assert_eq!(
            authorize_with_behavior(
                &gate,
                invocation.clone(),
                ToolBehavior::Shell,
                &Decision(decision),
            )
            .await,
            PermissionOutcome::Allowed
        );
        assert_eq!(
            authorize_with_behavior(
                &gate,
                invocation,
                ToolBehavior::Shell,
                &Decision(ApprovalDecision::Deny),
            )
            .await,
            PermissionOutcome::Allowed
        );
    }
}

#[tokio::test]
async fn compound_allow_requires_every_command_to_match() {
    let gate = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![PermissionRule {
            pattern: "bash(git status*)".to_owned(),
            action: PermissionDecision::Allow,
        }],
    });
    let read = vec![ToolCapability::ReadFilesystem];
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("git status", read.clone()),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::Deny)
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("git status && /bin/echo README", read),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::Deny)
        )
        .await,
        PermissionOutcome::Denied
    );
    for redirected in ["git status > changed", "git status 2>err"] {
        assert_eq!(
            authorize_with_behavior(
                &gate,
                request(redirected, vec![ToolCapability::ReadFilesystem]),
                ToolBehavior::Shell,
                &Decision(ApprovalDecision::Deny)
            )
            .await,
            PermissionOutcome::Denied
        );
    }
    for expanded in [
        "MODE=unsafe git status",
        "git status $FLAGS",
        "git status *.rs",
        "git status ~/other-worktree",
    ] {
        assert_eq!(
            authorize_with_behavior(
                &gate,
                request(expanded, vec![ToolCapability::ReadFilesystem]),
                ToolBehavior::Shell,
                &Decision(ApprovalDecision::Deny)
            )
            .await,
            PermissionOutcome::Denied,
            "runtime-expanded command bypassed the approval fallback: {expanded}"
        );
    }
}

#[tokio::test]
async fn session_rules_add_replace_remove_and_clear_through_the_gate() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    let approver = CountingDeny(AtomicUsize::new(0));
    let invocation = || request("cargo publish --dry-run", vec![ToolCapability::Execute]);
    gate.add_session_rule(PermissionRule {
        pattern: "bash(cargo publish*)".to_owned(),
        action: PermissionDecision::Allow,
    })
    .expect("valid session rule");
    assert_eq!(
        authorize_with_behavior(&gate, invocation(), ToolBehavior::Shell, &approver).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 0);
    assert_eq!(gate.snapshot().session_rules.len(), 1);

    gate.add_session_rule(PermissionRule {
        pattern: "bash(cargo publish*)".to_owned(),
        action: PermissionDecision::Deny,
    })
    .expect("replace session rule");
    assert_eq!(gate.snapshot().session_rules.len(), 1);
    assert_eq!(
        authorize_with_behavior(&gate, invocation(), ToolBehavior::Shell, &approver).await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 0);

    assert!(gate.remove_session_rule("bash(cargo publish*)"));
    assert!(!gate.remove_session_rule("bash(cargo publish*)"));
    assert_eq!(
        authorize_with_behavior(&gate, invocation(), ToolBehavior::Shell, &approver).await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 1);
    assert_eq!(gate.clear_session_rules(), 0);
    assert!(
        gate.add_session_rule(PermissionRule {
            pattern: "not a rule".to_owned(),
            action: PermissionDecision::Allow,
        })
        .is_err()
    );
}

#[tokio::test]
async fn trusted_project_allows_read_only_tools_but_preserves_explicit_denies() {
    let root = tempfile::tempdir().expect("tempdir");
    let request = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "trusted-glob".to_owned(),
        tool_name: "glob".to_owned(),
        arguments: json!({"pattern": "**/*.rs", "path": "."}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    let no_prompt = CountingDeny(AtomicUsize::new(0));
    let trusted = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_trusted_read_roots([root.path()]);
    assert_eq!(
        trusted.authorize(request.clone(), &no_prompt).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);

    let denied = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![PermissionRule {
            pattern: "glob(*)".to_owned(),
            action: PermissionDecision::Deny,
        }],
    })
    .with_workspace_roots([root.path()])
    .with_trusted_read_roots([root.path()]);
    assert_eq!(
        denied.authorize(request, &no_prompt).await,
        PermissionOutcome::Denied
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn trusted_workspace_allows_pathless_builtin_symbol_reads_only_with_full_authority() {
    let primary = tempfile::tempdir().expect("primary");
    let secondary = tempfile::tempdir().expect("secondary");
    let symbols = || PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "workspace-symbols".to_owned(),
        tool_name: "symbols".to_owned(),
        arguments: json!({"pattern": "ProviderRuntime"}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    let no_prompt = CountingDeny(AtomicUsize::new(0));
    let trusted = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([primary.path(), secondary.path()])
        .with_trusted_read_roots([primary.path(), secondary.path()]);
    assert_eq!(
        trusted.authorize(symbols(), &no_prompt).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);

    let partially_trusted = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([primary.path(), secondary.path()])
        .with_trusted_read_roots([primary.path()]);
    assert_eq!(
        partially_trusted.authorize(symbols(), &no_prompt).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn trusted_workspace_read_authority_rejects_extensions_network_and_explicit_denies() {
    let root = tempfile::tempdir().expect("workspace");
    let no_prompt = CountingDeny(AtomicUsize::new(0));
    let trusted = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_trusted_read_roots([root.path()]);
    let extension = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "extension-read".to_owned(),
        tool_name: "extension_read".to_owned(),
        arguments: json!({"path": "."}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        trusted.authorize(extension, &no_prompt).await,
        PermissionOutcome::Allowed
    );

    let network = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "network-symbols".to_owned(),
        tool_name: "symbols".to_owned(),
        arguments: json!({
            "pattern": "ProviderRuntime",
            "network_domains": ["example.com"]
        }),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        trusted.authorize(network, &no_prompt).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);

    let denied = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![PermissionRule {
            pattern: "symbols(*)".to_owned(),
            action: PermissionDecision::Deny,
        }],
    })
    .with_workspace_roots([root.path()])
    .with_trusted_read_roots([root.path()]);
    let symbols = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "denied-symbols".to_owned(),
        tool_name: "symbols".to_owned(),
        arguments: json!({"pattern": "ProviderRuntime"}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        denied.authorize(symbols, &no_prompt).await,
        PermissionOutcome::Denied
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn trusted_read_only_authority_is_scoped_to_each_workspace_root() {
    let primary = tempfile::tempdir().expect("primary");
    let secondary = tempfile::tempdir().expect("secondary");
    std::fs::write(primary.path().join("primary.rs"), "primary").expect("primary fixture");
    std::fs::write(secondary.path().join("secondary.rs"), "secondary").expect("secondary fixture");
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([primary.path(), secondary.path()])
        .with_trusted_read_roots([primary.path()]);
    let no_prompt = CountingDeny(AtomicUsize::new(0));

    let primary_read = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "primary-read".to_owned(),
        tool_name: "read".to_owned(),
        arguments: json!({"path": "@root/0/primary.rs"}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        gate.authorize(primary_read, &no_prompt).await,
        PermissionOutcome::Allowed
    );

    let secondary_read = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "secondary-read".to_owned(),
        tool_name: "read".to_owned(),
        arguments: json!({"path": "@root/1/secondary.rs"}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        gate.authorize(secondary_read, &no_prompt).await,
        PermissionOutcome::Allowed
    );

    let all_roots_glob = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "all-roots-glob".to_owned(),
        tool_name: "glob".to_owned(),
        arguments: json!({"pattern": "**/*.rs", "path": "."}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        gate.authorize(all_roots_glob, &no_prompt).await,
        PermissionOutcome::Allowed
    );
    let default_all_roots_ls = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "default-all-roots-ls".to_owned(),
        tool_name: "ls".to_owned(),
        arguments: json!({"recursive": false}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        gate.authorize(default_all_roots_ls, &no_prompt).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn trusted_secondary_root_allows_virtual_paths_without_trusting_primary() {
    let primary = tempfile::tempdir().expect("primary");
    let secondary = tempfile::tempdir().expect("secondary");
    std::fs::write(primary.path().join("primary.rs"), "primary").expect("primary fixture");
    std::fs::write(secondary.path().join("secondary.rs"), "secondary").expect("secondary fixture");
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([primary.path(), secondary.path()])
        .with_trusted_read_roots([secondary.path()]);
    let no_prompt = CountingDeny(AtomicUsize::new(0));
    let secondary_read = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "secondary-read".to_owned(),
        tool_name: "read".to_owned(),
        arguments: json!({"path": "@root/1/secondary.rs"}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        gate.authorize(secondary_read, &no_prompt).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn untrusted_nested_root_does_not_inherit_primary_read_authority() {
    let tree = tempfile::tempdir().expect("tree");
    let nested = tree.path().join("nested");
    std::fs::create_dir(&nested).expect("nested root");
    std::fs::write(nested.join("private.rs"), "private").expect("nested fixture");
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([tree.path(), nested.as_path()])
        .with_trusted_read_roots([tree.path()]);
    let prompt = CountingDeny(AtomicUsize::new(0));
    let request = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "nested-read".to_owned(),
        tool_name: "read".to_owned(),
        arguments: json!({"path": "@root/1/private.rs"}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        gate.authorize(request, &prompt).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(prompt.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn command_allow_rule_cannot_silently_add_network_authority() {
    let command_rule = PermissionRule {
        pattern: "bash(cargo test*)".to_owned(),
        action: PermissionDecision::Allow,
    };
    let invocation = |network| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "network-call".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": "cargo test",
            "network_domains": if network { vec!["example.com"] } else { Vec::new() },
        }),
        capabilities: vec![ToolCapability::Execute, ToolCapability::Network],
        approval_diff: None,
    };
    let gate = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![command_rule.clone()],
    });
    let deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(&gate, invocation(false), ToolBehavior::Shell, &deny).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        authorize_with_behavior(&gate, invocation(true), ToolBehavior::Shell, &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 1);

    let network_gate = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![
            command_rule,
            PermissionRule {
                pattern: "network(bash)".to_owned(),
                action: PermissionDecision::Allow,
            },
        ],
    });
    assert_eq!(
        authorize_with_behavior(&network_gate, invocation(true), ToolBehavior::Shell, &deny,).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn user_safe_list_is_zero_prompt_only_for_sandboxed_networkless_commands() {
    let safety = Arc::new(
        CommandSafetyClassifier::new(&["cargo test*".to_owned()]).expect("user safe-list"),
    );
    let gate = PermissionGate::new(PermissionDecision::Ask).with_command_safety(safety);
    let request = |command: &str, sandbox: &str, domains: Vec<&str>| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "safe-list-call".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": command,
            "sandbox": sandbox,
            "network_domains": domains,
        }),
        capabilities: vec![ToolCapability::Execute, ToolCapability::Network],
        approval_diff: None,
    };

    let deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("cargo test", "sandboxed", vec![]),
            ToolBehavior::Shell,
            &deny,
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("cargo test && rm -rf target", "sandboxed", vec![]),
            ToolBehavior::Shell,
            &deny,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("cargo test", "sandboxed", vec!["example.com"]),
            ToolBehavior::Shell,
            &deny,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("cargo test", "unsandboxed", vec![]),
            ToolBehavior::Shell,
            &deny,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn unsandboxed_escape_hatch_requires_explicit_and_exact_authority() {
    let root = tempfile::tempdir().expect("root");
    let unsandboxed = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "unsandboxed-call".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": "/bin/echo canary",
            "cwd": root.path(),
            "env": {},
            "network_domains": [],
            "sandbox": "unsandboxed",
        }),
        capabilities: vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
        approval_diff: None,
    };
    let generic_allow = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![PermissionRule {
            pattern: "bash(echo*)".to_owned(),
            action: PermissionDecision::Allow,
        }],
    });
    let prompted = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(
            &generic_allow,
            unsandboxed.clone(),
            ToolBehavior::Shell,
            &prompted,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(prompted.0.load(Ordering::SeqCst), 1);

    let gate = PermissionGate::new(PermissionDecision::Ask).with_workspace_roots([root.path()]);
    assert_eq!(
        authorize_with_behavior(
            &gate,
            unsandboxed.clone(),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::AllowSession),
        )
        .await,
        PermissionOutcome::Allowed
    );
    let no_prompt = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(&gate, unsandboxed.clone(), ToolBehavior::Shell, &no_prompt,).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);

    let mut sandboxed = unsandboxed.clone();
    sandboxed.arguments["sandbox"] = Value::String("sandboxed".to_owned());
    assert_eq!(
        authorize_with_behavior(&gate, sandboxed, ToolBehavior::Shell, &no_prompt).await,
        PermissionOutcome::Denied
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 1);

    let mode_deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior_in_mode(
            &PermissionGate::new(PermissionDecision::Ask),
            unsandboxed.clone(),
            ToolBehavior::Shell,
            &mode_deny,
            None,
            SessionMode::Plan,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(mode_deny.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        authorize_with_behavior(
            &PermissionGate::for_headless_mode(PermissionModeDescriptor::AutoSafe),
            unsandboxed.clone(),
            ToolBehavior::Shell,
            &mode_deny,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        authorize_with_behavior(
            &PermissionGate::for_headless_mode(PermissionModeDescriptor::Yolo),
            unsandboxed,
            ToolBehavior::Shell,
            &mode_deny,
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(mode_deny.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn auto_safe_allows_only_reversible_workspace_file_tools() {
    let fixture = tempfile::tempdir().expect("fixture");
    let primary = fixture.path().join("primary");
    let added = fixture.path().join("added");
    let outside = fixture.path().join("outside");
    for root in [&primary, &added, &outside] {
        fs::create_dir(root).expect("workspace fixture");
    }
    let gate = PermissionGate::for_headless_mode(PermissionModeDescriptor::AutoSafe)
        .with_workspace_roots([&primary, &added]);
    let approver = CountingDeny(AtomicUsize::new(0));
    let write = |path: &str| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "auto-safe-write".to_owned(),
        tool_name: "write".to_owned(),
        arguments: json!({"path": path, "content": "fixture"}),
        capabilities: vec![
            ToolCapability::ReadFilesystem,
            ToolCapability::WriteFilesystem,
        ],
        approval_diff: None,
    };

    assert_eq!(
        authorize_registered_file_mutation(&gate, write("new.txt"), &approver).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        authorize_registered_file_mutation(&gate, write("@root/1/new.txt"), &approver).await,
        PermissionOutcome::Allowed
    );
    let multi_edit = |path: &str| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "auto-safe-multi-edit".to_owned(),
        tool_name: "multi_edit".to_owned(),
        arguments: json!({
            "path": path,
            "edits": [{"old": "before", "new": "after"}],
        }),
        capabilities: vec![
            ToolCapability::ReadFilesystem,
            ToolCapability::WriteFilesystem,
        ],
        approval_diff: None,
    };
    assert_eq!(
        authorize_registered_file_mutation(&gate, multi_edit("@root/1/existing.txt"), &approver,)
            .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        authorize_registered_file_mutation(
            &gate,
            multi_edit(outside.join("existing.txt").to_str().expect("UTF-8")),
            &approver,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        authorize_registered_file_mutation(
            &gate,
            write(outside.join("escaped.txt").to_str().expect("UTF-8")),
            &approver,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        authorize_registered_file_mutation(&gate, write("../outside/escaped.txt"), &approver,)
            .await,
        PermissionOutcome::Denied
    );

    let mut network_write = write("network.txt");
    network_write.capabilities.push(ToolCapability::Network);
    assert_eq!(
        authorize_registered_file_mutation(&gate, network_write, &approver).await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn auto_safe_does_not_follow_workspace_symlinks_for_write_approval() {
    let fixture = tempfile::tempdir().expect("fixture");
    let workspace = fixture.path().join("workspace");
    let outside = fixture.path().join("outside");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&outside).expect("outside");
    std::os::unix::fs::symlink(&outside, workspace.join("escape")).expect("symlink");
    let gate = PermissionGate::for_headless_mode(PermissionModeDescriptor::AutoSafe)
        .with_workspace_roots([&workspace]);
    let request = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "symlink-write".to_owned(),
        tool_name: "edit".to_owned(),
        arguments: json!({"path": "escape/file.txt", "old": "a", "new": "b"}),
        capabilities: vec![
            ToolCapability::ReadFilesystem,
            ToolCapability::WriteFilesystem,
        ],
        approval_diff: None,
    };
    let approver = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_registered_file_mutation(&gate, request, &approver).await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn built_in_git_status_safe_list_binds_bare_git_and_rejects_workspace_paths() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    let approver = CountingDeny(AtomicUsize::new(0));
    let capabilities = vec![ToolCapability::ReadFilesystem, ToolCapability::Execute];
    assert_eq!(
        authorize_with_behavior_in_mode(
            &gate,
            request("git status --short", capabilities.clone()),
            ToolBehavior::Shell,
            &approver,
            None,
            SessionMode::Execute,
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        authorize_with_behavior_in_mode(
            &gate,
            request("./git status", capabilities.clone()),
            ToolBehavior::Shell,
            &approver,
            None,
            SessionMode::Execute,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        authorize_with_behavior_in_mode(
            &gate,
            request("git status && printf unsafe", capabilities),
            ToolBehavior::Shell,
            &approver,
            None,
            SessionMode::Execute,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 2);

    let denied = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![PermissionRule {
            pattern: "bash(git status*)".to_owned(),
            action: PermissionDecision::Deny,
        }],
    });
    assert_eq!(
        authorize_with_behavior_in_mode(
            &denied,
            request("git status", vec![ToolCapability::ReadFilesystem]),
            ToolBehavior::Shell,
            &approver,
            None,
            SessionMode::Execute,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 2);
    assert_eq!(
        authorize_with_behavior_in_mode(
            &gate,
            request("git status", vec![ToolCapability::ReadFilesystem]),
            ToolBehavior::Shell,
            &approver,
            Some(HookPermissionDecision::Deny),
            SessionMode::Execute,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn malicious_workspace_git_is_never_executed_or_exposed_by_safe_list() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("workspace");
    let marker = root.path().join("malicious-git-executed");
    let executable = root.path().join("git");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf HOST_SECRET_CANARY\ntouch '{}'\n",
            marker.display()
        ),
    )
    .expect("malicious git fixture");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("malicious git mode");

    let outcome = authorize_with_behavior_in_mode(
        &PermissionGate::new(PermissionDecision::Ask),
        request(
            "./git status",
            vec![ToolCapability::ReadFilesystem, ToolCapability::Execute],
        ),
        ToolBehavior::Shell,
        &CountingDeny(AtomicUsize::new(0)),
        None,
        SessionMode::Execute,
    )
    .await;
    let output = if outcome == PermissionOutcome::Allowed {
        std::process::Command::new("./git")
            .arg("status")
            .current_dir(root.path())
            .output()
            .expect("malicious git execution")
            .stdout
    } else {
        Vec::new()
    };
    assert_eq!(outcome, PermissionOutcome::Denied);
    assert!(!marker.exists(), "workspace-controlled git was executed");
    assert!(!String::from_utf8_lossy(&output).contains("HOST_SECRET_CANARY"));
}

#[tokio::test]
async fn plan_and_discuss_deny_mutation_even_under_yolo() {
    let gate = PermissionGate::for_headless_mode(PermissionModeDescriptor::Yolo);
    for mode in [SessionMode::Plan, SessionMode::Discuss] {
        assert_eq!(
            authorize_with_behavior_in_mode(
                &gate,
                request(
                    "rm -rf build",
                    vec![ToolCapability::Execute, ToolCapability::WriteFilesystem]
                ),
                ToolBehavior::Shell,
                &Decision(ApprovalDecision::AllowOnce),
                Some(HookPermissionDecision::Allow),
                mode,
            )
            .await,
            PermissionOutcome::Denied
        );
    }
}

#[tokio::test]
async fn default_policy_prompts_only_for_file_writes_and_unsafe_bash() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    let approver = CountingDeny(AtomicUsize::new(0));
    for (request, behavior) in [
        (
            PermissionRequest {
                invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
                id: "read".to_owned(),
                tool_name: "read".to_owned(),
                arguments: json!({"path": "README.md"}),
                capabilities: vec![ToolCapability::ReadFilesystem],
                approval_diff: None,
            },
            ToolBehavior::Standard,
        ),
        (
            PermissionRequest {
                invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
                id: "todo".to_owned(),
                tool_name: "todo".to_owned(),
                arguments: json!({"action": "clear"}),
                capabilities: Vec::new(),
                approval_diff: None,
            },
            ToolBehavior::Standard,
        ),
        (
            PermissionRequest {
                invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
                id: "webfetch".to_owned(),
                tool_name: "webfetch".to_owned(),
                arguments: json!({"url": "https://example.com/"}),
                capabilities: vec![ToolCapability::Network],
                approval_diff: None,
            },
            ToolBehavior::WebFetch,
        ),
        (
            PermissionRequest {
                invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
                id: "mcp".to_owned(),
                tool_name: "mcp__fixture__inspect".to_owned(),
                arguments: json!({}),
                capabilities: vec![ToolCapability::Network, ToolCapability::Execute],
                approval_diff: None,
            },
            ToolBehavior::Standard,
        ),
    ] {
        assert_eq!(
            authorize_with_behavior(&gate, request, behavior, &approver).await,
            PermissionOutcome::Allowed
        );
    }
    assert_eq!(approver.0.load(Ordering::SeqCst), 0);

    let write = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "write".to_owned(),
        tool_name: "write".to_owned(),
        arguments: json!({"path": "README.md", "content": "changed"}),
        capabilities: vec![
            ToolCapability::ReadFilesystem,
            ToolCapability::WriteFilesystem,
        ],
        approval_diff: None,
    };
    assert_eq!(
        gate.authorize(write, &approver).await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request(
                "/bin/echo unsafe",
                vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
            ),
            ToolBehavior::Shell,
            &approver,
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn runtime_yolo_is_session_local_reversible_and_never_weakens_explicit_denies() {
    let gate = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: vec![
            PermissionRule {
                pattern: "write(denied.txt)".to_owned(),
                action: PermissionDecision::Deny,
            },
            PermissionRule {
                pattern: "write(asked.txt)".to_owned(),
                action: PermissionDecision::Ask,
            },
        ],
    });
    let deny = CountingDeny(AtomicUsize::new(0));
    let write = |path: &str| PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: format!("write-{path}"),
        tool_name: "write".to_owned(),
        arguments: json!({"path": path, "content": "fixture"}),
        capabilities: vec![ToolCapability::WriteFilesystem],
        approval_diff: None,
    };

    assert_eq!(
        gate.authorize(write("allowed.txt"), &deny).await,
        PermissionOutcome::Denied
    );
    gate.set_runtime_mode(Some(PermissionModeDescriptor::Yolo))
        .expect("interactive yolo");
    assert_eq!(
        gate.authorize(write("allowed.txt"), &deny).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        gate.authorize(write("asked.txt"), &deny).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        gate.authorize(write("denied.txt"), &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        gate.snapshot().runtime_mode,
        Some(PermissionModeDescriptor::Yolo)
    );
    gate.set_runtime_mode(None)
        .expect("restore configured policy");
    assert_eq!(
        gate.authorize(write("allowed.txt"), &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 2);

    let fixed = PermissionGate::for_headless_mode(PermissionModeDescriptor::Strict);
    assert!(
        fixed
            .set_runtime_mode(Some(PermissionModeDescriptor::Yolo))
            .is_err(),
        "remote/headless strict policy must not be client-switchable"
    );
    assert!(root_yolo_footgun(true, &[PathBuf::from("/")]));
    assert!(!root_yolo_footgun(false, &[PathBuf::from("/")]));
    assert!(!root_yolo_footgun(true, &[PathBuf::from("/tmp/project")]));
}

#[tokio::test]
async fn runtime_yolo_survives_child_workspace_forks_and_never_prompts_for_subagent_control() {
    let parent = tempfile::tempdir().expect("parent workspace");
    let child = tempfile::tempdir().expect("child workspace");
    let gate = PermissionGate::from_config(PermissionConfig {
        default: PermissionDecision::Ask,
        rules: Vec::new(),
    })
    .with_workspace_roots([parent.path()]);
    let child_gate = gate
        .fork_for_workspace_roots([child.path()])
        .expect("child permission generation");
    gate.set_runtime_mode(Some(PermissionModeDescriptor::Yolo))
        .expect("interactive yolo");
    assert_eq!(
        child_gate.snapshot().runtime_mode,
        Some(PermissionModeDescriptor::Yolo),
        "an existing child must observe later parent permission-mode changes"
    );
    gate.add_session_rule(PermissionRule {
        pattern: "bash(cargo test*)".to_owned(),
        action: PermissionDecision::Allow,
    })
    .expect("parent session rule");
    assert_eq!(
        child_gate.snapshot().session_rules,
        gate.snapshot().session_rules,
        "an existing child must observe later parent session-rule changes"
    );

    let approver = CountingDeny(AtomicUsize::new(0));
    let spawn = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "spawn-general".to_owned(),
        tool_name: "spawn_agent".to_owned(),
        arguments: json!({
            "action": "spawn",
            "task": "inspect and update the delegated workspace",
            "agent": "general",
            "isolation": "shared",
        }),
        capabilities: Vec::new(),
        approval_diff: None,
    };
    assert_eq!(
        child_gate.authorize(spawn, &approver).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        approver.0.load(Ordering::SeqCst),
        0,
        "YOLO subagent control must not enter the interactive approval channel"
    );
}

#[tokio::test]
async fn hook_ask_requires_fresh_approval_for_allowed_and_remembered_requests() {
    let approver = CountingDeny(AtomicUsize::new(0));
    let allowed = PermissionGate::new(PermissionDecision::Allow);
    assert_eq!(
        allowed
            .authorize_with_override(
                request("echo policy", vec![ToolCapability::Execute]),
                &approver,
                Some(HookPermissionDecision::Ask)
            )
            .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 1);
    let remembered = PermissionGate::new(PermissionDecision::Ask);
    assert_eq!(
        remembered
            .authorize_with_override(
                request("echo policy", vec![ToolCapability::Execute]),
                &Decision(ApprovalDecision::AllowSession),
                None
            )
            .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        remembered
            .authorize_with_override(
                request("echo policy", vec![ToolCapability::Execute]),
                &approver,
                Some(HookPermissionDecision::Ask)
            )
            .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 2);
    let denied = PermissionGate::new(PermissionDecision::Deny);
    assert_eq!(
        denied
            .authorize_with_override(
                request("echo policy", vec![ToolCapability::Execute]),
                &approver,
                Some(HookPermissionDecision::Allow)
            )
            .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 2);
}
