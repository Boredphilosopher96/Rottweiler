//! Native child generations bind callbacks to the child actor, including reopen.
use super::{
    CapturingModel, CommandFixtureMode, RuntimeWorkspaceRootController,
    SharedCommandFixtureRedactor, ToolchainRuntime, UnboundQuestionAsker,
    WorkspaceRootAuthorization, test_provider_admission,
};
use crate::journal_service::JournalService;
use futures_util::StreamExt;
use rw_core::{PermissionGate, SessionActor, SessionHandle};
use rw_providers::{FixtureRedactor, Provider, ProviderEvent, ProviderRequest, ToolChoice};
use rw_tools::{
    BackgroundProcessLimits, BackgroundProcessManager, CommandSafetyClassifier, ExecutionLease,
    ReplayCommandExecutor,
};
use rw_types::{
    SessionId,
    config::{ThinkingLevel, ToolchainConfig, WebSearchConfig},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::{Arc, Mutex, RwLock},
};

fn controller(private: &Path, primary: &Path) -> RuntimeWorkspaceRootController {
    let private = private.to_path_buf();
    let primary = primary.to_path_buf();
    let checkpoint_root = private.join("child-checkpoints");
    let lease = Arc::new(
        ExecutionLease::acquire(private.join("child-execution.lock"))
            .expect("child execution lease"),
    );
    let journal_service = JournalService::new(&private).expect("journal owner");
    RuntimeWorkspaceRootController {
        native: super::super::native_registry_recipe::RootNativeBinding::Standalone,
        child_plugins: Arc::new(
            crate::extension_runtime::generations::PluginGenerationConfig {
                private_root: private.clone(),
                helper: crate::plugin_process::helper_executable().expect("test helper"),
                redactor: Arc::new(crate::extension_runtime::SharedPluginRedactor::new(
                    FixtureRedactor::default(),
                )),
                budget: Arc::new(crate::extension_runtime::PluginRuntimeBudget::default()),
                session_ui: Arc::new(crate::extension_runtime::ui::UiSessionBudget::default()),
            },
        ),
        transcripts: crate::transcript_service::TranscriptReader::new(Arc::clone(&journal_service)),
        index_pool: Arc::new(rw_tools::WorkspaceIndexPool::default()),
        journal_service,
        checkpoint_root: checkpoint_root.clone(),
        storage_root: private.clone(),
        question_asker: Arc::new(UnboundQuestionAsker),
        offline: false,
        global_proxy: None,
        deferred_global_proxy: None,
        command_fixture_mode: CommandFixtureMode::Live,
        execution_lease: lease,
        command_safety: Arc::new(CommandSafetyClassifier::default()),
        websearch_config: WebSearchConfig::default(),
        websearch_headers: BTreeMap::new(),
        deferred_websearch_headers: None,
        background_redactor: Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
        background_manager: Arc::new(BackgroundProcessManager::new(
            Arc::new(SharedCommandFixtureRedactor(FixtureRedactor::default())),
            BackgroundProcessLimits::default(),
        )),
        native_websearch_possible: false,
        trust_store_path: private.join("trust.json"),
        toolchain_config: ToolchainConfig::default(),
        toolchain_runtime: Arc::new(ToolchainRuntime::new(
            Arc::new(ReplayCommandExecutor::empty(&primary).expect("offline executor")),
            std::slice::from_ref(&primary),
        )),
        validated_wasm_hooks: Arc::from([]),
        extension_user_home: private.clone(),
        extension_user_rottweiler: private.join(".rottweiler"),
        dangerously_trust: true,
        instruction_workspace_roots: Arc::new(RwLock::new(vec![primary.clone()])),
        active_nested_instruction_sources: Arc::new(RwLock::new(BTreeSet::new())),
        pending_instruction_roots: Mutex::new(HashMap::new()),
        root_authorization: WorkspaceRootAuthorization::LocalUnrestricted,
    }
}

type CapturedProviders = Arc<Mutex<Vec<(String, Arc<dyn Provider>)>>>;
async fn child(
    owner: &RuntimeWorkspaceRootController,
    private: &Path,
    workspace: &Path,
) -> (SessionHandle, Arc<dyn Provider>) {
    let captured: CapturedProviders = Arc::default();
    let destination = captured.clone();
    let model = super::super::native_model_generations::ChildNativeModel {
        compose: Arc::new(move |providers| {
            *destination.lock().expect("provider capture") = providers;
            Arc::new(CapturingModel {
                request: Arc::default(),
            })
        }),
        redactor: FixtureRedactor::default(),
        resources: Arc::new(rw_core::NoopSessionResources),
    };
    let config = owner
        .child_config(
            private,
            &SessionId("namespace-parent".into()),
            &SessionId("namespace-child".into()),
            workspace,
            "fast",
            model,
            Arc::new(rw_core::NoopSecretRedactor),
            &PermissionGate::from_config(rw_core::PermissionConfig::default())
                .with_workspace_roots([workspace]),
            2,
            test_provider_admission(),
        )
        .await
        .expect("compose actual child generation");
    let providers = captured.lock().expect("captured providers").clone();
    assert_eq!(
        providers.len(),
        1,
        "child recipe receives its native provider"
    );
    assert_eq!(providers[0].0, "child/");
    let provider = providers[0].1.clone();
    (SessionActor::spawn(config).expect("child actor"), provider)
}

async fn entry(handle: &SessionHandle, key: &str) -> serde_json::Value {
    handle
        .plugin_session_capability("child-namespace")
        .expect("namespace capability")
        .read_state()
        .await
        .expect("canonical namespace")
        .entries
        .into_iter()
        .find(|entry| entry.key == key)
        .expect("committed entry")
        .value
}
async fn command(handle: &SessionHandle) {
    assert!(matches!(
        handle
            .send_message("/namespace")
            .await
            .expect("native command"),
        rw_core::MessageDisposition::Command
    ));
}
async fn infer(provider: &Arc<dyn Provider>) {
    let request = ProviderRequest {
        model: "fixture".into(),
        turns: Vec::new(),
        tools: Vec::new(),
        tool_choice: ToolChoice::None {},
        max_output_tokens: 16,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };
    let mut stream = provider
        .stream(request)
        .await
        .expect("child provider stream");
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        if let ProviderEvent::TextDelta { text: delta } = event.expect("provider event") {
            text.push_str(&delta);
        }
    }
    drop(stream);
    provider
        .settle_effects()
        .await
        .expect("provider effects settled");
    assert_eq!(text, "namespace-child");
}

#[tokio::test]
async fn sdk_child_provider_state_and_panels_have_private_generations_on_reopen() {
    let _admission = crate::native_fixture::admit().await;
    let root = tempfile::tempdir().expect("fixture");
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
    super::plugin_command_session::configure_plugin(
        root.path(),
        &storage,
        &workspace,
        "child-namespace",
        &["example.com"],
    )
    .await;
    let parent = super::plugin_command_session::compose_fixture_session(
        &storage,
        &workspace,
        "namespace-parent",
        false,
    )
    .await;
    command(&parent.handle).await;
    let parent_state = entry(&parent.handle, "command").await;
    assert_eq!(parent_state["session"], "namespace-parent");
    let owner = controller(&storage, &workspace);
    let (first, provider) = child(&owner, &storage, &workspace).await;
    assert!(
        first
            .ui_panels()
            .await
            .expect("fresh panels")
            .panels
            .is_empty()
    );
    command(&first).await;
    infer(&provider).await;
    let initial = entry(&first, "command").await;
    assert_eq!(initial["count"], 1);
    assert_eq!(initial["session"], "namespace-child");
    assert_ne!(
        initial["pid"], parent_state["pid"],
        "child does not reuse the parent process"
    );
    assert_eq!(
        entry(&first, "provider").await["session"],
        "namespace-child"
    );
    assert_eq!(
        first.ui_panels().await.expect("child panel").panels.len(),
        1
    );
    first.close().await.expect("close child generation");
    drop(provider);
    drop(first);
    let (reopened, provider) = child(&owner, &storage, &workspace).await;
    assert!(
        reopened
            .ui_panels()
            .await
            .expect("reopened ephemeral panels")
            .panels
            .is_empty()
    );
    command(&reopened).await;
    infer(&provider).await;
    let reopened_state = entry(&reopened, "command").await;
    assert_eq!(
        reopened_state["count"], 2,
        "child state is durable across generation replacement"
    );
    assert_ne!(
        reopened_state["pid"], initial["pid"],
        "reopen starts a new native process"
    );
    assert_eq!(entry(&reopened, "provider").await["count"], 2);
    assert_eq!(
        entry(&parent.handle, "command").await,
        parent_state,
        "child callbacks never mutate parent namespace"
    );
    assert_eq!(
        parent
            .handle
            .ui_panels()
            .await
            .expect("parent panel")
            .panels
            .len(),
        1
    );
    reopened.close().await.expect("reopened child effects");
    parent.handle.close().await.expect("parent effects");
}
