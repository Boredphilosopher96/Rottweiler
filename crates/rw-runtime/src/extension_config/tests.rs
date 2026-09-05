use super::*;
use tempfile::TempDir;

fn roots() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new().expect("temp");
    let user = temp.path().join("user");
    let project = temp.path().join("project");
    fs::create_dir_all(user.join(".rottweiler")).expect("user");
    fs::create_dir_all(project.join(".rottweiler")).expect("project");
    (temp, user, project)
}

#[test]
fn project_configuration_is_inert_until_trusted_then_overrides_with_origin() {
    let (_temp, user, project) = roots();
    fs::write(
        user.join(".rottweiler/mcp.toml"),
        "[servers.same]\nargv=['/usr/bin/false']\n",
    )
    .expect("user config");
    fs::write(
        project.join(".rottweiler/mcp.toml"),
        "[servers.same]\nargv=['/usr/bin/true']\n",
    )
    .expect("project config");
    let inert = discover_executable_configs(&user, &project, false).expect("inert");
    assert_eq!(inert.mcp_servers.len(), 1);
    assert!(matches!(
        inert.mcp_servers[0].origin,
        ExecutableConfigOrigin::User(_)
    ));
    assert_eq!(inert.warnings.len(), 1);
    let trusted = discover_executable_configs(&user, &project, true).expect("trusted");
    assert!(matches!(
        trusted.mcp_servers[0].origin,
        ExecutableConfigOrigin::TrustedProject(_)
    ));
}

#[test]
fn rejects_shell_strings_plaintext_environment_and_endpoint_credentials() {
    let (_temp, user, project) = roots();
    fs::write(
        user.join(".rottweiler/mcp.toml"),
        "[servers.bad]\nargv=[]\n",
    )
    .expect("config");
    assert!(discover_executable_configs(&user, &project, false).is_err());
    fs::write(
        user.join(".rottweiler/mcp.toml"),
        "[servers.bad]\nargv=['/usr/bin/true']\ninherit_env=['API_TOKEN']\n",
    )
    .expect("config");
    assert!(discover_executable_configs(&user, &project, false).is_err());
    fs::write(
        user.join(".rottweiler/mcp.toml"),
        "[servers.bad]\nendpoint='https://secret@example.com/mcp'\n",
    )
    .expect("config");
    assert!(discover_executable_configs(&user, &project, false).is_err());
    fs::write(
        user.join(".rottweiler/mcp.toml"),
        "[servers.bad]\nendpoint='https://example.com/mcp'\noauth_token='plaintext-secret'\n",
    )
    .expect("config");
    assert!(discover_executable_configs(&user, &project, false).is_err());
}

#[test]
fn oauth_login_configuration_is_complete_bounded_and_vault_only() {
    let (_temp, user, project) = roots();
    let config = user.join(".rottweiler/mcp.toml");
    fs::write(
        &config,
        r"
[servers.remote]
endpoint = 'https://mcp.example/mcp'
oauth_credential = 'mcp.remote.oauth'
oauth_resource = 'https://mcp.example/mcp'
oauth_audience = 'mcp.example'
oauth_authorization_endpoint = 'https://auth.example/authorize'
oauth_token_endpoint = 'https://auth.example/token'
oauth_client_id = 'public-native-client'
oauth_scopes = ['mcp:tools', 'offline_access']
oauth_proxy = 'http://127.0.0.1:8888'
",
    )
    .expect("config");
    let catalog = discover_executable_configs(&user, &project, false).expect("OAuth config");
    assert_eq!(catalog.mcp_servers.len(), 1);
    let (_, binding) = catalog.mcp_servers[0]
        .oauth_binding()
        .expect("OAuth binding");
    let refresh = binding.refresh.expect("refresh binding");
    assert_eq!(
        refresh.token_endpoint.as_str(),
        "https://auth.example/token"
    );
    assert_eq!(refresh.client_id, "public-native-client");
    assert_eq!(refresh.scopes, ["mcp:tools", "offline_access"]);
    assert_eq!(
        refresh.proxy.as_ref().map(Url::as_str),
        Some("http://127.0.0.1:8888/")
    );

    for invalid in [
        r"
[servers.remote]
endpoint = 'https://mcp.example/mcp'
oauth_credential = 'mcp.remote.oauth'
oauth_resource = 'https://mcp.example/mcp'
oauth_audience = 'mcp.example'
oauth_authorization_endpoint = 'https://auth.example/authorize'
",
        r"
[servers.remote]
endpoint = 'https://mcp.example/mcp'
oauth_credential = 'mcp.remote.oauth'
oauth_resource = 'https://other.example/'
oauth_audience = 'mcp.example'
",
        r"
[servers.remote]
endpoint = 'https://mcp.example/mcp'
oauth_credential = 'mcp.remote.oauth'
oauth_resource = 'https://mcp.example/mcp'
oauth_audience = 'mcp.example'
oauth_authorization_endpoint = 'https://auth.example/authorize?state=fixed'
oauth_token_endpoint = 'https://auth.example/token'
oauth_client_id = 'public-native-client'
",
    ] {
        fs::write(&config, invalid).expect("invalid config");
        assert!(
            discover_executable_configs(&user, &project, false).is_err(),
            "accepted invalid OAuth config"
        );
    }
}

#[test]
fn untrusted_invalid_project_file_is_not_parsed() {
    let (_temp, user, project) = roots();
    fs::write(
        project.join(".rottweiler/plugins.toml"),
        "definitely invalid",
    )
    .expect("config");
    let catalog = discover_executable_configs(&user, &project, false).expect("inert");
    assert!(catalog.plugins.is_empty());
    assert_eq!(catalog.warnings.len(), 1);
}

#[test]
fn agents_first_precedence_applies_at_project_then_user_level() {
    let (_temp, user, project) = roots();
    fs::create_dir_all(user.join(".agents")).expect("user agents");
    fs::create_dir_all(project.join(".agents")).expect("project agents");
    for (path, executable) in [
        (user.join(".rottweiler/mcp.toml"), "/usr/bin/false"),
        (user.join(".agents/mcp.toml"), "/usr/bin/true"),
        (project.join(".rottweiler/mcp.toml"), "/usr/bin/false"),
        (project.join(".agents/mcp.toml"), "/usr/bin/true"),
    ] {
        fs::write(path, format!("[servers.same]\nargv=['{executable}']\n")).expect("config");
    }
    let user_only = discover_executable_configs(&user, &project, false).expect("user");
    assert_eq!(
        user_only.mcp_servers[0].origin.path(),
        fs::canonicalize(user.join(".agents/mcp.toml")).expect("canonical user config")
    );
    let trusted = discover_executable_configs(&user, &project, true).expect("trusted");
    assert_eq!(
        trusted.mcp_servers[0].origin.path(),
        fs::canonicalize(project.join(".agents/mcp.toml")).expect("canonical project config")
    );
}

#[test]
fn plugin_domains_are_bounded_canonical_and_private_destinations_fail_closed() {
    assert!(
        validate_domains(vec![], "plugin")
            .expect("empty")
            .is_empty()
    );
    assert_eq!(
        validate_domains(vec!["API.Example.COM.".to_owned()], "plugin").expect("public"),
        vec!["api.example.com"]
    );
    for private in ["localhost", "service.local", "127.0.0.1", "10.0.0.1", "::1"] {
        assert!(
            validate_domains(vec![private.to_owned()], "plugin").is_err(),
            "accepted {private}"
        );
    }
    assert!(
        validate_domains(
            (0..33)
                .map(|index| format!("{index}.example.com"))
                .collect(),
            "plugin",
        )
        .is_err()
    );
}

#[test]
fn stdio_sandbox_authority_is_bounded_canonical_and_approval_bound() {
    let (_temp, user, project) = roots();
    let read_root = project.join("records");
    let write_root = project.join("generated");
    fs::create_dir(&read_root).expect("read root");
    fs::create_dir(&write_root).expect("write root");
    let config = project.join(".rottweiler/mcp.toml");
    fs::write(
            &config,
            "[servers.scoped]\nargv=['/usr/bin/true']\nread_roots=['records']\nwrite_roots=['generated']\nallowed_domains=['API.Example.COM.']\n",
        )
        .expect("config");
    let catalog = discover_executable_configs(&user, &project, true).expect("catalog");
    let server = &catalog.mcp_servers[0];
    let first_fingerprint = server.approval_fingerprint().expect("fingerprint");
    let runtime = server.runtime_config(|_| unreachable!()).expect("runtime");
    let McpTransportConfig::Stdio { sandbox, .. } = runtime.transport else {
        panic!("expected stdio");
    };
    assert_eq!(
        sandbox.read_roots,
        [fs::canonicalize(read_root).expect("read")]
    );
    assert_eq!(
        sandbox.write_roots,
        [fs::canonicalize(write_root).expect("write")]
    );
    assert_eq!(sandbox.allowed_domains, ["api.example.com"]);

    fs::write(
            &config,
            "[servers.scoped]\nargv=['/usr/bin/true']\nread_roots=['records']\nwrite_roots=['generated']\nallowed_domains=['other.example.com']\n",
        )
        .expect("changed config");
    let changed = discover_executable_configs(&user, &project, true).expect("changed");
    assert_ne!(
        first_fingerprint,
        changed.mcp_servers[0]
            .approval_fingerprint()
            .expect("changed fingerprint")
    );

    fs::write(
        &config,
        "[servers.scoped]\nargv=['/usr/bin/true']\nallowed_domains=['localhost']\n",
    )
    .expect("private config");
    assert!(discover_executable_configs(&user, &project, true).is_err());
}

#[test]
fn stdio_literal_environment_round_trips_redacted_and_approval_bound() {
    let (_temp, user, project) = roots();
    let config = user.join(".rottweiler/mcp.toml");
    let secret = "literal-secret-canary";
    fs::write(
            &config,
            format!(
                "[servers.docs]\nargv=['/usr/bin/true','--stdio']\n[servers.docs.environment]\nDOCS_TOKEN='{secret}'\n"
            ),
        )
        .expect("config");
    let catalog = discover_executable_configs(&user, &project, false).expect("catalog");
    let server = &catalog.mcp_servers[0];
    let debug = format!("{:?}", server.transport);
    assert!(debug.contains("DOCS_TOKEN"));
    assert!(!debug.contains(secret));
    let fingerprint = server.approval_fingerprint().expect("fingerprint");
    let runtime = server.runtime_config(|_| unreachable!()).expect("runtime");
    let McpTransportConfig::Stdio {
        executable,
        args,
        environment,
        working_directory,
        sandbox,
    } = runtime.transport
    else {
        panic!("expected stdio");
    };
    assert_eq!(executable, fs::canonicalize("/usr/bin/true").expect("true"));
    assert_eq!(args, ["--stdio"]);
    assert_eq!(environment, [("DOCS_TOKEN".to_owned(), secret.to_owned())]);
    assert_eq!(working_directory, None);
    assert_eq!(sandbox, rw_mcp::McpStdioSandboxPolicy::default());

    fs::write(
            &config,
            "[servers.docs]\nargv=['/usr/bin/true','--stdio']\n[servers.docs.environment]\nDOCS_TOKEN='changed'\n",
        )
        .expect("changed config");
    let changed = discover_executable_configs(&user, &project, false).expect("changed");
    assert_ne!(
        fingerprint,
        changed.mcp_servers[0]
            .approval_fingerprint()
            .expect("changed fingerprint")
    );
}

#[test]
fn tui_persisted_stdio_server_round_trips_through_executable_loader() {
    let (_temp, user, project) = roots();
    let loader = rw_store::config::ConfigLoader::new(
        user.join(".rottweiler/config.toml"),
        project.join(".rottweiler/config.toml"),
    );
    let executable = fs::canonicalize("/usr/bin/true").expect("true");
    loader
        .persist_tui_mcp_stdio_server(
            "docs",
            &executable,
            &["--stdio".to_owned()],
            &[("DOCS_TOKEN".to_owned(), "secret-canary".to_owned())],
        )
        .expect("persist stdio");

    let catalog = discover_executable_configs(&user, &project, false).expect("catalog");
    assert_eq!(catalog.mcp_servers.len(), 1);
    let runtime = catalog.mcp_servers[0]
        .runtime_config(|_| unreachable!())
        .expect("runtime");
    assert!(!runtime.enabled);
    assert!(runtime.defer_tools);
    assert_eq!(runtime.id, McpServerId::new("docs").expect("id"));
    assert_eq!(
        runtime.transport,
        McpTransportConfig::Stdio {
            executable,
            args: vec!["--stdio".to_owned()],
            working_directory: None,
            environment: vec![("DOCS_TOKEN".to_owned(), "secret-canary".to_owned())],
            sandbox: rw_mcp::McpStdioSandboxPolicy::default(),
        }
    );
}

#[test]
fn user_capability_overrides_use_exact_tool_then_server_precedence() {
    let (_temp, user, project) = roots();
    let project_config = project.join(".rottweiler/mcp.toml");
    let user_config = user.join(".rottweiler/mcp.toml");
    fs::write(
        &project_config,
        "[servers.scoped]\nargv=['/usr/bin/true']\n",
    )
    .expect("project config");
    fs::write(
            &user_config,
            "[capability_overrides.scoped]\ndefault=['reads_fs']\n[capability_overrides.scoped.tools]\ndelete=['network','exec']\nsafe=[]\n",
        )
        .expect("user override");
    let catalog = discover_executable_configs(&user, &project, true).expect("catalog");
    let server = &catalog.mcp_servers[0];
    assert_eq!(
        server.tool_capabilities.resolve("lookup"),
        CapabilityManifest::new([ToolCapability::ReadFilesystem])
    );
    assert_eq!(
        server.tool_capabilities.resolve("delete"),
        CapabilityManifest::new([ToolCapability::Network, ToolCapability::Execute])
    );
    assert_eq!(
        server.tool_capabilities.resolve("safe"),
        CapabilityManifest::default()
    );
    assert_eq!(
        server.capability_override_origin.as_deref(),
        Some(
            fs::canonicalize(&user_config)
                .expect("canonical override")
                .as_path()
        )
    );
    let first = server.approval_fingerprint().expect("first fingerprint");
    fs::write(
        &user_config,
        "[capability_overrides.scoped]\ndefault=['network','exec']\n",
    )
    .expect("changed override");
    let changed = discover_executable_configs(&user, &project, true).expect("changed");
    assert_ne!(
        first,
        changed.mcp_servers[0]
            .approval_fingerprint()
            .expect("changed fingerprint")
    );

    fs::write(
        &project_config,
        "[servers.scoped]\nargv=['/usr/bin/true']\n[capability_overrides.scoped]\ndefault=[]\n",
    )
    .expect("unsafe project override");
    assert!(discover_executable_configs(&user, &project, true).is_err());
}

#[test]
fn stdio_entrypoint_and_adjacent_lock_are_content_attested() {
    let (_temp, user, project) = roots();
    let entrypoint = user.join("server.sh");
    let lock = user.join("package.json");
    fs::write(&entrypoint, "echo one\n").expect("entrypoint");
    fs::write(&lock, "{\"version\":1}\n").expect("lock");
    let config = user.join(".rottweiler/mcp.toml");
    fs::write(
        &config,
        format!(
            "[servers.attested]\nargv=['/bin/sh','{}']\ncwd='{}'\n",
            entrypoint.display(),
            user.display()
        ),
    )
    .expect("config");
    let first = discover_executable_configs(&user, &project, false).expect("first");
    let first_server = &first.mcp_servers[0];
    assert!(first_server.attested_files.len() >= 3);
    let first_fingerprint = first_server.approval_fingerprint().expect("fingerprint");

    fs::write(&entrypoint, "echo two\n").expect("mutate entrypoint");
    assert!(
        first_server
            .attested_files
            .iter()
            .any(|identity| identity.validate().is_err())
    );
    let second = discover_executable_configs(&user, &project, false).expect("second");
    assert_ne!(
        first_fingerprint,
        second.mcp_servers[0]
            .approval_fingerprint()
            .expect("changed fingerprint")
    );

    fs::write(&lock, "{\"version\":2}\n").expect("mutate lock");
    assert!(
        second.mcp_servers[0]
            .attested_files
            .iter()
            .any(|identity| identity.validate().is_err())
    );
    let third = discover_executable_configs(&user, &project, false).expect("third");
    assert_ne!(
        second.mcp_servers[0]
            .approval_fingerprint()
            .expect("second fingerprint"),
        third.mcp_servers[0]
            .approval_fingerprint()
            .expect("third fingerprint")
    );
}

#[test]
fn oversized_attestation_is_rejected_before_content_read() {
    let root = TempDir::new().expect("temp");
    let oversized = root.path().join("oversized.lock");
    let file = fs::File::create(&oversized).expect("file");
    file.set_len(256 * 1024 * 1024 + 1).expect("sparse length");
    assert!(ContentAttestation::capture(&oversized, 256 * 1024 * 1024).is_err());
}

#[test]
fn package_runners_and_interpreter_eval_forms_fail_closed() {
    let root = TempDir::new().expect("temp");
    let entrypoint = root.path().join("plugin.ts");
    fs::write(&entrypoint, "export {};\n").expect("entrypoint");
    for runner in ["npx", "pnpm", "uvx", "cargo", "bunx"] {
        let argv = [format!("/usr/local/bin/{runner}"), "package".to_owned()];
        let error = attested_command_paths(&argv, root.path())
            .expect_err("package runners must not cross the approval boundary");
        assert!(
            error.to_string().contains("package-runner"),
            "{runner}: {error}"
        );
    }
    let argv = [
        "/usr/local/bin/node".to_owned(),
        "--eval".to_owned(),
        "process.exit(0)".to_owned(),
    ];
    assert!(attested_command_paths(&argv, root.path()).is_err());
}

#[test]
fn plugin_default_cwd_is_manifest_package_not_configuration_base() {
    let (_temp, user, project) = roots();
    let package = user.join("plugins/example");
    fs::create_dir_all(&package).expect("package");
    fs::write(
        package.join("manifest.json"),
        r#"{"name":"example","version":"1.0.0","protocol":3,"capabilities":{}}"#,
    )
    .expect("manifest");
    fs::write(
        user.join(".rottweiler/plugins.toml"),
        format!(
            "[[plugins]]\nname='example'\nargv=['/usr/bin/true']\nmanifest='{}'\n",
            package.join("manifest.json").display()
        ),
    )
    .expect("config");
    let catalog = discover_executable_configs(&user, &project, false).expect("catalog");
    assert_eq!(
        catalog.plugins[0].target,
        DiscoveredPluginTarget::Executable {
            argv: vec!["/usr/bin/true".to_owned()],
            cwd: package.canonicalize().expect("package"),
        }
    );
}

#[test]
fn source_plugin_target_owns_its_entry_and_manifest_locations() {
    let (_temp, user, project) = roots();
    let package = user.join("plugins/source-example");
    fs::create_dir_all(package.join("src")).expect("package");
    fs::write(package.join("src/index.ts"), "export const plugin = {};\n").expect("entry");
    fs::write(
        package.join("manifest.json"),
        r#"{"name":"source-example","version":"1.0.0","protocol":3,"capabilities":{}}"#,
    )
    .expect("manifest");
    fs::write(
        user.join(".rottweiler/plugins.toml"),
        format!(
            "[[plugins]]\nname='source-example'\nsource='{}'\n",
            package.display()
        ),
    )
    .expect("config");
    let catalog = discover_executable_configs(&user, &project, false).expect("catalog");
    let canonical = package.canonicalize().expect("canonical package");
    assert_eq!(
        catalog.plugins[0].target,
        DiscoveredPluginTarget::TypeScript {
            package_root: canonical.clone(),
            entry: canonical.join("src/index.ts"),
        }
    );
    assert_eq!(
        catalog.plugins[0].manifest_path,
        canonical.join("manifest.json")
    );
}

#[test]
fn plugin_target_rejects_dual_or_incomplete_ownership() {
    let (_temp, user, project) = roots();
    let package = user.join("plugins/invalid");
    fs::create_dir_all(package.join("src")).expect("package");
    fs::write(package.join("src/index.ts"), "export {};\n").expect("entry");
    fs::write(package.join("manifest.json"), "{}").expect("manifest");
    for config in [
        format!(
            "[[plugins]]\nname='invalid'\nsource='{}'\nargv=['/usr/bin/true']\nmanifest='{}'\n",
            package.display(),
            package.join("manifest.json").display()
        ),
        "[[plugins]]\nname='invalid'\nargv=['/usr/bin/true']\n".to_owned(),
    ] {
        fs::write(user.join(".rottweiler/plugins.toml"), config).expect("config");
        assert!(discover_executable_configs(&user, &project, false).is_err());
    }
}
