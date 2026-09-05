//! Actual SDK → RPC → actor callback ownership, with native sandbox execution.
#![allow(clippy::expect_used)]
use super::{HostedProviderMode, HostedSessionComposition, compose_hosted_actor};
use crate::{
    extension_config::discover_executable_configs, extension_runtime::PrivatePluginApprovalStore,
    journal_service::JournalService,
};
use rw_types::{
    PermissionModeDescriptor, SessionId, SessionMode,
    config::{Config, ProviderConfig},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

async fn bundle_fixture(root: &Path) -> (PathBuf, rw_plugin_protocol::PluginManifest) {
    let bun = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|path| path.join("bun"))
        .find(|path| path.is_file())
        .expect("pinned Bun on PATH");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/plugin-sdk/fixtures/conformance/command-session.ts");
    let output = root.join("plugin.js");
    let build = tokio::process::Command::new(&bun)
        .args(["build", "--target=bun"])
        .arg(source)
        .arg("--outfile")
        .arg(&output)
        .kill_on_drop(true)
        .output()
        .await
        .expect("bundle SDK fixture");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let manifest = tokio::process::Command::new(&bun)
        .arg(&output)
        .arg("--manifest")
        .kill_on_drop(true)
        .output()
        .await
        .expect("fixture manifest");
    assert!(
        manifest.status.success(),
        "{}",
        String::from_utf8_lossy(&manifest.stderr)
    );
    (
        bun,
        rw_plugin_protocol::PluginManifest::from_slice(&manifest.stdout).expect("typed manifest"),
    )
}

#[tokio::test]
async fn sdk_command_controls_state_and_panel_reenter_the_live_actor() {
    let _admission = crate::native_fixture::admit().await;
    let root = tempfile::tempdir().expect("fixture root");
    let storage = root.path().join("storage");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&storage).expect("storage");
    std::fs::create_dir(&workspace).expect("workspace");
    #[cfg(unix)]
    std::fs::set_permissions(
        &storage,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private storage");
    let workspace = workspace.canonicalize().expect("workspace identity");
    let package = workspace.join("fixture");
    std::fs::create_dir(&package).expect("package");
    let (bun, manifest) = bundle_fixture(&package).await;
    let manifest_path = package.join("manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("manifest bytes"),
    )
    .expect("manifest file");
    let project = workspace.join(".rottweiler");
    std::fs::create_dir(&project).expect("project settings");
    let config_path = project.join("plugins.toml");
    let settings = serde_json::json!({"plugins":[{
        "name":"command-session", "argv":[bun, package.join("plugin.js")], "cwd":package, "manifest":manifest_path
    }]});
    std::fs::write(
        &config_path,
        toml::to_string(&settings).expect("plugin config"),
    )
    .expect("settings file");
    let discovered = discover_executable_configs(root.path(), &workspace, true).expect("discovery");
    let plugin = discovered.plugins.first().expect("configured plugin");
    let approvals = PrivatePluginApprovalStore::open(&storage).expect("approval owner");
    rw_ext::approve_plugin_launch(
        &approvals,
        &manifest,
        &plugin.executable_process_config().expect("process config"),
        &format!("project:{}", config_path.display()),
    )
    .expect("exact fixture approval");
    let mut config = Config::default();
    config.models.default = "fast".into();
    config
        .models
        .aliases
        .insert("fast".into(), vec!["fixture/base".into()]);
    config.providers.insert(
        "fixture".into(),
        ProviderConfig {
            kind: "openai_compatible".into(),
            base_url: Some("http://127.0.0.1:1/v1/chat/completions".into()),
            ..ProviderConfig::default()
        },
    );
    let journal_service = JournalService::new(&storage).expect("journal owner");
    let runtime = compose_hosted_actor(HostedSessionComposition {
        transcripts: crate::transcript_service::TranscriptReader::new(Arc::clone(&journal_service)),
        provider_admission: Arc::new(
            crate::provider_admission::DurableProviderAdmission::open(storage.clone())
                .await
                .expect("model admission"),
        ),
        plugin_runtime_budget: Arc::new(crate::extension_runtime::PluginRuntimeBudget::default()),
        wasm_workers: rw_ext::WasmWorkerPool::new(),
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        journal_service,
        workspace: workspace.clone(),
        additional_workspaces: Vec::new(),
        allowed_workspace_roots: vec![workspace],
        storage_root: storage.clone(),
        credentials_path: storage.join("credentials.json"),
        config,
        session_id: SessionId("command-session-actor".into()),
        requested_model: None,
        resume: false,
        permission_mode: Some(PermissionModeDescriptor::Strict),
        max_turns: 2,
        provider_mode: HostedProviderMode::Live,
        dangerously_trust: true,
        wait_for_execution_lease: false,
    })
    .await
    .expect("production actor composition");
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.handle.send_message("/context-panel"),
    )
    .await
    .expect("duplex command deadline")
    .expect("SDK command completed through actor");
    assert!(matches!(outcome, rw_core::MessageDisposition::Command));
    assert_eq!(
        runtime
            .handle
            .snapshot()
            .await
            .expect("actor snapshot")
            .mode,
        SessionMode::Plan
    );
    let state = runtime
        .handle
        .plugin_session_capability("command-session")
        .expect("namespace")
        .read_state()
        .await
        .expect("canonical state");
    assert!(state.revision.is_some());
    assert_eq!(state.entries.len(), 1);
    assert_eq!(state.entries[0].key, "context/items");
    let panels = runtime
        .handle
        .ui_panels()
        .await
        .expect("host-projected panels");
    assert_eq!(panels.panels.len(), 1);
    assert_eq!(panels.panels[0].revision, 1);
    runtime
        .handle
        .close()
        .await
        .expect("actor and plugin effects settled");
}
