use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, miette};
use rw_core::{
    ClientCommand, ClientId, CreateSessionRequest, EngineEvent, EngineHostConfig, ProviderApiKey,
    SequenceId, SessionId,
};
use rw_runtime::session;
use rw_types::PermissionModeDescriptor as PermissionMode;

use crate::cli_args::{Cli, DEFAULT_MAX_TURNS};
#[cfg(unix)]
use crate::remote_session::spawn_detached_server;
#[cfg(unix)]
use crate::runtime_paths::{
    RuntimeDirectoryGuard, allocate_runtime_paths, create_guarded_server_runtime,
    locate_tui_executable, resolve_server_paths, session_metadata_path,
};
use crate::trust_cli::{
    canonical_workspace_roots, configuration_root, configuration_root_path,
    ensure_configuration_root, prompt_for_folder_trust,
};
use crate::{server, shell_broker, supervisor, tui_config};

pub(super) async fn run_local_tui(cli: &Cli) -> Result<()> {
    let launch_directory =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let workspace = discover_local_workspace(&launch_directory);
    // Resolve every user-supplied relative path while the process still has
    // the invocation directory as its cwd. Repository discovery may select an
    // ancestor as the effective workspace, but that must not silently change
    // what a relative `--add-dir` names.
    let workspace_roots = canonical_workspace_roots(&workspace, &cli.add_dirs)?;
    // The supervised engine inherits its working directory. Anchor it at the
    // discovered project root so relative read/glob/tool paths remain stable
    // when `rw` is launched from a repository subdirectory.
    if workspace != launch_directory {
        std::env::set_current_dir(&workspace).into_diagnostic()?;
    }
    let storage_root = configuration_root()?;
    prompt_for_folder_trust(&storage_root, &workspace_roots, cli.dangerously_trust)?;
    let project_assessment =
        rw_store::trust::FolderTrustStore::new(storage_root.join("trust.json"))
            .assess(&workspace)
            .into_diagnostic()?;
    let project_inventory = (cli.dangerously_trust
        || project_assessment.project_execution_enabled())
    .then(|| project_assessment.inventory());
    let (user_home, user_rottweiler) =
        session::extension_user_roots(&storage_root.join("credentials.toml"));
    let tui_keybindings = tui_config::load_keybindings(
        Some(&workspace),
        project_inventory,
        &user_home,
        &user_rottweiler,
    )
    .map_err(|error| miette!(error.to_string()))?;
    let tui_theme = rw_store::config::ConfigLoader::from_environment()
        .into_diagnostic()?
        .with_project_trust(cli.dangerously_trust || project_assessment.project_execution_enabled())
        .load()
        .into_diagnostic()?
        .config
        .ui
        .theme;
    let session_id = session::select_interactive_session(
        &storage_root,
        &workspace,
        cli.resume.as_deref(),
        cli.continue_latest,
    )?;
    if (cli.resume.is_some() || cli.continue_latest)
        && !session_metadata_path(&storage_root, &session_id).is_file()
    {
        return Err(miette!("session {session_id:?} does not exist"));
    }
    let paths = allocate_runtime_paths(&storage_root)?;
    let mut runtime_directory = RuntimeDirectoryGuard::capture(&paths.directory)?;
    let supervisor = supervisor::Supervisor::new(
        supervisor::SupervisorConfig {
            rw_executable: std::env::current_exe().into_diagnostic()?,
            tui_executable: locate_tui_executable()?,
            socket: paths.socket,
            token_file: paths.token,
            last_seen_file: paths.directory.join("last-seen"),
            fork_operation_directory: storage_root.join("control/pending-forks"),
            session_id,
            tui_keybindings,
            tui_theme,
            permission_mode: cli.permission_mode,
            max_turns: cli.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
            model: cli.model.clone(),
            additional_workspaces: workspace_roots.into_iter().skip(1).collect(),
            dangerously_trust: cli.dangerously_trust,
            in_memory_replay_script: cli.in_memory_replay_script.clone(),
            record_script_delay_ms: cli.record_script_delay_ms.unwrap_or_default(),
            shell_target: Some(shell_broker::ShellTarget::Local),
            detach: cli.detach,
            restart_policy: supervisor::RestartPolicy::default(),
        },
        supervisor::TokioProcessBackend,
        supervisor::ResumeHandoff::default(),
    )
    .map_err(|error| miette!(error.to_string()))?;
    let result = supervisor
        .run()
        .await
        .map_err(|error| miette!(error.to_string()));
    if cli.detach && result.is_ok() {
        runtime_directory.preserve();
    }
    result
}

pub(super) fn discover_local_workspace(launch_directory: &Path) -> PathBuf {
    launch_directory
        .ancestors()
        .find(|directory| {
            fs::symlink_metadata(directory.join(".git"))
                .is_ok_and(|metadata| metadata.is_dir() || metadata.is_file())
        })
        .unwrap_or(launch_directory)
        .to_path_buf()
}

#[derive(Clone, Default)]
pub(super) struct DeferredHostedEngine {
    pub(super) inner: Arc<RwLock<Option<Arc<dyn server::ServerEngine>>>>,
    pub(super) ready: Arc<AtomicBool>,
}

impl DeferredHostedEngine {
    pub(super) fn install(&self, engine: server::HostedEngine) {
        self.install_engine(Arc::new(engine));
    }

    pub(super) fn install_engine(&self, engine: Arc<dyn server::ServerEngine>) {
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(engine);
        self.ready.store(true, Ordering::Release);
    }

    pub(super) fn loaded(&self) -> std::result::Result<Arc<dyn server::ServerEngine>, String> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "engine session runtime is still starting".to_owned())
    }
}

#[async_trait]
impl server::ServerEngine for DeferredHostedEngine {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    async fn dispatch(
        &self,
        bound_client: ClientId,
        command: ClientCommand,
    ) -> std::result::Result<rw_core::HostReply, String> {
        self.loaded()?.dispatch(bound_client, command).await
    }

    async fn subscribe(
        &self,
        bound_client: ClientId,
        session_id: Option<SessionId>,
        last_seen: Option<SequenceId>,
    ) -> std::result::Result<
        tokio::sync::mpsc::Receiver<std::result::Result<EngineEvent, String>>,
        server::EventSubscriptionError,
    > {
        self.loaded()
            .map_err(server::EventSubscriptionError::Other)?
            .subscribe(bound_client, session_id, last_seen)
            .await
    }

    async fn complete_shell(
        &self,
        session_id: SessionId,
        shell_id: rw_core::ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> std::result::Result<(), String> {
        self.loaded()?
            .complete_shell(session_id, shell_id, status, captured_output)
            .await
    }

    async fn submit_provider_api_key(
        &self,
        bound_client: ClientId,
        session_id: SessionId,
        provider: String,
        api_key: ProviderApiKey,
    ) -> std::result::Result<rw_core::ProviderApiKeySubmission, String> {
        self.loaded()?
            .submit_provider_api_key(bound_client, session_id, provider, api_key)
            .await
    }

    async fn activate_provider(
        &self,
        bound_client: ClientId,
        session_id: SessionId,
        provider: String,
    ) -> std::result::Result<(), String> {
        self.loaded()?
            .activate_provider(bound_client, session_id, provider)
            .await
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_serve(
    socket: Option<PathBuf>,
    token_file: Option<PathBuf>,
    session: Option<String>,
    workspace: Option<PathBuf>,
    permission_mode: Option<PermissionMode>,
    max_turns: usize,
    model: Option<String>,
    detach: bool,
    add_dirs: Vec<PathBuf>,
    dangerously_trust: bool,
    in_memory_replay_script: Option<PathBuf>,
    record_script_delay_ms: u64,
    wait_for_execution_lease: bool,
) -> Result<()> {
    let storage_root = configuration_root_path()?;
    let paths = resolve_server_paths(socket, token_file, &storage_root)?;
    let session_id = session
        .or_else(|| std::env::var("ROTTWEILER_SESSION_ID").ok())
        .map_or_else(session::new_session_id, Ok)?;
    let workspace = workspace.unwrap_or(std::env::current_dir().into_diagnostic()?);
    let workspace_roots = canonical_workspace_roots(&workspace, &add_dirs)?;

    if detach {
        let workspace = workspace_roots[0].clone();
        return spawn_detached_server(
            &paths,
            &session_id,
            &workspace,
            permission_mode,
            max_turns,
            model.as_deref(),
            &workspace_roots[1..],
            dangerously_trust,
            wait_for_execution_lease,
        )
        .await;
    }

    let (_runtime_directory, runtime, listener) =
        create_guarded_server_runtime(paths, Some(&session_id))?;
    let deferred = DeferredHostedEngine::default();
    let state = server::ServerState::new(Arc::new(deferred.clone()), &runtime);
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let serve_task = tokio::spawn(server::serve(listener, state, shutdown_rx));
    let preparation: Result<()> = async {
        ensure_configuration_root(&storage_root)?;
        let workspace = workspace_roots[0].clone();
        let provider_mode = if let Some(script) = in_memory_replay_script.as_deref() {
            session::HostedProviderMode::DeterministicReplay {
                provider_name: "local-tui-replay".to_owned(),
                scripts: serde_json::from_slice(&fs::read(script).into_diagnostic()?)
                    .into_diagnostic()?,
                event_delay_ms: record_script_delay_ms,
            }
        } else {
            session::HostedProviderMode::Live
        };
        let options = rw_runtime::RuntimeHostOptions::from_environment(
            workspace_roots,
            dangerously_trust,
            permission_mode,
            max_turns,
            provider_mode,
            wait_for_execution_lease,
        )
        .map_err(|error| miette!(error.to_string()))?;
        let max_sessions = options.config.engine.max_concurrent_sessions;
        let factory = Arc::new(
            rw_runtime::RuntimeSessionFactory::new(options)
                .map_err(|error| miette!(error.to_string()))?,
        );
        let host = rw_runtime::HeadlessRuntimeBuilder::new(factory)
            .with_config(EngineHostConfig {
                max_sessions,
                ..EngineHostConfig::default()
            })
            .build()
            .map_err(|error| miette!(error.to_string()))?;
        // The authenticated control plane is usable as soon as the host and
        // its bounded registries exist. Session composition and provider
        // discovery must never gate health or make the supervisor kill an
        // otherwise healthy engine after 30s.
        let resume = session_metadata_path(&storage_root, &session_id).is_file();
        let hosted = server::HostedEngine::new(host.clone());
        host.prepare_session_after_reservation(
            CreateSessionRequest {
                session_id: SessionId(session_id),
                workspace: workspace.display().to_string(),
                model: model.map(rw_core::ModelAlias),
            },
            resume,
            || deferred.install(hosted),
        )
        .await
        .map_err(|error| miette!(error.to_string()))?;
        Ok(())
    }
    .await;
    match preparation {
        Ok(()) => {}
        Err(error) => {
            let _ = shutdown.send(true);
            serve_task.await.into_diagnostic()??;
            return Err(error);
        }
    }
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown.send(true);
        }
    });
    serve_task.await.into_diagnostic()?
}
