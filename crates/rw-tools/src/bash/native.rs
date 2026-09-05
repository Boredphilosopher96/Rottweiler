use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rw_sandbox::{
    EgressPolicy, NetworkPolicy as SandboxNetworkPolicy, SandboxPolicy, SupervisedEgressProxy,
    UpstreamProxy, shell_launch_plan,
};
use rw_types::ToolOutputStream;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use crate::registry::{CancellationToken, ToolError, ToolOutputSink};

use super::{BashSandboxMode, CommandExecutor, CommandOutcome, CommandRequest};

use super::safety::{
    CommandSafety, CommandSafetyClassifier, audited_bat, audited_system_git,
    audited_system_read_command, built_in_safe_segment, classify_safe_command, safe_bat_arguments,
    safe_command_segments, safe_git_diff_arguments, safe_git_status_arguments,
};

use super::execution_lease::ExecutionLease;
use super::watchdog::{ParentDeathWatchdog, arm_parent_death_watchdog};

use super::output::{copy_stream, finish_command_output};

use super::process_group::{terminate_and_wait_process_group, terminate_process_group};

/// Un-sandboxed M2 executor. Its tool manifest explicitly declares every ambient capability.
#[derive(Clone, Debug, Default)]
pub struct TokioCommandExecutor {
    native_cleanup: Arc<NativeCleanup>,
    execution_lease: Option<Arc<ExecutionLease>>,
    sandbox: Option<Arc<SandboxPolicy>>,
    policy_egress_available: bool,
    upstream_proxy: Option<UpstreamProxy>,
    safety: Arc<CommandSafetyClassifier>,
    #[cfg(test)]
    proxy_lifecycles: Option<Arc<Mutex<Vec<rw_sandbox::ProxyLifecycle>>>>,
    #[cfg(all(test, unix))]
    launch_gate_hook: Option<Arc<LaunchGateTestHook>>,
}

#[cfg(all(test, unix))]
#[derive(Debug, Default)]
pub(super) struct LaunchGateTestHook {
    child_id: std::sync::atomic::AtomicU32,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(all(test, unix))]
impl LaunchGateTestHook {
    pub(super) async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    pub(super) fn release(&self) {
        self.release.notify_one();
    }

    pub(super) fn child_id(&self) -> Option<rustix::process::Pid> {
        let raw_pid =
            i32::try_from(self.child_id.load(std::sync::atomic::Ordering::Acquire)).ok()?;
        rustix::process::Pid::from_raw(raw_pid)
    }
}

impl TokioCommandExecutor {
    /// Retains the session execution lease for this process boundary.
    #[must_use]
    pub fn with_execution_lease(execution_lease: Arc<ExecutionLease>) -> Self {
        Self {
            native_cleanup: Arc::default(),
            execution_lease: Some(execution_lease),
            sandbox: None,
            policy_egress_available: false,
            upstream_proxy: None,
            safety: Arc::new(CommandSafetyClassifier::default()),
            #[cfg(test)]
            proxy_lifecycles: None,
            #[cfg(all(test, unix))]
            launch_gate_hook: None,
        }
    }

    /// Runs every command inside the supplied native OS sandbox.
    #[must_use]
    pub fn sandboxed(mut self, policy: Arc<SandboxPolicy>) -> Self {
        self.sandbox = Some(policy);
        self
    }

    /// Uses the exact classifier shared with the permission gate and bash tool.
    #[must_use]
    pub fn with_command_safety(mut self, safety: Arc<CommandSafetyClassifier>) -> Self {
        self.safety = safety;
        self
    }

    /// Enables per-command supervised policy proxies on a backend that can bind
    /// the child to their exact endpoint.
    #[must_use]
    pub const fn with_policy_egress(mut self, available: bool) -> Self {
        self.policy_egress_available = available;
        self
    }

    /// Chains every approved command proxy through an explicit corporate
    /// proxy after the local target policy has allowed the destination.
    #[must_use]
    pub fn with_upstream_proxy(mut self, proxy: Option<UpstreamProxy>) -> Self {
        self.upstream_proxy = proxy;
        self
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(super) fn with_proxy_lifecycle_observer(
        mut self,
        lifecycles: Arc<Mutex<Vec<rw_sandbox::ProxyLifecycle>>>,
    ) -> Self {
        self.proxy_lifecycles = Some(lifecycles);
        self
    }

    #[cfg(all(test, unix))]
    pub(super) fn with_launch_gate_hook(mut self, hook: Arc<LaunchGateTestHook>) -> Self {
        self.launch_gate_hook = Some(hook);
        self
    }
}

#[async_trait]
impl CommandExecutor for TokioCommandExecutor {
    async fn settle_effects(&self) {
        self.native_cleanup.settle().await;
    }
    fn supports_background(&self) -> bool {
        self.sandbox.is_some()
    }

    #[allow(clippy::too_many_lines)]
    async fn run(
        &self,
        request: CommandRequest,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        // Keep the deliberately inheritable lease descriptor alive while the
        // command and its watchdog are spawned.
        let _execution_lease = self.execution_lease.as_ref();
        cancellation.check()?;
        let safe = request.sandbox != BashSandboxMode::Unsandboxed
            && request.network_domains.is_empty()
            && self.safety.classify(&request.command) == CommandSafety::SafeListed;
        let built_in_read_only = request.sandbox != BashSandboxMode::Unsandboxed
            && request.network_domains.is_empty()
            && classify_safe_command(&request.command) == CommandSafety::SafeListed;
        let egress_proxy = command_egress_proxy(
            &request,
            safe,
            self.policy_egress_available,
            self.upstream_proxy.as_ref(),
        )?;
        #[cfg(test)]
        if let (Some(proxy), Some(lifecycles)) =
            (egress_proxy.as_ref(), self.proxy_lifecycles.as_ref())
        {
            lifecycles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(proxy.lifecycle());
        }
        let read_only_policy = (request.sandbox == BashSandboxMode::ReadOnly || built_in_read_only)
            .then(|| self.sandbox.as_deref().map(SandboxPolicy::read_only))
            .flatten();
        let sandbox = if request.sandbox == BashSandboxMode::ReadOnly || built_in_read_only {
            read_only_policy.as_ref()
        } else if request.sandbox == BashSandboxMode::Sandboxed {
            self.sandbox.as_deref()
        } else {
            None
        };
        let mut guarded = guarded_process(&request, sandbox, egress_proxy.as_ref())?;
        let child = guarded
            .command
            .spawn()
            .map_err(|error| ToolError::Command(error.to_string()))?;
        #[cfg(target_os = "linux")]
        drop(guarded.helper_pin.take());
        let mut owner = NativeCommandOwner {
            state: Some(NativeCommandState {
                child_id: child.id(),
                child,
                watchdog: None,
                output: None,
                _proxy: egress_proxy,
                _lease: self.execution_lease.clone(),
            }),
            cleanup: Arc::clone(&self.native_cleanup),
        };
        let result = self
            .run_owned_child(
                owner.state.as_mut().ok_or_else(|| {
                    ToolError::Command("native command owner is missing".to_owned())
                })?,
                cancellation,
                output,
            )
            .await;
        let cleanup = owner.settle().await;
        cleanup?;
        result
    }
}

impl TokioCommandExecutor {
    async fn run_owned_child(
        &self,
        native: &mut NativeCommandState,
        cancellation: CancellationToken,
        output: Arc<dyn ToolOutputSink>,
    ) -> Result<CommandOutcome, ToolError> {
        let child_id = native.child_id;
        let Some(mut launch_gate) = native.child.stdin.take() else {
            return Err(ToolError::Command(
                "command launch gate was not created".to_owned(),
            ));
        };
        arm_parent_death_watchdog(
            &mut native.watchdog,
            child_id,
            self.execution_lease.as_deref(),
        )
        .await?;
        #[cfg(all(test, unix))]
        if let Some(hook) = &self.launch_gate_hook {
            hook.child_id.store(
                child_id.unwrap_or_default(),
                std::sync::atomic::Ordering::Release,
            );
            hook.reached.notify_one();
            hook.release.notified().await;
        }
        let launch_result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ToolError::Cancelled),
            result = launch_gate.write_all(b"armed\n") => result,
        };
        launch_result.map_err(|error| {
            ToolError::Command(format!("could not release guarded command: {error}"))
        })?;
        let _ = launch_gate.shutdown().await;
        drop(launch_gate);
        let (Some(stdout), Some(stderr)) = (native.child.stdout.take(), native.child.stderr.take())
        else {
            return Err(ToolError::Command(
                "command output pipes were not created".to_owned(),
            ));
        };
        native.output = Some((
            tokio::spawn(copy_stream(
                stdout,
                ToolOutputStream::Stdout,
                Arc::clone(&output),
            )),
            tokio::spawn(copy_stream(stderr, ToolOutputStream::Stderr, output)),
        ));
        let watchdog = native
            .watchdog
            .as_mut()
            .ok_or_else(|| ToolError::Command("native watchdog is missing".to_owned()))?;
        let status = tokio::select! {
            biased;
            watchdog_status = watchdog.wait_unexpected() => {
                return Err(ToolError::Command(format!("command watchdog exited before command completion: {watchdog_status}")));
            }
            () = cancellation.cancelled() => return Err(ToolError::Cancelled),
            status = native.child.wait() => status,
        };
        let status = status.map_err(|error| ToolError::Command(error.to_string()))?;
        Ok(CommandOutcome {
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct NativeCleanup {
    pending: Mutex<Vec<tokio::sync::watch::Receiver<bool>>>,
}

impl NativeCleanup {
    fn schedule(
        &self,
        mut state: NativeCommandState,
    ) -> tokio::sync::oneshot::Receiver<Result<(), ToolError>> {
        let (completed, completion) = tokio::sync::watch::channel(false);
        let (respond, result) = tokio::sync::oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.retain(|completion| !*completion.borrow());
            pending.push(completion);
        }
        tokio::spawn(async move {
            let result = state.settle().await;
            drop(state);
            completed.send_replace(true);
            let _ = respond.send(result);
        });
        result
    }

    async fn settle(&self) {
        loop {
            let pending = {
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.retain(|completion| !*completion.borrow());
                pending.clone()
            };
            if pending.is_empty() {
                return;
            }
            for mut completion in pending {
                while !*completion.borrow_and_update() {
                    if completion.changed().await.is_err() {
                        tracing::error!("native cleanup worker exited without proving settlement");
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }
}

pub(super) type CommandOutputTasks = (
    tokio::task::JoinHandle<Result<(), ToolError>>,
    tokio::task::JoinHandle<Result<(), ToolError>>,
);

pub(super) struct NativeCommandState {
    child: Child,
    child_id: Option<u32>,
    watchdog: Option<ParentDeathWatchdog>,
    output: Option<CommandOutputTasks>,
    _proxy: Option<SupervisedEgressProxy>,
    _lease: Option<Arc<ExecutionLease>>,
}

impl NativeCommandState {
    async fn settle(&mut self) -> Result<(), ToolError> {
        settle_command_child(&mut self.child, self.child_id).await;
        self.child_id = None;
        let watchdog_result = match self.watchdog.as_mut() {
            Some(watchdog) => watchdog.disarm().await,
            None => Ok(()),
        };
        let output_result = match self.output.take() {
            Some((stdout, stderr)) => finish_command_output(stdout, stderr).await,
            None => Ok(()),
        };
        watchdog_result.and(output_result)
    }
}

pub(super) struct NativeCommandOwner {
    state: Option<NativeCommandState>,
    cleanup: Arc<NativeCleanup>,
}

impl NativeCommandOwner {
    async fn settle(mut self) -> Result<(), ToolError> {
        let state = self
            .state
            .take()
            .ok_or_else(|| ToolError::Command("native command owner is missing".to_owned()))?;
        self.cleanup.schedule(state).await.map_err(|error| {
            ToolError::Command(format!("native command cleanup worker failed: {error}"))
        })?
    }
}

impl Drop for NativeCommandOwner {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            drop(self.cleanup.schedule(state));
        }
    }
}

pub(super) async fn settle_command_child(child: &mut Child, child_id: Option<u32>) {
    terminate_process_group(child_id);
    let _ = child.start_kill();
    if let Err(error) = child.wait().await {
        tracing::error!(%error, "command child could not be reaped; effect settlement remains blocked");
        std::future::pending::<()>().await;
    }
    if let Err(error) = terminate_and_wait_process_group(child_id).await {
        tracing::error!(%error, "command group exit could not be proven; effect settlement remains blocked");
        std::future::pending::<()>().await;
    }
}

pub(super) fn command_egress_proxy(
    request: &CommandRequest,
    safe: bool,
    available: bool,
    upstream_proxy: Option<&UpstreamProxy>,
) -> Result<Option<SupervisedEgressProxy>, ToolError> {
    if request.network_domains.is_empty() {
        return Ok(None);
    }
    if safe || !available {
        return Err(ToolError::Command(
            "requested command domains cannot be routed safely on this host".to_owned(),
        ));
    }
    let mut policy = EgressPolicy::default();
    for domain in &request.network_domains {
        if !policy.allow_domain(domain) {
            return Err(ToolError::InvalidInput(format!(
                "invalid requested network domain {domain:?}"
            )));
        }
    }
    SupervisedEgressProxy::start_with_upstream(policy, upstream_proxy.cloned())
        .map(Some)
        .map_err(|error| {
            ToolError::Command(format!("supervised egress proxy could not start: {error}"))
        })
}

pub(super) fn guarded_process(
    request: &CommandRequest,
    sandbox: Option<&SandboxPolicy>,
    egress_proxy: Option<&SupervisedEgressProxy>,
) -> Result<GuardedCommand, ToolError> {
    #[cfg(target_os = "macos")]
    if sandbox.is_some() && command_can_escape_process_group(&request.command) {
        return Err(ToolError::Command(
            "daemonizing commands are unavailable until descendant lifetime isolation is active"
                .to_owned(),
        ));
    }
    let safe_invocation = safe_builtin_invocation(&request.command);
    let hardened_safe_compound = hardened_safe_compound(&request.command);
    let network =
        egress_proxy.is_some() && safe_invocation.is_none() && hardened_safe_compound.is_none();
    let shell_command = hardened_safe_compound
        .as_deref()
        .unwrap_or(&request.command);
    let shell_args = safe_invocation.as_ref().map_or_else(
        || {
            vec![
                OsString::from("-c"),
                OsString::from("IFS= read -r _ || exit 125; exec /bin/sh -c \"$1\""),
                OsString::from("rottweiler-command-launcher"),
                OsString::from(shell_command),
            ]
        },
        |argv| {
            let mut shell_args = vec![
                OsString::from("-c"),
                OsString::from("IFS= read -r _ || exit 125; exec \"$@\""),
                OsString::from("rottweiler-safe-command-launcher"),
            ];
            shell_args.extend(argv.iter().map(OsString::from));
            shell_args
        },
    );
    #[cfg(target_os = "linux")]
    let mut helper_pin = None;
    let (program, args) = if let Some(base_policy) = sandbox {
        let policy = if network {
            let proxy = egress_proxy.ok_or_else(|| {
                ToolError::Command(
                    "network was approved but the supervised egress proxy is unavailable"
                        .to_owned(),
                )
            })?;
            base_policy.with_network(SandboxNetworkPolicy::PolicyProxy {
                port: proxy.address().port(),
                relay_path: proxy.relay_path().map(Path::to_path_buf),
            })
        } else {
            base_policy.with_network(SandboxNetworkPolicy::Deny)
        };
        let executable = std::env::current_exe()
            .map_err(|error| ToolError::Command(format!("sandbox helper unavailable: {error}")))?;
        let plan = shell_launch_plan(&policy, &executable, Path::new("/bin/sh"), &shell_args)
            .map_err(|error| ToolError::Command(error.to_string()))?;
        #[cfg(target_os = "linux")]
        let plan = {
            let mut plan = plan;
            helper_pin = plan.take_helper_pin();
            plan
        };
        (plan.program, plan.args)
    } else {
        (PathBuf::from("/bin/sh"), shell_args)
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&request.cwd)
        .envs(&request.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    sanitize_shell_control_environment(&mut command);
    configure_proxy_environment(&mut command, network.then_some(egress_proxy).flatten());
    if safe_invocation.is_some() || hardened_safe_compound.is_some() {
        sanitize_safe_command_environment(&mut command, request);
    }
    #[cfg(unix)]
    command.process_group(0);
    Ok(GuardedCommand {
        command,
        #[cfg(target_os = "linux")]
        helper_pin,
    })
}

pub(super) struct GuardedCommand {
    command: Command,
    #[cfg(target_os = "linux")]
    helper_pin: Option<std::fs::File>,
}

#[cfg(target_os = "macos")]
pub(super) fn command_can_escape_process_group(command: &str) -> bool {
    let Ok(words) = shell_words::split(command) else {
        return true;
    };
    words.iter().any(|word| {
        Path::new(word)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "setsid" | "nohup" | "daemon"))
    })
}

pub(super) fn sanitize_shell_control_environment(command: &mut Command) {
    for key in ["BASH_ENV", "ENV", "SHELLOPTS", "CDPATH"] {
        command.env_remove(key);
    }
}

pub(super) fn safe_builtin_invocation(command: &str) -> Option<Vec<String>> {
    let segments = safe_command_segments(command)?;
    if segments.len() != 1 || !built_in_safe_segment(&segments[0].0) {
        return None;
    }
    hardened_safe_argv(&segments[0].0)
}

pub(super) fn hardened_safe_compound(command: &str) -> Option<String> {
    let segments = safe_command_segments(command)?;
    if segments.len() < 2
        || !segments
            .iter()
            .all(|(segment, _)| built_in_safe_segment(segment))
    {
        return None;
    }
    let mut hardened = String::new();
    for (segment, operator) in segments {
        let argv = hardened_safe_argv(&segment)?;
        if !hardened.is_empty() {
            hardened.push(' ');
        }
        hardened.push_str(
            &argv
                .iter()
                .map(|argument| shell_words::quote(argument).into_owned())
                .collect::<Vec<_>>()
                .join(" "),
        );
        if let Some(operator) = operator {
            hardened.push(' ');
            hardened.push_str(&operator);
        }
    }
    Some(hardened)
}

pub(super) fn hardened_safe_argv(command: &str) -> Option<Vec<String>> {
    let supplied = shell_words::split(command).ok()?;
    match supplied.first().map(String::as_str) {
        Some("git") => hardened_git_argv(command),
        Some(name @ ("cat" | "ls")) => {
            let executable = audited_system_read_command(name)?;
            let mut argv = vec![executable.to_string_lossy().into_owned()];
            argv.extend(supplied.into_iter().skip(1));
            Some(argv)
        }
        Some("bat") if safe_bat_arguments(&supplied[1..]) => {
            let executable = audited_bat()?;
            let mut argv = vec![executable.to_string_lossy().into_owned()];
            argv.extend(supplied.into_iter().skip(1));
            Some(argv)
        }
        _ => None,
    }
}

pub(super) fn hardened_git_argv(command: &str) -> Option<Vec<String>> {
    let mut supplied = shell_words::split(command).ok()?;
    let git = audited_system_git()?;
    supplied.remove(0);
    let subcommand = supplied.first()?.clone();
    let arguments = supplied.split_off(1);
    if (subcommand == "status" && !safe_git_status_arguments(&arguments))
        || (subcommand == "diff" && !safe_git_diff_arguments(&arguments))
        || !matches!(subcommand.as_str(), "status" | "diff")
    {
        return None;
    }
    let mut argv = vec![
        git.to_string_lossy().into_owned(),
        "-c".to_owned(),
        "core.fsmonitor=false".to_owned(),
        "-c".to_owned(),
        "core.untrackedCache=false".to_owned(),
        "-c".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "-c".to_owned(),
        "core.attributesFile=/dev/null".to_owned(),
        "-c".to_owned(),
        "diff.external=".to_owned(),
        "-c".to_owned(),
        "pager.status=false".to_owned(),
        "-c".to_owned(),
        "pager.diff=false".to_owned(),
        subcommand.clone(),
    ];
    if subcommand == "diff" {
        argv.extend(["--no-ext-diff".to_owned(), "--no-textconv".to_owned()]);
    }
    argv.extend(arguments);
    Some(argv)
}

pub(super) fn sanitize_safe_command_environment(command: &mut Command, _request: &CommandRequest) {
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/dev/null")
        .env("LC_ALL", "C")
        .env("XDG_CONFIG_HOME", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
}

pub(super) fn configure_proxy_environment(
    command: &mut Command,
    proxy: Option<&SupervisedEgressProxy>,
) {
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    if let Some(proxy) = proxy {
        let url = proxy.url();
        command
            .env("HTTP_PROXY", &url)
            .env("HTTPS_PROXY", &url)
            .env("http_proxy", &url)
            .env("https_proxy", &url)
            .env("NO_PROXY", "")
            .env("no_proxy", "");
    }
}
