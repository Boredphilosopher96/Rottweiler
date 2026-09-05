use super::super::project_store::{load_project_approvals, persist_project_approvals};
use super::*;

#[cfg(unix)]
#[tokio::test]
async fn exact_bash_session_and_project_approvals_do_not_collide() {
    let root = tempfile::tempdir().expect("tempdir");
    for (scope, decision) in [
        ("session", ApprovalDecision::AllowSession),
        ("project", ApprovalDecision::AllowProject),
    ] {
        let gate = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots([root.path()])
            .with_project_approval_file(root.path().join(format!("{scope}.json")));
        let approved = bash_request("/bin/echo safe", root.path());
        assert_eq!(
            authorize_with_behavior(
                &gate,
                approved.clone(),
                ToolBehavior::Shell,
                &Decision(decision),
            )
            .await,
            PermissionOutcome::Allowed
        );
        let deny = CountingDeny(AtomicUsize::new(0));
        for command in [
            "FLAG=changed /bin/echo safe",
            "/bin/echo 'safe value'",
            "/bin/echo safe && /bin/echo done",
            "/bin/echo done && /bin/echo safe",
        ] {
            assert_eq!(
                authorize_with_behavior(
                    &gate,
                    bash_request(command, root.path()),
                    ToolBehavior::Shell,
                    &deny,
                )
                .await,
                PermissionOutcome::Denied,
                "{scope} approval collided for {command}"
            );
        }
        assert_eq!(deny.0.load(Ordering::SeqCst), 4);
    }
}

#[tokio::test]
async fn unavailable_project_approval_persistence_degrades_to_allow_once() {
    let invocation = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "write".to_owned(),
        tool_name: "write".to_owned(),
        arguments: json!({"path": "file.txt", "content": "content"}),
        capabilities: vec![ToolCapability::WriteFilesystem],
        approval_diff: None,
    };

    let without_store = PermissionGate::new(PermissionDecision::Ask);
    assert_eq!(
        without_store
            .authorize(
                invocation.clone(),
                &Decision(ApprovalDecision::AllowProject),
            )
            .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(without_store.snapshot().project_approvals, 0);
    assert_eq!(
        without_store
            .authorize(invocation.clone(), &Decision(ApprovalDecision::Deny))
            .await,
        PermissionOutcome::Denied,
        "the fallback must not accidentally become a remembered approval"
    );

    let root = tempfile::tempdir().expect("tempdir");
    let blocked_parent = root.path().join("not-a-directory");
    fs::write(&blocked_parent, "file").expect("blocking file");
    let failing_store = PermissionGate::new(PermissionDecision::Ask)
        .with_project_approval_file(blocked_parent.join("approvals.json"));
    assert_eq!(
        failing_store
            .authorize(
                invocation.clone(),
                &Decision(ApprovalDecision::AllowProject)
            )
            .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(failing_store.snapshot().project_approvals, 0);
    assert_eq!(
        failing_store
            .authorize(invocation, &Decision(ApprovalDecision::Deny))
            .await,
        PermissionOutcome::Denied
    );
}

#[cfg(windows)]
#[tokio::test]
async fn project_approval_without_portable_file_lock_degrades_to_allow_once() {
    let root = tempfile::tempdir().expect("tempdir");
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(root.path().join("approvals.json"));
    let request = PermissionRequest {
        id: "write".to_owned(),
        tool_name: "write".to_owned(),
        arguments: json!({"path": "file.txt", "content": "content"}),
        capabilities: vec![ToolCapability::WriteFilesystem],
        approval_diff: None,
    };
    assert_eq!(
        gate.authorize(request, &Decision(ApprovalDecision::AllowProject))
            .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(gate.snapshot().project_approvals, 0);
}

#[tokio::test]
async fn remembered_glob_approval_applies_without_reprompt_and_survives_reload() {
    let root = tempfile::tempdir().expect("tempdir");
    let approval_file = root.path().join("approvals.json");
    let request = PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "glob-first".to_owned(),
        tool_name: "glob".to_owned(),
        arguments: json!({"pattern": "**/*.rs", "path": "."}),
        capabilities: vec![ToolCapability::ReadFilesystem],
        approval_diff: None,
    };
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&approval_file);
    assert_eq!(
        gate.authorize(request.clone(), &Decision(ApprovalDecision::AllowProject),)
            .await,
        PermissionOutcome::Allowed
    );
    let no_prompt = CountingDeny(AtomicUsize::new(0));
    let mut repeated = request.clone();
    repeated.id = "glob-second".to_owned();
    assert_eq!(
        gate.authorize(repeated.clone(), &no_prompt).await,
        PermissionOutcome::Allowed
    );
    let reloaded = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&approval_file);
    assert_eq!(
        reloaded.authorize(repeated, &no_prompt).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(no_prompt.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn approval_listing_is_opaque_and_revocation_updates_private_persistence() {
    let root = tempfile::tempdir().expect("tempdir");
    let approval_file = root.path().join("approvals.json");
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&approval_file);
    let session_request = request(
        "printf SECRET_SESSION_CANARY",
        vec![ToolCapability::Execute],
    );
    let project_request = request(
        "printf SECRET_PROJECT_CANARY",
        vec![ToolCapability::Execute],
    );
    let hidden_fingerprint =
        PermissionKey::from_request(&project_request, &workspace_namespace([root.path()]))
            .arguments_fingerprint;
    assert_eq!(
        gate.authorize(
            session_request.clone(),
            &Decision(ApprovalDecision::AllowSession),
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        gate.authorize(
            project_request.clone(),
            &Decision(ApprovalDecision::AllowProject),
        )
        .await,
        PermissionOutcome::Allowed
    );
    let approvals = gate.approval_snapshot();
    assert_eq!(approvals.session.len(), 1);
    assert_eq!(approvals.project.len(), 1);
    let rendered = serde_json::to_string(&approvals).expect("approval snapshot");
    assert!(!rendered.contains("SECRET_SESSION_CANARY"));
    assert!(!rendered.contains("SECRET_PROJECT_CANARY"));
    assert!(!rendered.contains("arguments_fingerprint"));
    assert!(!rendered.contains(&hidden_fingerprint));
    assert_eq!(
        approvals.project[0].canonical_summary,
        "exact-invocation=hidden capabilities=Execute approval=none"
    );
    assert!(
        !std::fs::read_to_string(&approval_file)
            .expect("private approvals")
            .contains("SECRET_PROJECT_CANARY")
    );
    let stable = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&approval_file)
        .approval_snapshot();
    assert_eq!(stable.project[0].id, approvals.project[0].id);

    assert_eq!(
        gate.revoke_session_approvals(Some(&approvals.session[0].id)),
        1
    );
    assert_eq!(
        gate.revoke_project_approvals(Some(&approvals.project[0].id))
            .expect("persist project revocation"),
        1
    );
    assert!(gate.approval_snapshot().session.is_empty());
    assert!(gate.approval_snapshot().project.is_empty());
    let deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        gate.authorize(session_request, &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        gate.authorize(project_request, &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 2);

    let reloaded = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&approval_file);
    assert_eq!(reloaded.snapshot().project_approvals, 0);
}

#[test]
fn independent_project_stores_reload_merge_and_never_resurrect_revoked_authority() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("approvals.json");
    let first = independent_project_store(&path);
    let second = independent_project_store(&path);
    let namespace = workspace_namespace([root.path()]);
    let key_a = PermissionKey::from_request(
        &request("printf authority-a", vec![ToolCapability::Execute]),
        &namespace,
    );
    let key_b = PermissionKey::from_request(
        &request("printf authority-b", vec![ToolCapability::Execute]),
        &namespace,
    );
    first.grant(key_a.clone()).expect("grant A");
    let stale = second.refresh().expect("stale gate load");
    let id_a = stale
        .iter()
        .find(|entry| entry.key == key_a)
        .expect("A")
        .id
        .clone();
    assert_eq!(first.revoke(Some(&id_a)).expect("revoke A"), 1);
    assert!(!second.contains(&key_a).expect("fresh deny in gate2"));

    second.grant(key_b.clone()).expect("stale gate grants B");
    let authoritative = first.refresh().expect("authoritative reload");
    assert!(!contains_approval(&authoritative, &key_a));
    assert!(contains_approval(&authoritative, &key_b));
    assert_eq!(authoritative.len(), 1);
    let stable_id = authoritative.iter().next().expect("B").id.clone();
    let reloaded = independent_project_store(&path)
        .refresh()
        .expect("reload stable id");
    assert_eq!(reloaded.iter().next().expect("reloaded B").id, stable_id);
}

#[tokio::test]
async fn project_revocation_in_one_gate_immediately_denies_another_gate() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("approvals.json");
    let first = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&path);
    let second = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&path);
    let authority_a = request("printf authority-a", vec![ToolCapability::Execute]);
    let authority_b = request("printf authority-b", vec![ToolCapability::Execute]);
    assert_eq!(
        first
            .authorize(
                authority_a.clone(),
                &Decision(ApprovalDecision::AllowProject),
            )
            .await,
        PermissionOutcome::Allowed
    );
    let deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        second.authorize(authority_a.clone(), &deny).await,
        PermissionOutcome::Allowed
    );
    let id_a = first.approval_snapshot().project[0].id.clone();
    assert_eq!(
        first
            .revoke_project_approvals(Some(&id_a))
            .expect("revoke A"),
        1
    );
    assert_eq!(
        second.authorize(authority_a.clone(), &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        second
            .authorize(
                authority_b.clone(),
                &Decision(ApprovalDecision::AllowProject)
            )
            .await,
        PermissionOutcome::Allowed
    );
    let authoritative = first.approval_snapshot();
    assert_eq!(authoritative.project.len(), 1);
    assert_eq!(authoritative.project[0].tool_name, "bash");
    assert_eq!(
        first.authorize(authority_a, &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        first.authorize(authority_b, &deny).await,
        PermissionOutcome::Allowed
    );
}

#[test]
fn concurrent_independent_project_grant_and_revoke_serialize_without_lost_updates() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("approvals.json");
    let first = Arc::new(independent_project_store(&path));
    let second = Arc::new(independent_project_store(&path));
    let namespace = workspace_namespace([root.path()]);
    let key_a = PermissionKey::from_request(
        &request("printf authority-a", vec![ToolCapability::Execute]),
        &namespace,
    );
    let key_b = PermissionKey::from_request(
        &request("printf authority-b", vec![ToolCapability::Execute]),
        &namespace,
    );
    first.grant(key_a.clone()).expect("grant A");
    let id_a = first
        .refresh()
        .expect("load A")
        .iter()
        .next()
        .expect("A")
        .id
        .clone();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let first = Arc::clone(&first);
        let first_barrier = Arc::clone(&barrier);
        scope.spawn(move || {
            first_barrier.wait();
            first.revoke(Some(&id_a)).expect("concurrent revoke A");
        });
        let second = Arc::clone(&second);
        let second_barrier = Arc::clone(&barrier);
        let key_b = key_b.clone();
        scope.spawn(move || {
            second_barrier.wait();
            second.grant(key_b).expect("concurrent grant B");
        });
    });
    let authoritative = independent_project_store(&path)
        .refresh()
        .expect("authoritative reload");
    assert!(!contains_approval(&authoritative, &key_a));
    assert!(contains_approval(&authoritative, &key_b));
    assert_eq!(authoritative.len(), 1);
}

#[cfg(unix)]
#[test]
fn durable_project_write_is_private_unique_and_cleans_failed_rename() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("tempdir");
    let target = root.path().join("approvals.json");
    fs::create_dir(&target).expect("rename-blocking directory");
    let namespace = workspace_namespace([root.path()]);
    let key = PermissionKey::from_request(
        &request("printf durable", vec![ToolCapability::Execute]),
        &namespace,
    );
    let approval = RememberedApproval::new("project", key).expect("random id");
    let approvals = BTreeSet::from([approval]);
    assert!(persist_project_approvals(&target, &approvals).is_err());
    let prefix = format!(
        "{}.",
        target.file_name().expect("target name").to_string_lossy()
    );
    assert!(
        fs::read_dir(root.path())
            .expect("root entries")
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(&prefix))
    );

    fs::remove_dir(&target).expect("remove blocker");
    persist_project_approvals(&target, &approvals).expect("durable write");
    assert_eq!(
        fs::metadata(&target)
            .expect("ledger metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(load_project_approvals(&target).expect("reload"), approvals);
}

#[cfg(unix)]
#[test]
fn malformed_project_ledger_is_not_overwritten_by_a_stale_grant() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("approvals.json");
    fs::write(&path, b"{malformed").expect("malformed ledger");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private ledger");
    let namespace = workspace_namespace([root.path()]);
    let key = PermissionKey::from_request(
        &request("printf stale", vec![ToolCapability::Execute]),
        &namespace,
    );
    let store = independent_project_store(&path);
    assert!(store.grant(key).is_err());
    assert_eq!(
        fs::read(&path).expect("unchanged malformed ledger"),
        b"{malformed"
    );
}

#[tokio::test]
async fn clear_session_removes_rules_and_allow_session_approvals() {
    let gate = PermissionGate::new(PermissionDecision::Ask);
    gate.add_session_rule(PermissionRule {
        pattern: "bash(cargo test*)".to_owned(),
        action: PermissionDecision::Allow,
    })
    .expect("session rule");
    assert_eq!(
        gate.authorize(
            request("printf remember", vec![ToolCapability::Execute]),
            &Decision(ApprovalDecision::AllowSession),
        )
        .await,
        PermissionOutcome::Allowed
    );
    let cleared = gate.clear_session_permissions();
    assert_eq!(cleared.rules, 1);
    assert_eq!(cleared.approvals, 1);
    assert!(gate.snapshot().session_rules.is_empty());
    assert_eq!(gate.snapshot().session_approvals, 0);
}

#[tokio::test]
async fn workspace_generation_swap_invalidates_old_session_and_project_approvals() {
    let root = tempfile::tempdir().expect("tempdir");
    let added = root.path().join("added");
    std::fs::create_dir(&added).expect("added root");
    let approval_file = root.path().join("approvals.json");
    let gate = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&approval_file);
    let session_request = request("printf session", vec![ToolCapability::Execute]);
    let project_request = request("printf project", vec![ToolCapability::Execute]);
    assert_eq!(
        gate.authorize(
            session_request.clone(),
            &Decision(ApprovalDecision::AllowSession),
        )
        .await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        gate.authorize(
            project_request.clone(),
            &Decision(ApprovalDecision::AllowProject),
        )
        .await,
        PermissionOutcome::Allowed
    );
    let update = gate
        .replace_workspace_roots([root.path(), added.as_path()])
        .expect("workspace generation swap");
    assert_eq!(update.generation, 2);
    assert_eq!(update.invalidated_session_approvals, 1);
    assert_eq!(update.invalidated_project_approvals, 1);
    assert_eq!(gate.snapshot().session_approvals, 0);
    assert_eq!(gate.snapshot().project_approvals, 0);

    let deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        gate.authorize(session_request, &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        gate.authorize(project_request, &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(deny.0.load(Ordering::SeqCst), 2);
    let unchanged = gate
        .replace_workspace_roots([root.path(), added.as_path()])
        .expect("same generation");
    assert_eq!(unchanged.generation, 2);
    assert_eq!(unchanged.invalidated_session_approvals, 0);
    assert_eq!(unchanged.invalidated_project_approvals, 0);
}

#[tokio::test]
async fn workspace_gate_fork_preserves_rules_without_inheriting_session_authority() {
    let root = tempfile::tempdir().expect("tempdir");
    let added = root.path().join("added");
    fs::create_dir(&added).expect("added root");
    let approval_file = root.path().join("approvals.json");
    let original = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([root.path()])
        .with_project_approval_file(&approval_file);
    original
        .add_session_rule(PermissionRule {
            pattern: "bash(cargo test*)".to_owned(),
            action: PermissionDecision::Allow,
        })
        .expect("session rule");
    let remembered = request("printf remembered", vec![ToolCapability::Execute]);
    assert_eq!(
        original
            .authorize(
                remembered.clone(),
                &Decision(ApprovalDecision::AllowSession),
            )
            .await,
        PermissionOutcome::Allowed
    );
    let project = bash_request("/bin/echo project", root.path());
    assert_eq!(
        authorize_with_behavior(
            &original,
            project.clone(),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::AllowProject),
        )
        .await,
        PermissionOutcome::Allowed
    );
    let persisted_before = fs::read(&approval_file).expect("project ledger");
    let replacement = original
        .fork_for_workspace_roots([root.path(), added.as_path()])
        .expect("replacement gate");
    assert_eq!(
        replacement.snapshot().session_rules,
        original.snapshot().session_rules
    );
    assert_eq!(replacement.snapshot().session_approvals, 0);
    assert_eq!(replacement.snapshot().project_approvals, 0);
    assert_eq!(
        fs::read(&approval_file).expect("unchanged ledger"),
        persisted_before
    );
    let deny = CountingDeny(AtomicUsize::new(0));
    assert_eq!(
        replacement.authorize(remembered.clone(), &deny).await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        original.authorize(remembered, &deny).await,
        PermissionOutcome::Allowed
    );
    assert_eq!(
        authorize_with_behavior(&replacement, project.clone(), ToolBehavior::Shell, &deny,).await,
        PermissionOutcome::Denied
    );
    assert_eq!(
        authorize_with_behavior(&original, project, ToolBehavior::Shell, &deny).await,
        PermissionOutcome::Allowed
    );
}

#[tokio::test]
async fn project_approval_round_trips_privately() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("approvals.json");
    let gate = PermissionGate::new(PermissionDecision::Ask).with_project_approval_file(&path);
    let invocation = request("git status", vec![ToolCapability::ReadFilesystem]);
    assert_eq!(
        authorize_with_behavior(
            &gate,
            invocation.clone(),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::AllowProject)
        )
        .await,
        PermissionOutcome::Allowed
    );
    let recovered = PermissionGate::new(PermissionDecision::Ask).with_project_approval_file(&path);
    assert_eq!(
        authorize_with_behavior(
            &recovered,
            invocation,
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::Deny),
        )
        .await,
        PermissionOutcome::Allowed
    );
}

#[tokio::test]
async fn project_approvals_bind_ordered_complete_workspace_roots() {
    let temp = tempfile::tempdir().expect("tempdir");
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let replacement = temp.path().join("replacement");
    for root in [&first, &second, &replacement] {
        fs::create_dir(root).expect("workspace root");
    }
    let approvals = temp.path().join("approvals.json");
    let invocation = request("/bin/echo test", vec![ToolCapability::Execute]);
    let initial = PermissionGate::new(PermissionDecision::Ask)
        .with_workspace_roots([&first, &second])
        .with_project_approval_file(&approvals);
    assert_eq!(
        authorize_with_behavior(
            &initial,
            invocation.clone(),
            ToolBehavior::Shell,
            &Decision(ApprovalDecision::AllowProject),
        )
        .await,
        PermissionOutcome::Allowed
    );
    for roots in [[&second, &first], [&first, &replacement]] {
        let reloaded = PermissionGate::new(PermissionDecision::Ask)
            .with_workspace_roots(roots)
            .with_project_approval_file(&approvals);
        assert_eq!(
            authorize_with_behavior(
                &reloaded,
                invocation.clone(),
                ToolBehavior::Shell,
                &Decision(ApprovalDecision::Deny),
            )
            .await,
            PermissionOutcome::Denied
        );
    }
}
