#![allow(clippy::expect_used)]

use tempfile::TempDir;

use super::*;

#[test]
fn empty_project_extension_inventory_never_requests_a_trust_decision() {
    let root = TempDir::new().expect("root");
    let workspace = root.path().join("repo");
    fs::create_dir_all(&workspace).expect("workspace");
    let store = FolderTrustStore::new(root.path().join("user/trust.json"));

    let assessment = store.assess(&workspace).expect("assessment");

    assert!(assessment.inventory().is_empty());
    assert!(!assessment.requires_confirmation());
}

#[test]
fn supporting_project_extension_artifacts_are_part_of_the_trust_inventory() {
    let root = TempDir::new().expect("root");
    let workspace = root.path().join("repo");
    fs::create_dir_all(workspace.join(".rottweiler")).expect("extension directory");
    fs::write(
        workspace.join(".rottweiler/README.md"),
        "supporting extension documentation",
    )
    .expect("supporting artifact");
    let store = FolderTrustStore::new(root.path().join("user/trust.json"));

    let assessment = store.assess(&workspace).expect("assessment");

    assert!(assessment.requires_confirmation());
    assert!(matches!(
        assessment.inventory(),
        [TrustInventoryItem { kind, path, .. }]
            if kind == "project_extension" && path == ".rottweiler/README.md"
    ));
}

#[test]
fn malicious_project_is_inert_until_exact_inventory_is_trusted() {
    let canary = Path::new("/tmp/rottweiler-untrusted-folder-pwned");
    let _ = fs::remove_file(canary);
    let root = TempDir::new().expect("root");
    let workspace = root.path().join("repo");
    let agents = workspace.join(".agents/commands");
    let rottweiler = workspace.join(".rottweiler");
    fs::create_dir_all(&agents).expect("agents");
    fs::create_dir_all(&rottweiler).expect("rottweiler");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/untrusted-project");
    fs::write(
        agents.join("x.md"),
        fs::read(fixture.join(".agents/commands/x.md")).expect("command fixture"),
    )
    .expect("command");
    fs::write(
        rottweiler.join("plugins.toml"),
        fs::read(fixture.join(".rottweiler/plugins.toml")).expect("plugin fixture"),
    )
    .expect("plugin");
    let store = FolderTrustStore::new(root.path().join("user/trust.json"));
    let first = store.assess(&workspace).expect("assessment");
    assert_eq!(first.state(), FolderTrustState::Untrusted);
    assert!(!first.project_execution_enabled());
    assert!(first.requires_confirmation());
    assert!(first.inventory().iter().any(|item| item.kind == "command"));
    assert!(first.inventory().iter().any(|item| item.kind == "plugin"));
    let prompt = first.render_prompt();
    assert!(prompt.contains(".agents/commands/x.md"));
    assert!(prompt.contains(".rottweiler/plugins.toml"));
    assert!(
        !canary.exists(),
        "inventory must never execute project content"
    );

    store.grant(&first).expect("grant");
    let trusted = store.assess(&workspace).expect("trusted");
    assert_eq!(trusted.state(), FolderTrustState::Trusted);
    assert!(!trusted.requires_confirmation());
    assert!(
        !canary.exists(),
        "grant persistence must not execute project content"
    );

    fs::write(agents.join("x.md"), "!`touch /tmp/changed`\n").expect("change");
    let changed = store.assess(&workspace).expect("changed");
    assert_eq!(changed.state(), FolderTrustState::Changed);
    assert!(!changed.project_execution_enabled());
    assert!(changed.requires_confirmation());
    assert!(matches!(
        changed.changes(),
        [TrustInventoryChange::Modified { after, .. }] if after.path == ".agents/commands/x.md"
    ));

    fs::remove_file(agents.join("x.md")).expect("remove command");
    fs::remove_file(rottweiler.join("plugins.toml")).expect("remove plugin");
    let removed = store.assess(&workspace).expect("removed inventory");
    assert_eq!(removed.state(), FolderTrustState::Changed);
    assert!(removed.inventory().is_empty());
    assert!(!removed.project_execution_enabled());
    assert!(!removed.requires_confirmation());
}

#[test]
fn decisions_are_keyed_by_canonical_absolute_workspace() {
    let root = TempDir::new().expect("root");
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir_all(&first).expect("first");
    fs::create_dir_all(&second).expect("second");
    let store = FolderTrustStore::new(root.path().join("user/trust.json"));
    let first_assessment = store.assess(&first).expect("first assessment");
    store.grant(&first_assessment).expect("grant first");
    assert_eq!(
        store.assess(&first).expect("first trusted").state(),
        FolderTrustState::Trusted
    );
    assert_eq!(
        store.assess(&second).expect("second untrusted").state(),
        FolderTrustState::Untrusted
    );

    let second_assessment = store.assess(&second).expect("second assessment");
    store.grant(&second_assessment).expect("grant second");
    assert_eq!(
        store.assess(&first).expect("first retained").state(),
        FolderTrustState::Trusted
    );
    assert_eq!(
        store.assess(&second).expect("second trusted").state(),
        FolderTrustState::Trusted
    );
}

#[test]
fn concurrent_writer_lock_fails_closed_instead_of_losing_a_decision() {
    let root = TempDir::new().expect("root");
    let workspace = root.path().join("repo");
    fs::create_dir_all(&workspace).expect("workspace");
    let path = root.path().join("user/trust.json");
    let store = FolderTrustStore::new(path.clone());
    let assessment = store.assess(&workspace).expect("assessment");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::create_dir(path.with_extension("lock")).expect("competing lock");
    assert!(matches!(
        store.grant(&assessment),
        Err(FolderTrustError::LedgerLocked(_))
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_project_extension_is_untrustable_without_a_partial_fingerprint() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("root");
    let workspace = root.path().join("repo");
    fs::create_dir_all(workspace.join(".agents/commands")).expect("commands");
    fs::write(
        workspace.join(".agents/commands/valid.md"),
        "---\ndescription: valid\n---\nbody",
    )
    .expect("valid command");
    fs::write(root.path().join("outside"), "payload").expect("outside");
    symlink(
        root.path().join("outside"),
        workspace.join(".agents/commands/x.md"),
    )
    .expect("symlink");
    let ledger = root.path().join("user/trust.json");
    let store = FolderTrustStore::new(ledger.clone());
    let assessment = store.assess(&workspace).expect("untrustable assessment");
    let canonical_offending = assessment.workspace().join(".agents/commands/x.md");

    assert_eq!(assessment.state(), FolderTrustState::Untrustable);
    assert!(!assessment.project_execution_enabled());
    assert!(!assessment.requires_confirmation());
    assert!(assessment.inventory().is_empty());
    assert_eq!(assessment.executable_hash(), None);
    let failure = assessment.inventory_failure().expect("inventory failure");
    assert_eq!(failure.path(), canonical_offending);
    assert!(
        assessment
            .render_prompt()
            .contains("no fingerprint was produced")
    );
    assert!(matches!(
        store.grant(&assessment),
        Err(FolderTrustError::Untrustable { path, .. })
            if path == canonical_offending
    ));
    assert!(!ledger.exists(), "refused grant must not create a ledger");
}

#[test]
fn claude_skills_and_commands_are_fingerprinted_and_changes_reprompt() {
    let root = TempDir::new().expect("root");
    let workspace = root.path().join("repo");
    fs::create_dir_all(workspace.join(".claude/skills/review")).expect("skill directory");
    fs::create_dir_all(workspace.join(".claude/commands")).expect("commands");
    fs::write(
        workspace.join(".claude/skills/review/SKILL.md"),
        "---\nname: review\ndescription: review\n---\nbody",
    )
    .expect("skill");
    fs::write(workspace.join(".claude/settings.local.json"), "{}").expect("settings");
    let store = FolderTrustStore::new(root.path().join("user/trust.json"));

    let first = store.assess(&workspace).expect("assessment");
    assert_eq!(first.state(), FolderTrustState::Untrusted);
    assert!(matches!(
        first.inventory(),
        [TrustInventoryItem { kind, path, .. }]
            if kind == "skill" && path == ".claude/skills/review/SKILL.md"
    ));
    store.grant(&first).expect("grant");
    assert_eq!(
        store.assess(&workspace).expect("trusted").state(),
        FolderTrustState::Trusted
    );

    fs::write(workspace.join(".claude/settings.local.json"), "{\"x\":1}").expect("settings");
    assert_eq!(
        store.assess(&workspace).expect("settings ignored").state(),
        FolderTrustState::Trusted,
        "only the .claude trees discovery reads participate in trust"
    );

    fs::write(workspace.join(".claude/commands/ship.md"), "!`deploy`\n").expect("command");
    let added = store.assess(&workspace).expect("added command");
    assert_eq!(added.state(), FolderTrustState::Changed);
    assert!(matches!(
        added.changes(),
        [TrustInventoryChange::Added(item)]
            if item.path == ".claude/commands/ship.md" && item.kind == "command"
    ));
    store.grant(&added).expect("grant command");

    fs::write(workspace.join(".claude/skills/review/SKILL.md"), "changed").expect("change");
    let changed = store.assess(&workspace).expect("changed skill");
    assert_eq!(changed.state(), FolderTrustState::Changed);
    assert!(matches!(
        changed.changes(),
        [TrustInventoryChange::Modified { after, .. }]
            if after.path == ".claude/skills/review/SKILL.md"
    ));
}

#[cfg(unix)]
#[test]
fn in_bounds_skill_links_are_fingerprinted_through_their_targets() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("root");
    let workspace = root.path().join("repo");
    let home = root.path().join("home");
    let shared = home.join("library/shared");
    fs::create_dir_all(&shared).expect("shared skill");
    fs::write(shared.join("SKILL.md"), "shared v1").expect("shared manifest");
    fs::create_dir_all(workspace.join("vendor/local")).expect("vendored skill");
    fs::write(workspace.join("vendor/local/SKILL.md"), "local").expect("local manifest");
    fs::create_dir_all(workspace.join(".claude/skills")).expect("claude skills");
    symlink(&shared, workspace.join(".claude/skills/shared")).expect("home link");
    fs::create_dir_all(workspace.join(".agents")).expect("agents");
    symlink(workspace.join("vendor"), workspace.join(".agents/skills")).expect("project link");
    let store = FolderTrustStore::new(root.path().join("user/trust.json")).with_user_home(&home);

    let first = store.assess(&workspace).expect("assessment");
    assert_eq!(first.state(), FolderTrustState::Untrusted);
    let paths = first
        .inventory()
        .iter()
        .map(|item| (item.path.as_str(), item.kind.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            (".agents/skills/local/SKILL.md", "skill"),
            (".claude/skills/shared/SKILL.md", "skill"),
        ]
    );
    store.grant(&first).expect("grant");
    assert_eq!(
        store.assess(&workspace).expect("trusted").state(),
        FolderTrustState::Trusted
    );

    fs::write(shared.join("SKILL.md"), "shared v2").expect("change target");
    let changed = store.assess(&workspace).expect("changed target");
    assert_eq!(changed.state(), FolderTrustState::Changed);
    assert!(matches!(
        changed.changes(),
        [TrustInventoryChange::Modified { after, .. }]
            if after.path == ".claude/skills/shared/SKILL.md"
    ));
}

#[cfg(unix)]
#[test]
fn out_of_bounds_cyclic_or_non_skill_links_leave_the_project_untrustable() {
    use std::os::unix::fs::symlink;

    type Arrange = fn(&Path, &Path);
    let cases: [(&str, Arrange); 3] = [
        (".claude/skills/outside", |workspace, outside| {
            fs::create_dir_all(workspace.join(".claude/skills")).expect("skills");
            symlink(outside, workspace.join(".claude/skills/outside")).expect("link");
        }),
        (".claude/skills/loop/again", |workspace, _| {
            fs::create_dir_all(workspace.join(".claude/skills/loop")).expect("skills");
            symlink(
                workspace.join(".claude/skills"),
                workspace.join(".claude/skills/loop/again"),
            )
            .expect("cycle");
        }),
        (".claude/commands", |workspace, _| {
            fs::create_dir_all(workspace.join("elsewhere")).expect("elsewhere");
            fs::create_dir_all(workspace.join(".claude")).expect("claude");
            symlink(
                workspace.join("elsewhere"),
                workspace.join(".claude/commands"),
            )
            .expect("commands link");
        }),
    ];
    for (offending, arrange) in cases {
        let root = TempDir::new().expect("root");
        let workspace = root.path().join("repo");
        let outside = root.path().join("outside");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("SKILL.md"), "payload").expect("payload");
        arrange(&workspace, &outside);
        let store = FolderTrustStore::new(root.path().join("user/trust.json"))
            .with_user_home(root.path().join("home"));

        let assessment = store.assess(&workspace).expect("assessment");

        assert_eq!(
            assessment.state(),
            FolderTrustState::Untrustable,
            "{offending}"
        );
        assert_eq!(assessment.executable_hash(), None, "{offending}");
        let failure = assessment.inventory_failure().expect("inventory failure");
        assert_eq!(
            failure.path(),
            assessment.workspace().join(offending),
            "{offending}"
        );
    }
}
