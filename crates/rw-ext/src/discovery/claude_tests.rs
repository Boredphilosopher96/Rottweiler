#![allow(clippy::expect_used)]

//! `.claude` roots, linked skills, and skill invocation rendering.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::{
    ArtifactKind, ArtifactLocation, ArtifactScope, ExtensionCatalog, ExtensionDiscoveryConfig,
    SkippedBundleReason, TemplatePart,
};

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
    fs::write(path, contents).expect("write fixture");
}

const CLAUDE_SKILL: &str = "---\nname: careful\nversion: 0.1.0\ndescription: |\n  Safety guardrails for destructive commands.\n  Use when asked to \"be careful\".\nallowed-tools:\n  - Bash\n  - Read\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"bash ${CLAUDE_SKILL_DIR}/bin/check.sh\"\n---\nRun ${CLAUDE_SKILL_DIR}/bin/check.sh before destructive commands.\n";

#[cfg(unix)]
#[test]
fn user_claude_skills_load_through_linked_manifests_and_directories() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let library = fixture.path().join("library");
    write(&library.join("careful/SKILL.md"), CLAUDE_SKILL);
    write(&library.join("careful/bin/check.sh"), "#!/bin/sh\n");
    write(
        &library.join("SKILL.md"),
        "---\nname: library\ndescription: >-\n  Whole library\n  as one skill.\n---\nbody",
    );
    let skills = home.join(".claude/skills");
    fs::create_dir_all(skills.join("careful")).expect("skill directory");
    symlink(
        library.join("careful/SKILL.md"),
        skills.join("careful/SKILL.md"),
    )
    .expect("manifest link");
    symlink(&library, skills.join("library")).expect("directory link");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));

    assert!(
        catalog.diagnostics().is_empty(),
        "{:?}",
        catalog.diagnostics()
    );
    let careful = catalog.skill("careful").expect("careful");
    assert_eq!(
        careful.description(),
        "Safety guardrails for destructive commands.\nUse when asked to \"be careful\"."
    );
    assert_eq!(careful.allowed_tools(), ["Bash", "Read"]);
    assert_eq!(careful.origin().location(), ArtifactLocation::Claude);
    assert_eq!(careful.origin().scope(), ArtifactScope::User);
    assert_eq!(careful.origin().path(), skills.join("careful/SKILL.md"));
    let canonical = fs::canonicalize(library.join("careful")).expect("canonical");
    assert_eq!(careful.root(), canonical);
    let rendered = careful.render_invocation("now").expect("render");
    assert!(rendered.contains(&format!("Run {}/bin/check.sh", canonical.display())));
    assert!(rendered.contains("- bin/check.sh"));
    assert!(rendered.contains("## Invocation arguments\n\nnow"));
    let library_skill = catalog.skill("library").expect("library");
    assert_eq!(library_skill.description(), "Whole library as one skill.");
}

#[test]
fn claude_roots_have_lowest_precedence_within_each_scope() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    for root in [
        home.join(".agents/skills/shared"),
        home.join(".claude/skills/shared"),
        project.join(".claude/skills/local"),
    ] {
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .expect("name");
        write(
            &root.join("SKILL.md"),
            &format!("---\ndescription: {}\n---\nbody", root.display()),
        );
        assert!(!name.is_empty());
    }
    let catalog = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
    );
    let shared = catalog.skill("shared").expect("shared");
    assert_eq!(shared.origin().location(), ArtifactLocation::Agents);
    let local = catalog.skill("local").expect("project .claude skill");
    assert_eq!(local.origin().scope(), ArtifactScope::Project);
    assert_eq!(catalog.shadowed().len(), 1);
    assert_eq!(catalog.shadowed()[0].kind(), ArtifactKind::Skill);
    assert_eq!(
        catalog.shadowed()[0].origin().path(),
        home.join(".claude/skills/shared/SKILL.md")
    );
}

#[test]
fn untrusted_project_claude_skills_stay_inert() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &project.join(".claude/skills/local/SKILL.md"),
        "---\ndescription: local\n---\nbody",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert!(catalog.skill("local").is_none());
    assert_eq!(catalog.inert_project_artifacts().len(), 1);
    assert_eq!(catalog.inert_project_artifacts()[0].name(), "local");
}

#[cfg(unix)]
#[test]
fn project_skill_links_must_stay_inside_project_or_home() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let outside = fixture.path().join("outside/escape");
    write(
        &outside.join("SKILL.md"),
        "---\ndescription: escape\n---\nbody",
    );
    let in_home = home.join("library/shared");
    write(
        &in_home.join("SKILL.md"),
        "---\ndescription: shared\n---\nbody",
    );
    let skills = project.join(".claude/skills");
    fs::create_dir_all(&skills).expect("skills");
    symlink(&outside, skills.join("escape")).expect("outside link");
    symlink(&in_home, skills.join("shared")).expect("home link");

    let catalog = ExtensionCatalog::discover(
        &ExtensionDiscoveryConfig::new(&project, &home).with_project_trusted(true),
    );
    assert!(catalog.skill("shared").is_some());
    assert!(catalog.skill("escape").is_none());
    assert_eq!(catalog.diagnostics().len(), 1);
    assert_eq!(catalog.diagnostics()[0].path(), skills.join("escape"));
    assert!(
        catalog.diagnostics()[0]
            .message()
            .contains("outside the project")
    );
}

#[test]
fn claude_agent_definitions_are_not_read() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &home.join(".claude/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Claude agent\ntools: Read, Grep\nmodel: sonnet\n---\nprompt",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    assert!(catalog.agent("reviewer").is_none());
    assert!(catalog.diagnostics().is_empty());
}

#[test]
fn commands_without_frontmatter_use_their_first_line() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    write(
        &home.join(".claude/commands/fix.md"),
        "# Fix the failing test\n\nFix $0 in $ARGUMENTS[1] ($ARGUMENTS).\n",
    );
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let command = catalog.command("fix").expect("command");
    assert_eq!(command.description(), "Fix the failing test");
    assert_eq!(command.origin().location(), ArtifactLocation::Claude);
    let template = command.load_template().expect("template");
    assert!(
        template
            .parts()
            .contains(&TemplatePart::PositionalArgument(1))
    );
    assert!(
        template
            .parts()
            .contains(&TemplatePart::PositionalArgument(2))
    );
    assert!(template.parts().contains(&TemplatePart::Arguments));
}

#[test]
fn skill_instructions_reload_after_edit() {
    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let path = home.join(".agents/skills/notes/SKILL.md");
    write(&path, "---\ndescription: notes\n---\noriginal");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    fs::write(&path, "---\ndescription: notes\n---\nedited").expect("edit");
    let skill = catalog.skill("notes").expect("notes");
    assert_eq!(skill.load_instructions().expect("reload"), "edited");
    fs::remove_file(&path).expect("remove");
    let error = skill.load_instructions().expect_err("removed");
    assert!(error.to_string().contains("SKILL.md"), "{error}");
}

#[cfg(unix)]
#[test]
fn bundle_listing_is_bounded_and_skips_links_and_dependency_trees() {
    use std::os::unix::fs::symlink;

    let fixture = TempDir::new().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let root = home.join(".agents/skills/big");
    write(&root.join("SKILL.md"), "---\ndescription: big\n---\nbody");
    for index in 0..100 {
        write(&root.join(format!("docs/{index:03}.md")), "doc");
    }
    write(&root.join("node_modules/pkg/index.js"), "code");
    write(&root.join(".git/HEAD"), "ref");
    fs::write(root.join("blob.bin"), [0_u8, 159, 146, 150]).expect("binary");
    symlink(fixture.path(), root.join("escape")).expect("link");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let skill = catalog.skill("big").expect("big");
    let listing = skill.bundle_listing(8);
    assert_eq!(listing.files.len(), 8);
    assert!(listing.truncated);
    assert_eq!(listing.files[0], PathBuf::from("blob.bin"));
    let reasons = listing
        .skipped
        .iter()
        .map(|entry| (entry.relative_path.clone(), entry.reason))
        .collect::<Vec<_>>();
    assert!(reasons.contains(&(PathBuf::from("escape"), SkippedBundleReason::SymbolicLink)));
    assert!(reasons.contains(&(
        PathBuf::from("node_modules"),
        SkippedBundleReason::HiddenOrDependencyDirectory
    )));
    assert!(skill.read_bundled_file("blob.bin").is_err());
    assert!(skill.read_bundled_file("escape/project").is_err());
    let rendered = skill.render_invocation("").expect("render");
    assert!(rendered.contains("more files not listed"));
    assert!(!rendered.contains("Invocation arguments"));
}

/// Acceptance check over a copy of a real `~/.claude/skills` tree. Run with
/// `ROTTWEILER_CLAUDE_SKILLS_FIXTURE=$HOME/.claude/skills cargo test -p rw-ext
/// real_claude_skill_tree -- --ignored`. The tree is copied (links preserved)
/// into a temporary home; the source is only read.
#[cfg(unix)]
#[test]
#[ignore = "reads a user-supplied skills tree"]
fn real_claude_skill_tree_discovers_every_skill() {
    let Some(source) = std::env::var_os("ROTTWEILER_CLAUDE_SKILLS_FIXTURE") else {
        return;
    };
    let fixture = TempDir::new().expect("fixture");
    let home = fixture.path().join("home");
    let skills = home.join(".claude/skills");
    fs::create_dir_all(home.join(".claude")).expect("claude directory");
    let status = std::process::Command::new("cp")
        .arg("-RP")
        .arg(&source)
        .arg(&skills)
        .status()
        .expect("copy skills tree");
    assert!(status.success());
    let expected = fs::read_dir(&skills)
        .expect("skills")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("SKILL.md").exists())
        .count();
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(
        fixture.path().join("project"),
        &home,
    ));
    assert!(
        catalog.diagnostics().is_empty(),
        "{:#?}",
        catalog.diagnostics()
    );
    assert_eq!(catalog.skills().len(), expected);
    for skill in catalog.skills() {
        skill.render_invocation("").expect("every skill renders");
    }
    eprintln!(
        "discovered {expected} skills from {}",
        Path::new(&source).display()
    );
}
