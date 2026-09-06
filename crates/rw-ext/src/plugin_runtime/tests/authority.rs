use super::*;

#[test]
fn environment_is_a_small_safe_allowlist() {
    let config = PluginProcessConfig::new(PathBuf::from("/bin/sh")).expect("shell");
    assert!(
        config
            .clone()
            .with_environment_allowlist(["LANG", "TERM"])
            .is_ok()
    );
    assert!(matches!(
        config
            .clone()
            .with_environment_allowlist(["OPENAI_API_KEY"]),
        Err(crate::plugin::PluginProcessConfigError::UnsafeEnvironmentName)
    ));
    assert!(matches!(
        config.with_environment_allowlist(["DYLD_INSERT_LIBRARIES"]),
        Err(crate::plugin::PluginProcessConfigError::UnsafeEnvironmentName)
    ));
}

#[test]
fn direct_executable_approval_identity_has_no_source_projection() {
    let config = PluginProcessConfig::new(PathBuf::from("/bin/sh")).expect("shell");
    let identity = approval_identity(&manifest(), &config, "user:fixture").expect("identity");
    let serialized = serde_json::to_value(identity).expect("approval identity JSON");
    assert!(serialized.get("source").is_none());
}

#[test]
fn executable_substitution_invalidates_identity_and_approval() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().expect("tempdir");
    let executable = root.path().join("plugin");
    std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").expect("write executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let config = PluginProcessConfig::new(&executable)
        .expect("config")
        .with_cwd(root.path())
        .expect("cwd");
    let store = MemoryApproval::default();
    approve_plugin_launch(&store, &manifest(), &config, "project:test").expect("approve");
    let replacement = root.path().join("replacement");
    std::fs::write(&replacement, b"#!/bin/sh\nexit 1\n").expect("replacement");
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700))
        .expect("chmod replacement");
    std::fs::rename(&replacement, &executable).expect("replace executable");
    assert!(config.validate_executable_identity().is_err());
}

#[test]
fn interpreted_entrypoint_and_lock_mutation_require_rediscovery_and_reapproval() {
    let root = TempDir::new().expect("tempdir");
    let entrypoint = root.path().join("plugin.js");
    let lock = root.path().join("bun.lock");
    std::fs::write(&entrypoint, "console.log('one')\n").expect("entrypoint");
    std::fs::write(&lock, "lock-v1\n").expect("lock");
    let configured = || {
        PluginProcessConfig::new(PathBuf::from("/bin/sh"))
            .expect("shell")
            .with_argv([entrypoint.clone().into_os_string()])
            .expect("argv")
            .with_cwd(root.path())
            .expect("cwd")
            .with_attested_files([entrypoint.clone(), lock.clone()])
            .expect("attestation")
    };
    let original = configured();
    let store = MemoryApproval::default();
    approve_plugin_launch(&store, &manifest(), &original, "project:interpreted").expect("approve");

    std::fs::write(&entrypoint, "console.log('two')\n").expect("mutate entrypoint");
    assert!(original.validate_executable_identity().is_err());
    let rediscovered = configured();
    assert!(matches!(
        plugin_launch_approval_requirement(
            &store,
            &manifest(),
            &rediscovered,
            "project:interpreted"
        )
        .expect("requirement"),
        ApprovalRequirement::ManifestChanged { .. }
    ));

    approve_plugin_launch(&store, &manifest(), &rediscovered, "project:interpreted")
        .expect("reapprove entrypoint");
    std::fs::write(&lock, "lock-v2\n").expect("mutate lock");
    assert!(rediscovered.validate_executable_identity().is_err());
    assert!(matches!(
        plugin_launch_approval_requirement(
            &store,
            &manifest(),
            &configured(),
            "project:interpreted"
        )
        .expect("lock requirement"),
        ApprovalRequirement::ManifestChanged { .. }
    ));
}

#[test]
fn oversized_attested_file_is_rejected_before_hashing() {
    let root = TempDir::new().expect("tempdir");
    let oversized = root.path().join("oversized.lock");
    let file = std::fs::File::create(&oversized).expect("file");
    file.set_len(256 * 1024 * 1024 + 1).expect("sparse length");
    assert!(matches!(
        PluginProcessConfig::new(PathBuf::from("/bin/sh"))
            .expect("shell")
            .with_attested_files([oversized]),
        Err(crate::plugin::PluginProcessConfigError::AttestationLimit)
    ));
}

#[test]
fn code_root_rejects_escape_symlink_and_directory_replacement() {
    let root = TempDir::new().expect("tempdir");
    let code = root.path().join("code");
    std::fs::create_dir(&code).expect("code root");
    let entrypoint = code.join("plugin.js");
    let escaped = root.path().join("escaped.js");
    std::fs::write(&entrypoint, "export {}\n").expect("entrypoint");
    std::fs::write(&escaped, "export {}\n").expect("escaped");
    let config = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
        .expect("shell")
        .with_code_root(&code)
        .expect("code root")
        .with_attested_files([entrypoint])
        .expect("contained attestation");
    assert!(
        PluginProcessConfig::new(PathBuf::from("/bin/sh"))
            .expect("shell")
            .with_code_root(&code)
            .expect("code root")
            .with_attested_files([escaped])
            .is_err()
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&code, root.path().join("code-link")).expect("symlink");
        assert!(
            PluginProcessConfig::new(PathBuf::from("/bin/sh"))
                .expect("shell")
                .with_code_root(root.path().join("code-link"))
                .is_err()
        );
    }
    std::fs::rename(&code, root.path().join("old-code")).expect("replace root");
    std::fs::create_dir(&code).expect("new code root");
    assert!(config.validate_executable_identity().is_err());
}

#[tokio::test]
async fn manifest_rejects_workspace_root_as_code_root() {
    let root = TempDir::new().expect("tempdir");
    let config = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
        .expect("shell")
        .with_cwd(root.path())
        .expect("cwd")
        .with_code_root(root.path())
        .expect("code root");
    let manifest = PluginManifest {
        name: "workspace-root-code".to_owned(),
        version: "1.0.0".to_owned(),
        protocol: rw_plugin_protocol::PROTOCOL_VERSION,
        capabilities: PluginCapabilities::default(),
    };
    let store = MemoryApproval::default();
    approve_plugin_launch(&store, &manifest, &config, "project:root-code").expect("approve");
    let result = PluginHost::launch_approved(
        &TestDirectLauncher,
        Arc::new(store),
        &config,
        "project:root-code",
        &[root.path().to_path_buf()],
        manifest,
        Arc::new(DenyPushHandler),
        Arc::new(NoopPluginBoundaryRedactor),
    )
    .await;
    let Err(error) = result else {
        panic!("workspace root cannot be relabeled as intrinsic code");
    };
    assert!(
        error
            .to_string()
            .contains("cannot expose an approved workspace root")
    );
}

#[test]
fn capability_violation_is_permanently_poisoned_and_retries_failed_kill() {
    let process = Arc::new(FakeProcess::default());
    process.kill_fails.store(true, Ordering::Release);
    let enforcer = CapabilityEnforcer::new(&manifest(), process.clone());
    let first = enforcer.check_tool("undeclared").expect_err("violation");
    assert!(first.termination_error.is_some());
    let later = enforcer
        .check_tool("fixture_tool")
        .expect_err("poisoned forever");
    assert_eq!(later, first);
    assert_eq!(process.killed.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn direct_argv_launcher_never_invokes_a_shell_implicitly() {
    let root = TempDir::new().expect("tempdir");
    let marker = root.path().join("marker");
    let config = PluginProcessConfig::new(PathBuf::from("/bin/sh"))
        .expect("shell")
        .with_argv([
            "-c",
            "read request; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}'",
        ])
        .expect("argv")
        .with_cwd(root.path())
        .expect("cwd");
    let launched = TestDirectLauncher
        .launch(
            &config,
            &PluginSandboxProfile {
                mode: PluginSandboxMode::Approved,
                capabilities: PluginCapabilities::default(),
                approved_roots: vec![root.path().to_path_buf()],
                allowed_domains: Vec::new(),
            },
        )
        .await
        .expect("direct launch");
    assert!(!marker.exists());
    launched.process.kill_tree().expect("kill direct child");
    tokio::time::timeout(Duration::from_secs(2), launched.process.reap())
        .await
        .expect("bounded reap")
        .expect("reap");
}
