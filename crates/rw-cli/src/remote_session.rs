use std::{
    fs,
    io::{self},
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
    RuntimeDirectoryGuard, allocate_runtime_paths, locate_tui_executable,
    read_private_bootstrap_token, remove_stale_forward_socket, runtime_artifacts_ready,
    runtime_is_live, valid_bootstrap_token, write_private_file_atomic,
};
use crate::trust_cli::configuration_root;
use crate::{remote, server, shell_broker, tui_config};

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

    if runtime_is_live(paths).await {
        let token = read_private_bootstrap_token(&paths.token)?
            .ok_or_else(|| miette!("live engine bootstrap token failed validation"))?;
        println!(
            "{}",
            serde_json::json!({
                "version": 1,
                "socket": paths.socket,
                "token": token,
                "session_id": session_id,
                "started": false,
            })
        );
        return Ok(());
    }
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
    let mut child = command.spawn().into_diagnostic()?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if runtime_artifacts_ready(paths) {
            let token = read_private_bootstrap_token(&paths.token)?
                .ok_or_else(|| miette!("new engine bootstrap token failed validation"))?;
            println!(
                "{}",
                serde_json::json!({
                    "version": 1,
                    "socket": paths.socket,
                    "token": token,
                    "session_id": session_id,
                    "started": true,
                })
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait().into_diagnostic()? {
            return Err(miette!(
                "detached engine exited before becoming ready with status {status}"
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            return Err(miette!(
                "detached engine did not become ready within 5 seconds"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

pub(super) fn append_execution_lease_restart_flag(
    command: &mut tokio::process::Command,
    wait_for_execution_lease: bool,
) {
    if wait_for_execution_lease {
        command.arg("--wait-for-execution-lease");
    }
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_remote_tui(host: &str, cli: &Cli) -> Result<()> {
    if cli.continue_latest {
        return Err(miette!(
            "--continue is ambiguous for a remote host; use --resume <session> or the session picker"
        ));
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
    let _runtime_directory = RuntimeDirectoryGuard::capture(&local_paths.directory)?;
    let uid = rustix::process::geteuid().as_raw();
    let session_key = blake3::hash(session_id.as_bytes()).to_hex();
    let remote_socket = PathBuf::from(format!(
        "/tmp/rottweiler-{uid}/engine-{}/engine.sock",
        &session_key[..16]
    ));
    let config = remote::RemoteConfig {
        ssh_executable: std::env::var_os("ROTTWEILER_SSH_BIN")
            .map_or_else(|| PathBuf::from("/usr/bin/ssh"), PathBuf::from),
        host: host.to_owned(),
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
    let tui_executable = locate_tui_executable()?;
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
        return Err(miette!(error.message));
    }
    let (watchdog_control, watchdog_commands) = tokio::sync::mpsc::channel(2);
    let mut watchdog = tokio::spawn(remote::run_controlled_watchdog(
        remote_runtime,
        watchdog_commands,
        remote::WatchdogPolicy::default(),
    ));
    let (broker_ready, broker_ready_rx) = tokio::sync::oneshot::channel();
    let mut broker = tokio::spawn(shell_broker::run(
        shell_broker::ShellBrokerConfig {
            socket: local_paths.socket.clone(),
            token_file: local_paths.token.clone(),
            session_id: SessionId(session_id.clone()),
            target: shell_broker::ShellTarget::Remote {
                host: host.to_owned(),
            },
        },
        broker_ready,
    ));
    let broker_readiness = tokio::select! {
        readiness = broker_ready_rx => match readiness {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(miette!(error)),
            Err(error) => Err(error).into_diagnostic(),
        },
        result = &mut watchdog => {
            broker.abort();
            match result {
                Ok(Ok(())) => Err(miette!("remote connection watchdog stopped before broker readiness")),
                Ok(Err(error)) => Err(miette!(error)),
                Err(error) => Err(miette!(error.to_string())),
            }
        }
    };
    if let Err(error) = broker_readiness {
        broker.abort();
        let remote_shutdown = finish_remote_watchdog(
            &watchdog_control,
            &mut watchdog,
            &config,
            &local_paths,
            (!cli.detach).then_some(owned_engine.as_ref()),
        )
        .await;
        if let Err(shutdown_error) = remote_shutdown {
            tracing::warn!(reason = %shutdown_error, "attached remote cleanup also failed");
        }
        return Err(error);
    }
    let tui = run_remote_tui_process(
        tui_executable,
        &local_paths,
        &fork_operation_directory,
        &session_id,
        tui_keybindings.as_deref(),
    );
    tokio::pin!(tui);
    let result = tokio::select! {
        result = &mut tui => result,
        result = &mut broker => match result {
            Ok(Ok(())) => Err(miette!("foreground-shell broker stopped unexpectedly")),
            Ok(Err(error)) => Err(miette!(error.to_string())),
            Err(error) => Err(miette!(error.to_string())),
        },
        result = &mut watchdog => match result {
            Ok(Ok(())) => Err(miette!("remote connection watchdog stopped unexpectedly")),
            Ok(Err(error)) => Err(miette!(error)),
            Err(error) => Err(miette!(error.to_string())),
        },
    };
    broker.abort();
    let remote_shutdown = finish_remote_watchdog(
        &watchdog_control,
        &mut watchdog,
        &config,
        &local_paths,
        (!cli.detach).then_some(owned_engine.as_ref()),
    )
    .await;
    match (result, remote_shutdown) {
        (Err(error), Err(shutdown_error)) => {
            tracing::warn!(reason = %shutdown_error, "attached remote cleanup also failed");
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), shutdown) => shutdown,
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
    watchdog: &mut tokio::task::JoinHandle<std::result::Result<(), String>>,
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
        if tokio::time::timeout(std::time::Duration::from_secs(2), &mut *watchdog)
            .await
            .is_err()
        {
            watchdog.abort();
            let _ = watchdog.await;
        }
    }

    if shutdown_owned_engine && direct_shutdown.is_err() {
        shutdown_remote_with_fresh_tunnel(config, paths).await
    } else {
        direct_shutdown
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
    runtime.stop_tunnel().await;
    result
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

    pub(super) async fn stop_tunnel(&mut self) {
        if let Some(mut child) = self.tunnel.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
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

        self.stop_tunnel().await;
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
        let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
            .await
            .map_err(|_| "remote attach-or-start command timed out".to_owned())?
            .map_err(|error| format!("could not run remote attach-or-start command: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "remote engine attach-or-start failed with SSH status {}",
                output.status
            ));
        }
        let ready: DetachedServerReady = serde_json::from_slice(&output.stdout)
            .map_err(|_| "remote engine returned an invalid readiness descriptor".to_owned())?;
        if ready.version != 1
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DetachedServerReady {
    pub(super) version: u16,
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

pub(super) async fn run_remote_tui_process(
    tui: PathBuf,
    paths: &server::ServerRuntimePaths,
    fork_operation_directory: &Path,
    session_id: &str,
    keybindings: Option<&str>,
) -> Result<()> {
    use std::process::Stdio;

    let cursor = paths.directory.join("last-seen");
    for attempt in 0..=5_u8 {
        let mut command = tokio::process::Command::new(&tui);
        command
            .env_remove("ROTTWEILER_TUI_KEYBINDINGS")
            .env("ROTTWEILER_ENGINE_SOCKET", &paths.socket)
            .env("ROTTWEILER_ENGINE_TOKEN_FILE", &paths.token)
            .env("ROTTWEILER_SESSION_ID", session_id)
            .env("ROTTWEILER_LAST_SEEN_FILE", &cursor)
            .env(
                "ROTTWEILER_FORK_OPERATION_DIRECTORY",
                fork_operation_directory,
            )
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(keybindings) = keybindings {
            command.env("ROTTWEILER_TUI_KEYBINDINGS", keybindings);
        }
        let mut child = command.spawn().into_diagnostic()?;
        let status = tokio::select! {
            status = child.wait() => status.into_diagnostic()?,
            interrupted = wait_for_remote_shutdown_signal() => {
                interrupted.into_diagnostic()?;
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Ok(());
            }
        };
        if status.success() {
            return Ok(());
        }
        if attempt == 5 {
            return Err(miette!("remote TUI restart budget exhausted"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(
            50_u64.saturating_mul(1_u64 << attempt),
        ))
        .await;
    }
    Err(miette!("remote TUI stopped unexpectedly"))
}

#[cfg(unix)]
pub(super) async fn wait_for_remote_shutdown_signal() -> io::Result<()> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    tokio::select! {
        _ = interrupt.recv() => Ok(()),
        _ = terminate.recv() => Ok(()),
        _ = hangup.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
pub(super) async fn wait_for_remote_shutdown_signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}
