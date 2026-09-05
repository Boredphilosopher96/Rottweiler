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

async fn bundle_fixture(
    root: &Path,
    fixture: &str,
) -> (PathBuf, rw_plugin_protocol::PluginManifest) {
    let bun = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|path| path.join("bun"))
        .find(|path| path.is_file())
        .expect("pinned Bun on PATH");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../packages/plugin-sdk/fixtures/conformance/{fixture}.ts"
    ));
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

pub(super) async fn configure_plugin(root: &Path, storage: &Path, workspace: &Path, fixture: &str) {
    let package = workspace.join("fixture");
    std::fs::create_dir(&package).expect("package");
    let (bun, manifest) = bundle_fixture(&package, fixture).await;
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
        "name":manifest.name, "argv":[bun, package.join("plugin.js")], "cwd":package, "manifest":manifest_path
    }]});
    std::fs::write(
        &config_path,
        toml::to_string(&settings).expect("plugin config"),
    )
    .expect("settings file");
    let discovered = discover_executable_configs(root, workspace, true).expect("discovery");
    let plugin = discovered.plugins.first().expect("configured plugin");
    let approvals = PrivatePluginApprovalStore::open(storage).expect("approval owner");
    rw_ext::approve_plugin_launch(
        &approvals,
        &manifest,
        &plugin.executable_process_config().expect("process config"),
        &format!("project:{}", config_path.display()),
    )
    .expect("exact fixture approval");
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
    std::fs::write(workspace.join("broker.txt"), "broker owned bytes").expect("broker input");
    configure_plugin(root.path(), &storage, &workspace, "command-session").await;
    let runtime =
        compose_fixture_session(&storage, &workspace, "command-session-actor", false).await;
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
    verify_root_recomposition(&runtime, &workspace, state.revision).await;
    super::plugin_navigation::verify_deferred_navigation(&runtime.handle).await;
    runtime
        .handle
        .close()
        .await
        .expect("actor and plugin effects settled");
}

async fn verify_root_recomposition(
    runtime: &super::super::runtime_options::HostedActorRuntime,
    workspace: &Path,
    previous_revision: Option<rw_types::SequenceId>,
) {
    let old_catalog = runtime
        .handle
        .ui_catalog()
        .await
        .expect("initial UI catalog");
    let old_generation =
        rw_core::ModelCatalogSource::generation(runtime.model_generations.as_ref());
    let added = workspace.join("additional");
    std::fs::create_dir(&added).expect("new workspace root");
    runtime
        .handle
        .send_message(format!("/add-dir {}", added.display()))
        .await
        .expect("publish complete root generation");
    assert_eq!(
        runtime
            .handle
            .snapshot()
            .await
            .expect("published root snapshot")
            .workspace_roots
            .len(),
        2
    );
    assert!(
        rw_core::ModelCatalogSource::generation(runtime.model_generations.as_ref())
            > old_generation
    );
    let catalog = runtime
        .handle
        .ui_catalog()
        .await
        .expect("replacement catalog");
    assert_eq!(catalog.entries.len(), old_catalog.entries.len());
    assert_ne!(
        catalog.entries[0].owner.generation,
        old_catalog.entries[0].owner.generation
    );
    assert!(
        runtime
            .handle
            .ui_panels()
            .await
            .expect("fresh panel state")
            .panels
            .is_empty()
    );
    assert!(
        runtime
            .handle
            .command_descriptors()
            .iter()
            .any(|entry| entry.name() == "context-panel")
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        runtime.handle.send_message("/context-panel"),
    )
    .await
    .expect("replacement callback deadline")
    .expect("new plugin generation is bound to live actor");
    let state = runtime
        .handle
        .plugin_session_capability("command-session")
        .expect("namespace")
        .read_state()
        .await
        .expect("canonical state survives process retirement");
    assert!(state.revision > previous_revision);
    assert_eq!(state.entries.len(), 1);
    assert_eq!(
        runtime
            .handle
            .ui_panels()
            .await
            .expect("replacement panel")
            .panels
            .len(),
        1
    );
}

pub(super) async fn compose_fixture_session(
    storage: &Path,
    workspace: &Path,
    session_id: &str,
    resume: bool,
) -> super::super::runtime_options::HostedActorRuntime {
    let storage = storage.to_path_buf();
    let workspace = workspace.to_path_buf();
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
    compose_hosted_actor(HostedSessionComposition {
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
        allowed_workspace_roots: vec![workspace.clone()],
        storage_root: storage.clone(),
        credentials_path: storage.join("credentials.json"),
        config,
        session_id: SessionId(session_id.to_owned()),
        requested_model: None,
        resume,
        permission_mode: Some(PermissionModeDescriptor::Strict),
        max_turns: 2,
        provider_mode: HostedProviderMode::Live,
        dangerously_trust: true,
        wait_for_execution_lease: false,
    })
    .await
    .expect("production actor composition")
}
