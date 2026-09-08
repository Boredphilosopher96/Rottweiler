use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use miette::{IntoDiagnostic, Result, miette};
use rw_core::SessionId;
use rw_runtime::session;
use rw_types::PermissionModeDescriptor as PermissionMode;

use crate::cli_args::Cli;
#[cfg(unix)]
use crate::runtime_paths::{
    RuntimeDirectoryGuard, allocate_runtime_paths, locate_js_host_executable,
    read_private_bootstrap_token, remove_stale_forward_socket, valid_bootstrap_token,
    write_private_file_atomic,
};
use crate::trust_cli::configuration_root;
use crate::{remote, server, shell_broker, tui_config};

mod detached;

#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_detached_server(
    paths: &server::ServerRuntimePaths,
    session_id: &str,
    workspace: &Path,
    permission_mode: Option<PermissionMode>,
    max_turns: usize,
    model: Option<&str>,
    additional_workspaces: &[PathBuf],
    dangerously_trust: bool,
    wait_for_execution_lease: bool,
) -> Result<()> {
    use std::process::Stdio;

    let mut command = tokio::process::Command::new(std::env::current_exe().into_diagnostic()?);
    command
        .arg("serve")
        .arg("--socket")
        .arg(&paths.socket)
        .arg("--token-file")
        .arg(&paths.token)
        .arg("--session")
        .arg(session_id)
        .arg("--workspace")
        .arg(workspace)
        .arg("--max-turns")
        .arg(max_turns.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(mode) = permission_mode {
        command.arg("--permission-mode").arg(mode.as_str());
    }
    if let Some(model) = model {
        command.arg("--model").arg(model);
    }
    for root in additional_workspaces {
        command.arg("--add-dir").arg(root);
    }
    if dangerously_trust {
        command.arg("--dangerously-trust");
    }
    append_execution_lease_restart_flag(&mut command, wait_for_execution_lease);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    // This engine becomes independent after readiness transfer, even when the
    // invoking process itself was launched by an interactive supervisor.
    command.env_remove(crate::parent_death::SUPERVISOR_PID_ENV);
    let paths = paths.clone();
    let session_id = session_id.to_owned();
    crate::tui_session::run(move |stop| {
        Box::pin(detached::start(
            command,
            paths,
            session_id,
            stop,
            detached::announce,
        ))
    })
    .await
}

pub(super) fn append_execution_lease_restart_flag(
    command: &mut tokio::process::Command,
    wait_for_execution_lease: bool,
) {
    if wait_for_execution_lease {
        command.arg("--wait-for-execution-lease");
    }
}

struct RemoteTuiRequest {
    remote_workspace: Option<PathBuf>,
    resume: Option<String>,
    add_dirs: Vec<PathBuf>,
    dangerously_trust: bool,
    model: Option<String>,
    permission_mode: Option<PermissionMode>,
    detach: bool,
}

pub(super) async fn run_remote_tui(host: &str, cli: &Cli) -> Result<()> {
    if cli.continue_latest {
        return Err(miette!(
            "--continue is ambiguous for a remote host; use --resume <session> or the session picker"
        ));
    }
    let request = RemoteTuiRequest {
        remote_workspace: cli.remote_workspace.clone(),
        resume: cli.resume.clone(),
        add_dirs: cli.add_dirs.clone(),
        dangerously_trust: cli.dangerously_trust,
        model: cli.model.clone(),
        permission_mode: cli.permission_mode,
        detach: cli.detach,
    };
    let host = host.to_owned();
    crate::tui_session::run(move |stop| Box::pin(run_remote_session(host, request, stop))).await
}

#[allow(clippy::too_many_lines)]
async fn run_remote_session(
    host: String,
    cli: RemoteTuiRequest,
    mut stop: crate::tui_session::Stop,
) -> Result<()> {
    if stop.requested() {
        return Ok(());
    }
    let local_workspace =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let remote_workspace = cli.remote_workspace.clone().unwrap_or(local_workspace);
    let session_id = cli
        .resume
        .clone()
        .map_or_else(session::new_session_id, Ok)?;
    let storage_root = configuration_root()?;
    let local_paths = allocate_runtime_paths(&storage_root)?;
    let mut runtime_directory = RuntimeDirectoryGuard::capture(&local_paths.directory)?;
    let uid = rustix::process::geteuid().as_raw();
    let session_key = blake3::hash(session_id.as_bytes()).to_hex();
    let remote_socket = PathBuf::from(format!(
        "/tmp/rottweiler-{uid}/engine-{}/engine.sock",
        &session_key[..16]
    ));
    let config = remote::RemoteConfig {
        ssh_executable: std::env::var_os("ROTTWEILER_SSH_BIN")
            .map_or_else(|| PathBuf::from("/usr/bin/ssh"), PathBuf::from),
        host: host.clone(),
        remote_rw_executable: std::env::var_os("ROTTWEILER_REMOTE_RW")
            .map_or_else(|| PathBuf::from("/usr/local/bin/rw"), PathBuf::from),
        remote_socket,
        local_socket: local_paths.socket.clone(),
        session_id: session_id.clone(),
        remote_workspace,
        additional_workspaces: cli.add_dirs.clone(),
        dangerously_trust: cli.dangerously_trust,
        model: cli.model.clone(),
        permission_mode: cli.permission_mode,
    };
    let js_host_executable = locate_js_host_executable()?;
    let fork_operation_directory = storage_root.join("control/pending-forks");
    let (user_home, user_rottweiler) =
        session::extension_user_roots(&storage_root.join("credentials.toml"));
    // Validate all fallible local-only TUI setup before starting a detached
    // remote engine, so invalid user configuration cannot create an orphan.
    let tui_keybindings = tui_config::load_keybindings(None, None, &user_home, &user_rottweiler)
        .map_err(|error| miette!(error.to_string()))?;
    let mut remote_runtime = TokioRemoteRecoveryRuntime::new(config.clone(), local_paths.clone());
    let owned_engine = remote_runtime.ownership();
    if let Err(error) = remote::initialize_remote(&mut remote_runtime).await {
        if !cli.detach
            && let Some(attachment) = error
                .attachment
                .as_ref()
                .filter(|attachment| attachment.started)
            && let Err(shutdown_error) =
                shutdown_remote_using_runtime(&mut remote_runtime, &attachment.bootstrap_token)
                    .await
        {
            tracing::warn!(reason = %shutdown_error, "failed to roll back owned remote startup");
        }
        if let Err(cleanup) = remote_runtime.stop_tunnel().await {
            preserve_failed_runtime(&mut runtime_directory);
            return Err(miette!("{}; {cleanup}", error.message));
        }
        return Err(miette!(error.message));
    }
    let (watchdog_control, watchdog_commands) = tokio::sync::mpsc::channel(2);
    let mut watchdog = RemoteWatchdog::start(remote_runtime, watchdog_commands);
    let (broker_ready, broker_ready_rx) = tokio::sync::oneshot::channel();
    let mut broker = tokio::spawn(shell_broker::run(
        shell_broker::ShellBrokerConfig {
            socket: local_paths.socket.clone(),
            token_file: local_paths.token.clone(),
            session_id: SessionId(session_id.clone()),
            target: shell_broker::ShellTarget::Remote {
                host: host.clone(),
            },
        },
        broker_ready,
    ));
    let broker_readiness = tokio::select! {
        biased;
        () = stop.cancelled() => Ok(false),
        readiness = broker_ready_rx => match readiness {
            Ok(Ok(())) => Ok(true),
            Ok(Err(error)) => Err(miette!(error)),
            Err(error) => Err(error).into_diagnostic(),
        },
        result = watchdog.wait() => match result {
            Ok(()) => Err(miette!("remote connection watchdog stopped before broker readiness")),
            Err(error) => Err(miette!(error)),
        },
    };
    if !matches!(broker_readiness, Ok(true)) {
        broker.abort();
        let _ = broker.await;
        let remote_shutdown = finish_remote_watchdog(
            &watchdog_control,
            &mut watchdog,
            &config,
            &local_paths,
            (!cli.detach).then_some(owned_engine.as_ref()),
        )
        .await;
        if remote_shutdown.is_err() {
            preserve_failed_runtime(&mut runtime_directory);
        }
        return broker_readiness.map(|_| ()).and(remote_shutdown);
    }
    let cursor = local_paths.directory.join("last-seen");
    let mut tui = crate::tui_launch::TuiProcess::start(&crate::tui_launch::TuiLaunch {
        executable: &js_host_executable,
        socket: &local_paths.socket,
        token_file: &local_paths.token,
        session_id: &session_id,
        last_seen_file: &cursor,
        fork_operation_directory: &fork_operation_directory,
        keybindings: tui_keybindings.as_deref(),
        theme: "",
        replay: false,
    });
    let mut broker_finished = false;
    let result = tokio::select! {
        result = tui.wait() => result.into_diagnostic(),
        () = stop.cancelled() => Ok(()),
        result = &mut broker => { broker_finished = true; match result {
            Ok(Ok(())) => Err(miette!("foreground-shell broker stopped unexpectedly")),
            Ok(Err(error)) => Err(miette!(error.to_string())),
            Err(error) => Err(miette!(error.to_string())),
        }},
        result = watchdog.wait() => match result {
            Ok(()) => Err(miette!("remote connection watchdog stopped unexpectedly")),
            Err(error) => Err(miette!(error)),
        },
    };
    let tui_cleanup = tui.shutdown().await.into_diagnostic();
    if tui_cleanup.is_err() {
        preserve_failed_runtime(&mut runtime_directory);
    }
    let result = result.and(tui_cleanup);
    if !broker_finished {
        broker.abort();
        let _ = broker.await;
    }
    let remote_shutdown = finish_remote_watchdog(
        &watchdog_control,
        &mut watchdog,
        &config,
        &local_paths,
        (!cli.detach).then_some(owned_engine.as_ref()),
    )
    .await;
    if remote_shutdown.is_err() {
        preserve_failed_runtime(&mut runtime_directory);
    }
    match (result, remote_shutdown) {
        (Err(error), Err(shutdown_error)) => {
            tracing::warn!(reason = %shutdown_error, "attached remote cleanup also failed");
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), shutdown) => shutdown,
    }
}

fn preserve_failed_runtime(directory: &mut RuntimeDirectoryGuard) {
    directory.preserve();
    tracing::warn!(path = %directory.path.display(), "retained remote runtime because cleanup failed");
}

pub(super) struct RemoteWatchdog {
    task: Option<tokio::task::JoinHandle<std::result::Result<(), String>>>,
}
impl RemoteWatchdog {
    fn start(
        mut runtime: TokioRemoteRecoveryRuntime,
        commands: tokio::sync::mpsc::Receiver<remote::WatchdogCommand>,
    ) -> Self {
        Self {
            task: Some(tokio::spawn(async move {
                let result = remote::run_controlled_watchdog(
                    &mut runtime,
                    commands,
                    remote::WatchdogPolicy::default(),
                )
                .await;
                let cleanup = runtime.stop_tunnel().await;
                result.and(cleanup)
            })),
        }
    }
    fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
    }
    async fn wait(&mut self) -> std::result::Result<(), String> {
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        let result = task.await;
        self.task = None;
        result.map_err(|error| error.to_string())?
    }
}

pub(super) async fn pause_remote_watchdog(
    watchdog_control: &tokio::sync::mpsc::Sender<remote::WatchdogCommand>,
) -> Result<()> {
    let (acknowledged, paused) = tokio::sync::oneshot::channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(25),
        watchdog_control.send(remote::WatchdogCommand::Pause(acknowledged)),
    )
    .await
    .map_err(|_| miette!("remote watchdog pause timed out before attached shutdown"))?
    .map_err(|_| miette!("remote watchdog stopped before attached shutdown"))?;
    tokio::time::timeout(std::time::Duration::from_secs(25), paused)
        .await
        .map_err(|_| miette!("remote watchdog pause acknowledgement timed out"))?
        .map_err(|_| miette!("remote watchdog stopped before acknowledging attached shutdown"))?;
    Ok(())
}

pub(super) async fn finish_remote_watchdog(
    watchdog_control: &tokio::sync::mpsc::Sender<remote::WatchdogCommand>,
    watchdog: &mut RemoteWatchdog,
    config: &remote::RemoteConfig,
    paths: &server::ServerRuntimePaths,
    shutdown_if_owned: Option<&AtomicBool>,
) -> Result<()> {
    let pause = if shutdown_if_owned.is_some() && !watchdog.is_finished() {
        pause_remote_watchdog(watchdog_control).await
    } else {
        Ok(())
    };
    // Load ownership only after recovery is quiescent. A watchdog pass may
    // replace a dead user-owned engine with one created by this invocation.
    let shutdown_owned_engine =
        shutdown_if_owned.is_some_and(|owned_engine| owned_engine.load(Ordering::Acquire));
    let direct_shutdown = if shutdown_owned_engine && pause.is_ok() && !watchdog.is_finished() {
        shutdown_authenticated_remote(paths).await
    } else if shutdown_owned_engine {
        Err(miette!(
            "remote watchdog tunnel is unavailable for attached shutdown"
        ))
    } else {
        Ok(())
    };

    if !watchdog.is_finished() {
        let _ = watchdog_control
            .send(remote::WatchdogCommand::Shutdown)
            .await;
    }
    // An active recovery step owns bounded attach (15s), tunnel startup (5s),
    // and health (1s) work. Its task must reach explicit tunnel reap; aborting a
    // join after an unrelated shorter timeout would abandon that settlement.
    let settled = watchdog.wait().await.map_err(|error| miette!(error));

    if shutdown_owned_engine && direct_shutdown.is_err() {
        shutdown_remote_with_fresh_tunnel(config, paths)
            .await
            .and(settled)
    } else {
        direct_shutdown.and(settled)
    }
}

pub(super) async fn shutdown_authenticated_remote(
    paths: &server::ServerRuntimePaths,
) -> Result<()> {
    let token = read_private_bootstrap_token(&paths.token)?
        .ok_or_else(|| miette!("remote engine token disappeared before attached shutdown"))?;
    remote::shutdown_authenticated_host(&paths.socket, &token, std::time::Duration::from_secs(5))
        .await
        .map_err(|error| miette!(error))
}

pub(super) async fn shutdown_remote_using_runtime(
    runtime: &mut TokioRemoteRecoveryRuntime,
    bootstrap_token: &str,
) -> Result<()> {
    let direct = remote::shutdown_authenticated_host(
        &runtime.paths.socket,
        bootstrap_token,
        std::time::Duration::from_secs(5),
    )
    .await;
    if direct.is_err() {
        remote::RemoteRecoveryRuntime::restart_tunnel(runtime)
            .await
            .map_err(|error| miette!(error))?;
    }
    let result = if direct.is_ok() {
        Ok(())
    } else {
        remote::shutdown_authenticated_host(
            &runtime.paths.socket,
            bootstrap_token,
            std::time::Duration::from_secs(5),
        )
        .await
        .map_err(|error| miette!(error))
    };
    let cleanup = runtime.stop_tunnel().await.map_err(|error| miette!(error));
    result.and(cleanup)
}

pub(super) async fn shutdown_remote_with_fresh_tunnel(
    config: &remote::RemoteConfig,
    paths: &server::ServerRuntimePaths,
) -> Result<()> {
    let token = read_private_bootstrap_token(&paths.token)?
        .ok_or_else(|| miette!("remote engine token disappeared before fallback shutdown"))?;
    let mut runtime = TokioRemoteRecoveryRuntime::new(config.clone(), paths.clone());
    shutdown_remote_using_runtime(&mut runtime, &token).await
}

pub(super) struct TokioRemoteRecoveryRuntime {
    pub(super) config: remote::RemoteConfig,
    pub(super) paths: server::ServerRuntimePaths,
    pub(super) tunnel: Option<tokio::process::Child>,
    pub(super) owned_engine: Arc<AtomicBool>,
}

impl TokioRemoteRecoveryRuntime {
    const HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

    pub(super) fn new(config: remote::RemoteConfig, paths: server::ServerRuntimePaths) -> Self {
        Self {
            config,
            paths,
            tunnel: None,
            owned_engine: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn ownership(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.owned_engine)
    }

    pub(super) async fn stop_tunnel(&mut self) -> std::result::Result<(), String> {
        if let Some(mut child) = self.tunnel.take() {
            let _ = child.start_kill();
            child
                .wait()
                .await
                .map_err(|error| format!("SSH tunnel reap failed: {error}"))?;
        }
        Ok(())
    }
}

#[async_trait]
impl remote::RemoteRecoveryRuntime for TokioRemoteRecoveryRuntime {
    async fn authenticated_health(&mut self) -> std::result::Result<bool, String> {
        let Some(token) =
            read_private_bootstrap_token(&self.paths.token).map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        match remote::probe_authenticated_health(&self.paths.socket, &token, Self::HEALTH_TIMEOUT)
            .await
        {
            Ok(healthy) => Ok(healthy),
            Err(error) => {
                tracing::debug!(reason = %error, "forwarded remote engine health probe failed");
                Ok(false)
            }
        }
    }

    async fn tunnel_alive(&mut self) -> std::result::Result<bool, String> {
        let Some(tunnel) = self.tunnel.as_mut() else {
            return Ok(false);
        };
        let exited = tunnel
            .try_wait()
            .map_err(|error| format!("could not inspect SSH forwarding process: {error}"))?
            .is_some();
        if exited {
            self.tunnel = None;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    async fn restart_tunnel(&mut self) -> std::result::Result<(), String> {
        use std::process::Stdio;

        self.stop_tunnel().await?;
        remove_stale_forward_socket(&self.paths.socket)?;
        let forward = self
            .config
            .forward_command()
            .map_err(|error| error.to_string())?;
        let mut command = tokio::process::Command::new(&forward.program);
        command
            .args(&forward.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.as_std_mut().process_group(0);
        }
        let mut tunnel = command
            .spawn()
            .map_err(|error| format!("could not start SSH socket forwarding: {error}"))?;
        if let Err(error) = wait_for_socket_or_child(&self.paths.socket, &mut tunnel).await {
            let _ = tunnel.kill().await;
            let _ = tunnel.wait().await;
            return Err(error.to_string());
        }
        self.tunnel = Some(tunnel);
        Ok(())
    }

    async fn attach_or_start(
        &mut self,
        wait_for_execution_lease: bool,
    ) -> std::result::Result<remote::RemoteAttachment, String> {
        use std::process::Stdio;

        let start = if wait_for_execution_lease {
            self.config.engine_recovery_command()
        } else {
            self.config.engine_start_command()
        }
        .map_err(|error| error.to_string())?;
        let mut command = tokio::process::Command::new(&start.program);
        command
            .args(&start.args)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let output = read_remote_readiness(&mut command).await?;
        if !output.status.success() {
            return Err(format!(
                "remote engine attach-or-start failed with SSH status {}",
                output.status
            ));
        }
        let ready: DetachedServerReady = serde_json::from_slice(&output.stdout)
            .map_err(|_| "remote engine returned an invalid readiness descriptor".to_owned())?;
        if ready.version != 1
            || ready.socket != self.config.remote_socket
            || ready.session_id != self.config.session_id
            || !valid_bootstrap_token(&ready.token)
        {
            return Err("remote engine readiness descriptor failed validation".to_owned());
        }
        if ready.started {
            self.owned_engine.store(true, Ordering::Release);
        }
        Ok(remote::RemoteAttachment {
            bootstrap_token: ready.token,
            started: ready.started,
        })
    }

    async fn install_bootstrap_token(&mut self, token: &str) -> std::result::Result<(), String> {
        if !valid_bootstrap_token(token) {
            return Err("refusing to install invalid remote bootstrap token".to_owned());
        }
        write_private_file_atomic(&self.paths.token, token.as_bytes())
            .map_err(|error| error.to_string())
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DetachedServerReady {
    pub(super) version: u16,
    pub(super) socket: PathBuf,
    pub(super) token: String,
    pub(super) session_id: String,
    pub(super) started: bool,
}

pub(super) async fn wait_for_socket_or_child(
    socket: &Path,
    child: &mut tokio::process::Child,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if fs::symlink_metadata(socket).is_ok() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().into_diagnostic()? {
            return Err(miette!(
                "SSH socket forwarding exited before becoming ready with status {status}"
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(miette!(
                "SSH socket forwarding did not become ready within 5 seconds"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

// Readiness contains only the bounded session/socket/token descriptor. It is not
// an SSH transcript and must not collect arbitrary remote stdout.
const MAX_REMOTE_READINESS_BYTES: u64 = 64 * 1024;

async fn read_remote_readiness(
    command: &mut tokio::process::Command,
) -> std::result::Result<std::process::Output, String> {
    use tokio::io::AsyncReadExt as _;
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run remote attach-or-start command: {error}"))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.start_kill();
        child
            .wait()
            .await
            .map_err(|error| format!("remote startup reap failed: {error}"))?;
        return Err("remote startup stdout was not captured".into());
    };
    let mut bytes = Vec::new();
    let operation = async {
        stdout
            .take(MAX_REMOTE_READINESS_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| format!("remote readiness read failed: {error}"))?;
        if bytes.len() as u64 > MAX_REMOTE_READINESS_BYTES {
            return Err("remote readiness exceeds 64KiB descriptor limit".into());
        }
        child
            .wait()
            .await
            .map_err(|error| format!("remote startup wait failed: {error}"))
    };
    let result = match tokio::time::timeout(std::time::Duration::from_secs(15), operation).await {
        Ok(result) => result,
        Err(_) => Err("remote attach-or-start command timed out".into()),
    };
    match result {
        Ok(status) => Ok(std::process::Output {
            status,
            stdout: bytes,
            stderr: Vec::new(),
        }),
        Err(error) => {
            let _ = child.start_kill();
            child
                .wait()
                .await
                .map_err(|reap| format!("{error}; remote startup reap failed: {reap}"))?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod lifecycle_tests;
