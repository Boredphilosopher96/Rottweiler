//! Production OS-sandboxed launcher for approved RPC plugins.

mod launch_bytes;
mod proxy_settlement;
use launch_bytes::LaunchBytes;
mod retirement;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use rw_ext::{
    CapabilityViolation, LaunchedPluginProcess, PluginLaunchError, PluginLauncher,
    PluginProcessConfig, PluginProcessError, PluginSandboxProfile, SupervisedPluginProcess,
};
use rw_plugin_protocol::PluginToolEffect;
use rw_tools::{
    NetworkPolicy, SandboxPolicy, SandboxSupport, SupervisedEgressProxy, probe_sandbox,
    shell_launch_plan,
};
use tokio::{
    io::{AsyncReadExt as _, BufReader},
    process::Child,
};

const MAX_PLUGIN_STDERR_BYTES: u64 = 256 * 1024;
const PLUGIN_HANDOFF_PROOF_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(all(test, unix))]
mod handoff_tests;
#[cfg(all(test, unix))]
mod pinned_tests;

/// A launcher that refuses ambient networking and executes only through the
/// native Rottweiler sandbox helper. The approved manifest remains the sole
/// source of capability truth.
pub struct SandboxedPluginLauncher {
    scratch: PathBuf,
    helper: rw_tools::SandboxHelper,
}

impl SandboxedPluginLauncher {
    /// Creates a launcher from canonical scratch and approved bootstrap authority.
    ///
    /// # Errors
    /// Returns an error when scratch is unsafe or sandbox enforcement is unavailable.
    pub fn new(
        scratch: &Path,
        helper: &rw_tools::SandboxHelper,
    ) -> Result<Self, PluginProcessError> {
        let scratch = std::fs::canonicalize(scratch).map_err(|error| process_error(&error))?;
        let helper = helper.clone();
        if !scratch.is_dir() {
            return Err(error("plugin launcher scratch/helper is invalid"));
        }
        if probe_sandbox().support != SandboxSupport::Enforced {
            return Err(error("OS sandbox enforcement is unavailable for plugins"));
        }
        Ok(Self { scratch, helper })
    }
}

#[async_trait]
impl PluginLauncher for SandboxedPluginLauncher {
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginLaunchError> {
        let waiting = std::time::Instant::now();
        tracing::debug!(target: "rw_performance", stage = "plugin.process_admission", phase = "queued");
        let admission =
            rw_resources::acquire(rw_resources::ResourceClass::Process, std::future::pending())
                .await
                .map_err(|failure| PluginLaunchError::Rejected(error(&failure.to_string())))?;
        tracing::debug!(target: "rw_performance", stage = "plugin.process_admission", phase = "admitted",
            admission_ms = waiting.elapsed().as_secs_f64() * 1000.0);
        let owned_config = config.clone();
        let profile = profile.clone();
        let scratch = self.scratch.clone();
        let helper = self.helper.clone();
        handoff_in_worker(config.clone(), helper.clone(), admission, move || {
            spawn_sandboxed_plugin(&owned_config, &profile, &scratch, &helper)
                .map_err(PluginLaunchError::Rejected)
        })
        .await
    }
}

async fn handoff_in_worker(
    config: PluginProcessConfig,
    helper: rw_tools::SandboxHelper,
    admission: rw_resources::ResourceLease,
    spawn: impl FnOnce() -> Result<SpawnedPlugin, PluginLaunchError> + Send + 'static,
) -> Result<LaunchedPluginProcess, PluginLaunchError> {
    let runtime = tokio::runtime::Handle::current();
    // The physical worker owns admission, helper bytes and the complete
    // handoff. Caller cancellation cannot discard a raw spawned child.
    let waiting = std::time::Instant::now();
    tracing::debug!(target: "rw_performance", stage = "plugin.verify_and_spawn", phase = "queued");
    rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        tracing::debug!(target: "rw_performance", stage = "plugin.verify_and_spawn", phase = "admitted",
            admission_ms = waiting.elapsed().as_secs_f64() * 1000.0);
        let started = std::time::Instant::now();
        let SpawnedPlugin { child, proxy, bytes } = spawn()?;
        // Establish the complete physical owner synchronously before any
        // callback, tracing subscriber or async handoff can fail or be dropped.
        let handoff = attach_supervisor(child, proxy, &config, helper, admission, bytes);
        tracing::debug!(target: "rw_performance", stage = "plugin.verify_and_spawn",
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0, succeeded = true);
        let started = std::time::Instant::now();
        let result = runtime.block_on(handoff);
        tracing::debug!(target: "rw_performance", stage = "plugin.handoff",
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0, succeeded = result.is_ok());
        result
    })
    .await
    .map_err(|failure| match failure {
        rw_resources::WorkError::Admission(cause) => {
            PluginLaunchError::Rejected(error(&cause.to_string()))
        }
        rw_resources::WorkError::Worker(_) => PluginLaunchError::EffectsUnsettled {
            message: "plugin launch worker exited without handoff proof".into(),
        },
    })?
}

fn approved_write_roots(
    config: &PluginProcessConfig,
    profile: &PluginSandboxProfile,
    scratch: &Path,
) -> Result<Vec<PathBuf>, PluginProcessError> {
    let has_effect = |wanted| {
        profile
            .capabilities
            .tools
            .iter()
            .flat_map(|tool| tool.caps.iter())
            .any(|effect| *effect == wanted)
    };
    if (has_effect(PluginToolEffect::Network) || !profile.capabilities.providers.is_empty())
        && profile.allowed_domains.is_empty()
    {
        return Err(error(
            "network/provider plugin requires explicit allowed_domains",
        ));
    }
    if profile
        .allowed_domains
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        != config
            .allowed_domains()
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
    {
        return Err(error(
            "plugin launch domains differ from the approved config identity",
        ));
    }
    config.validate_code_root_identity()?;
    // Exact executable/content validation is performed while copying into the
    // immutable launch owner, without a redundant full-file hash pass.
    // Manifest effects are delegated by the host, never ambient worker grants.
    let roots = vec![scratch.to_path_buf()];
    Ok(roots)
}

fn spawn_sandboxed_plugin(
    config: &PluginProcessConfig,
    profile: &PluginSandboxProfile,
    scratch: &Path,
    helper: &rw_tools::SandboxHelper,
) -> Result<SpawnedPlugin, PluginProcessError> {
    let roots = approved_write_roots(config, profile, scratch)?;
    let bytes = Arc::new(LaunchBytes::capture(config, profile)?);
    spawn_pinned_plugin(config, profile, scratch, helper, bytes, &roots)
}

fn spawn_pinned_plugin(
    config: &PluginProcessConfig,
    profile: &PluginSandboxProfile,
    scratch: &Path,
    helper: &rw_tools::SandboxHelper,
    bytes: Arc<LaunchBytes>,
    roots: &[PathBuf],
) -> Result<SpawnedPlugin, PluginProcessError> {
    bytes.validate_write_roots(roots)?;
    let (policy, proxy) = plugin_sandbox_policy(config, profile, scratch, roots, &bytes)?;
    #[allow(unused_mut)]
    let mut plan = shell_launch_plan(&policy, helper, bytes.program(config), bytes.args(config))
        .map_err(|sandbox| error(&sandbox.to_string()))?;
    if !plan.warnings.is_empty() {
        return Err(error("plugin sandbox produced a degradation warning"));
    }
    let mut command = tokio::process::Command::new(&plan.program);
    command
        .args(&plan.args)
        .current_dir(bytes.cwd(config))
        .env_clear()
        .env("HOME", scratch)
        .env("TMPDIR", scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in config.environment_allowlist() {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let Some(proxy) = &proxy {
        command
            .env("HTTP_PROXY", proxy.url())
            .env("HTTPS_PROXY", proxy.url())
            .env("NO_PROXY", "");
    }
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().map_err(|error| process_error(&error))?;
    #[cfg(target_os = "linux")]
    drop(plan.take_helper_pin());
    Ok(SpawnedPlugin {
        child,
        proxy,
        bytes,
    })
}

fn plugin_sandbox_policy(
    config: &PluginProcessConfig,
    profile: &PluginSandboxProfile,
    scratch: &Path,
    roots: &[PathBuf],
    bytes: &LaunchBytes,
) -> Result<(SandboxPolicy, Option<SupervisedEgressProxy>), PluginProcessError> {
    #[cfg(target_os = "linux")]
    if let rw_ext::PluginSandboxMode::Preparation { filesystem } = &profile.mode {
        if profile.capabilities != rw_plugin_protocol::PluginCapabilities::default()
            || !profile.approved_roots.is_empty()
            || !profile.allowed_domains.is_empty()
        {
            return Err(error(
                "source preparation cannot request plugin capabilities",
            ));
        }
        return SandboxPolicy::for_preparation(filesystem.as_ref().clone())
            .map(|policy| (policy.without_process_creation(), None))
            .map_err(|sandbox| error(&sandbox.to_string()));
    }
    let read_roots = intrinsic_plugin_read_roots(bytes, scratch)?;
    let policy = SandboxPolicy::new(roots, NetworkPolicy::Deny)
        .and_then(|policy| policy.with_read_roots(read_roots))
        .map_err(|sandbox| error(&sandbox.to_string()))?
        .with_only_declared_reads()
        .with_self_process_reads()
        .without_process_creation();
    #[cfg(target_os = "macos")]
    let policy = if matches!(profile.mode, rw_ext::PluginSandboxMode::Preparation { .. }) {
        policy
            .with_read_directory_ancestors(config.cwd())
            .map_err(|sandbox| error(&sandbox.to_string()))?
    } else {
        policy
    };
    Ok((policy, None))
}

fn attach_supervisor(
    mut child: Child,
    proxy: Option<SupervisedEgressProxy>,
    config: &PluginProcessConfig,
    helper: rw_tools::SandboxHelper,
    admission: rw_resources::ResourceLease,
    bytes: Arc<LaunchBytes>,
) -> impl std::future::Future<Output = Result<LaunchedPluginProcess, PluginLaunchError>> {
    let process_group = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let denials = proxy.as_ref().map(SupervisedEgressProxy::denials);
    let process = Arc::new(PluginChild {
        bytes,
        helper,
        admission: Mutex::new(Some(admission)),
        settlement: tokio::sync::Mutex::new(()),
        child: Mutex::new(Some(child)),
        process_group: Mutex::new(process_group),
        violation: Arc::new(Mutex::new(None)),
        proxy: proxy_settlement::PluginProxy::new(proxy),
    });
    let executable_identity = config.executable_identity().clone();
    let mut handoff = PendingPluginHandoff {
        process: Arc::clone(&process),
        settled: false,
    };
    async move {
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            // A failed handoff still owns the process and every descendant.
            let _ = process.kill_tree();
            let proof =
                tokio::time::timeout(PLUGIN_HANDOFF_PROOF_TIMEOUT, process.settle_effects()).await;
            match proof {
                Ok(Ok(())) => {
                    handoff.settled = true;
                    return Err(PluginLaunchError::Rejected(error(
                        "plugin stdio is unavailable",
                    )));
                }
                Ok(Err(error)) => {
                    return Err(PluginLaunchError::EffectsUnsettled {
                        message: error.to_string(),
                    });
                }
                Err(_) => {
                    return Err(PluginLaunchError::EffectsUnsettled {
                        message: "plugin launch cleanup proof deadline expired".to_owned(),
                    });
                }
            }
        };
        if let Some(denials) = denials {
            let weak = Arc::downgrade(&process);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let Some(process) = weak.upgrade() else {
                        return;
                    };
                    if denials.count() == 0 {
                        continue;
                    }
                    if let Ok(mut violation) = process.violation.lock() {
                        *violation = Some(
                        "plugin attempted network egress outside its approved manifest/domain allowlist"
                            .to_owned(),
                    );
                    }
                    tracing::error!("plugin killed after network capability/domain violation");
                    let _ = process.kill_tree();
                    return;
                }
            });
        }
        let process: Arc<dyn SupervisedPluginProcess> = process;
        let launched = LaunchedPluginProcess {
            stdin: Box::pin(stdin),
            stdout: Box::pin(BufReader::new(stdout)),
            stderr: Box::pin(BufReader::new(stderr.take(MAX_PLUGIN_STDERR_BYTES))),
            process,
            executable_identity,
        };
        handoff.settled = true;
        Ok(launched)
    }
}

struct PendingPluginHandoff {
    process: Arc<PluginChild>,
    settled: bool,
}
impl Drop for PendingPluginHandoff {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.process.kill_tree();
            std::mem::forget(Arc::clone(&self.process));
        }
    }
}

struct SpawnedPlugin {
    child: Child,
    proxy: Option<SupervisedEgressProxy>,
    bytes: Arc<LaunchBytes>,
}

struct PluginChild {
    bytes: Arc<LaunchBytes>,
    settlement: tokio::sync::Mutex<()>,
    admission: Mutex<Option<rw_resources::ResourceLease>>,
    helper: rw_tools::SandboxHelper,
    child: Mutex<Option<Child>>,
    process_group: Mutex<Option<u32>>,
    violation: Arc<Mutex<Option<String>>>,
    proxy: proxy_settlement::PluginProxy,
}

impl Drop for PluginChild {
    fn drop(&mut self) {
        retirement::retire_dropped(self);
    }
}

#[async_trait]
impl SupervisedPluginProcess for PluginChild {
    async fn settle_effects(&self) -> Result<(), PluginProcessError> {
        let _settlement = self.settlement.lock().await;
        if self
            .admission
            .lock()
            .map_err(|_| error("plugin process admission owner poisoned"))?
            .is_none()
        {
            return Ok(());
        }
        let (process, proxy) = tokio::join!(
            async {
                self.wait_for_exit().await?;
                let group = *self
                    .process_group
                    .lock()
                    .map_err(|_| error("plugin group owner poisoned"))?;
                rw_tools::terminate_and_wait_process_group(group)
                    .await
                    .map_err(|failure| error(&failure.to_string()))?;
                self.process_group
                    .lock()
                    .map_err(|_| error("plugin group owner poisoned"))?
                    .take();
                Ok::<(), PluginProcessError>(())
            },
            self.proxy.settle()
        );
        process?;
        proxy?;
        self.admission
            .lock()
            .map_err(|_| error("plugin process admission owner poisoned"))?
            .take();
        Ok(())
    }

    fn mark_capability_violation(&self, violation: &CapabilityViolation) {
        if let Ok(mut value) = self.violation.lock() {
            *value = Some(violation.to_string().chars().take(512).collect());
        }
    }

    fn kill_tree(&self) -> Result<(), PluginProcessError> {
        let admission = self
            .admission
            .lock()
            .map_err(|_| error("plugin process admission owner poisoned"))?;
        if admission.is_none() {
            return Ok(());
        }
        let group = self.kill_original_group();
        let child = self
            .child
            .lock()
            .map_err(|_| error("plugin child lock was poisoned"))
            .and_then(|mut child| {
                child
                    .as_mut()
                    .ok_or_else(|| error("plugin child owner is unavailable"))?
                    .start_kill()
                    .map_err(|error| process_error(&error))
            });
        group.and(child)
    }

    async fn wait(&self) -> Result<Option<i32>, PluginProcessError> {
        let status = self.wait_for_exit().await?;
        if let Some(violation) = self
            .violation
            .lock()
            .map_err(|_| error("plugin violation lock was poisoned"))?
            .clone()
        {
            return Err(error(&violation));
        }
        Ok(status)
    }
}

impl PluginChild {
    fn kill_original_group(&self) -> Result<(), PluginProcessError> {
        #[cfg(unix)]
        if let Some(group) = self
            .process_group
            .lock()
            .map_err(|_| error("plugin group owner poisoned"))?
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            rustix::process::kill_process_group(group, rustix::process::Signal::KILL)
                .or_else(|errno| {
                    if errno == rustix::io::Errno::SRCH {
                        Ok(())
                    } else {
                        Err(errno)
                    }
                })
                .map_err(|errno| error(&errno.to_string()))?;
        }
        Ok(())
    }

    async fn wait_for_exit(&self) -> Result<Option<i32>, PluginProcessError> {
        loop {
            let status = self
                .child
                .lock()
                .map_err(|_| error("plugin child lock was poisoned"))?
                .as_mut()
                .ok_or_else(|| error("plugin child owner is unavailable"))?
                .try_wait()
                .map_err(|error| process_error(&error))?;
            if let Some(status) = status {
                return Ok(status.code());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn intrinsic_plugin_read_roots(
    bytes: &LaunchBytes,
    scratch: &Path,
) -> Result<Vec<PathBuf>, PluginProcessError> {
    let mut roots = vec![scratch.to_path_buf()];
    roots.extend(bytes.read_roots());
    for candidate in [
        "/System",
        "/Library/Apple",
        "/usr/lib",
        "/usr/share",
        "/lib",
        "/lib64",
        "/etc/ld.so.cache",
        "/dev",
        "/proc/sys/vm/overcommit_memory",
        "/proc/sys/vm/mmap_min_addr",
        "/sys/kernel/mm/transparent_hugepage/enabled",
        "/proc/meminfo",
        "/sys/devices/system/cpu",
        "/sys/fs/cgroup",
        "/private/etc",
        "/private/var/db",
        "/private/var/OOPJit",
    ] {
        let path = PathBuf::from(candidate);
        if path.exists() {
            roots.push(path);
        }
    }
    roots.sort();
    roots.dedup();
    if roots.len() > 128 {
        return Err(error("plugin intrinsic read-root limit exceeded"));
    }
    Ok(roots)
}

fn process_error(error: &std::io::Error) -> PluginProcessError {
    PluginProcessError {
        message: error.to_string(),
    }
}
fn error(message: &str) -> PluginProcessError {
    PluginProcessError {
        message: message.chars().take(512).collect(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    #[path = "code_only.rs"]
    mod code_only;

    use super::*;
    use rw_ext::{
        ApprovalStore, ApprovalStoreError, DenyPushHandler, PluginHost, PluginLauncher,
        approve_plugin_launch,
    };
    use rw_plugin_protocol::{PluginCapabilities, PluginManifest, PluginToolCapability};
    use rw_tools::EgressPolicy;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn intrinsic_runtime_reads_do_not_require_fake_manifest_capability_and_network_fails_closed() {
        let scratch = tempfile::tempdir().expect("scratch");
        let helper = helper_executable().expect("fixture sandbox helper prerequisite");
        let Ok(launcher) = SandboxedPluginLauncher::new(scratch.path(), &helper) else {
            return;
        };
        let executable = std::fs::canonicalize("/usr/bin/true").expect("true");
        let config = PluginProcessConfig::new(executable).expect("config");
        let profile = PluginSandboxProfile {
            mode: rw_ext::PluginSandboxMode::Approved,
            capabilities: PluginCapabilities::default(),
            approved_roots: vec![],
            allowed_domains: vec![],
        };
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let child = runtime
            .block_on(launcher.launch(&config, &profile))
            .expect("intrinsic execution reads are host-owned");
        runtime
            .block_on(child.process.wait())
            .expect("wait for fixture");
        let profile = PluginSandboxProfile {
            mode: rw_ext::PluginSandboxMode::Approved,
            capabilities: PluginCapabilities {
                tools: vec![PluginToolCapability {
                    name: "x".to_owned(),
                    description: "x".to_owned(),
                    schema: json!({}),
                    caps: vec![PluginToolEffect::ReadsFilesystem, PluginToolEffect::Network],
                }],
                ..PluginCapabilities::default()
            },
            approved_roots: vec![],
            allowed_domains: vec![],
        };
        assert!(
            runtime
                .block_on(launcher.launch(&config, &profile))
                .is_err()
        );

        let config = config
            .with_allowed_domains(["api.example.com"])
            .expect("domain config");
        let profile = PluginSandboxProfile {
            mode: rw_ext::PluginSandboxMode::Approved,
            capabilities: PluginCapabilities {
                tools: vec![PluginToolCapability {
                    name: "x".to_owned(),
                    description: "x".to_owned(),
                    schema: json!({}),
                    caps: vec![PluginToolEffect::ReadsFilesystem, PluginToolEffect::Network],
                }],
                ..PluginCapabilities::default()
            },
            approved_roots: vec![],
            allowed_domains: vec!["api.example.com".to_owned()],
        };
        let child = runtime
            .block_on(launcher.launch(&config, &profile))
            .expect("public-domain launch");
        runtime
            .block_on(child.process.wait())
            .expect("wait for fixture");
    }

    #[derive(Default)]
    struct MemoryApproval(StdMutex<BTreeMap<String, String>>);

    impl ApprovalStore for MemoryApproval {
        fn approved_fingerprint(&self, name: &str) -> Result<Option<String>, ApprovalStoreError> {
            Ok(self.0.lock().expect("approval lock").get(name).cloned())
        }

        fn record_approval(&self, name: &str, fingerprint: &str) -> Result<(), ApprovalStoreError> {
            self.0
                .lock()
                .expect("approval lock")
                .insert(name.to_owned(), fingerprint.to_owned());
            Ok(())
        }
    }

    fn bun_and_sdk() -> (PathBuf, PathBuf) {
        let path = std::env::var_os("PATH").expect("PATH");
        let bun = std::env::split_paths(&path)
            .map(|path| path.join("bun"))
            .find(|path| path.is_file())
            .expect("Bun is required for production plugin conformance");
        let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/plugin-sdk")
            .canonicalize()
            .expect("SDK fixture root");
        (bun, sdk)
    }

    fn compiled_fixture_config(
        bun: &Path,
        sdk: &Path,
        package: &Path,
        name: &str,
    ) -> PluginProcessConfig {
        let fixture = sdk.join("fixtures/conformance").join(name);
        let executable = package.join(name.trim_end_matches(".ts"));
        let build_tmp = package.join("bun-tmp");
        std::fs::create_dir(&build_tmp).expect("Bun temp directory");
        let mut command = std::process::Command::new(bun);
        command
            .args(["build", "--compile"])
            .arg(&fixture)
            .arg("--outfile")
            .arg(&executable)
            .current_dir(package)
            .env("TMPDIR", &build_tmp);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().expect("compile TypeScript plugin fixture");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll Bun compiler") {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                #[cfg(unix)]
                if let Some(group) = i32::try_from(child.id())
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
                {
                    let _ =
                        rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                panic!("Bun fixture compile exceeded 30 seconds: {name}");
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        std::fs::remove_dir_all(&build_tmp).expect("remove Bun temp directory");
        assert!(status.success(), "fixture compile failed: {name}");
        PluginProcessConfig::new(&executable)
            .expect("compiled config")
            .with_cwd(package)
            .expect("package cwd")
            .with_code_root(package)
            .expect("package code root")
    }

    async fn approved_production_host(
        launcher: &dyn PluginLauncher,
        config: &PluginProcessConfig,
        sdk: &Path,
        manifest: PluginManifest,
        origin: &str,
    ) -> PluginHost {
        let approvals = MemoryApproval::default();
        approve_plugin_launch(&approvals, &manifest, config, origin).expect("exact approval");
        PluginHost::launch_approved(
            launcher,
            Arc::new(approvals),
            config,
            origin,
            &[sdk.to_path_buf()],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(crate::extension_runtime::SharedPluginRedactor::new(
                rw_providers::FixtureRedactor::default(),
            )),
        )
        .await
        .expect("production sandbox launch")
    }

    #[tokio::test]
    async fn three_independent_typescript_shapes_cross_production_sandbox() {
        let _admission = crate::native_fixture::admit().await;
        let (bun, sdk) = bun_and_sdk();
        let scratch = tempfile::tempdir().expect("scratch");
        let workspace = tempfile::tempdir().expect("workspace");
        let package = workspace.path().join("plugin-code");
        std::fs::create_dir(&package).expect("package directory");
        let helper = helper_executable().expect("fixture sandbox helper prerequisite");
        let Ok(launcher) = SandboxedPluginLauncher::new(scratch.path(), &helper) else {
            return;
        };
        let fixtures = [
            (
                "pre-tool-deny-custom-tool.ts",
                json!({
                    "name":"conformance-policy-tool", "version":"1.0.0", "protocol":3,
                    "capabilities": {
                        "tools":[{"name":"fixture_echo","description":"Echo bounded fixture input","schema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}}},"caps":[]}],
                        "hooks":[{"name":"pre_tool", "class": "policy","failure_policy":"fail-closed"}]
                    }
                }),
            ),
            (
                "event-subscriber.ts",
                json!({
                    "name":"conformance-event-subscriber", "version":"1.0.0", "protocol":3,
                    "capabilities":{"event_subscriptions":["turn_finished"],"push":["session/set_status"]}
                }),
            ),
            (
                "provider.ts",
                json!({
                    "name":"conformance-provider", "version":"1.0.0", "protocol":3,
                    "capabilities":{"providers":[{"alias-prefix":"fixture/"}]}
                }),
            ),
        ];
        for (index, (name, value)) in fixtures.into_iter().enumerate() {
            let mut config = compiled_fixture_config(&bun, &sdk, &package, name);
            if name == "provider.ts" {
                config = config
                    .with_allowed_domains(["example.com"])
                    .expect("provider domain");
            }
            let manifest = serde_json::from_value(value).expect("fixture manifest");
            let host = approved_production_host(
                &launcher,
                &config,
                workspace.path(),
                manifest,
                &format!("conformance:production:{index}"),
            )
            .await;
            host.shutdown().await.expect("fixture shutdown");
        }
    }

    #[tokio::test]
    async fn no_reads_plugin_cannot_read_sibling_workspace_secret() {
        let _admission = crate::native_fixture::admit().await;
        let (bun, sdk) = bun_and_sdk();
        let scratch = tempfile::tempdir().expect("scratch");
        let workspace = tempfile::tempdir().expect("workspace");
        let package = workspace.path().join("plugin-code");
        std::fs::create_dir(&package).expect("package directory");
        std::fs::write(
            workspace.path().join("workspace-secret.txt"),
            "SIBLING_SECRET_CANARY",
        )
        .expect("secret fixture");
        let helper = helper_executable().expect("fixture sandbox helper prerequisite");
        let Ok(launcher) = SandboxedPluginLauncher::new(scratch.path(), &helper) else {
            return;
        };
        let config =
            compiled_fixture_config(&bun, &sdk, &package, "read-sibling-without-capability.ts");
        let manifest: PluginManifest = serde_json::from_value(json!({
            "name":"read-sibling-without-capability", "version":"1.0.0", "protocol":3,
            "capabilities":{"tools":[{
                "name":"read_sibling_probe",
                "description":"Verify sibling workspace reads are denied",
                "schema":{"type":"object"},
                "caps":[]
            }]}
        }))
        .expect("adversarial manifest");
        let approvals = MemoryApproval::default();
        approve_plugin_launch(
            &approvals,
            &manifest,
            &config,
            "conformance:production:no-reads",
        )
        .expect("exact approval");
        let host = PluginHost::launch_approved(
            &launcher,
            Arc::new(approvals),
            &config,
            "conformance:production:no-reads",
            &[workspace.path().to_path_buf()],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(crate::extension_runtime::SharedPluginRedactor::new(
                rw_providers::FixtureRedactor::default(),
            )),
        )
        .await
        .expect("production no-reads host");
        let response = host
            .client()
            .call_tool(
                rw_plugin_protocol::ToolCallParams {
                    name: "read_sibling_probe".to_owned(),
                    input: json!({}),
                    lifetime: rw_plugin_protocol::OperationLifetime::default(),
                },
                &rw_tools::CancellationToken::default(),
                Arc::new(rw_tools::NoopProgressSink),
                None,
            )
            .await
            .expect("probe response");
        assert_eq!(response["content"], "denied");
        assert!(!response.to_string().contains("SIBLING_SECRET_CANARY"));
        host.shutdown().await.expect("fixture shutdown");
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn failed_stdio_handoff_settles_the_spawned_process_tree() {
        use tokio::io::AsyncReadExt as _;
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "sleep 60 & echo $!; wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn().expect("native fixture");
        let mut pid = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let byte = child
                    .stdout
                    .as_mut()
                    .expect("stdout")
                    .read_u8()
                    .await
                    .expect("child pid");
                if byte == b'\n' {
                    break;
                }
                pid.push(byte);
            }
        })
        .await
        .expect("descendant published");
        let pid: u32 = String::from_utf8(pid)
            .expect("pid text")
            .parse()
            .expect("pid number");
        let proxy = SupervisedEgressProxy::start(EgressPolicy::new(std::iter::empty::<&str>()))
            .expect("private proxy");
        let config = PluginProcessConfig::new("/bin/sh").expect("identity");
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            attach_supervisor(
                child,
                Some(proxy),
                &config,
                rw_tools::SandboxHelper::from_running(
                    &std::env::current_exe().expect("executable"),
                )
                .expect("helper"),
                process_fixture_lease(),
                fixture_launch_bytes(),
            ),
        )
        .await
        .expect("failed handoff settles");
        assert!(outcome.is_err());
        let observed = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "stat="])
            .output()
            .expect("observe descendant");
        let status = String::from_utf8(observed.stdout).expect("process status");
        assert!(
            status.trim().is_empty() || status.trim().starts_with('Z'),
            "descendant is still executing: {status}"
        );
    }
}

#[cfg(all(test, unix))]
mod child_signals;

/// Selects the trusted helper owned by this executable host.
pub(crate) fn helper_executable() -> std::io::Result<rw_tools::SandboxHelper> {
    #[cfg(test)]
    {
        crate::native_fixture::sandbox_helper()
    }
    #[cfg(not(test))]
    {
        rw_tools::SandboxHelper::from_running(&std::env::current_exe()?)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}

#[cfg(all(test, unix))]
fn process_fixture_lease() -> rw_resources::ResourceLease {
    rw_resources::try_acquire(rw_resources::ResourceClass::Process)
        .unwrap_or_else(|failure| panic!("fixture process admission: {failure}"))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
fn fixture_launch_bytes() -> Arc<LaunchBytes> {
    Arc::new(LaunchBytes::Harness {
        _helper: rw_tools::SandboxHelper::from_running(
            &std::env::current_exe().expect("test executable"),
        )
        .expect("kernel-owned fixture helper"),
    })
}
