use super::{render_session_search_text, sync_install_paths};
use crate::auth_cli::write_github_device_prompt;
use crate::cli_args::{
    Cli, Command, McpServerCommand, ModelsCommand, OutputFormat, PromptCommand, UpgradeChannel,
    scripted_provider_options, validate_cli_option_scope,
};
#[cfg(unix)]
use crate::interactive::{DeferredHostedEngine, discover_local_workspace};
#[cfg(unix)]
use crate::remote_session::{DetachedServerReady, append_execution_lease_restart_flag};
#[cfg(unix)]
use crate::runtime_paths::{
    MAX_UNIX_SOCKET_PATH_BYTES, RuntimeDirectoryGuard, create_guarded_server_runtime,
    resolve_tui_executable, runtime_root, rustix_device_id, rustix_mode_bits,
    valid_bootstrap_token, write_private_file_atomic,
};
use crate::trust_cli::ensure_folder_trust_grantable;
use std::path::PathBuf;

use async_trait::async_trait;
use clap::{CommandFactory as _, Parser as _};

#[test]
fn recent_session_text_labels_dates_turns_and_ids_without_fake_cost() {
    let sessions = [rw_store::session::SessionSummary {
        id: "session-fixture".to_owned(),
        title: "Investigate startup".to_owned(),
        updated_unix_ms: 1_776_508_645_000,
        cost_micros: 0,
        turn_count: 3,
    }];

    assert_eq!(
        render_session_search_text(&sessions)
            .unwrap_or_else(|error| panic!("text must render: {error}")),
        "UPDATED (UTC)\tTURNS\tTITLE\tSESSION\n2026-04-18 10:37\t3\tInvestigate startup\tsession-fixture\n"
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::expect_used)]
fn trust_grant_path_refuses_uninventoriable_project() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    let offending = workspace.join(".agents/commands/foo.md");
    std::fs::create_dir_all(offending.parent().expect("commands")).expect("commands");
    std::fs::write(root.path().join("outside.md"), "outside").expect("outside");
    symlink(root.path().join("outside.md"), &offending).expect("symlink");
    let ledger = root.path().join("private/trust.json");
    let store = rw_store::trust::FolderTrustStore::new(ledger.clone());
    let assessment = store.assess(&workspace).expect("assessment");
    let offending = assessment.workspace().join(".agents/commands/foo.md");

    let error = ensure_folder_trust_grantable(&assessment).expect_err("grant refusal");
    assert!(error.to_string().contains("inventory is incomplete"));
    assert!(error.to_string().contains(&offending.display().to_string()));
    assert!(!ledger.exists());
}

#[derive(Default)]
struct ProviderMutationProbe {
    activations: std::sync::Mutex<Vec<(rw_core::ClientId, rw_core::SessionId, String)>>,
    api_keys: std::sync::Mutex<Vec<(rw_core::ClientId, rw_core::SessionId, String, String)>>,
}

#[async_trait]
impl crate::server::ServerEngine for ProviderMutationProbe {
    async fn dispatch(
        &self,
        _bound_client: rw_core::ClientId,
        _command: rw_core::ClientCommand,
    ) -> std::result::Result<rw_core::HostReply, String> {
        Ok(rw_core::HostReply::command(
            rw_core::CommandOutcome::Accepted {},
        ))
    }

    async fn subscribe(
        &self,
        _bound_client: rw_core::ClientId,
        _session_id: Option<rw_core::SessionId>,
        _last_seen: Option<rw_core::SequenceId>,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<rw_core::HostEvent, String>>,
        crate::server::EventSubscriptionError,
    > {
        let (_send, receive) = tokio::sync::mpsc::channel(1);
        Ok(receive)
    }

    async fn complete_shell(
        &self,
        _session_id: rw_core::SessionId,
        _shell_id: rw_core::ShellId,
        _status: i32,
        _captured_output: Option<String>,
    ) -> std::result::Result<(), String> {
        Ok(())
    }

    async fn submit_provider_api_key(
        &self,
        bound_client: rw_core::ClientId,
        session_id: rw_core::SessionId,
        provider: String,
        api_key: rw_core::ProviderApiKey,
    ) -> std::result::Result<rw_core::ProviderApiKeySubmission, String> {
        self.api_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                bound_client,
                session_id,
                provider,
                api_key.expose_secret().to_owned(),
            ));
        Ok(rw_core::ProviderApiKeySubmission {
            stored: true,
            activated: true,
            warnings: Vec::new(),
        })
    }

    async fn activate_provider(
        &self,
        bound_client: rw_core::ClientId,
        session_id: rw_core::SessionId,
        provider: String,
    ) -> std::result::Result<(), String> {
        self.activations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((bound_client, session_id, provider));
        Ok(())
    }
}

#[tokio::test]
async fn deferred_engine_forwards_provider_mutations_after_readiness() {
    use crate::server::ServerEngine as _;

    let deferred = DeferredHostedEngine::default();
    let client = rw_core::ClientId("provider-forwarding-client".to_owned());
    let session = rw_core::SessionId("provider-forwarding-session".to_owned());
    let before_install = match deferred
        .activate_provider(client.clone(), session.clone(), "github_copilot".to_owned())
        .await
    {
        Ok(()) => panic!("provider activation must fail before the engine is installed"),
        Err(error) => error,
    };
    assert!(before_install.contains("still starting"));
    let probe = std::sync::Arc::new(ProviderMutationProbe::default());
    deferred.install_engine(probe.clone());
    deferred
        .activate_provider(client.clone(), session.clone(), "github_copilot".to_owned())
        .await
        .unwrap_or_else(|error| {
            panic!("provider activation must reach the loaded engine: {error}")
        });
    let submission = deferred
        .submit_provider_api_key(
            client.clone(),
            session.clone(),
            "openai".to_owned(),
            rw_core::ProviderApiKey::from_terminal_input("test-only-key".to_owned())
                .unwrap_or_else(|error| panic!("test key must validate: {error}")),
        )
        .await
        .unwrap_or_else(|error| panic!("API-key submission must reach the loaded engine: {error}"));
    assert!(submission.stored);
    assert!(submission.activated);
    assert_eq!(
        *probe
            .activations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [(client.clone(), session.clone(), "github_copilot".to_owned())]
    );
    assert_eq!(
        *probe
            .api_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [(
            client,
            session,
            "openai".to_owned(),
            "test-only-key".to_owned()
        )]
    );
}

#[test]
fn detached_recovery_threads_the_execution_lease_wait_flag_to_the_real_child() {
    let mut command = tokio::process::Command::new("rw");
    command.arg("serve");
    append_execution_lease_restart_flag(&mut command, true);
    assert!(
        command
            .as_std()
            .get_args()
            .any(|argument| argument == "--wait-for-execution-lease")
    );
}

#[test]
fn long_configuration_roots_use_a_short_runtime_socket_root() {
    let storage_root = PathBuf::from(format!("/tmp/{}", "long-home-segment-".repeat(12)));
    let root = runtime_root(&storage_root);
    let socket = root.join("engine-0000000000000000").join("engine.sock");
    assert_ne!(root, storage_root.join("run"));
    assert!(socket.as_os_str().as_encoded_bytes().len() <= MAX_UNIX_SOCKET_PATH_BYTES);
}

#[cfg(unix)]
#[test]
fn unix_identity_helpers_preserve_signed_failure_and_lossless_mode_widening() {
    assert_eq!(rustix_device_id(-1_i32), None);
    assert_eq!(rustix_device_id(41_i32), Some(41));
    assert_eq!(rustix_device_id(42_u64), Some(42));
    assert_eq!(rustix_mode_bits(0o755_u16), 0o755);
    assert_eq!(rustix_mode_bits(0o700_u32), 0o700);
}

#[test]
fn detached_readiness_requires_explicit_process_ownership() -> Result<(), serde_json::Error> {
    let started: DetachedServerReady = serde_json::from_value(serde_json::json!({
        "version": 1,
        "token": "a".repeat(64),
        "session_id": "session",
        "started": true,
    }))?;
    assert!(started.started);

    let pre_existing: DetachedServerReady = serde_json::from_value(serde_json::json!({
        "version": 1,
        "token": "b".repeat(64),
        "session_id": "session",
        "started": false,
    }))?;
    assert!(!pre_existing.started);
    let descriptor = serde_json::json!({
        "version": 1,
        "token": "c".repeat(64),
        "session_id": "session",
        "started": false,
    });
    for field in ["version", "token", "session_id", "started"] {
        let mut incomplete = descriptor.clone();
        incomplete
            .as_object_mut()
            .expect("descriptor")
            .remove(field);
        assert!(serde_json::from_value::<DetachedServerReady>(incomplete).is_err());
    }
    let mut unknown = descriptor;
    unknown["extra"] = serde_json::json!(false);
    assert!(serde_json::from_value::<DetachedServerReady>(unknown).is_err());
    Ok(())
}

#[test]
fn cli_definition_is_internally_consistent() {
    Cli::command().debug_assert();
}

#[test]
fn run_only_flags_are_not_accepted_after_a_subcommand() {
    assert!(
        Cli::try_parse_from(["rw", "trust", "status", "--detach"]).is_err(),
        "subcommand help must not advertise or accept run-only flags"
    );
}

#[test]
fn run_only_flags_before_a_subcommand_are_rejected_by_scope_validation() {
    let cli = Cli::try_parse_from(["rw", "--add-dir", "/work/second", "trust", "status"])
        .unwrap_or_else(|error| panic!("CLI should parse: {error}"));
    let error = validate_cli_option_scope(&cli)
        .err()
        .unwrap_or_else(|| panic!("scope validation must reject --add-dir"));
    assert!(error.to_string().contains("--add-dir"));

    let explicit_defaults = Cli::try_parse_from([
        "rw",
        "--output-format",
        "text",
        "--max-turns",
        "32",
        "trust",
        "status",
    ])
    .unwrap_or_else(|error| panic!("CLI should parse: {error}"));
    let error = validate_cli_option_scope(&explicit_defaults)
        .err()
        .unwrap_or_else(|| panic!("scope validation must reject explicit defaults"));
    assert!(error.to_string().contains("--output-format"));
    assert!(error.to_string().contains("--max-turns"));
}

#[test]
fn output_format_cannot_be_silently_ignored_by_the_interactive_tui() {
    let cli = Cli::try_parse_from(["rw", "--output-format", "json"])
        .unwrap_or_else(|error| panic!("CLI should parse before semantic validation: {error}"));
    let error = validate_cli_option_scope(&cli)
        .err()
        .unwrap_or_else(|| panic!("scope validation must reject ignored output"));
    assert!(error.to_string().contains("--output-format"));

    let print = Cli::try_parse_from(["rw", "--output-format", "stream-json", "-p", "hi"])
        .unwrap_or_else(|error| panic!("print CLI should parse: {error}"));
    validate_cli_option_scope(&print)
        .unwrap_or_else(|error| panic!("print mode consumes output format: {error}"));
}

#[test]
fn engine_subcommands_accept_only_the_shared_options_they_consume() {
    let prompt = Cli::try_parse_from([
        "rw",
        "--max-turns",
        "4",
        "--model",
        "fast",
        "prompt",
        "dump",
    ])
    .unwrap_or_else(|error| panic!("CLI should parse: {error}"));
    validate_cli_option_scope(&prompt)
        .unwrap_or_else(|error| panic!("prompt consumes engine options: {error}"));

    let prompt = Cli::try_parse_from([
        "rw",
        "prompt",
        "--max-turns",
        "5",
        "dump",
        "--model",
        "fast",
        "--output-format",
        "stream-json",
    ])
    .unwrap_or_else(|error| panic!("prompt options should remain global in its subtree: {error}"));
    assert!(matches!(
        prompt.command,
        Some(Command::Prompt {
            options,
            command: PromptCommand::Dump { .. },
        }) if options.engine.max_turns == Some(5)
            && options.model.as_deref() == Some("fast")
            && options.output_format == Some(OutputFormat::StreamJson)
    ));

    let mcp_server = Cli::try_parse_from([
        "rw",
        "mcp-server",
        "stdio",
        "--permission-mode",
        "auto-safe",
        "--add-dir",
        "/work/second",
    ])
    .unwrap_or_else(|error| panic!("MCP engine options should parse after stdio: {error}"));
    assert!(matches!(
        mcp_server.command,
        Some(Command::McpServer {
            engine,
            command: McpServerCommand::Stdio { .. },
            ..
        }) if engine.permission_mode == Some(super::PermissionMode::AutoSafe)
            && engine.add_dirs == [std::path::PathBuf::from("/work/second")]
    ));

    let serve = Cli::try_parse_from([
        "rw",
        "serve",
        "--max-turns",
        "6",
        "--model",
        "balanced",
        "--detach",
    ])
    .unwrap_or_else(|error| panic!("serve options should parse after subcommand: {error}"));
    assert!(matches!(
        serve.command,
        Some(Command::Serve {
            engine,
            model: Some(model),
            detach: true,
            ..
        }) if engine.max_turns == Some(6) && model == "balanced"
    ));

    let models = Cli::try_parse_from(["rw", "models", "list", "--output-format", "stream-json"])
        .unwrap_or_else(|error| panic!("model output option should parse at its leaf: {error}"));
    assert!(matches!(
        models.command,
        Some(Command::Models {
            command: ModelsCommand::List { output, .. },
        }) if output.output_format == Some(OutputFormat::StreamJson)
    ));

    let trust = Cli::try_parse_from(["rw", "--model", "fast", "trust", "status"])
        .unwrap_or_else(|error| panic!("CLI should parse: {error}"));
    let error = validate_cli_option_scope(&trust)
        .err()
        .unwrap_or_else(|| panic!("model must be rejected for trust"));
    assert!(error.to_string().contains("--model"));
}

#[test]
fn scripted_provider_options_merge_across_command_placement() {
    for argv in [
        vec![
            "rw",
            "--in-memory-replay-script",
            "fixture.json",
            "serve",
            "--record-script-delay-ms",
            "7",
        ],
        vec![
            "rw",
            "--record-script-delay-ms",
            "7",
            "serve",
            "--in-memory-replay-script",
            "fixture.json",
        ],
    ] {
        let cli = Cli::try_parse_from(argv)
            .unwrap_or_else(|error| panic!("cross-placement options should parse: {error}"));
        let root_script = cli.in_memory_replay_script;
        let root_delay = cli.record_script_delay_ms;
        let Some(Command::Serve {
            scripted_provider, ..
        }) = cli.command
        else {
            panic!("serve command should parse");
        };
        let (script, delay) = scripted_provider_options(
            root_script,
            scripted_provider.in_memory_replay_script,
            root_delay,
            scripted_provider.record_script_delay_ms,
        )
        .unwrap_or_else(|error| panic!("cross-placement options should merge: {error}"));
        assert_eq!(script, Some(std::path::PathBuf::from("fixture.json")));
        assert_eq!(delay, 7);
    }
}

#[test]
fn duplicate_scalar_placement_rejects_and_workspace_roots_accumulate() {
    let cli = Cli::try_parse_from([
        "rw",
        "--max-turns",
        "4",
        "--add-dir",
        "/work/root",
        "serve",
        "--max-turns",
        "5",
        "--add-dir",
        "/work/serve",
    ])
    .unwrap_or_else(|error| panic!("placements should parse before merge: {error}"));
    let Some(Command::Serve { engine, .. }) = cli.command else {
        panic!("serve command should parse");
    };
    let error = super::max_turns(cli.max_turns, engine.max_turns)
        .err()
        .unwrap_or_else(|| panic!("duplicate scalar placement must reject"));
    assert!(error.to_string().contains("both before and after"));
    assert_eq!(
        super::merge_workspace_roots(cli.add_dirs, engine.add_dirs),
        [
            std::path::PathBuf::from("/work/root"),
            std::path::PathBuf::from("/work/serve"),
        ]
    );

    let prompt = Cli::try_parse_from([
        "rw",
        "--output-format",
        "json",
        "prompt",
        "--output-format",
        "text",
        "dump",
    ])
    .unwrap_or_else(|error| panic!("output placements should parse before merge: {error}"));
    let Some(Command::Prompt { options, .. }) = prompt.command else {
        panic!("prompt command should parse");
    };
    super::output_format(prompt.output_format, options.output_format)
        .err()
        .unwrap_or_else(|| panic!("duplicate output format must reject"));
}

#[test]
fn stats_accepts_session_utc_range_and_json_output() {
    let cli = Cli::try_parse_from([
        "rw",
        "stats",
        "--session",
        "session-1",
        "--from",
        "2026-07-01",
        "--to",
        "2026-07-31",
        "--json",
    ])
    .unwrap_or_else(|error| panic!("stats CLI should parse: {error}"));
    assert!(matches!(
        cli.command,
        Some(Command::Stats {
            session: Some(ref session),
            from: Some(ref from),
            through: Some(ref through),
            json: true,
            ..
        }) if session == "session-1" && from == "2026-07-01" && through == "2026-07-31"
    ));
}

#[test]
fn doctor_network_probe_is_explicit_and_bounded() {
    let cli = Cli::try_parse_from(["rw", "doctor", "--network", "--timeout-ms", "750", "--json"])
        .unwrap_or_else(|error| panic!("doctor CLI should parse: {error}"));
    assert!(matches!(
        cli.command,
        Some(Command::Doctor {
            network: true,
            timeout_ms: 750,
            json: true,
            ..
        })
    ));
}

#[test]
fn upgrade_channel_and_downgrade_policy_are_explicit() {
    let cli = Cli::try_parse_from([
        "rw",
        "upgrade",
        "--channel",
        "beta",
        "--allow-downgrade",
        "--timeout-ms",
        "5000",
    ])
    .unwrap_or_else(|error| panic!("upgrade CLI should parse: {error}"));
    assert!(matches!(
        cli.command,
        Some(Command::Upgrade {
            channel: Some(UpgradeChannel::Beta),
            allow_downgrade: true,
            rollback: false,
            timeout_ms: 5_000,
        })
    ));
    assert!(Cli::try_parse_from(["rw", "update"]).is_err());
}

#[test]
fn installer_sync_flushes_files_and_directories_without_following_links() {
    let root =
        tempfile::tempdir().unwrap_or_else(|error| panic!("sync root should be created: {error}"));
    let file = root.path().join("runtime");
    std::fs::write(&file, b"runtime")
        .unwrap_or_else(|error| panic!("runtime fixture should be written: {error}"));
    sync_install_paths(&[file.clone(), root.path().to_owned()])
        .unwrap_or_else(|error| panic!("durability sync should succeed: {error}"));
    let link = root.path().join("runtime-link");
    std::os::unix::fs::symlink(&file, &link)
        .unwrap_or_else(|error| panic!("link fixture should be created: {error}"));
    assert!(sync_install_paths(&[link]).is_err());
}

#[test]
fn tui_resolution_follows_public_launcher_to_private_runtime_sibling() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
    let private = root.path().join("Cellar/rottweiler/1.2.3/libexec");
    let public = root.path().join("bin");
    std::fs::create_dir_all(&private)
        .unwrap_or_else(|error| panic!("private runtime must exist: {error}"));
    std::fs::create_dir_all(&public)
        .unwrap_or_else(|error| panic!("public bin must exist: {error}"));
    let rw = private.join("rw");
    let tui = private.join("rottweiler-tui");
    for executable in [&rw, &tui] {
        std::fs::write(executable, b"fixture")
            .unwrap_or_else(|error| panic!("executable fixture must exist: {error}"));
        std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("fixture must be executable: {error}"));
    }
    let launcher = public.join("rw");
    symlink(&rw, &launcher)
        .unwrap_or_else(|error| panic!("public launcher symlink must exist: {error}"));

    assert_eq!(
        resolve_tui_executable(&launcher, None, &root.path().join("missing"))
            .unwrap_or_else(|error| panic!("TUI sibling must resolve: {error}")),
        std::fs::canonicalize(&tui)
            .unwrap_or_else(|error| panic!("TUI sibling must canonicalize: {error}"))
    );

    let override_path = root.path().join("test-tui");
    std::fs::write(&override_path, b"override")
        .unwrap_or_else(|error| panic!("override fixture must exist: {error}"));
    std::fs::set_permissions(&override_path, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("override must be executable: {error}"));
    assert_eq!(
        resolve_tui_executable(&launcher, Some(override_path.clone()), &tui)
            .unwrap_or_else(|error| panic!("explicit override must win: {error}")),
        override_path
    );
}

#[test]
fn owned_runtime_cleanup_removes_only_known_private_artifacts() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
    let runtime = root.path().join("engine-fixture");
    std::fs::create_dir(&runtime)
        .unwrap_or_else(|error| panic!("runtime directory must exist: {error}"));
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("runtime directory must be private: {error}"));
    for name in ["auth.token", "runtime.json", "last-seen"] {
        std::fs::write(runtime.join(name), b"fixture")
            .unwrap_or_else(|error| panic!("runtime artifact must exist: {error}"));
    }
    let listener = std::os::unix::net::UnixListener::bind(runtime.join("engine.sock"))
        .unwrap_or_else(|error| panic!("runtime socket must bind: {error}"));
    let mut guard = RuntimeDirectoryGuard::capture(&runtime)
        .unwrap_or_else(|error| panic!("runtime guard must capture: {error}"));
    drop(listener);
    guard
        .cleanup()
        .unwrap_or_else(|error| panic!("known runtime artifacts must clean: {error}"));
    assert!(!runtime.exists());
}

#[test]
fn guarded_server_creates_a_missing_selected_runtime_before_capture() {
    let root = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
    let directory = root.path().join("remote/engine-fixture");
    let paths = crate::server::ServerRuntimePaths {
        socket: directory.join("engine.sock"),
        token: directory.join("auth.token"),
        descriptor: directory.join("runtime.json"),
        directory: directory.clone(),
    };
    let (mut guard, runtime, listener) =
        create_guarded_server_runtime(paths, Some("remote-session"))
            .unwrap_or_else(|error| panic!("missing selected runtime must start: {error}"));
    assert!(directory.is_dir());
    drop(listener);
    drop(runtime);
    guard
        .cleanup()
        .unwrap_or_else(|error| panic!("created runtime must clean: {error}"));
    assert!(!directory.exists());
}

#[test]
fn owned_runtime_cleanup_refuses_unexpected_or_replaced_directory() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
    let runtime = root.path().join("engine-fixture");
    std::fs::create_dir(&runtime)
        .unwrap_or_else(|error| panic!("runtime directory must exist: {error}"));
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("runtime directory must be private: {error}"));
    std::fs::write(runtime.join("unexpected"), b"keep")
        .unwrap_or_else(|error| panic!("unexpected fixture must exist: {error}"));
    let mut unexpected = RuntimeDirectoryGuard::capture(&runtime)
        .unwrap_or_else(|error| panic!("runtime guard must capture: {error}"));
    assert!(unexpected.cleanup().is_err());
    unexpected.preserve();
    assert!(runtime.join("unexpected").is_file());

    let replacement = root.path().join("engine-replacement");
    std::fs::create_dir(&replacement)
        .unwrap_or_else(|error| panic!("replacement directory must exist: {error}"));
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("replacement directory must be private: {error}"));
    let mut replaced = RuntimeDirectoryGuard::capture(&replacement)
        .unwrap_or_else(|error| panic!("replacement guard must capture: {error}"));
    let moved = root.path().join("moved-original");
    std::fs::rename(&replacement, &moved)
        .unwrap_or_else(|error| panic!("runtime directory must move: {error}"));
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside)
        .unwrap_or_else(|error| panic!("outside directory must exist: {error}"));
    std::fs::write(outside.join("keep"), b"unchanged")
        .unwrap_or_else(|error| panic!("outside fixture must exist: {error}"));
    symlink(&outside, &replacement)
        .unwrap_or_else(|error| panic!("replacement symlink must exist: {error}"));
    assert!(replaced.cleanup().is_err());
    replaced.preserve();
    assert_eq!(
        std::fs::read(outside.join("keep"))
            .unwrap_or_else(|error| panic!("outside fixture must remain: {error}")),
        b"unchanged"
    );
}

#[test]
fn copilot_device_prompt_surfaces_only_the_user_facing_values() {
    let mut output = Vec::new();
    write_github_device_prompt(&mut output, "https://github.com/login/device", "ABCD-EFGH")
        .unwrap_or_else(|error| panic!("device prompt must render: {error}"));
    assert_eq!(
        String::from_utf8(output)
            .unwrap_or_else(|error| panic!("device prompt must be UTF-8: {error}")),
        "Open https://github.com/login/device\nEnter code: ABCD-EFGH\n"
    );
}

#[test]
fn remote_bootstrap_token_rotation_is_atomic_private_and_idempotent() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
    let path = directory.path().join("auth.token");
    let first = "a".repeat(64);
    let second = "b".repeat(64);
    write_private_file_atomic(&path, first.as_bytes())
        .unwrap_or_else(|error| panic!("first token install must succeed: {error}"));
    write_private_file_atomic(&path, first.as_bytes())
        .unwrap_or_else(|error| panic!("same token install must be idempotent: {error}"));
    write_private_file_atomic(&path, second.as_bytes())
        .unwrap_or_else(|error| panic!("token rotation must succeed: {error}"));

    assert_eq!(
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("token must be readable: {error}")),
        second
    );
    let mode = std::fs::metadata(&path)
        .unwrap_or_else(|error| panic!("token metadata must exist: {error}"))
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0);
    assert!(valid_bootstrap_token(&first));
    assert!(!valid_bootstrap_token("not-a-token"));
}

#[test]
fn remote_bootstrap_rotation_refuses_symlink_target() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
    let outside = directory.path().join("outside");
    std::fs::write(&outside, "unchanged")
        .unwrap_or_else(|error| panic!("outside fixture must exist: {error}"));
    let path = directory.path().join("auth.token");
    symlink(&outside, &path).unwrap_or_else(|error| panic!("symlink fixture must exist: {error}"));

    let error = match write_private_file_atomic(&path, "c".repeat(64).as_bytes()) {
        Ok(()) => panic!("symlink token must be refused"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unsafe remote bootstrap-token"));
    assert_eq!(
        std::fs::read_to_string(outside)
            .unwrap_or_else(|read_error| panic!("outside must remain readable: {read_error}")),
        "unchanged"
    );
}

#[test]
fn local_tui_discovers_the_repository_root_from_a_nested_launch_directory() {
    let root = tempfile::tempdir()
        .unwrap_or_else(|error| panic!("temporary directory must exist: {error}"));
    let repository = root.path().join("project");
    let nested = repository.join("scripts/tests");
    std::fs::create_dir_all(repository.join(".git"))
        .unwrap_or_else(|error| panic!("git marker must exist: {error}"));
    std::fs::create_dir_all(&nested)
        .unwrap_or_else(|error| panic!("nested directory must exist: {error}"));

    assert_eq!(discover_local_workspace(&nested), repository);
    assert_eq!(discover_local_workspace(root.path()), root.path());
}
