use super::*;

#[test]
fn tui_settings_persist_user_only_with_provenance_and_merge_concurrently() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    fs::write(&project, "[compaction]\nauto = true\n").expect("project config");
    let loader = ConfigLoader::new(user.clone(), project.clone());
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::write(
            &user,
            "[providers.manual]\nkind = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\napi_key_env = \"MANUAL_KEY\"\n",
        )
        .expect("manual user provider");
    fs::write(
        user.parent().expect("user parent").join(".config-old.tmp"),
        "stale",
    )
    .expect("crash temporary");

    let first = loader.clone();
    let second = loader.clone();
    let theme = std::thread::spawn(move || first.persist_tui_setting("ui.theme", "daylight"));
    let compact =
        std::thread::spawn(move || second.persist_tui_setting("compaction.auto", "false"));
    theme.join().expect("theme worker").expect("theme setting");
    compact
        .join()
        .expect("compaction worker")
        .expect("compaction setting");
    loader
        .persist_tui_setting("models.thinking.fast", "high")
        .expect("thinking setting");
    let effective = loader
        .persist_tui_setting("permissions.default", "deny")
        .expect("permission setting");

    assert_eq!(effective.config.ui.theme, "daylight");
    assert!(!effective.config.compaction.auto);
    assert_eq!(
        effective.config.models.thinking["fast"],
        rw_types::config::ThinkingLevel::High
    );
    assert_eq!(
        effective.config.permissions.default,
        PermissionDecision::Deny
    );
    assert!(
        matches!(effective.provenance("permissions.default"), Some(ConfigSource::UserTui(path)) if path == &user)
    );
    assert!(matches!(
        effective.provenance("providers.manual.kind"),
        Some(ConfigSource::UserFile(path)) if path == &user
    ));
    assert!(
        effective
            .render_with_provenance()
            .contains("user (set via TUI)")
    );
    assert_eq!(
        fs::read_to_string(project).expect("project unchanged"),
        "[compaction]\nauto = true\n"
    );
    let persisted = fs::read_to_string(&user).expect("user config");
    assert!(persisted.contains("last updated via TUI"));
    assert!(persisted.contains("theme = \"daylight\""));
    assert!(persisted.contains("default = \"deny\""));
}

#[cfg(unix)]
#[test]
fn tui_settings_same_process_writer_waits_past_external_deadline() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let held = super::acquire_tui_settings_lock(
        user.parent().expect("user parent"),
        "test-same-process-contention",
    )
    .expect("held lock");
    let loader = ConfigLoader::new(user, project);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
    let writer = std::thread::spawn(move || {
        started_tx.send(()).expect("signal writer");
        loader.persist_tui_setting("ui.theme", "daylight")
    });
    started_rx.recv().expect("writer started");

    // Longer than the external flock deadline: a sibling thread must wait
    // for the in-process read-modify-write, not consume that deadline.
    std::thread::sleep(std::time::Duration::from_millis(150));
    drop(held);

    writer
        .join()
        .expect("writer thread")
        .expect("serialized setting");
}

#[test]
fn tui_settings_reject_non_allowlisted_security_keys() {
    let root = tempdir().expect("root");
    let loader = ConfigLoader::new(
        root.path().join("user/config.toml"),
        root.path().join("repo/.rottweiler/config.toml"),
    );
    fs::create_dir_all(root.path().join("repo/.rottweiler")).expect("project root");
    let error = loader
        .persist_tui_setting("providers.openai.base_url", "https://attacker.invalid")
        .expect_err("provider mutation must be rejected");
    assert!(
        matches!(error, ConfigError::InvalidUserSetting { .. }),
        "unexpected error: {error:?}"
    );
}

#[test]
fn tui_budget_settings_parse_human_dollars_and_validate_percent() {
    let config = Config::default();
    let session_key = "budget.session_cost_cap_micros_usd";
    let daily_key = "budget.daily_cost_cap_micros_usd";
    let token_key = "budget.session_token_cap";
    let warning_key = "budget.warn_at_percent";

    for value in ["1", "12.5", " 12.5 ", "999999.99", "unlimited", "UNLIMITED"] {
        super::validate_tui_setting(&config, session_key, value)
            .unwrap_or_else(|error| panic!("{value:?} should be valid: {error}"));
    }
    for value in ["0", "0.00", "-1", "12.345", "1000000", "$12.50"] {
        assert!(
            matches!(
                super::validate_tui_setting(&config, session_key, value),
                Err(ConfigError::InvalidUserSetting { .. })
            ),
            "{value:?} should be rejected",
        );
    }
    for value in ["1", "100", " 80 "] {
        super::validate_tui_setting(&config, warning_key, value)
            .unwrap_or_else(|error| panic!("{value:?} should be valid: {error}"));
    }
    for value in ["0", "101"] {
        assert!(matches!(
            super::validate_tui_setting(&config, warning_key, value),
            Err(ConfigError::InvalidUserSetting { .. })
        ));
    }
    for value in ["1", "250000", " 1000000 ", "unlimited", "UNLIMITED"] {
        super::validate_tui_setting(&config, token_key, value)
            .unwrap_or_else(|error| panic!("{value:?} should be valid: {error}"));
    }
    for value in ["", "0", "-1", "1.5", "1,000"] {
        assert!(matches!(
            super::validate_tui_setting(&config, token_key, value),
            Err(ConfigError::InvalidUserSetting { .. })
        ));
    }

    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let loader = ConfigLoader::new(user.clone(), project);
    let applied = loader
        .persist_tui_setting(session_key, "12")
        .expect("whole-dollar session cap");
    assert_eq!(
        applied.config.budget.session_cost_cap_micros_usd,
        Some(12_000_000)
    );
    let applied = loader
        .persist_tui_setting(daily_key, "12.50")
        .expect("cent-precise daily cap");
    assert_eq!(
        applied.config.budget.daily_cost_cap_micros_usd,
        Some(12_500_000)
    );
    let applied = loader
        .persist_tui_setting(warning_key, "1")
        .expect("lower warning bound");
    assert_eq!(applied.config.budget.warn_at_percent, 1);
    let applied = loader
        .persist_tui_setting(warning_key, "100")
        .expect("upper warning bound");
    assert_eq!(applied.config.budget.warn_at_percent, 100);
    let applied = loader
        .persist_tui_setting(token_key, "250000")
        .expect("session token cap");
    assert_eq!(applied.config.budget.session_token_cap, Some(250_000));

    let persisted = fs::read_to_string(user).expect("user config");
    assert!(persisted.contains("session_cost_cap_micros_usd = 12000000"));
    assert!(persisted.contains("daily_cost_cap_micros_usd = 12500000"));
    assert!(persisted.contains("warn_at_percent = 100"));
    assert!(persisted.contains("session_token_cap = 250000"));
}

#[test]
fn tui_budget_unlimited_clears_an_existing_cap_leaf() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    fs::write(
        &user,
        "[budget]\nsession_cost_cap_micros_usd = 5000000\nwarn_at_percent = 75\n",
    )
    .expect("budget config");
    let loader = ConfigLoader::new(user.clone(), project);

    let applied = loader
        .persist_tui_setting("budget.session_cost_cap_micros_usd", " UnLiMiTeD ")
        .expect("clear session cap");

    assert_eq!(applied.config.budget.session_cost_cap_micros_usd, None);
    assert_eq!(applied.config.budget.warn_at_percent, 75);
    let persisted = fs::read_to_string(user).expect("user config");
    assert!(!persisted.contains("session_cost_cap_micros_usd"));
    assert!(persisted.contains("warn_at_percent = 75"));
}

#[test]
fn tui_budget_setting_refuses_a_trusted_project_override_without_writing_user_config() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    fs::write(
        &project,
        "[budget]\nsession_cost_cap_micros_usd = 5000000\n",
    )
    .expect("project budget config");
    let loader = ConfigLoader::new(user.clone(), project).with_project_trust(true);

    let error = loader
        .persist_tui_setting("budget.session_cost_cap_micros_usd", "1")
        .expect_err("trusted project budget must refuse a user-layer write");

    match error {
        ConfigError::InvalidUserSetting { key, reason } => {
            assert_eq!(key, "budget.session_cost_cap_micros_usd");
            assert_eq!(
                reason,
                "a trusted project configuration already sets this budget; edit the project's config file to change it"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(
        !user.exists(),
        "refused setting must not create the user file"
    );
}

#[test]
fn resolved_provider_setup_is_fixed_user_scoped_and_idempotent() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    fs::write(&project, "[providers.project]\nkind = \"openai\"\n").expect("project config");
    let loader = ConfigLoader::new(user.clone(), project.clone());

    for provider in ["openai_codex", "github_copilot"] {
        let effective = loader
            .configure_provider_profile(provider, provider)
            .expect("built-in setup");
        assert_eq!(effective.config.providers[provider].kind, provider);
        assert!(matches!(
            effective.provenance(&format!("providers.{provider}.kind")),
            Some(ConfigSource::UserTui(path)) if path == &user
        ));
        loader
            .configure_provider_profile(provider, provider)
            .expect("idempotent setup");
    }
    assert_eq!(
        fs::read_to_string(project).expect("project unchanged"),
        "[providers.project]\nkind = \"openai\"\n"
    );
    let persisted = fs::read_to_string(user).expect("user config");
    assert!(persisted.contains("[providers.openai_codex]"));
    assert!(persisted.contains("[providers.github_copilot]"));
}

#[test]
fn project_model_preference_is_private_concrete_and_independent_of_project_trust() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    fs::write(&project, "[providers.hostile]\nkind = \"openai\"\n")
        .expect("untrusted project config");
    let loader = ConfigLoader::new(user.clone(), project.clone());

    loader
        .persist_tui_project_model("github_copilot/gpt-5-mini")
        .expect("concrete preference");

    assert_eq!(
        loader.tui_project_model().expect("preference").as_deref(),
        Some("github_copilot/gpt-5-mini")
    );
    assert_eq!(
        ConfigLoader::new(user.clone(), project.clone())
            .tui_project_model()
            .expect("restart preference")
            .as_deref(),
        Some("github_copilot/gpt-5-mini")
    );
    assert_eq!(
        fs::read_to_string(project).expect("project unchanged"),
        "[providers.hostile]\nkind = \"openai\"\n"
    );
    loader
        .persist_tui_project_model("fast")
        .expect("alias preference");
    assert_eq!(
        loader.tui_project_model().expect("alias").as_deref(),
        Some("fast")
    );
    assert!(loader.persist_tui_project_model("not valid").is_err());
}

#[test]
fn keybinding_and_mcp_settings_preserve_existing_user_details_and_enforce_caps() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let keybindings = user.with_file_name("keybindings.toml");
    let mcp = user.with_file_name("mcp.toml");
    fs::write(
        &keybindings,
        "preset='standard'\n[bindings]\nsubmit='enter'\n",
    )
    .expect("keybindings");
    fs::write(
        &mcp,
        "[servers.docs]\nargv=['/usr/bin/docs']\ndefer_tools=true\n",
    )
    .expect("mcp");
    let loader = ConfigLoader::new(user, project);

    loader
        .persist_tui_keybinding_preset("vim")
        .expect("keybinding preset");
    loader
        .persist_tui_mcp_enabled("docs", false)
        .expect("MCP toggle");

    let keybindings_text = fs::read_to_string(&keybindings).expect("keybindings text");
    assert!(keybindings_text.contains("preset = \"vim\""));
    assert!(keybindings_text.contains("submit = \"enter\""));
    let mcp_text = fs::read_to_string(&mcp).expect("MCP text");
    assert!(mcp_text.contains("argv = [\"/usr/bin/docs\"]"));
    assert!(mcp_text.contains("defer_tools = true"));
    assert!(mcp_text.contains("enabled = false"));
    assert_eq!(
        loader.tui_mcp_servers().expect("MCP list"),
        [("docs".to_owned(), false)]
    );

    fs::write(
        &keybindings,
        vec![b'x'; super::MAX_TUI_AUX_CONFIG_BYTES + 1],
    )
    .expect("oversized keybindings");
    assert!(loader.persist_tui_keybinding_preset("standard").is_err());
    fs::write(&mcp, vec![b'x'; super::MAX_TUI_AUX_CONFIG_BYTES + 1]).expect("oversized MCP");
    assert!(loader.persist_tui_mcp_enabled("docs", true).is_err());
}

#[test]
fn tui_stdio_mcp_persistence_has_loader_compatible_shape_and_redacts_errors() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let loader = ConfigLoader::new(user.clone(), project);
    let secret = "stdio-secret-canary";
    loader
        .persist_tui_mcp_stdio_server(
            "docs",
            Path::new("/usr/local/bin/docs-mcp"),
            &["--stdio".to_owned(), "docs".to_owned()],
            &[("DOCS_TOKEN".to_owned(), secret.to_owned())],
        )
        .expect("persist stdio server");

    let path = user.with_file_name("mcp.toml");
    let document = fs::read_to_string(&path).expect("MCP config");
    let parsed = toml::from_str::<toml::Value>(&document).expect("loader-compatible TOML");
    let server = parsed
        .get("servers")
        .and_then(|servers| servers.get("docs"))
        .expect("docs server");
    assert_eq!(
        server.get("argv").and_then(toml::Value::as_array),
        Some(&vec![
            toml::Value::String("/usr/local/bin/docs-mcp".to_owned()),
            toml::Value::String("--stdio".to_owned()),
            toml::Value::String("docs".to_owned()),
        ])
    );
    assert_eq!(
        server
            .get("environment")
            .and_then(|environment| environment.get("DOCS_TOKEN"))
            .and_then(toml::Value::as_str),
        Some(secret)
    );
    assert_eq!(
        server.get("enabled").and_then(toml::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        server.get("defer_tools").and_then(toml::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        loader.tui_mcp_servers().expect("MCP list"),
        [("docs".to_owned(), false)]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }

    let oversized_secret = format!(
        "{secret}{}",
        "x".repeat(super::MAX_MCP_ENVIRONMENT_VALUE_BYTES)
    );
    let error = loader
        .persist_tui_mcp_stdio_server(
            "other",
            Path::new("/usr/local/bin/docs-mcp"),
            &[],
            &[("OTHER_TOKEN".to_owned(), oversized_secret.clone())],
        )
        .expect_err("oversized environment must fail");
    let message = error.to_string();
    assert!(!message.contains(secret));
    assert!(!message.contains(&oversized_secret));
}

#[test]
fn tui_mcp_remove_deletes_only_the_server_and_matching_override() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let mcp = user.with_file_name("mcp.toml");
    fs::write(
            &mcp,
            "[servers.docs]\nargv=['/usr/bin/docs']\n[servers.search]\nendpoint='https://example.com/mcp'\n[capability_overrides.docs]\ndefault=['reads_fs']\n[capability_overrides.search]\ndefault=['network']\n",
        )
        .expect("MCP config");
    let loader = ConfigLoader::new(user, project);
    loader.remove_tui_mcp_server("docs").expect("remove docs");
    let document = fs::read_to_string(&mcp).expect("MCP config");
    assert!(!document.contains("servers.docs"));
    assert!(!document.contains("capability_overrides.docs"));
    assert!(document.contains("servers.search"));
    assert!(document.contains("capability_overrides.search"));
    assert!(loader.remove_tui_mcp_server("missing").is_err());
}

#[cfg(unix)]
#[test]
fn project_identity_distinguishes_non_utf8_canonical_paths() {
    use std::os::unix::ffi::OsStringExt as _;

    let first = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'p', 0x80]));
    let second = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b'p', 0x81]));

    assert_ne!(
        super::hash_project_identity(&first),
        super::hash_project_identity(&second)
    );
}

#[cfg(unix)]
#[test]
fn project_model_preference_rejects_symlink_and_hardlink_tampering() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let preference = user.with_file_name("project-model-preferences.json");
    let outside = root.path().join("outside.json");
    fs::write(&outside, "{}").expect("outside");
    symlink(&outside, &preference).expect("symlink");
    let loader = ConfigLoader::new(user, project);
    assert!(loader.persist_tui_project_model("openai/gpt-5").is_err());
    fs::remove_file(&preference).expect("remove symlink");
    fs::hard_link(&outside, &preference).expect("hardlink");
    assert!(loader.persist_tui_project_model("openai/gpt-5").is_err());
}

#[test]
fn tui_settings_reject_malformed_and_oversized_provenance_without_changing_config() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    fs::write(&user, "[ui]\ntheme = \"kennel-dark\"\n").expect("user config");
    let provenance = user.with_file_name("config-tui-provenance.json");
    fs::write(&provenance, b"not-json").expect("malformed provenance");
    make_private(&provenance);
    let loader = ConfigLoader::new(user.clone(), project);

    assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
    assert_eq!(
        fs::read_to_string(&user).expect("unchanged user config"),
        "[ui]\ntheme = \"kennel-dark\"\n"
    );

    fs::write(&provenance, vec![b'x'; 64 * 1024 + 1]).expect("oversized provenance");
    make_private(&provenance);
    assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
    assert_eq!(
        fs::read_to_string(&user).expect("unchanged user config"),
        "[ui]\ntheme = \"kennel-dark\"\n"
    );
}

#[cfg(unix)]
#[test]
fn tui_settings_reject_symlink_and_hardlink_targets() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    let outside = root.path().join("outside.toml");
    fs::write(&outside, "").expect("outside");
    symlink(&outside, &user).expect("symlink");
    let loader = ConfigLoader::new(user.clone(), root.path().join("project.toml"));
    assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
    fs::remove_file(&user).expect("remove symlink");
    fs::hard_link(&outside, &user).expect("hardlink");
    assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
}

#[cfg(unix)]
#[test]
fn tui_settings_reject_unsafe_provenance_targets() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::write(&user, "[ui]\ntheme = \"kennel-dark\"\n").expect("user config");
    let provenance = user.with_file_name("config-tui-provenance.json");
    let outside = root.path().join("outside.json");
    fs::write(&outside, "{}").expect("outside");
    symlink(&outside, &provenance).expect("provenance symlink");
    let loader = ConfigLoader::new(user.clone(), root.path().join("project.toml"));

    assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
    assert_eq!(
        fs::read_to_string(&user).expect("unchanged user config"),
        "[ui]\ntheme = \"kennel-dark\"\n"
    );
    fs::remove_file(&provenance).expect("remove provenance symlink");
    fs::hard_link(&outside, &provenance).expect("provenance hardlink");
    assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
}

#[cfg(unix)]
#[test]
fn tui_settings_lock_contention_fails_without_blocking_driver_lifecycle() {
    let root = tempdir().expect("root");
    let user = root.path().join("user/config.toml");
    let project = root.path().join("repo/.rottweiler/config.toml");
    fs::create_dir_all(user.parent().expect("user parent")).expect("user dir");
    fs::create_dir_all(project.parent().expect("project parent")).expect("project dir");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(user.parent().expect("user parent").join("config.toml.lock"))
        .expect("lock file");
    rustix::fs::flock(&held, rustix::fs::FlockOperation::LockExclusive)
        .expect("held external lock");
    let loader = ConfigLoader::new(user, project);
    let started = std::time::Instant::now();

    assert!(loader.persist_tui_setting("ui.theme", "daylight").is_err());
    assert!(started.elapsed() < std::time::Duration::from_millis(250));
    drop(held);
}
