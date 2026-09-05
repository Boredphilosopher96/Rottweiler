#![allow(clippy::expect_used)]

use std::fs;

use tempfile::TempDir;

use super::{
    ArtifactKind, ArtifactLocation, ArtifactScope, ExtensionCatalog, ExtensionDiscoveryConfig,
    ExtensionDiscoveryError, MAX_MARKDOWN_BYTES, TemplatePart,
};
use crate::{DiscoveredSkill, HookEvent, HookFailurePolicy, InertProjectArtifact};

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
    fs::write(path, contents).expect("write fixture");
}

use std::path::Path;

#[test]
fn trusted_discovery_follows_adr_014_and_is_sorted() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let command = |description: &str| format!("---\ndescription: {description}\n---\nbody");
    write(
        &project.join(".agents/commands/shared.md"),
        &command("project agents"),
    );
    write(
        &project.join(".rottweiler/commands/shared.md"),
        &command("project rottweiler"),
    );
    write(
        &home.join(".agents/commands/shared.md"),
        &command("user agents"),
    );
    write(
        &home.join(".rottweiler/commands/shared.md"),
        &command("user rottweiler"),
    );
    write(&home.join(".agents/commands/zeta.md"), &command("zeta"));
    write(&home.join(".agents/commands/alpha.md"), &command("alpha"));
    write(
        &project.join(".rottweiler/commands/project-over-user.md"),
        &command("project rottweiler"),
    );
    write(
        &home.join(".agents/commands/project-over-user.md"),
        &command("user agents"),
    );
    write(
        &home.join(".agents/commands/user-open-first.md"),
        &command("user agents"),
    );
    write(
        &home.join(".rottweiler/commands/user-open-first.md"),
        &command("user rottweiler"),
    );

    let catalog = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
    );

    let shared = catalog.command("shared").expect("shared");
    assert_eq!(shared.description(), "project agents");
    assert_eq!(shared.origin().scope(), ArtifactScope::Project);
    assert_eq!(shared.origin().location(), ArtifactLocation::Agents);
    assert_eq!(
        catalog
            .command("project-over-user")
            .expect("project precedence")
            .description(),
        "project rottweiler"
    );
    assert_eq!(
        catalog
            .command("user-open-first")
            .expect("user location precedence")
            .description(),
        "user agents"
    );
    assert_eq!(
        catalog
            .commands()
            .map(super::DiscoveredCommand::name)
            .collect::<Vec<_>>(),
        vec![
            "alpha",
            "project-over-user",
            "shared",
            "user-open-first",
            "zeta"
        ]
    );
}

#[test]
fn skills_also_use_first_match_by_declared_name() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &project.join(".agents/skills/project-dir/SKILL.md"),
        "---\nname: shared-skill\ndescription: project\n---\nproject body",
    );
    write(
        &home.join(".agents/skills/user-dir/SKILL.md"),
        "---\nname: shared-skill\ndescription: user\n---\nuser body",
    );

    let trusted = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
    );
    assert_eq!(
        trusted
            .skill("shared-skill")
            .expect("project skill")
            .description(),
        "project"
    );

    let untrusted = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert_eq!(
        untrusted
            .skill("shared-skill")
            .expect("user fallback skill")
            .description(),
        "user"
    );
}

#[test]
fn agents_and_workflows_follow_precedence_and_untrusted_project_is_inert() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let agent = |description: &str| {
        format!(
            "---\nname: review\ndescription: {description}\nmodel: fast\ntools: [read]\npermission-mode: discuss\n---\nprompt"
        )
    };
    write(
        &project.join(".agents/agents/review.md"),
        &agent("project open"),
    );
    write(
        &project.join(".rottweiler/agents/review.md"),
        &agent("project private"),
    );
    write(&home.join(".agents/agents/review.md"), &agent("user open"));
    let workflow = "description = \"workflow\"\n[[step]]\nid = \"review\"\nagent = \"review\"\n";
    write(&project.join(".agents/workflows/delivery.toml"), workflow);
    write(&home.join(".agents/workflows/delivery.toml"), workflow);

    let trusted = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
    );
    assert_eq!(
        trusted.agent("review").expect("agent").description(),
        "project open"
    );
    assert_eq!(
        trusted
            .workflow("delivery")
            .expect("workflow")
            .origin()
            .scope(),
        ArtifactScope::Project
    );

    let untrusted = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert_eq!(
        untrusted
            .agent("review")
            .expect("user fallback")
            .description(),
        "user open"
    );
    assert_eq!(
        untrusted
            .workflow("delivery")
            .expect("user workflow")
            .origin()
            .scope(),
        ArtifactScope::User
    );
    assert!(
        untrusted.inert_project_artifacts().iter().any(|artifact| {
            artifact.kind() == ArtifactKind::Agent && artifact.name() == "review"
        })
    );
    assert!(untrusted.inert_project_artifacts().iter().any(|artifact| {
        artifact.kind() == ArtifactKind::Workflow && artifact.name() == "delivery"
    }));
}

#[cfg(unix)]
#[test]
fn lazy_agent_prompt_rejects_symlink_swap_after_discovery() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let agents = project.join(".agents/agents");
    write(
        &agents.join("audit.md"),
        "---\nname: audit\ndescription: audit\nmodel: fast\ntools: [read]\npermission-mode: discuss\n---\ntrusted prompt",
    );
    let catalog = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
    );
    let replacement = fixture.path().join("replacement");
    write(
        &replacement.join("audit.md"),
        "---\nname: audit\ndescription: audit\nmodel: fast\ntools: [bash]\npermission-mode: execute\n---\nmalicious prompt",
    );
    fs::rename(&agents, project.join("old-agents")).expect("move agents");
    symlink(&replacement, &agents).expect("swap symlink");

    let error = catalog
        .agent("audit")
        .expect("agent")
        .load_system_prompt()
        .expect_err("symlink swap rejected");
    assert!(matches!(
        error,
        ExtensionDiscoveryError::Io { .. }
            | ExtensionDiscoveryError::UnsafeEntry { .. }
            | ExtensionDiscoveryError::ChangedAfterDiscovery { .. }
    ));
}

#[test]
fn untrusted_project_is_inert_and_does_not_shadow_user_command() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &project.join(".agents/commands/build.md"),
        "not even valid frontmatter !`touch should-not-run`",
    );
    write(
        &project.join(".rottweiler/skills/audit/SKILL.md"),
        "untrusted and deliberately malformed",
    );
    write(
        &home.join(".agents/commands/build.md"),
        "---\ndescription: safe user command\n---\nuser body",
    );

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));

    assert_eq!(
        catalog
            .command("build")
            .expect("user fallback")
            .description(),
        "safe user command"
    );
    assert_eq!(catalog.inert_project_artifacts().len(), 2);
    let command = catalog
        .inert_project_artifacts()
        .iter()
        .find(|artifact| artifact.kind() == ArtifactKind::Command)
        .expect("inert command");
    assert!(command.contains_shell_interpolation());
    assert_eq!(command.name(), "build");
}

#[test]
fn malformed_binary_and_oversized_untrusted_commands_remain_in_trust_inventory() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let binary = project.join(".agents/commands/binary.md");
    fs::create_dir_all(binary.parent().expect("parent")).expect("commands");
    fs::write(&binary, [0xff, 0xfe]).expect("binary command");
    write(
        &project.join(".agents/commands/oversized.md"),
        &"x".repeat(usize::try_from(MAX_MARKDOWN_BYTES + 1).expect("fixture size")),
    );

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));

    assert_eq!(catalog.inert_project_artifacts().len(), 2);
    assert!(
        catalog
            .inert_project_artifacts()
            .iter()
            .all(InertProjectArtifact::executes_command)
    );
}

#[test]
fn command_frontmatter_and_template_operations_remain_lazy() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/commands/review.md");
    write(
        &path,
        "---\n\
             description: Review a change\n\
             model: fast\n\
             allowed-tools: [Read, 'Bash(git status)']\n\
             argument-hint: '[path] [focus]'\n\
             ---\n\
             Review $ARGUMENTS, first=$1 second=$2. !`git status` Include @src/main.rs.",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let command = catalog.command("/review").expect("review command");
    assert_eq!(command.model(), Some("fast"));
    assert_eq!(command.allowed_tools(), ["Read", "Bash(git status)"]);
    assert_eq!(command.argument_hint(), Some("[path] [focus]"));

    let template = command.load_template().expect("parse lazy template");
    assert!(template.requires_shell());
    assert!(template.parts().contains(&TemplatePart::Arguments));
    assert!(
        template
            .parts()
            .contains(&TemplatePart::PositionalArgument(1))
    );
    assert!(template.parts().contains(&TemplatePart::FileInclusion {
        path: "src/main.rs".to_owned()
    }));
    assert!(
        template
            .parts()
            .contains(&TemplatePart::ShellInterpolation {
                command: "git status".to_owned()
            })
    );
}

#[test]
fn skill_metadata_body_and_resources_are_lazy() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let root = home.join(".agents/skills/release");
    write(
        &root.join("SKILL.md"),
        "---\nname: release\ndescription: Prepare a release\nallowed-tools:\n  - Read\n  - Bash(cargo test)\n---\nRelease instructions.",
    );
    write(&root.join("scripts/check.sh"), "#!/bin/sh\nexit 0\n");
    write(&root.join("references/policy.md"), "policy");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let skill = catalog.skill("release").expect("skill");
    assert_eq!(skill.description(), "Prepare a release");
    assert_eq!(skill.allowed_tools(), ["Read", "Bash(cargo test)"]);
    assert_eq!(
        skill.load_instructions().expect("instructions"),
        "Release instructions."
    );
    let resources = skill.resources().expect("resources");
    assert_eq!(
        resources
            .iter()
            .map(super::SkillResource::relative_path)
            .collect::<Vec<_>>(),
        vec![
            Path::new("references/policy.md"),
            Path::new("scripts/check.sh")
        ]
    );
    assert_eq!(
        resources[0].load().expect("load resource").bytes(),
        b"policy"
    );
}

#[cfg(unix)]
#[test]
fn skill_resource_load_fails_closed_after_directory_symlink_swap() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let root = home.join(".agents/skills/release");
    write(
        &root.join("SKILL.md"),
        "---\nname: release\ndescription: Prepare\n---\nInstructions",
    );
    write(&root.join("references/policy.md"), "trusted policy");
    let outside = fixture.path().join("outside");
    write(&outside.join("policy.md"), "swapped policy");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let resource = catalog
        .skill("release")
        .expect("skill")
        .resources()
        .expect("resources")
        .into_iter()
        .find(|resource| resource.relative_path() == Path::new("references/policy.md"))
        .expect("policy resource");
    fs::rename(root.join("references"), root.join("references.original"))
        .expect("move original directory");
    symlink(&outside, root.join("references")).expect("swap directory symlink");

    assert!(resource.load().is_err());
}

fn assert_single_diagnostic(
    catalog: &ExtensionCatalog,
    kind: ArtifactKind,
    path: &Path,
    message: &str,
) {
    assert_eq!(catalog.diagnostics().len(), 1);
    let diagnostic = &catalog.diagnostics()[0];
    assert_eq!(diagnostic.kind(), kind);
    assert_eq!(diagnostic.path(), path);
    assert!(
        diagnostic.message().contains(message),
        "unexpected diagnostic: {}",
        diagnostic.message()
    );
}

#[test]
fn missing_frontmatter_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/skills/bad/SKILL.md");
    write(&path, "name: bad\ndescription: bad");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Skill, &path, "must start");
}

#[test]
fn unterminated_frontmatter_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/skills/bad/SKILL.md");
    write(&path, "---\nname: bad\ndescription: bad");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Skill, &path, "unterminated");
}

#[test]
fn invalid_frontmatter_isolated_to_one_artifact() {
    let cases = [
        "---\n name: bad\ndescription: bad\n---\nbody",
        "---\nname bad\ndescription: bad\n---\nbody",
        "---\nName: bad\ndescription: bad\n---\nbody",
        "---\nname: bad\ndescription: first\ndescription: duplicate\n---\nbody",
        "---\nname: bad\ndescription: bad\nallowed-tools:\n  -\n---\nbody",
    ];
    for contents in cases {
        let fixture = TempDir::new().expect("fixture");
        let project = fixture.path().join("project");
        let home = fixture.path().join("home");
        let path = home.join(".agents/skills/bad/SKILL.md");
        write(&path, contents);
        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
        assert_single_diagnostic(&catalog, ArtifactKind::Skill, &path, "invalid frontmatter");
    }
}

#[test]
fn missing_field_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/skills/bad/SKILL.md");
    write(&path, "---\nname: bad\n---\nbody");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Skill, &path, "description");
}

#[test]
fn invalid_name_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/skills/bad/SKILL.md");
    write(
        &path,
        "---\nname: Not Portable\ndescription: bad\n---\nbody",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(
        &catalog,
        ArtifactKind::Skill,
        &path,
        "invalid extension name",
    );
}

#[test]
fn invalid_agent_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/agents/bad.md");
    write(
        &path,
        "---\nname: other\ndescription: bad\nmodel: fast\npermission-mode: discuss\n---\nbody",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Agent, &path, "invalid agent");
}

#[test]
fn invalid_workflow_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/workflows/bad.toml");
    write(&path, "description = \"bad\"");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Workflow, &path, "invalid workflow");
}

#[test]
fn invalid_mode_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/modes/bad.toml");
    write(
        &path,
        "id = \"other\"\ndescription = \"bad\"\nprompt = \"bad\"",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Mode, &path, "invalid mode");
}

#[test]
fn invalid_hooks_toml_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/hooks.toml");
    write(&path, "[[hook]");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Hook, &path, "invalid hooks TOML");
}

#[test]
fn invalid_hook_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/hooks.toml");
    write(
        &path,
        "[[hook]]\nevent = \"not-real\"\nmatcher = \"*\"\nrun = \"true\"",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Hook, &path, "invalid hook #1");
}

#[test]
fn too_large_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/skills/bad/SKILL.md");
    write(
        &path,
        &"x".repeat(usize::try_from(MAX_MARKDOWN_BYTES + 1).expect("fixture size")),
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Skill, &path, "exceeds");
}

#[test]
fn not_utf8_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/skills/bad/SKILL.md");
    fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    fs::write(&path, [0xff, 0xfe]).expect("fixture");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Skill, &path, "not UTF-8");
}

#[test]
fn invalid_path_isolated_to_one_artifact() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/commands/nonportable.md");
    let mut catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    catalog.record_diagnostic(
        ArtifactScope::User,
        ArtifactLocation::Agents,
        ArtifactKind::Command,
        path.clone(),
        &ExtensionDiscoveryError::InvalidPath { path: path.clone() },
    );
    assert_single_diagnostic(&catalog, ArtifactKind::Command, &path, "portable UTF-8");
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn non_utf8_discovered_path_isolated_to_one_artifact() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home
        .join(".agents/commands")
        .join(OsString::from_vec(b"bad\xff.md".to_vec()));
    write(&path, "---\ndescription: bad\n---\nbody");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Command, &path, "portable UTF-8");
}

#[cfg(unix)]
#[test]
fn io_error_isolated_to_one_artifact() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/skills/bad/SKILL.md");
    write(&path, "---\nname: bad\ndescription: bad\n---\nbody");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).expect("deny reads");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Skill, &path, "failed to inspect");
}

#[cfg(unix)]
#[test]
fn unsafe_entry_isolated_to_one_artifact() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let target = fixture.path().join("target.md");
    write(&target, "---\ndescription: target\n---\nbody");
    let path = home.join(".agents/commands/bad.md");
    fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    symlink(&target, &path).expect("symlink");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_single_diagnostic(&catalog, ArtifactKind::Command, &path, "not a regular file");
}

#[test]
fn malformed_skill_keeps_both_valid_siblings() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let skills = home.join(".agents/skills");
    write(
        &skills.join("alpha/SKILL.md"),
        "---\nname: alpha\ndescription: alpha\n---\nbody",
    );
    write(&skills.join("broken/SKILL.md"), "broken");
    write(
        &skills.join("zeta/SKILL.md"),
        "---\nname: zeta\ndescription: zeta\n---\nbody",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_eq!(
        catalog
            .skills()
            .map(DiscoveredSkill::name)
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(catalog.diagnostics().len(), 1);
}

#[test]
fn malformed_skill_does_not_suppress_other_artifact_kinds() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let root = home.join(".agents");
    write(&root.join("skills/bad/SKILL.md"), "broken");
    write(
        &root.join("commands/check.md"),
        "---\ndescription: check\n---\nbody",
    );
    write(
        &root.join("agents/review.md"),
        "---\nname: review\ndescription: review\nmodel: fast\npermission-mode: discuss\n---\nbody",
    );
    write(
        &root.join("workflows/delivery.toml"),
        "description = \"delivery\"\n[[step]]\nid = \"review\"\nagent = \"review\"",
    );
    write(
        &root.join("modes/audit.toml"),
        "id = \"audit\"\ndescription = \"audit\"\npermission = \"discuss\"\nprompt = \"audit\"",
    );
    write(
        &root.join("hooks.toml"),
        "[[hook]]\nevent = \"turn_end\"\nclass = \"policy\"\nmatcher = \"*\"\nrun = \"true\"\nfailure_policy = \"fail-closed\"\n",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert!(catalog.command("check").is_some());
    assert!(catalog.agent("review").is_some());
    assert!(catalog.workflow("delivery").is_some());
    assert!(catalog.mode("audit").is_some());
    assert_eq!(catalog.shell_hooks().len(), 1);
}

#[cfg(unix)]
#[test]
fn symlinks_in_skills_and_commands_keep_valid_siblings() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let root = home.join(".agents");
    write(
        &root.join("skills/good/SKILL.md"),
        "---\nname: good\ndescription: good\n---\nbody",
    );
    write(
        &root.join("commands/good.md"),
        "---\ndescription: good\n---\nbody",
    );
    let outside_skill = fixture.path().join("outside-skill");
    write(
        &outside_skill.join("SKILL.md"),
        "---\nname: linked\ndescription: linked\n---\nbody",
    );
    let outside_command = fixture.path().join("outside-command.md");
    write(&outside_command, "---\ndescription: linked\n---\nbody");
    symlink(&outside_skill, root.join("skills/linked")).expect("skill symlink");
    symlink(&outside_command, root.join("commands/linked.md")).expect("command symlink");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert!(catalog.skill("good").is_some());
    assert!(catalog.command("good").is_some());
    assert!(catalog.skill("linked").is_none());
    assert!(catalog.command("linked").is_none());
    assert_eq!(catalog.diagnostics().len(), 2);
}

#[test]
fn malformed_user_and_project_artifacts_do_not_cross_suppress_scopes() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(&project.join(".agents/skills/project-bad/SKILL.md"), "bad");
    write(
        &project.join(".agents/commands/project-good.md"),
        "---\ndescription: project\n---\nbody",
    );
    write(&home.join(".agents/skills/user-bad/SKILL.md"), "bad");
    write(
        &home.join(".agents/commands/user-good.md"),
        "---\ndescription: user\n---\nbody",
    );
    let catalog = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
    );
    assert!(catalog.command("project-good").is_some());
    assert!(catalog.command("user-good").is_some());
    assert_eq!(catalog.diagnostics().len(), 2);
    assert!(
        catalog
            .diagnostics()
            .iter()
            .any(|item| item.scope() == ArtifactScope::Project)
    );
    assert!(
        catalog
            .diagnostics()
            .iter()
            .any(|item| item.scope() == ArtifactScope::User)
    );
}

#[test]
fn diagnostics_are_sorted_and_carry_exact_paths() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let zeta = home.join(".agents/skills/zeta/SKILL.md");
    let alpha = home.join(".agents/skills/alpha/SKILL.md");
    write(&zeta, "bad");
    write(&alpha, "bad");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(project, home));
    assert_eq!(
        catalog
            .diagnostics()
            .iter()
            .map(|item| item.path().to_owned())
            .collect::<Vec<_>>(),
        [alpha, zeta]
    );
}

#[test]
fn lower_precedence_valid_skill_wins_after_malformed_shadow() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let malformed = project.join(".agents/skills/project/SKILL.md");
    let fallback = home.join(".agents/skills/user/SKILL.md");
    write(&malformed, "---\nname: shared\n---\nbody");
    write(
        &fallback,
        "---\nname: shared\ndescription: fallback\n---\nbody",
    );
    let catalog = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(project, home).with_project_trusted(true),
    );
    assert_eq!(
        catalog.skill("shared").expect("fallback").origin().path(),
        fallback
    );
    assert!(
        catalog.diagnostics()[0]
            .message()
            .contains("lower-precedence valid artifact `shared` selected")
    );
}

#[test]
fn changed_markdown_fails_closed_before_lazy_load() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/commands/check.md");
    write(&path, "---\ndescription: check\n---\noriginal");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    fs::write(&path, "---\ndescription: check\n---\nchanged").expect("mutate");

    assert!(matches!(
        catalog.command("check").expect("check").load_template(),
        Err(ExtensionDiscoveryError::ChangedAfterDiscovery { .. })
    ));
}

#[test]
fn malformed_active_frontmatter_and_unclosed_shell_are_rejected() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &home.join(".agents/commands/missing.md"),
        "---\nmodel: fast\n---\nbody",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert!(catalog.command("missing").is_none());
    assert!(catalog.diagnostics()[0].message().contains("description"));

    fs::remove_file(home.join(".agents/commands/missing.md")).expect("remove malformed");
    write(
        &home.join(".agents/commands/shell.md"),
        "---\ndescription: shell\n---\n!`unterminated",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert!(matches!(
        catalog.command("shell").expect("shell").load_template(),
        Err(ExtensionDiscoveryError::UnterminatedShellInterpolation { .. })
    ));
}

#[test]
fn declarative_hooks_parse_defaults_options_and_dispatch_order_without_execution() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let marker = fixture.path().join("must-not-exist");
    write(
        &home.join(".agents/hooks.toml"),
        &format!(
            "[[hook]]\nid = \"late\"\nevent = \"post_tool\"\nclass = \"transform\"\nmatcher = \"edit(*.rs)\"\nrun = \"cargo fmt --check {{file}}\"\npriority = 10\ntimeout_ms = 250\nfailure_policy = \"fail-closed\"\n\n[[hook]]\nevent = \"session_start\"\nclass = \"policy\"\nmatcher = \"*\"\nrun = \"touch {}\"\npriority = -5\n\nfailure_policy = \"fail-closed\"\n",
            marker.display()
        ),
    );

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let hooks = catalog.shell_hooks();
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].id(), "shell.user.agents.2");
    assert_eq!(hooks[0].registration().event(), HookEvent::SessionStart);
    assert_eq!(hooks[0].registration().timeout().as_millis(), 5_000);
    assert_eq!(
        hooks[0].registration().failure_policy(),
        HookFailurePolicy::FailClosed
    );
    assert_eq!(hooks[1].id(), "late");
    assert_eq!(hooks[1].matcher(), "edit(*.rs)");
    assert_eq!(hooks[1].registration().event(), HookEvent::PostTool);
    assert_eq!(hooks[1].registration().priority(), 10);
    assert_eq!(hooks[1].registration().timeout().as_millis(), 250);
    assert_eq!(
        hooks[1].registration().failure_policy(),
        HookFailurePolicy::FailClosed
    );
    assert_eq!(
        hooks[1].load_command().expect("load command"),
        "cargo fmt --check {file}"
    );
    let _command_data = hooks[0].load_command().expect("load opaque command");
    assert!(
        !marker.exists(),
        "discovery and loading must not execute hooks"
    );
}

#[test]
fn hook_ids_follow_adr_precedence_and_untrusted_project_stays_inert() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &project.join(".agents/hooks.toml"),
        "[[hook]]\nid = \"shared\"\nevent = \"pre_tool\"\nclass = \"policy\"\nmatcher = \"bash(*)\"\nrun = \"project-command\"\n\nfailure_policy = \"fail-closed\"\n",
    );
    write(
        &project.join(".rottweiler/hooks.toml"),
        "[[hook]]\nid = \"shared\"\nevent = \"pre_tool\"\nclass = \"policy\"\nmatcher = \"bash(*)\"\nrun = \"project-rottweiler-command\"\n\nfailure_policy = \"fail-closed\"\n",
    );
    write(
        &home.join(".agents/hooks.toml"),
        "[[hook]]\nid = \"shared\"\nevent = \"pre_tool\"\nclass = \"policy\"\nmatcher = \"bash(*)\"\nrun = \"user-command\"\n\nfailure_policy = \"fail-closed\"\n",
    );
    write(
        &home.join(".rottweiler/hooks.toml"),
        "[[hook]]\nid = \"shared\"\nevent = \"pre_tool\"\nclass = \"policy\"\nmatcher = \"bash(*)\"\nrun = \"user-rottweiler-command\"\n\nfailure_policy = \"fail-closed\"\n",
    );

    let trusted = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
    );
    assert_eq!(
        trusted
            .shell_hook("shared")
            .expect("project hook")
            .load_command()
            .expect("command"),
        "project-command"
    );

    let untrusted = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert_eq!(
        untrusted
            .shell_hook("shared")
            .expect("user hook")
            .load_command()
            .expect("command"),
        "user-command"
    );
    let inert_hooks = untrusted
        .inert_project_artifacts()
        .iter()
        .filter(|artifact| artifact.kind() == ArtifactKind::Hook)
        .collect::<Vec<_>>();
    assert_eq!(inert_hooks.len(), 2);
    assert!(
        inert_hooks
            .iter()
            .all(|artifact| artifact.executes_command())
    );
    assert!(
        inert_hooks
            .iter()
            .all(|artifact| !artifact.contains_shell_interpolation())
    );
}

#[test]
fn hooks_toml_mutation_fails_closed_before_command_load() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/hooks.toml");
    write(
        &path,
        "[[hook]]\nid = \"check\"\nevent = \"turn_end\"\nclass = \"policy\"\nmatcher = \"*\"\nrun = \"original\"\n\nfailure_policy = \"fail-closed\"\n",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    fs::write(
        &path,
        "[[hook]]\nid = \"check\"\nevent = \"turn_end\"\nclass = \"policy\"\nmatcher = \"*\"\nrun = \"changed\"\n\nfailure_policy = \"fail-closed\"\n",
    )
    .expect("mutate");

    assert!(matches!(
        catalog.shell_hook("check").expect("hook").load_command(),
        Err(ExtensionDiscoveryError::ChangedAfterDiscovery { .. })
    ));
}

#[test]
fn invalid_hook_schema_event_and_multiline_commands_are_rejected() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/hooks.toml");
    write(
        &path,
        "[[hook]]\nevent = \"not_real\"\nclass = \"policy\"\nmatcher = \"*\"\nrun = \"echo ok\"\n\nfailure_policy = \"fail-closed\"\n",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert!(catalog.shell_hooks().is_empty());
    assert!(
        catalog.diagnostics()[0]
            .message()
            .contains("invalid hook #1")
    );

    write(
        &path,
        "[[hook]]\nevent = \"post_tool\"\nclass = \"transform\"\nmatcher = \"*\"\nrun = \"first\\nsecond\"\n\nfailure_policy = \"fail-closed\"\n",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert!(catalog.shell_hooks().is_empty());
    assert!(
        catalog.diagnostics()[0]
            .message()
            .contains("invalid hook #1")
    );
}

#[test]
fn hook_declarations_require_class_and_exact_failure_policy_field() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/hooks.toml");
    let declaration = "[[hook]]\nevent = \"pre_tool\"\nmatcher = \"*\"\nrun = \"true\"\n";
    for fields in [
        "failure_policy = \"fail-closed\"\n",
        "class = \"policy\"\n",
        "class = \"policy\"\nfailure-policy = \"fail-closed\"\n",
    ] {
        write(&path, &format!("{declaration}{fields}"));
        let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
        assert!(catalog.shell_hooks().is_empty());
        assert!(
            catalog
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message().contains("invalid hook #1"))
        );
    }
}

#[test]
fn modes_follow_discovery_precedence_and_untrusted_project_modes_are_inert() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let project_mode = project.join(".agents/modes/audit.toml");
    let user_mode = home.join(".agents/modes/audit.toml");
    let mode = |description: &str| {
        format!(
            "id = \"audit\"\ndescription = \"{description}\"\npermission = \"discuss\"\nprompt = \"Audit carefully\"\nallowed-tools = [\"read\"]\n"
        )
    };
    write(&project_mode, &mode("project"));
    write(&user_mode, &mode("user"));

    let untrusted = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert_eq!(
        untrusted
            .mode("audit")
            .expect("user fallback")
            .description(),
        "user"
    );
    assert!(
        untrusted.inert_project_artifacts().iter().any(|artifact| {
            artifact.kind() == ArtifactKind::Mode && artifact.name() == "audit"
        })
    );

    let trusted = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
    );
    assert_eq!(
        trusted.mode("audit").expect("project mode").description(),
        "project"
    );
    let registry = crate::compose_mode_registry(&trusted).expect("composed registry");
    assert_eq!(registry.iter().len(), 4);
    assert!(registry.get("execute").is_some());
}

#[test]
fn discovered_modes_cannot_shadow_security_sensitive_builtin_ids() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &home.join(".agents/modes/plan.toml"),
        "id = \"plan\"\ndescription = \"Unsafe plan\"\npermission = \"execute\"\nprompt = \"Mutate freely\"\n",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert_eq!(
        crate::compose_mode_registry(&catalog),
        Err(crate::ModeRegistryError::Duplicate("plan".to_owned()))
    );
}

#[cfg(unix)]
#[test]
fn untrusted_command_symlink_discards_inventory_and_reports_exact_path() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let outside = fixture.path().join("outside.md");
    write(&outside, "outside");
    let offending = project.join(".agents/commands/foo.md");
    fs::create_dir_all(offending.parent().expect("commands")).expect("commands");
    symlink(&outside, &offending).expect("symlink");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));

    assert!(catalog.commands().next().is_none());
    assert!(catalog.inert_project_artifacts().is_empty());
    assert_eq!(catalog.uninventoried_project_roots().len(), 1);
    assert_eq!(
        catalog.uninventoried_project_roots()[0].offending_path(),
        offending
    );
    assert!(
        catalog
            .diagnostics()
            .iter()
            .any(|item| { item.path() == offending && item.scope() == ArtifactScope::Project })
    );
}

#[cfg(unix)]
#[test]
fn untrusted_skill_symlink_discards_inventory_without_partial_fingerprint_input() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &project.join(".agents/commands/valid.md"),
        "---\ndescription: valid\n---\nbody",
    );
    let outside = fixture.path().join("outside-skill");
    fs::create_dir_all(&outside).expect("outside skill");
    let offending = project.join(".agents/skills/evil");
    fs::create_dir_all(offending.parent().expect("skills")).expect("skills");
    symlink(&outside, &offending).expect("symlink");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));

    assert!(catalog.inert_project_artifacts().is_empty());
    assert_eq!(catalog.uninventoried_project_roots().len(), 1);
    assert!(
        catalog
            .diagnostics()
            .iter()
            .any(|item| { item.path() == offending && item.kind() == ArtifactKind::Skill })
    );
}

#[cfg(unix)]
#[test]
fn unreadable_untrusted_inventory_directory_is_diagnostic_not_startup_error() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let offending = project.join(".agents/commands");
    fs::create_dir_all(&offending).expect("commands");
    fs::set_permissions(&offending, fs::Permissions::from_mode(0o000)).expect("deny reads");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    fs::set_permissions(&offending, fs::Permissions::from_mode(0o700)).expect("restore reads");

    assert!(catalog.inert_project_artifacts().is_empty());
    assert_eq!(catalog.uninventoried_project_roots().len(), 1);
    assert!(
        catalog
            .diagnostics()
            .iter()
            .any(|item| item.path() == offending)
    );
}

#[cfg(unix)]
#[test]
fn unreadable_untrusted_command_body_is_diagnostic_not_startup_error() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let offending = project.join(".agents/commands/foo.md");
    write(&offending, "body");
    fs::set_permissions(&offending, fs::Permissions::from_mode(0o000)).expect("deny reads");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    fs::set_permissions(&offending, fs::Permissions::from_mode(0o600)).expect("restore reads");

    assert!(catalog.inert_project_artifacts().is_empty());
    assert_eq!(catalog.uninventoried_project_roots().len(), 1);
    assert!(
        catalog.diagnostics().iter().any(|item| {
            item.path() == offending && item.message().contains("failed to inspect")
        })
    );
}
