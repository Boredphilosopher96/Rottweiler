use super::credential_resolution::DeferredToolProxy;
use super::credential_resolution::ResolvedToolProxy;
use super::secret_redaction::SharedCommandFixtureRedactor;
use super::tool_composition::command_mode_can_open_proxy;
use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_providers::FixtureRedactor;
use rw_tools::CancellationToken;
use rw_tools::CommandExecutor;
use rw_tools::CommandOutcome as ToolCommandOutcome;
use rw_tools::CommandRequest;
use rw_tools::CommandSafetyClassifier;
use rw_tools::ExecutionLease;
use rw_tools::NetworkPolicy as SandboxNetworkPolicy;
use rw_tools::RecordingCommandExecutor;
use rw_tools::ReplayCommandExecutor;
use rw_tools::SandboxPolicy;
use rw_tools::SandboxSupport;
use rw_tools::TokioCommandExecutor;
use rw_tools::ToolError;
use rw_tools::ToolOutputSink;
use rw_tools::probe_policy_egress;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OnceCell;

#[derive(Clone)]
pub(super) enum CommandFixtureMode {
    Live,
    Record {
        directory: PathBuf,
        redactor: FixtureRedactor,
    },
    Replay {
        directory: PathBuf,
    },
    Offline,
}

pub(super) const READ_ONLY_HOOK_COMMAND_FIXTURE_NAMESPACE: &str = "read-only-hooks";

pub(super) fn command_fixture_namespace(
    mode: CommandFixtureMode,
    namespace: &str,
) -> CommandFixtureMode {
    match mode {
        CommandFixtureMode::Record {
            directory,
            redactor,
        } => CommandFixtureMode::Record {
            directory: directory.join(namespace),
            redactor,
        },
        CommandFixtureMode::Replay { directory } => CommandFixtureMode::Replay {
            directory: directory.join(namespace),
        },
        CommandFixtureMode::Live => CommandFixtureMode::Live,
        CommandFixtureMode::Offline => CommandFixtureMode::Offline,
    }
}

pub(super) fn build_command_executor(
    workspace_roots: &[PathBuf],
    workspace: &Path,
    command_fixture_mode: CommandFixtureMode,
    execution_lease: &Arc<ExecutionLease>,
    command_safety: &Arc<CommandSafetyClassifier>,
    global_proxy: Option<&ResolvedToolProxy>,
) -> Result<Arc<dyn CommandExecutor>> {
    let scratch = PrivateScratch::create("sandbox")?;
    let mut sandbox_roots = workspace_roots.to_vec();
    sandbox_roots.push(scratch.path().to_path_buf());
    let sandbox_policy = Arc::new(
        SandboxPolicy::new(&sandbox_roots, SandboxNetworkPolicy::Deny)
            .map_err(|error| miette!("OS sandbox policy could not be built: {error}"))?,
    );
    let executor = build_command_executor_for_policy(
        &sandbox_policy,
        workspace,
        command_fixture_mode,
        execution_lease,
        command_safety,
        global_proxy,
        true,
    )?;
    Ok(Arc::new(ScratchGuardedCommandExecutor {
        inner: executor,
        _scratch: scratch,
    }))
}

pub(super) fn build_read_only_hook_executor(
    command_fixture_mode: CommandFixtureMode,
    execution_lease: &Arc<ExecutionLease>,
    command_safety: &Arc<CommandSafetyClassifier>,
) -> Result<(Arc<dyn CommandExecutor>, PathBuf)> {
    let command_fixture_mode = command_fixture_namespace(
        command_fixture_mode,
        READ_ONLY_HOOK_COMMAND_FIXTURE_NAMESPACE,
    );
    let scratch = PrivateScratch::create("hook-readonly")?;
    let sandbox_policy = Arc::new(
        SandboxPolicy::new([scratch.path()], SandboxNetworkPolicy::Deny)
            .map_err(|error| miette!("read-only hook sandbox could not be built: {error}"))?,
    );
    let executor = build_command_executor_for_policy(
        &sandbox_policy,
        scratch.path(),
        command_fixture_mode,
        execution_lease,
        command_safety,
        None,
        false,
    )?;
    let path = scratch.path().to_path_buf();
    Ok((
        Arc::new(ScratchGuardedCommandExecutor {
            inner: executor,
            _scratch: scratch,
        }),
        path,
    ))
}

pub(super) struct PrivateScratch {
    pub(super) path: PathBuf,
}

impl PrivateScratch {
    pub(super) fn create(kind: &str) -> Result<Self> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random)
            .map_err(|error| miette!("scratch randomness failed: {error}"))?;
        let path = std::env::temp_dir().join(format!(
            "rottweiler-{kind}-{}-{}",
            std::process::id(),
            u64::from_ne_bytes(random)
        ));
        create_private_sandbox_scratch(&path)?;
        Ok(Self { path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateScratch {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), %error, "scratch cleanup failed");
        }
    }
}

pub(super) struct ScratchGuardedCommandExecutor {
    pub(super) inner: Arc<dyn CommandExecutor>,
    pub(super) _scratch: PrivateScratch,
}

#[async_trait]
impl CommandExecutor for ScratchGuardedCommandExecutor {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        self.inner.settle_effects().await
    }
    fn supports_background(&self) -> bool {
        self.inner.supports_background()
    }

    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> std::result::Result<ToolCommandOutcome, ToolError> {
        self.inner.run(request, cancellation, output).await
    }
}

pub(super) struct DeferredCommandExecutor {
    pub(super) workspace_roots: Vec<PathBuf>,
    pub(super) workspace: PathBuf,
    pub(super) command_fixture_mode: CommandFixtureMode,
    pub(super) execution_lease: Arc<ExecutionLease>,
    pub(super) command_safety: Arc<CommandSafetyClassifier>,
    pub(super) global_proxy: DeferredToolProxy,
    pub(super) inner: OnceCell<Arc<dyn CommandExecutor>>,
}

impl DeferredCommandExecutor {
    pub(super) fn new(
        workspace_roots: &[PathBuf],
        workspace: &Path,
        command_fixture_mode: CommandFixtureMode,
        execution_lease: Arc<ExecutionLease>,
        command_safety: Arc<CommandSafetyClassifier>,
        global_proxy: DeferredToolProxy,
    ) -> Self {
        Self {
            workspace_roots: workspace_roots.to_vec(),
            workspace: workspace.to_path_buf(),
            command_fixture_mode,
            execution_lease,
            command_safety,
            global_proxy,
            inner: OnceCell::new(),
        }
    }

    pub(super) async fn inner(&self) -> std::result::Result<&Arc<dyn CommandExecutor>, ToolError> {
        self.inner
            .get_or_try_init(|| async {
                let proxy = self
                    .global_proxy
                    .resolve()
                    .await
                    .map_err(ToolError::Command)?;
                let workspace_roots = self.workspace_roots.clone();
                let workspace = self.workspace.clone();
                let command_fixture_mode = self.command_fixture_mode.clone();
                let execution_lease = Arc::clone(&self.execution_lease);
                let command_safety = Arc::clone(&self.command_safety);
                tokio::task::spawn_blocking(move || {
                    build_command_executor(
                        &workspace_roots,
                        &workspace,
                        command_fixture_mode,
                        &execution_lease,
                        &command_safety,
                        Some(&proxy),
                    )
                    .map_err(|error| ToolError::Command(error.to_string()))
                })
                .await
                .map_err(|error| {
                    ToolError::Command(format!("command startup worker failed: {error}"))
                })?
            })
            .await
    }
}

#[async_trait]
impl CommandExecutor for DeferredCommandExecutor {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        if let Some(inner) = self.inner.get() {
            inner.settle_effects().await?;
        }
        Ok(())
    }
    fn supports_background(&self) -> bool {
        matches!(
            self.command_fixture_mode,
            CommandFixtureMode::Live | CommandFixtureMode::Record { .. }
        )
    }

    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> std::result::Result<ToolCommandOutcome, ToolError> {
        self.inner().await?.run(request, cancellation, output).await
    }
}

pub(super) fn build_command_executor_for_policy(
    sandbox_policy: &Arc<SandboxPolicy>,
    workspace: &Path,
    command_fixture_mode: CommandFixtureMode,
    execution_lease: &Arc<ExecutionLease>,
    command_safety: &Arc<CommandSafetyClassifier>,
    global_proxy: Option<&ResolvedToolProxy>,
    allow_policy_egress: bool,
) -> Result<Arc<dyn CommandExecutor>> {
    // Each approved live command receives its own supervised proxy. macOS
    // binds Seatbelt to its exact port; Linux exposes that port only inside a
    // disposable user/network namespace and relays over a private Unix socket.
    // Replay/offline never probes, resolves credentials, or binds sockets.
    let policy_egress_available = allow_policy_egress
        && command_mode_can_open_proxy(&command_fixture_mode)
        && probe_policy_egress().support == SandboxSupport::Enforced;
    let live_command_executor = || -> Arc<dyn CommandExecutor> {
        Arc::new(
            TokioCommandExecutor::with_execution_lease(Arc::clone(execution_lease))
                .sandboxed(Arc::clone(sandbox_policy))
                .with_command_safety(Arc::clone(command_safety))
                .with_policy_egress(policy_egress_available)
                .with_upstream_proxy(global_proxy.map(|proxy| proxy.upstream.clone())),
        )
    };
    match command_fixture_mode {
        CommandFixtureMode::Live => Ok(live_command_executor()),
        CommandFixtureMode::Record {
            directory,
            redactor,
        } => RecordingCommandExecutor::new_with_redactor(
            live_command_executor(),
            directory,
            workspace,
            Arc::new(SharedCommandFixtureRedactor(redactor)),
        )
        .map(|executor| Arc::new(executor) as Arc<dyn CommandExecutor>)
        .map_err(|error| miette!("command recorder could not start: {error}")),
        CommandFixtureMode::Replay { directory } => {
            ReplayCommandExecutor::load(directory, workspace)
                .map(|executor| Arc::new(executor) as Arc<dyn CommandExecutor>)
                .map_err(|error| miette!("command replay could not load: {error}"))
        }
        CommandFixtureMode::Offline => ReplayCommandExecutor::empty(workspace)
            .map(|executor| Arc::new(executor) as Arc<dyn CommandExecutor>)
            .map_err(|error| miette!("offline command replay could not start: {error}")),
    }
}

pub(super) fn create_private_sandbox_scratch(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .map_err(|error| miette!("sandbox scratch directory could not be created: {error}"))?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| miette!("sandbox scratch directory could not be inspected: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(miette!(
            "sandbox scratch path must be a real directory, never a symlink"
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(miette!(
                "sandbox scratch directory must be owned by the current user"
            ));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| miette!("sandbox scratch permissions could not be secured: {error}"),
        )?;
    }
    Ok(())
}
