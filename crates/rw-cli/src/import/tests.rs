use super::formats::*;
use super::*;
use tempfile::tempdir;

#[test]
fn claude_import_is_dry_run_apply_idempotent_and_secret_free() {
    let source = tempdir().expect("source");
    let target = tempdir().expect("target");
    fs::create_dir_all(source.path().join(".claude/commands")).expect("commands");
    fs::write(source.path().join("CLAUDE.md"), "guidance").expect("claude");
    fs::write(
        source.path().join(".claude/commands/test.md"),
        "run $0 then $1",
    )
    .expect("command");
    fs::write(source.path().join(".mcp.json"), r#"{"mcpServers":{"ok":{"command":"/usr/bin/true","env":{"TOKEN":"literal-secret","SAFE":"${SAFE}"}}}}"#).expect("mcp");
    fs::write(
            source.path().join(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"check-policy"}]}]}}"#,
        )
        .expect("settings");
    let options = ImportOptions {
        source: ImportSource::Claude,
        source_root: source.path().to_path_buf(),
        target_root: target.path().to_path_buf(),
        dry_run: true,
    };
    assert!(
        run(&options)
            .expect("plan")
            .items
            .iter()
            .any(|item| item.status == ImportStatus::Planned)
    );
    let mut apply = options;
    apply.dry_run = false;
    run(&apply).expect("apply");
    run(&apply).expect("idempotent");
    let command =
        fs::read_to_string(apply.target_root.join(".agents/commands/test.md")).expect("command");
    assert!(command.contains("description: Imported command test"));
    assert!(command.ends_with("run $1 then $2"));
    assert_eq!(
        fs::read_to_string(apply.target_root.join("AGENTS.md")).expect("instructions"),
        "guidance"
    );
    let mcp = fs::read_to_string(apply.target_root.join(".agents/mcp.toml")).expect("mcp");
    assert!(!mcp.contains("SAFE"));
    assert!(!mcp.contains("literal-secret"));
    let hooks = fs::read_to_string(apply.target_root.join(".agents/hooks.toml")).expect("hooks");
    assert!(hooks.contains("event = \"pre_tool\""));
    assert!(hooks.contains("check-policy"));

    let user = tempdir().expect("user home");
    let catalog = rw_ext::ExtensionCatalog::discover(
        &rw_ext::ExtensionDiscoveryConfig::new(&apply.target_root, user.path())
            .with_project_trusted(true),
    );
    assert!(catalog.command("test").is_some());
    assert_eq!(catalog.shell_hooks().len(), 1);
    let executable = rw_runtime::executable_config::discover_executable_configs(
        user.path(),
        &apply.target_root,
        true,
    )
    .expect("imported MCP must be consumable");
    assert_eq!(executable.mcp_servers.len(), 1);
    assert!(
        rw_core::load_root_project_instructions(&apply.target_root)
            .expect("instructions load")
            .is_some()
    );
}

#[test]
fn opencode_and_pi_adapters_preserve_declarative_artifacts() {
    for (source_kind, folder, source_file, target_file) in [
        (
            ImportSource::Opencode,
            ".opencode/commands",
            "hello.md",
            "commands/hello.md",
        ),
        (
            ImportSource::Pi,
            ".pi/prompts",
            "ship.md",
            "commands/ship.md",
        ),
    ] {
        let source = tempdir().expect("source");
        let target = tempdir().expect("target");
        fs::create_dir_all(source.path().join(folder)).expect("folder");
        fs::write(source.path().join(folder).join(source_file), "prompt").expect("prompt");
        run(&ImportOptions {
            source: source_kind,
            source_root: source.path().to_path_buf(),
            target_root: target.path().to_path_buf(),
            dry_run: false,
        })
        .expect("import");
        assert!(target.path().join(".agents").join(target_file).is_file());
    }
}

#[test]
fn malformed_config_and_existing_conflict_are_honest() {
    let source = tempdir().expect("source");
    let target = tempdir().expect("target");
    fs::write(source.path().join("opencode.jsonc"), "{ broken").expect("broken");
    let mut options = ImportOptions {
        source: ImportSource::Opencode,
        source_root: source.path().to_path_buf(),
        target_root: target.path().to_path_buf(),
        dry_run: false,
    };
    assert!(run(&options).is_err());
    fs::remove_file(source.path().join("opencode.jsonc")).expect("remove broken");
    fs::write(source.path().join("AGENTS.md"), "incoming").expect("source agents");
    fs::write(options.target_root.join("AGENTS.md"), "existing").expect("existing");
    options.dry_run = true;
    let report = run(&options).expect("conflict report");
    assert!(
        report
            .items
            .iter()
            .any(|item| { item.target == "AGENTS.md" && item.status == ImportStatus::Conflict })
    );
}

#[test]
fn jsonc_preserves_unicode_and_string_literals_and_removes_spaced_trailing_commas() {
    let cleaned = strip_jsonc(
        br#"{
                // comment
                "unicode": "caf\u00e9",
                "literal": ",}",
                "array": [1, 2, ],
            }"#,
    )
    .expect("JSONC");
    let value = parse_json(cleaned.as_bytes(), "fixture").expect("valid JSON");
    assert_eq!(value["unicode"], "caf\u{e9}");
    assert_eq!(value["literal"], ",}");
    assert_eq!(value["array"], serde_json::json!([1, 2]));
    assert!(strip_jsonc(b"{/* unterminated").is_err());
}

#[test]
fn opencode_inline_commands_and_nested_prompt_files_are_discoverable() {
    let source = tempdir().expect("source");
    let target = tempdir().expect("target");
    fs::create_dir_all(source.path().join(".opencode/commands/release")).expect("nested commands");
    fs::write(
        source.path().join(".opencode/commands/release/check.md"),
        "Review $ARGUMENTS",
    )
    .expect("nested command");
    fs::write(
        source.path().join("opencode.jsonc"),
        r#"{"command":{"test":{"template":"Run $ARGUMENTS","description":"Run tests"}}}"#,
    )
    .expect("config");
    run(&ImportOptions {
        source: ImportSource::Opencode,
        source_root: source.path().to_path_buf(),
        target_root: target.path().to_path_buf(),
        dry_run: false,
    })
    .expect("import");
    let user = tempdir().expect("user");
    let catalog = rw_ext::ExtensionCatalog::discover(
        &rw_ext::ExtensionDiscoveryConfig::new(target.path(), user.path())
            .with_project_trusted(true),
    );
    assert!(catalog.command("test").is_some());
    assert!(catalog.command("release-check").is_some());
}

#[test]
fn claude_hook_alternation_maps_every_exact_tool() {
    let mut diagnostics = Vec::new();
    let rendered = render_claude_hooks(
        &serde_json::json!({
            "hooks": {"PreToolUse": [{
                "matcher": "Bash|Write",
                "hooks": [{"type": "command", "command": "check"}]
            }]}
        }),
        &mut diagnostics,
    )
    .expect("render")
    .expect("hooks");
    assert!(rendered.contains("matcher = \"bash(*)\""));
    assert!(rendered.contains("matcher = \"write(*)\""));
    assert!(diagnostics.is_empty());
}

#[cfg(unix)]
#[test]
fn symlink_hardlink_and_oversized_sources_fail_closed() {
    use std::os::unix::fs::symlink;
    let source = tempdir().expect("source");
    let target = tempdir().expect("target");
    fs::create_dir_all(source.path().join(".claude/commands")).expect("commands");
    let outside = source.path().join("outside");
    fs::write(&outside, "secret").expect("outside");
    symlink(&outside, source.path().join(".claude/commands/link.md")).expect("link");
    let options = ImportOptions {
        source: ImportSource::Claude,
        source_root: source.path().to_path_buf(),
        target_root: target.path().to_path_buf(),
        dry_run: true,
    };
    assert!(run(&options).is_err());
    fs::remove_file(source.path().join(".claude/commands/link.md")).expect("remove");
    fs::hard_link(&outside, source.path().join(".claude/commands/hard.md")).expect("hard link");
    assert!(run(&options).is_err());
    fs::remove_file(source.path().join(".claude/commands/hard.md")).expect("remove hard link");
    fs::write(
        source.path().join(".claude/commands/big.md"),
        vec![b'x'; MAX_FILE_BYTES + 1],
    )
    .expect("big");
    assert!(run(&options).is_err());
}

#[cfg(unix)]
#[test]
fn unsafe_late_target_is_rejected_before_any_file_is_created() {
    use std::os::unix::fs::symlink;

    let source = tempdir().expect("source");
    let target = tempdir().expect("target");
    let outside = tempdir().expect("outside");
    fs::write(source.path().join("CLAUDE.md"), "instructions").expect("instructions");
    fs::write(
        source.path().join(".mcp.json"),
        r#"{"mcpServers":{"local":{"command":"/usr/bin/true"}}}"#,
    )
    .expect("MCP");
    symlink(outside.path(), target.path().join(".agents")).expect("target symlink");
    let result = run(&ImportOptions {
        source: ImportSource::Claude,
        source_root: source.path().to_path_buf(),
        target_root: target.path().to_path_buf(),
        dry_run: false,
    });
    assert!(result.is_err());
    assert!(!target.path().join("AGENTS.md").exists());
    assert!(
        fs::read_dir(outside.path())
            .expect("outside")
            .next()
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn unsafe_candidate_directory_is_not_silently_skipped() {
    use std::os::unix::fs::symlink;

    let source = tempdir().expect("source");
    let outside = tempdir().expect("outside");
    let target = tempdir().expect("target");
    fs::create_dir(source.path().join(".claude")).expect("claude");
    symlink(outside.path(), source.path().join(".claude/commands")).expect("candidate symlink");
    assert!(
        run(&ImportOptions {
            source: ImportSource::Claude,
            source_root: source.path().to_path_buf(),
            target_root: target.path().to_path_buf(),
            dry_run: true,
        })
        .is_err()
    );
}
