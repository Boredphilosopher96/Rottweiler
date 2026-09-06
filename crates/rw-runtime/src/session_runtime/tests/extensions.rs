#![cfg(test)]
use super::Arc;
#[cfg(target_os = "macos")]
use super::BTreeMap;
use super::BashSandboxMode;
use super::BashTool;
use super::Block;
#[cfg(target_os = "macos")]
use super::CancellationToken;
use super::CommandFixtureMode;
#[cfg(target_os = "macos")]
use super::CommandRequest;
use super::CommandSafetyClassifier;
use super::ExecutionLease;
use super::ExtensionCatalog;
use super::ExtensionDiscoveryConfig;
use super::FixtureCodeIntelligence;
#[cfg(target_os = "macos")]
use super::FixtureRedactor;
use super::FixtureToolchainExecutor;
#[cfg(target_os = "macos")]
use super::HookCommandCapture;
use super::HookDispatcher;
use super::HookEvent;
use super::PluginManifest;
#[cfg(target_os = "macos")]
use super::READ_ONLY_HOOK_COMMAND_FIXTURE_NAMESPACE;
use super::ReadTool;
use super::SessionCommandAction;
use super::SessionCommandContext;
use super::ToolLimits;
use super::ToolRegistry;
use super::ToolchainConfig;
use super::ToolchainRuntime;
use super::WasmHookLimits;
use super::WasmProcessHook;
use super::build_read_only_hook_executor;
use super::builtin_hook_dispatcher;
use super::compose_runtime_commands;
use super::compose_runtime_hooks_with_extensions;
use super::discover_runtime_extensions;
use super::extension_startup_notifications;
use super::register_declarative_hooks;
use super::register_retained_wasm_hooks;
use super::semantic_file_tools;
use super::skill_index_turn;
use super::tempdir;
use super::wasm_startup_notice;
#[cfg(target_os = "macos")]
use crate::session_runtime::command_execution::build_command_executor;

#[test]
fn runtime_extension_startup_accepts_malformed_user_skill() {
    let fixture = tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let storage = fixture.path().join("storage");
    std::fs::create_dir_all(&project).expect("project");
    let skill = home.join(".agents/skills/broken/SKILL.md");
    std::fs::create_dir_all(skill.parent().expect("skill parent")).expect("skill directory");
    std::fs::write(&skill, "missing frontmatter").expect("skill fixture");

    let catalog = discover_runtime_extensions(
        &[project],
        &storage.join("trust.json"),
        &home,
        &home.join(".rottweiler"),
        false,
    )
    .expect("startup discovery remains usable");

    assert!(catalog.skills().next().is_none());
    assert_eq!(catalog.diagnostics().len(), 1);
    assert_eq!(catalog.diagnostics()[0].path(), skill);
    let notifications = extension_startup_notifications(&catalog);
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0].status, "unavailable");
    assert!(notifications[0].message.contains("must start"));
}

#[cfg(unix)]
#[test]
fn runtime_extension_startup_accepts_uninventoriable_untrusted_project() {
    use std::os::unix::fs::symlink;

    let fixture = tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let storage = fixture.path().join("storage");
    let offending = project.join(".agents/commands/foo.md");
    std::fs::create_dir_all(offending.parent().expect("commands")).expect("commands");
    std::fs::write(fixture.path().join("outside.md"), "outside").expect("outside");
    symlink(fixture.path().join("outside.md"), &offending).expect("symlink");

    let catalog = discover_runtime_extensions(
        &[project],
        &storage.join("trust.json"),
        &home,
        &home.join(".rottweiler"),
        false,
    )
    .expect("startup discovery remains usable");

    assert!(catalog.commands().next().is_none());
    assert!(catalog.inert_project_artifacts().is_empty());
    assert_eq!(catalog.uninventoried_project_roots().len(), 1);
    assert!(
        catalog
            .diagnostics()
            .iter()
            .any(|item| item.path() == offending)
    );
    assert!(
        extension_startup_notifications(&catalog)
            .iter()
            .any(|item| item.message.contains(&offending.display().to_string()))
    );
}

#[tokio::test]
async fn custom_command_shadow_expansion_and_skill_selection_are_live() {
    let fixture = tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    std::fs::create_dir_all(project.join("src")).expect("project");
    let project = std::fs::canonicalize(project).expect("canonical project");
    std::fs::write(project.join("src/lib.rs"), "fn visible() {}\n").expect("source");
    let agents = home.join(".agents/commands/code-review.md");
    std::fs::create_dir_all(agents.parent().expect("commands")).expect("agents commands");
    std::fs::write(
            &agents,
            "---\ndescription: Ported Claude review\nmodel: fast\nallowed-tools: [Read]\nargument-hint: '[path] [focus]'\n---\nReview $ARGUMENTS first=$1 second=$2 source=@src/lib.rs",
        )
        .expect("agents command");
    let rottweiler = home.join(".rottweiler/commands/code-review.md");
    std::fs::create_dir_all(rottweiler.parent().expect("commands")).expect("rottweiler commands");
    std::fs::write(rottweiler, "---\ndescription: shadowed\n---\nWRONG").expect("shadowed command");
    let skill = home.join(".agents/skills/release/SKILL.md");
    std::fs::create_dir_all(skill.parent().expect("skill")).expect("skill directory");
    std::fs::write(
            &skill,
            "---\nname: release\ndescription: Prepare release\nallowed-tools: [Read]\n---\nRelease instructions",
        )
        .expect("skill");
    std::fs::write(
        skill.parent().expect("skill").join("policy.md"),
        "resource policy",
    )
    .expect("skill resource");

    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let index = skill_index_turn(&catalog)
        .expect("index")
        .expect("skill index");
    let Block::Text { text } = &index.blocks[0] else {
        panic!("skill index is text")
    };
    assert!(text.contains("Prepare release"));
    assert!(!text.contains("Release instructions"));

    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(ReadTool::new(ToolLimits::default())))
        .expect("read tool");
    let tools = Arc::new(tools);
    let registry = compose_runtime_commands(
        &catalog,
        std::slice::from_ref(&project),
        &fixture.path().join("state"),
        &tools,
    )
    .expect("commands");
    let mut context = SessionCommandContext::default();
    assert!(
        registry
            .descriptors()
            .any(|descriptor| descriptor.name() == "review")
    );
    let review = registry
        .dispatch_line(&mut context, "/code-review 'src/lib.rs' correctness")
        .await
        .expect("review command");
    let SessionCommandAction::SubmitPrompt {
        content,
        model_alias,
        allowed_tools,
        permission_patterns,
        tool_calls,
    } = review.action
    else {
        panic!("review submits prompt")
    };
    assert!(!content.contains("WRONG"));
    assert!(content.contains("first=src/lib.rs second=correctness"));
    assert!(!content.contains("fn visible() {}"));
    assert!(content.contains("ROTTWEILER_COMMAND_TOOL"));
    assert_eq!(model_alias.as_deref(), Some("fast"));
    assert_eq!(allowed_tools, Some(vec!["read".to_owned()]));
    assert_eq!(permission_patterns, vec!["read(*)"]);
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].name, "read");
    assert_eq!(tool_calls[0].arguments["path"], "src/lib.rs");

    let release = registry
        .dispatch_line(&mut context, "/release v1")
        .await
        .expect("skill command");
    let SessionCommandAction::SubmitPrompt { content, .. } = release.action else {
        panic!("skill submits prompt")
    };
    assert!(content.contains("Release instructions"));
    assert!(content.contains("resource policy"));
    assert!(content.contains("Invocation arguments:\nv1"));
}

#[tokio::test]
async fn custom_shell_interpolation_is_deferred_as_a_typed_sandboxed_tool_call() {
    let fixture = tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    std::fs::create_dir_all(&project).expect("project");
    let project = std::fs::canonicalize(project).expect("canonical project");
    let command = home.join(".agents/commands/shell.md");
    std::fs::create_dir_all(command.parent().expect("commands")).expect("commands");
    std::fs::write(
        command,
        "---\ndescription: shell\n---\nresult=!`fixture-shell`",
    )
    .expect("command");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let executor = Arc::new(FixtureToolchainExecutor::default());
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(BashTool::new(
            executor.clone(),
            ToolLimits::default(),
        )))
        .expect("bash");
    let tools = Arc::new(tools);

    let registry = compose_runtime_commands(
        &catalog,
        std::slice::from_ref(&project),
        &fixture.path().join("state"),
        &tools,
    )
    .expect("commands");
    let output = registry
        .dispatch_line(&mut SessionCommandContext::default(), "/shell")
        .await
        .expect("typed interpolation");
    let SessionCommandAction::SubmitPrompt {
        content,
        tool_calls,
        ..
    } = output.action
    else {
        panic!("shell command submits prompt")
    };
    assert!(content.contains("ROTTWEILER_COMMAND_TOOL"));
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].name, "bash");
    assert_eq!(tool_calls[0].arguments["command"], "fixture-shell");
    assert_eq!(tool_calls[0].arguments["sandbox"], "sandboxed");
    assert!(executor.calls.lock().expect("calls").is_empty());
}

#[tokio::test]
async fn declarative_pre_tool_hook_matches_and_blocks_through_shared_executor() {
    let fixture = tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    std::fs::create_dir_all(&project).expect("project");
    let project = std::fs::canonicalize(project).expect("canonical project");
    let hooks = home.join(".agents/hooks.toml");
    std::fs::create_dir_all(hooks.parent().expect("hooks root")).expect("hooks root");
    std::fs::write(
            hooks,
            "[[hook]]\nid = \"deny-rust-edit\"\nevent = \"pre_tool\"\nclass = \"policy\"\nmatcher = \"edit(*.rs)\"\nrun = \"fixture-lint {file}\"\nfailure_policy = \"fail-closed\"\n",
        )
        .expect("hooks");
    std::fs::write(project.join("lib.rs"), "fn main() {}\n").expect("source");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let executor = Arc::new(FixtureToolchainExecutor::default());
    let runtime = Arc::new(ToolchainRuntime::new(
        executor.clone(),
        std::slice::from_ref(&project),
    ));
    let dispatcher = compose_runtime_hooks_with_extensions(
        &ToolchainConfig::default(),
        &runtime,
        semantic_file_tools(),
        &catalog,
        Arc::new(FixtureCodeIntelligence),
        &[],
    )
    .expect("dispatcher");
    let ignored = dispatcher
        .dispatch(serde_json::from_value::<rw_ext::HookInput>(serde_json::json!({"hook":"pre_tool","payload":serde_json::json!({"id":"call","name":"edit","arguments":{"path":"README.md"}})})).expect("typed hook fixture"))
        .await.expect("settled hook");
    assert!(ignored.completed());
    assert!(executor.calls.lock().expect("calls").is_empty());

    let blocked = dispatcher
        .dispatch(serde_json::from_value::<rw_ext::HookInput>(serde_json::json!({"hook":"pre_tool","payload":serde_json::json!({"id":"call","name":"edit","arguments":{"path":"lib.rs"}})})).expect("typed hook fixture"))
        .await.expect("settled hook");
    assert!(matches!(
        blocked.status(),
        rw_ext::HookDispatchStatus::Blocked { hook_id, message }
            if hook_id == "deny-rust-edit" && message.contains("fixture diagnostic")
    ));
    let calls = executor.calls.lock().expect("calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].sandbox, BashSandboxMode::Sandboxed);
}

#[tokio::test]
async fn composition_and_recomposition_retain_inert_wasm_hooks() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = tempdir().expect("fixture");
    let helper = fixture.path().join("validated-helper");
    std::fs::write(&helper, b"validated before generation was retained").expect("helper");
    let manifest = PluginManifest::from_slice(
        br#"{
                "name":"retained-hook",
                "version":"1.0.0",
                "protocol":3,
                "capabilities":{"hooks":[{"name":"pre_tool", "class": "policy","failure_policy":"fail-closed"}]}
            }"#,
    )
    .expect("manifest");
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
        .expect("executable fixture");
    let approved = rw_tools::ApprovedExecutable::from_installed(
        &helper.canonicalize().expect("path"),
        &rw_tools::ExecutableDigest {
            bytes: 40,
            sha256: "79b086bc9698e1dd8158ad2262eada56bb7536a6d01e7822c1c2c1174d39aff6".to_owned(),
        },
    )
    .expect("fixture approval");
    let pool = rw_ext::WasmWorkerPool::new();
    let host = WasmProcessHook::new(
        Arc::clone(&pool),
        approved,
        manifest,
        vec![0],
        WasmHookLimits::default(),
    )
    .expect("proxy");
    let mut initial = HookDispatcher::new();
    let (retained, notices) = super::super::wasm_hooks::register_wasm_hook_proxies(
        &mut initial,
        vec![("retained-hook".to_owned(), host)],
        Vec::new(),
    );
    assert!(notices.is_empty(), "component compilation is deferred");
    assert_eq!(retained.len(), 1);
    assert_eq!(pool.stats().process_starts, 0);
    assert_eq!(pool.stats().component_loads, 0);
    std::fs::remove_file(helper).expect("remove original helper");

    let mut recomposed = HookDispatcher::new();
    register_retained_wasm_hooks(&mut recomposed, &retained)
        .expect("retained generation registers without reloading disk state");
    assert_eq!(recomposed.registrations(HookEvent::PreTool).len(), 1);

    let error = register_retained_wasm_hooks(&mut recomposed, &retained)
        .expect_err("registration conflicts must not be discarded");
    assert!(error.to_string().contains("could not re-register"));
    assert_eq!(pool.stats().process_starts, 0);
    let result = initial
        .dispatch(rw_ext::HookInput::PreTool(
            rw_types::hook_contract::HookToolInput {
                id: "call".to_owned(),
                name: "read".to_owned(),
                arguments: serde_json::json!({}),
            },
        ))
        .await
        .expect("failed helper launch is physically settled");
    assert!(!result.completed(), "first use applies fail-closed policy");
    assert_eq!(result.failures().len(), 1);
    pool.shutdown().await.expect("pool settled");
}

#[test]
fn wasm_startup_notices_strip_terminal_controls_before_persistence() {
    let notice = wasm_startup_notice(
        "wasm:bad\u{1b}[31m\nname",
        "failure\u{7}\r\nwith\u{1b}[2J controls",
    );
    assert_eq!(notice.plugin_id, "wasm:bad[31mname");
    assert_eq!(notice.message, "failurewith[2J controls");
    assert!(!notice.plugin_id.chars().any(char::is_control));
    assert!(!notice.message.chars().any(char::is_control));
}

#[test]
fn declarative_lifecycle_shell_hooks_must_declare_read_only_effect() {
    let fixture = tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    std::fs::create_dir_all(&project).expect("project");
    let project = std::fs::canonicalize(project).expect("canonical project");
    let hooks_path = home.join(".agents/hooks.toml");
    std::fs::create_dir_all(hooks_path.parent().expect("hooks root")).expect("hooks root");
    std::fs::write(
        &hooks_path,
        "[[hook]]\nevent = \"pre_compact\"\nclass = \"policy\"\nmatcher = \"*\"\nrun = \"fixture-shell\"\n\nfailure_policy = \"fail-closed\"\n",
    )
    .expect("mutating lifecycle hook");
    let executor = Arc::new(FixtureToolchainExecutor::default());
    let runtime = Arc::new(ToolchainRuntime::new(
        executor,
        std::slice::from_ref(&project),
    ));
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let mut dispatcher = builtin_hook_dispatcher().expect("dispatcher");
    let error = register_declarative_hooks(&mut dispatcher, &catalog, &runtime)
        .expect_err("mutating lifecycle hook rejected");
    assert!(error.to_string().contains("cannot mutate the workspace"));

    std::fs::write(
            hooks_path,
            "[[hook]]\nevent = \"pre_compact\"\nclass = \"policy\"\nmatcher = \"*\"\neffect = \"read-only\"\nrun = \"fixture-shell\"\n\nfailure_policy = \"fail-closed\"\n",
        )
        .expect("read-only lifecycle hook");
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let mut dispatcher = builtin_hook_dispatcher().expect("dispatcher");
    register_declarative_hooks(&mut dispatcher, &catalog, &runtime)
        .expect("read-only lifecycle hook registers");
}

#[tokio::test]
async fn read_only_shell_hooks_cannot_write_workspace_for_tool_or_lifecycle_events() {
    let fixture = tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let home = fixture.path().join("home");
    let private = fixture.path().join("private");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&private).expect("private");
    let project = std::fs::canonicalize(project).expect("canonical project");
    let target = project.join("target.txt");
    let lifecycle = project.join("lifecycle.txt");
    std::fs::write(&target, "original").expect("target");
    let hooks_path = home.join(".agents/hooks.toml");
    std::fs::create_dir_all(hooks_path.parent().expect("hooks root")).expect("hooks root");
    std::fs::write(
            hooks_path,
            format!(
                "[[hook]]\nid = \"readonly-tool\"\nevent = \"pre_tool\"\nclass = \"policy\"\nmatcher = \"edit(*)\"\neffect = \"read-only\"\nfailure_policy = \"fail-closed\"\nrun = \"printf changed > {}\"\n\n[[hook]]\nid = \"readonly-lifecycle\"\nevent = \"pre_compact\"\nclass = \"policy\"\nmatcher = \"*\"\neffect = \"read-only\"\nfailure_policy = \"fail-closed\"\nrun = \"printf changed > {}\"\n",
                shell_words::quote(&target.to_string_lossy()),
                shell_words::quote(&lifecycle.to_string_lossy())
            ),
        )
        .expect("hooks");
    let lease =
        Arc::new(ExecutionLease::acquire(private.join("execution.lock")).expect("execution lease"));
    let (read_only, scratch) = build_read_only_hook_executor(
        CommandFixtureMode::Live,
        &lease,
        &Arc::new(CommandSafetyClassifier::default()),
    )
    .expect("read-only executor");
    let fixture_executor = Arc::new(FixtureToolchainExecutor::default());
    let runtime = Arc::new(ToolchainRuntime::new_with_read_only(
        fixture_executor.clone(),
        fixture_executor,
        read_only,
        scratch,
        std::slice::from_ref(&project),
    ));
    let catalog = ExtensionCatalog::discover(&ExtensionDiscoveryConfig::new(&project, &home));
    let mut dispatcher = builtin_hook_dispatcher().expect("dispatcher");
    register_declarative_hooks(&mut dispatcher, &catalog, &runtime).expect("hooks register");

    let tool_result = dispatcher
        .dispatch(serde_json::from_value::<rw_ext::HookInput>(serde_json::json!({"hook":"pre_tool","payload":serde_json::json!({"id":"edit","name":"edit","arguments":{"path":"target.txt"}})})).expect("typed hook fixture"))
        .await.expect("settled hook");
    assert!(!tool_result.completed());
    let lifecycle_result = dispatcher
        .dispatch(rw_ext::HookInput::PreCompact(
            rw_types::hook_contract::HookCompactionInput {
                reason: rw_types::CompactionReason::Manual,
                conversation_turns: 1,
                injected_context: vec![],
                replacement_prompt: None,
                suppress_auto_continue: false,
            },
        ))
        .await
        .expect("settled hook");
    assert!(!lifecycle_result.completed());
    assert_eq!(
        std::fs::read_to_string(target).expect("unchanged target"),
        "original"
    );
    assert!(!lifecycle.exists());
}

// Linux must execute this acceptance path from a harness-free binary whose
// entry point dispatches the self-hosted sandbox helper. The equivalent
// coverage lives in rw-tools/tests/linux_command_recording.rs; a libtest
// binary exits on the helper argv before the guarded shell can start.
#[cfg(target_os = "macos")]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn ordinary_and_read_only_hook_commands_record_and_replay_in_distinct_streams() {
    let fixture = tempdir().expect("fixture");
    let project = fixture.path().join("project");
    let private = fixture.path().join("private");
    let recordings = fixture.path().join("recordings");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&private).expect("private");
    let project = std::fs::canonicalize(project).expect("canonical project");
    let lease =
        Arc::new(ExecutionLease::acquire(private.join("execution.lock")).expect("execution lease"));
    let safety = Arc::new(CommandSafetyClassifier::default());
    let record_mode = CommandFixtureMode::Record {
        directory: recordings.clone(),
        redactor: FixtureRedactor::default(),
    };
    let ordinary = build_command_executor(
        std::slice::from_ref(&project),
        &project,
        record_mode.clone(),
        &lease,
        &safety,
        None,
    )
    .expect("ordinary recorder");
    let (read_only, scratch) = build_read_only_hook_executor(record_mode, &lease, &safety)
        .expect("read-only hook recorder");
    let ordinary_request = CommandRequest {
        command: "printf ordinary".to_owned(),
        cwd: project.clone(),
        env: BTreeMap::new(),
        network_domains: Vec::new(),
        sandbox: BashSandboxMode::Sandboxed,
    };
    let hook_request = CommandRequest {
        command: "printf hook".to_owned(),
        cwd: scratch.clone(),
        env: BTreeMap::from([
            ("HOME".to_owned(), scratch.to_string_lossy().into_owned()),
            ("TMPDIR".to_owned(), scratch.to_string_lossy().into_owned()),
        ]),
        network_domains: Vec::new(),
        sandbox: BashSandboxMode::Sandboxed,
    };
    ordinary
        .run(
            ordinary_request.clone(),
            CancellationToken::default(),
            Arc::new(HookCommandCapture::default()),
        )
        .await
        .expect("record ordinary command");
    read_only
        .run(
            hook_request.clone(),
            CancellationToken::default(),
            Arc::new(HookCommandCapture::default()),
        )
        .await
        .expect("record read-only hook command");
    for path in [
        recordings.join("commands.json"),
        recordings
            .join(READ_ONLY_HOOK_COMMAND_FIXTURE_NAMESPACE)
            .join("commands.json"),
    ] {
        let occurrences: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("persisted command fixture"))
                .expect("valid command fixture");
        assert_eq!(occurrences.as_array().map(Vec::len), Some(1));
    }
    drop(ordinary);
    drop(read_only);

    let replay_mode = CommandFixtureMode::Replay {
        directory: recordings,
    };
    let ordinary = build_command_executor(
        std::slice::from_ref(&project),
        &project,
        replay_mode.clone(),
        &lease,
        &safety,
        None,
    )
    .expect("ordinary replay");
    let (read_only, replay_scratch) =
        build_read_only_hook_executor(replay_mode, &lease, &safety).expect("read-only hook replay");
    let mut replay_hook_request = hook_request;
    replay_hook_request.cwd = replay_scratch.clone();
    replay_hook_request.env = BTreeMap::from([
        (
            "HOME".to_owned(),
            replay_scratch.to_string_lossy().into_owned(),
        ),
        (
            "TMPDIR".to_owned(),
            replay_scratch.to_string_lossy().into_owned(),
        ),
    ]);
    ordinary
        .run(
            ordinary_request.clone(),
            CancellationToken::default(),
            Arc::new(HookCommandCapture::default()),
        )
        .await
        .expect("replay ordinary command");
    read_only
        .run(
            replay_hook_request.clone(),
            CancellationToken::default(),
            Arc::new(HookCommandCapture::default()),
        )
        .await
        .expect("replay read-only hook command");
    for (executor, request) in [
        (ordinary, ordinary_request),
        (read_only, replay_hook_request),
    ] {
        let error = executor
            .run(
                request,
                CancellationToken::default(),
                Arc::new(HookCommandCapture::default()),
            )
            .await
            .expect_err("each namespaced occurrence is consumed exactly once");
        assert!(error.to_string().contains("exhausted"));
    }
}
