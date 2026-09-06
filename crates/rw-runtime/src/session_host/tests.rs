use rw_core::EngineEvent;
#![allow(clippy::expect_used)]
use rw_store::session::SessionEventLog;

#[cfg(unix)]
use std::time::Instant;

use rw_core::{ModelAliasDescriptor, ModelCacheBehavior, ModelCapabilities, ModelDescriptor};
use rw_store::session::{SessionProjection, SessionSummary as StoredSessionSummary};
use tempfile::tempdir;

use super::*;
mod command_receipts;
mod fork;
mod workspace;

#[tokio::test]
async fn editable_setting_keys_round_trip_through_one_grammar() {
    for key in [
        "ui.keybindings.preset",
        "project.models.default",
        "ui.theme",
        "models.thinking.fast",
        "compaction.auto",
        "permissions.default",
        "budget.session_cost_cap_micros_usd",
        "budget.daily_cost_cap_micros_usd",
        "budget.warn_at_percent",
        "mcp.servers.docs.enabled",
        "mcp.add_http.docs",
    ] {
        let parsed = EditableSettingKey::parse(key)
            .unwrap_or_else(|| panic!("setting key should parse: {key}"));
        assert_eq!(parsed.render(), key);
    }

    for key in [
        "models.default",
        "models.thinking.",
        "mcp.add_http.",
        "mcp.add_http.has/slash",
        "mcp.servers..enabled",
        "mcp.servers.docs.enabled.extra",
        "mcp.servers.docs.with.dot.enabled",
    ] {
        assert!(EditableSettingKey::parse(key).is_none(), "parsed {key}");
    }
}

#[tokio::test]
async fn setting_descriptors_render_keys_from_the_editable_contract() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    fs::write(
        &user,
        "[models]\ndefault = \"fast\"\n[models.aliases]\nfast = [\"openai/gpt-5-mini\"]\n",
    )
    .expect("user config");
    let loaded = ConfigLoader::new(user, project)
        .load()
        .expect("loaded config");
    let session = SessionDescriptor {
        session_id: SessionId("settings-contract".to_owned()),
        title: "Settings contract".to_owned(),
        workspace_name: "repo".to_owned(),
        model: ModelAlias("fast".to_owned()),
        driver_client_id: None,
        shell_active: false,
    };
    let settings = RuntimeSessionFactory::setting_descriptors(
        &loaded,
        &session,
        Some("openai/gpt-5-mini"),
        "vim",
        &[("docs".to_owned(), true)],
    );

    for descriptor in settings {
        let parsed = EditableSettingKey::parse(&descriptor.key)
            .unwrap_or_else(|| panic!("descriptor key should parse: {}", descriptor.key));
        assert_eq!(parsed.render(), descriptor.key);
    }
}

#[tokio::test]
async fn catalog_current_keeps_selected_alias_and_marks_actual_fallback_route() {
    let capabilities = ModelCapabilities {
        tool_calling: true,
        vision: false,
        thinking: false,
        cache_behavior: ModelCacheBehavior::None,
        max_context_tokens: None,
        max_output_tokens: None,
    };
    let model = |id: &str| ModelDescriptor {
        id: id.to_owned(),
        display_name: id.to_owned(),
        provider: id
            .split_once('/')
            .map_or("", |(provider, _)| provider)
            .to_owned(),
        aliases: vec![ModelAlias("fast".to_owned())],
        current: false,
        available: true,
        status: None,
        capabilities: capabilities.clone(),
    };
    let mut catalog = ModelCatalogSnapshot {
        aliases: vec![ModelAliasDescriptor {
            alias: ModelAlias("fast".to_owned()),
            candidates: vec!["primary/model".to_owned(), "fallback/model".to_owned()],
            current: false,
        }],
        models: vec![model("primary/model"), model("fallback/model")],
        providers: Vec::new(),
        cached: false,
        truncated: false,
    };
    overlay_catalog_current(&mut catalog, Some("fast"), Some("fallback/model"));
    assert!(catalog.aliases[0].current);
    assert!(!catalog.models[0].current);
    assert!(catalog.models[1].current);

    overlay_catalog_current(&mut catalog, Some("primary/model"), Some("fallback/model"));
    assert!(!catalog.aliases[0].current);
    assert!(catalog.models[0].current);
    assert!(!catalog.models[1].current);
}

async fn factory(root: &Path, workspace: &Path) -> RuntimeSessionFactory {
    factory_with_allowed_workspaces(root, vec![workspace.to_path_buf()]).await
}

async fn factory_with_allowed_workspaces(
    root: &Path,
    allowed_workspaces: Vec<PathBuf>,
) -> RuntimeSessionFactory {
    let storage_root = private_test_directory(&root.join("state"));
    RuntimeSessionFactory::new(RuntimeHostOptions {
        credentials_path: storage_root.join("credentials.json"),
        storage_root,
        config: Config::default(),
        allowed_workspaces,
        permission_mode: Some(PermissionMode::Strict),
        max_turns: 2,
        provider_mode: HostedProviderMode::DeterministicReplay {
            provider_name: "offline-host".to_owned(),
            scripts: Vec::new(),
            event_delay_ms: 0,
        },
        dangerously_trust: false,
        wait_for_execution_lease: false,
    })
    .await
    .expect("factory")
}

fn private_test_directory(path: &Path) -> PathBuf {
    fs::create_dir_all(path).expect("private test directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("private test directory permissions");
    }
    fs::canonicalize(path).expect("canonical private test directory")
}

#[tokio::test]
async fn factory_initialization_defers_pricing_catalog_parse() {
    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let storage_root = private_test_directory(&root.path().join("state"));
    fs::write(storage_root.join("models.toml"), "not valid pricing").expect("pricing fixture");

    let factory = RuntimeSessionFactory::new(RuntimeHostOptions {
        credentials_path: storage_root.join("credentials.json"),
        storage_root,
        config: Config::default(),
        allowed_workspaces: vec![workspace],
        permission_mode: Some(PermissionMode::Strict),
        max_turns: 2,
        provider_mode: HostedProviderMode::DeterministicReplay {
            provider_name: "offline-host".to_owned(),
            scripts: Vec::new(),
            event_delay_ms: 0,
        },
        dangerously_trust: false,
        wait_for_execution_lease: false,
    })
    .await
    .expect("readiness must not parse pricing");

    let error = factory
        .model_catalog(true, None, None)
        .await
        .expect_err("the first live catalog lookup must report invalid pricing");
    assert!(error.to_string().contains("invalid pricing table"));
}

#[test]
fn durable_session_queries_tolerate_blocking_pool_scheduling_delay() {
    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let admission_runtime = tokio::runtime::Runtime::new().expect("admission runtime");
    let factory = admission_runtime.block_on(factory(root.path(), &workspace));
    SessionIndex::open(&factory.options.storage_root)
        .and_then(|index| {
            index.upsert(&SessionProjection {
                summary: StoredSessionSummary {
                    id: "scheduling-delay".to_owned(),
                    title: "Scheduling delay".to_owned(),
                    updated_unix_ms: 1,
                    cost_micros: 0,
                    turn_count: 1,
                },
                explicit_title: false,
                complete: true,
                source: rw_store::session::journal::JournalPrefixIdentity::empty(),
            })
        })
        .expect("searchable session index");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_time()
        .build()
        .expect("bounded test runtime");

    runtime.block_on(async move {
        let (started, running) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            std::thread::sleep(Duration::from_millis(250));
        });
        running.await.expect("blocking worker started");
        assert!(
            factory
                .persisted_sessions()
                .await
                .expect("session list after scheduling delay")
                .is_empty()
        );
        blocker.await.expect("first blocker");

        let (started, running) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            std::thread::sleep(Duration::from_millis(250));
        });
        running.await.expect("blocking worker started");
        let (sessions, truncated) = factory
            .search_persisted_sessions("scheduling", 10)
            .await
            .expect("session search after scheduling delay");
        assert!(sessions.is_empty());
        assert!(!truncated);
        blocker.await.expect("second blocker");
    });
}

#[tokio::test]
async fn hosted_create_and_rename_are_immediately_searchable() {
    use rw_core::{
        ClientCommand, ClientId, ClientRole, CommandMeta, CommandOutcome, PROTOCOL_VERSION,
        RequestId,
    };

    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let factory = factory(root.path(), &workspace).await;
    SessionIndex::open(&factory.options.storage_root).expect("empty session index");
    let session_id = SessionId("hosted-search-freshness".to_owned());
    let driver = ClientId("hosted-search-driver".to_owned());
    let hosted = factory
        .create(CreateSessionRequest {
            session_id: session_id.clone(),
            workspace: workspace.display().to_string(),
            model: None,
        })
        .await
        .expect("hosted session");
    let mut events = hosted.handle().subscribe().expect("subscription");
    assert_eq!(
        hosted
            .handle()
            .dispatch(ClientCommand::AttachSession {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: driver.clone(),
                    request_id: RequestId("hosted-search-attach".to_owned()),
                },
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach"),
        CommandOutcome::Accepted {}
    );
    let (created, truncated) = factory
        .search_persisted_sessions("New session", 10)
        .await
        .expect("search created session");
    assert!(!truncated);
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].session_id, session_id);
    assert_eq!(
        hosted
            .handle()
            .dispatch(ClientCommand::RenameSession {
                meta: CommandMeta {
                    protocol_version: PROTOCOL_VERSION,
                    client_id: driver,
                    request_id: RequestId("hosted-search-rename".to_owned()),
                },
                session_id: session_id.clone(),
                title: "Durable Search Rename".to_owned(),
            })
            .await
            .expect("rename"),
        CommandOutcome::Accepted {}
    );
    loop {
        if matches!(
            events.recv().await.expect("rename event"),
            EngineEvent::SessionTitleUpdated { ref title, .. }
                if title == "Durable Search Rename"
        ) {
            break;
        }
    }

    let (matches, truncated) = factory
        .search_persisted_sessions("Durable Search Rename", 10)
        .await
        .expect("search renamed session");
    assert!(!truncated);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].session_id, session_id);
    assert_eq!(matches[0].title, "Durable Search Rename");
}

#[tokio::test]
async fn session_export_uses_cli_renderer_redaction_and_atomic_force_semantics() {
    use rw_core::{EventMeta, PROTOCOL_VERSION, SequenceId};

    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let factory = factory(root.path(), &workspace).await;
    let session = SessionDescriptor {
        session_id: SessionId("golden".to_owned()),
        title: "Golden".to_owned(),
        workspace_name: workspace_name(&workspace),
        model: ModelAlias("fast".to_owned()),
        driver_client_id: Some(rw_core::ClientId("driver".to_owned())),
        shell_active: false,
    };
    let mut log =
        SessionEventLog::open(&factory.options.storage_root, "golden").expect("session event log");
    log.append(EngineEvent::UiNotification {
        meta: EventMeta {
            protocol_version: PROTOCOL_VERSION,
            session_id: session.session_id.clone(),
            sequence_id: SequenceId(0),
            emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            caused_by: None,
        },
        plugin_id: "fixture".to_owned(),
        title: "<script>alert(1)</script>".to_owned(),
        message: "key sk-AbCdEf0123456789GhIjKlMn at /Users/alice/private".to_owned(),
    })
    .expect("fixture event");
    drop(log);

    let output_dir = tempdir().expect("output");
    let output = output_dir.path().join("transcript.md");
    let resolved = factory
        .export_session_blocking(&session, TranscriptFormat::Markdown, &output, false)
        .expect("first export");
    assert_eq!(
        resolved,
        fs::canonicalize(output_dir.path())
            .expect("canonical output")
            .join("transcript.md")
            .display()
            .to_string()
    );
    assert_eq!(
        fs::read(&output).expect("exported transcript"),
        include_bytes!("../../tests/golden/history.md")
    );
    let rendered = fs::read_to_string(&output).expect("UTF-8 transcript");
    assert!(!rendered.contains("sk-AbCd"));
    assert!(!rendered.contains("/Users/alice"));

    let error = factory
        .export_session_blocking(&session, TranscriptFormat::Markdown, &output, false)
        .expect_err("existing output requires force");
    assert!(error.to_string().contains("pass --force"));
    fs::write(&output, b"replace me").expect("replacement canary");
    factory
        .export_session_blocking(&session, TranscriptFormat::Markdown, &output, true)
        .expect("forced export");
    assert_eq!(
        fs::read(&output).expect("forced transcript"),
        include_bytes!("../../tests/golden/history.md")
    );

    assert!(
        factory
            .export_session_blocking(&session, TranscriptFormat::Json, Path::new("/"), false,)
            .is_err()
    );
}

#[cfg(unix)]
fn git(workspace: &Path, arguments: &[&str]) {
    assert!(
        Command::new("git")
            .current_dir(workspace)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git fixture command")
            .success(),
        "git {arguments:?}"
    );
}

#[tokio::test]
async fn model_descriptors_expose_extension_provider_names_in_fallback_order() {
    let providers = configured_alias_providers(&[
        "openai-work/gpt-5".to_owned(),
        "extension-provider/model".to_owned(),
        "copilot/gpt-4.1".to_owned(),
        "openai-work/gpt-4.1".to_owned(),
        "malformed".to_owned(),
        "/missing-provider".to_owned(),
        "bad\nprovider/model".to_owned(),
        format!("{}/model", "x".repeat(MAX_PROVIDER_DISPLAY_NAME_BYTES + 1)),
    ]);
    assert_eq!(providers, ["openai-work", "extension-provider", "copilot"]);
}

#[tokio::test]
async fn create_persists_remote_safe_descriptor_and_resume_recovers_exact_identity() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join("needle.rs"), "fn needle() {}\n").expect("query fixture");
    let factory = factory(root.path(), &workspace).await;
    let session_id = SessionId("session-create-resume".to_owned());
    let created = factory
        .create(CreateSessionRequest {
            session_id: session_id.clone(),
            workspace: workspace.display().to_string(),
            model: None,
        })
        .await
        .expect("create");
    assert_eq!(created.descriptor().session_id, session_id);
    assert!(!created.descriptor().workspace_name.contains('/'));
    let (matches, truncated) = factory
        .search_workspace_files(&created.descriptor(), "needle", 10)
        .await
        .expect("search");
    assert!(!truncated);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].path, "needle.rs");
    let preview = factory
        .preview_workspace_file(&created.descriptor(), "needle.rs", 1024)
        .await
        .expect("preview");
    assert_eq!(
        preview.data,
        AttachmentData::Text {
            content: "fn needle() {}\n".to_owned()
        }
    );
    fs::write(
        workspace.join("screen shot.png"),
        b"\x89PNG\r\n\x1a\nattachment bytes",
    )
    .expect("image fixture");
    let preview = factory
        .preview_workspace_file(&created.descriptor(), "screen shot.png", 1024)
        .await
        .expect("image preview");
    assert_eq!(preview.media_type, "image/png");
    assert!(matches!(preview.data, AttachmentData::InlineBase64 { .. }));
    drop(created);
    tokio::task::yield_now().await;
    let resumed = factory.resume(&session_id).await.expect("resume");
    assert_eq!(resumed.descriptor().session_id, session_id);
    assert_eq!(resumed.descriptor().workspace_name, "workspace");
}

#[tokio::test]
async fn hosted_add_dir_enforces_allowed_roots_before_generation_or_tool_access() {
    let root = tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let allowed = root.path().join("allowed");
    let outside = root.path().join("outside");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&allowed).expect("allowed");
    fs::create_dir_all(&outside).expect("outside");
    fs::write(outside.join("OUTSIDE_CANARY.txt"), "outside").expect("outside canary");
    let workspace = fs::canonicalize(workspace).expect("canonical workspace");
    let allowed = fs::canonicalize(allowed).expect("canonical allowed");
    let outside = fs::canonicalize(outside).expect("canonical outside");
    let factory =
        factory_with_allowed_workspaces(root.path(), vec![workspace.clone(), allowed.clone()])
            .await;
    let session_id = SessionId("hosted-add-root-policy".to_owned());
    let hosted = factory
        .create(CreateSessionRequest {
            session_id: session_id.clone(),
            workspace: workspace.display().to_string(),
            model: None,
        })
        .await
        .expect("create hosted session");
    let handle = hosted.handle();

    let denied = handle
        .send_message(format!("/add-dir {}", outside.display()))
        .await
        .expect_err("outside root must be denied");
    assert!(denied.to_string().contains("authorization policy"));
    let unchanged = handle.snapshot().await.expect("unchanged snapshot");
    assert_eq!(unchanged.workspace_generation, 0);
    assert_eq!(unchanged.workspace_roots.len(), 1);
    assert_eq!(
        factory
            .workspace_roots_for_session(&hosted.descriptor())
            .expect("host roots after denial"),
        vec![workspace.clone()]
    );
    assert!(
        factory
            .preview_workspace_file(&hosted.descriptor(), "@root/1/OUTSIDE_CANARY.txt", 1024,)
            .await
            .is_err(),
        "denied root must not become queryable through hosted tool paths"
    );

    let allowed_session_id = SessionId("hosted-add-root-allowed".to_owned());
    let allowed_hosted = factory
        .create(CreateSessionRequest {
            session_id: allowed_session_id,
            workspace: workspace.display().to_string(),
            model: None,
        })
        .await
        .expect("create allowed-root session");
    let allowed_handle = allowed_hosted.handle();
    allowed_handle
        .send_message(format!("/add-dir {}", allowed.display()))
        .await
        .expect("configured allowed root");
    let changed = allowed_handle.snapshot().await.expect("changed snapshot");
    assert_eq!(changed.workspace_generation, 1);
    assert_eq!(changed.workspace_roots.len(), 2);
    assert_eq!(
        factory
            .workspace_roots_for_session(&allowed_hosted.descriptor())
            .expect("host roots after allowed add"),
        vec![workspace, allowed]
    );
    assert!(!outside.join("created-by-tool.txt").exists());
}

#[tokio::test]
async fn thinking_setting_uses_configured_alias_after_concrete_model_selection() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    fs::write(
        &user,
        "[models]\ndefault = \"fast\"\n[models.aliases]\nfast = [\"openai/gpt-5-mini\"]\n",
    )
    .expect("user config");
    let loaded = ConfigLoader::new(user, project)
        .load()
        .expect("loaded config");
    let session = SessionDescriptor {
        session_id: SessionId("concrete".to_owned()),
        title: "Concrete model".to_owned(),
        workspace_name: "repo".to_owned(),
        model: ModelAlias("openai/gpt-5-mini".to_owned()),
        driver_client_id: None,
        shell_active: false,
    };

    let settings =
        RuntimeSessionFactory::setting_descriptors(&loaded, &session, None, "standard", &[]);

    assert!(
        settings
            .iter()
            .any(|setting| setting.key == "models.thinking.fast")
    );
    assert!(
        settings
            .iter()
            .all(|setting| !setting.key.contains("openai/gpt-5-mini"))
    );
}

#[tokio::test]
async fn theme_setting_leaves_choices_to_the_tui_theme_catalog() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let loaded = ConfigLoader::new(user, project)
        .load()
        .expect("loaded config");
    let session = SessionDescriptor {
        session_id: SessionId("theme-settings".to_owned()),
        title: "Theme settings".to_owned(),
        workspace_name: "repo".to_owned(),
        model: ModelAlias("fast".to_owned()),
        driver_client_id: None,
        shell_active: false,
    };

    let settings =
        RuntimeSessionFactory::setting_descriptors(&loaded, &session, None, "standard", &[]);
    let theme = settings
        .iter()
        .find(|setting| setting.key == "ui.theme")
        .expect("theme setting");

    assert!(theme.choices.is_empty());
}

#[tokio::test]
async fn budget_setting_descriptors_format_human_values_without_choices() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let mut loaded = ConfigLoader::new(user, project)
        .load()
        .expect("loaded config");
    loaded.config.budget.session_cost_cap_micros_usd = Some(12_500_000);
    loaded.config.budget.daily_cost_cap_micros_usd = None;
    loaded.config.budget.warn_at_percent = 80;
    let session = SessionDescriptor {
        session_id: SessionId("budget-settings".to_owned()),
        title: "Budget settings".to_owned(),
        workspace_name: "repo".to_owned(),
        model: ModelAlias("fast".to_owned()),
        driver_client_id: None,
        shell_active: false,
    };

    let settings =
        RuntimeSessionFactory::setting_descriptors(&loaded, &session, None, "standard", &[]);
    let descriptor = |key: &str| {
        settings
            .iter()
            .find(|setting| setting.key == key)
            .unwrap_or_else(|| panic!("missing descriptor {key}"))
    };

    assert_eq!(
        descriptor("budget.session_cost_cap_micros_usd").value,
        "$12.50"
    );
    assert_eq!(
        descriptor("budget.daily_cost_cap_micros_usd").value,
        "Unlimited"
    );
    assert_eq!(descriptor("budget.warn_at_percent").value, "80%");
    for key in [
        "budget.session_cost_cap_micros_usd",
        "budget.daily_cost_cap_micros_usd",
        "budget.warn_at_percent",
    ] {
        assert!(descriptor(key).choices.is_empty());
        assert!(!descriptor(key).applies_immediately);
    }
}

#[tokio::test]
async fn project_model_preferences_are_isolated_by_the_session_workspace() {
    let root = tempdir().expect("root");
    let first = private_test_directory(&root.path().join("first"));
    let second = private_test_directory(&root.path().join("second"));
    let factory =
        factory_with_allowed_workspaces(root.path(), vec![first.clone(), second.clone()]).await;

    factory
        .settings_loader_for(&first)
        .persist_tui_project_model("openai/first")
        .expect("first preference");
    factory
        .settings_loader_for(&second)
        .persist_tui_project_model("openai/second")
        .expect("second preference");

    assert_eq!(
        factory
            .settings_loader_for(&first)
            .tui_project_model()
            .expect("first")
            .as_deref(),
        Some("openai/first")
    );
    assert_eq!(
        factory
            .settings_loader_for(&second)
            .tui_project_model()
            .expect("second")
            .as_deref(),
        Some("openai/second")
    );
}

#[tokio::test]
async fn fresh_factory_uses_the_persisted_project_model_without_catalog_interaction() {
    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let first = factory(root.path(), &workspace).await;
    first
        .settings_loader_for(&workspace)
        .persist_tui_project_model("openai_codex/gpt-5.6-sol")
        .expect("persist selected model");
    drop(first);

    let restarted = factory(root.path(), &workspace).await;
    assert_eq!(
        restarted
            .requested_model_for_compose(&workspace, None, false)
            .expect("load the restart selection")
            .as_deref(),
        Some("openai_codex/gpt-5.6-sol")
    );
}

#[tokio::test]
async fn resume_ignores_a_corrupt_project_model_preference() {
    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let factory = factory(root.path(), &workspace).await;
    let preference = factory
        .options
        .credentials_path
        .with_file_name("project-model-preferences.json");
    fs::write(&preference, "not-json").expect("corrupt preference");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&preference, fs::Permissions::from_mode(0o600))
            .expect("private corrupt preference");
    }

    assert_eq!(
        factory
            .requested_model_for_compose(&workspace, None, true)
            .expect("resume ignores preference"),
        None
    );
    assert!(
        factory
            .requested_model_for_compose(&workspace, None, false)
            .is_err()
    );
}
