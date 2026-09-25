use super::*;

fn allow(pattern: &str) -> PermissionRule {
    PermissionRule {
        pattern: pattern.to_owned(),
        action: PermissionDecision::Allow,
    }
}

#[tokio::test]
async fn project_rules_persist_across_sessions_of_the_same_project() {
    let root = tempfile::tempdir().expect("tempdir");
    let ledger = root.path().join("project.json");
    let first = PermissionGate::new(PermissionDecision::Ask).with_project_approval_file(&ledger);
    first
        .add_project_rule(allow("bash(cargo publish*)"))
        .expect("save project rule");

    let later = PermissionGate::new(PermissionDecision::Ask).with_project_approval_file(&ledger);
    let approver = CountingDeny(AtomicUsize::new(0));
    let invocation = |command: &str| request(command, vec![ToolCapability::Execute]);
    assert_eq!(
        authorize_with_behavior(
            &later,
            invocation("cargo publish --dry-run"),
            ToolBehavior::Shell,
            &approver
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        approver.0.load(Ordering::SeqCst),
        0,
        "a saved rule never prompts"
    );
    assert_eq!(
        authorize_with_behavior(
            &later,
            invocation("cargo yank"),
            ToolBehavior::Shell,
            &approver
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        approver.0.load(Ordering::SeqCst),
        1,
        "unmatched commands still ask"
    );
    assert_eq!(
        later.snapshot().project_rules,
        vec![allow("bash(cargo publish*)")]
    );

    assert!(
        later
            .remove_project_rule("bash(cargo publish*)")
            .expect("remove")
    );
    assert!(
        !later
            .remove_project_rule("bash(cargo publish*)")
            .expect("remove again")
    );
    assert!(
        first.snapshot().project_rules.is_empty(),
        "removal is visible to every session"
    );
}

#[tokio::test]
async fn session_deny_rules_still_override_saved_project_allows() {
    let root = tempfile::tempdir().expect("tempdir");
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_project_approval_file(root.path().join("project.json"));
    gate.add_project_rule(allow("bash(cargo publish*)"))
        .expect("save project rule");
    gate.add_session_rule(PermissionRule {
        pattern: "bash(cargo publish*)".to_owned(),
        action: PermissionDecision::Deny,
    })
    .expect("session deny");
    let approver = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("cargo publish", vec![ToolCapability::Execute]),
            ToolBehavior::Shell,
            &approver
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn project_rules_require_durable_storage_and_valid_patterns() {
    let volatile = PermissionGate::new(PermissionDecision::Ask);
    assert!(volatile.add_project_rule(allow("bash(ls*)")).is_err());

    let root = tempfile::tempdir().expect("tempdir");
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_project_approval_file(root.path().join("project.json"));
    assert!(gate.add_project_rule(allow("not a rule")).is_err());
    assert!(gate.snapshot().project_rules.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn project_rule_ledger_is_private_and_fails_closed_on_damage() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("tempdir");
    let ledger = root.path().join("project.json");
    let gate = PermissionGate::new(PermissionDecision::Ask).with_project_approval_file(&ledger);
    gate.add_project_rule(allow("bash(cargo test*)"))
        .expect("save project rule");
    let rules_file = root.path().join("project.json.rules");
    let mode = std::fs::metadata(&rules_file)
        .expect("rule ledger exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "the rule ledger is owner-only");

    // Entries that fail validation are ignored rather than trusted.
    std::fs::write(
        &rules_file,
        r#"[{"match":"not a rule","action":"allow"},{"match":"bash(cargo test*)","action":"allow"}]"#,
    )
    .expect("rewrite ledger");
    std::fs::set_permissions(&rules_file, std::fs::Permissions::from_mode(0o600))
        .expect("keep private");
    assert_eq!(
        gate.snapshot().project_rules,
        vec![allow("bash(cargo test*)")]
    );

    // A ledger readable by others grants nothing.
    std::fs::set_permissions(&rules_file, std::fs::Permissions::from_mode(0o644))
        .expect("widen mode");
    assert!(gate.snapshot().project_rules.is_empty());
    let approver = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        authorize_with_behavior(
            &gate,
            request("cargo test", vec![ToolCapability::Execute]),
            ToolBehavior::Shell,
            &approver
        )
        .await,
        PermissionOutcome::Denied
    );
    assert_eq!(approver.0.load(Ordering::SeqCst), 1);
}
