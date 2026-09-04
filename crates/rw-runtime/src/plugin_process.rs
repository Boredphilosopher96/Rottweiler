//! Production OS-sandboxed launcher for approved RPC plugins.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use rw_ext::{
    CapabilityViolation, LaunchedPluginProcess, PluginLauncher, PluginProcessConfig,
    PluginProcessError, PluginSandboxProfile, SupervisedPluginProcess,
};
use rw_plugin_protocol::PluginToolEffect;
use rw_tools::{
    EgressPolicy, NetworkPolicy, SandboxPolicy, SandboxSupport, SupervisedEgressProxy,
    probe_sandbox, shell_launch_plan,
};
use tokio::{
    io::{AsyncReadExt as _, BufReader},
    process::Child,
};

const MAX_PLUGIN_STDERR_BYTES: u64 = 256 * 1024;

/// A launcher that refuses ambient networking and executes only through the
/// native Rottweiler sandbox helper. The approved manifest remains the sole
/// source of capability truth.
pub struct SandboxedPluginLauncher {
    scratch: PathBuf,
    helper: PathBuf,
}

impl SandboxedPluginLauncher {
    /// Creates a launcher from canonical scratch and sandbox-helper paths.
    ///
    /// # Errors
    /// Returns an error when either path is unsafe or sandbox enforcement is unavailable.
    pub fn new(scratch: &Path, helper: &Path) -> Result<Self, PluginProcessError> {
        let scratch = std::fs::canonicalize(scratch).map_err(|error| process_error(&error))?;
        let helper = std::fs::canonicalize(helper).map_err(|error| process_error(&error))?;
        if !scratch.is_dir() || !helper.is_file() {
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
    ) -> Result<LaunchedPluginProcess, PluginProcessError> {
        let (child, proxy) = spawn_sandboxed_plugin(config, profile, &self.scratch, &self.helper)?;
        attach_supervisor(child, proxy, config)
    }
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
    config.validate_executable_identity()?;
    let mut roots = vec![scratch.to_path_buf()];
    if has_effect(PluginToolEffect::WritesFilesystem) {
        if profile.approved_roots.is_empty() {
            return Err(error(
                "writes-fs requires at least one explicitly approved root",
            ));
        }
        for root in &profile.approved_roots {
            let canonical = std::fs::canonicalize(root).map_err(|error| process_error(&error))?;
            if !canonical.is_dir() {
                return Err(error("approved plugin root is not a directory"));
            }
            roots.push(canonical);
        }
    }
    Ok(roots)
}

fn spawn_sandboxed_plugin(
    config: &PluginProcessConfig,
    profile: &PluginSandboxProfile,
    scratch: &Path,
    helper: &Path,
) -> Result<(Child, SupervisedEgressProxy), PluginProcessError> {
    let roots = approved_write_roots(config, profile, scratch)?;
    // Keep even no-network plugins on an empty policy proxy so denied
    // egress is observable and terminal instead of an invisible EPERM.
    let proxy = SupervisedEgressProxy::start(EgressPolicy::new(&profile.allowed_domains))
        .map_err(|sandbox| error(&sandbox.to_string()))?;
    let network = NetworkPolicy::PolicyProxy {
        port: proxy.address().port(),
        relay_path: proxy.relay_path().map(Path::to_path_buf),
    };
    let mut read_roots = intrinsic_plugin_read_roots(config, scratch)?;
    if profile.allows_workspace_reads() {
        read_roots.extend(profile.approved_roots.iter().cloned());
    }
    let policy = SandboxPolicy::new(&roots, network)
        .and_then(|policy| policy.with_read_roots(read_roots))
        .map_err(|sandbox| error(&sandbox.to_string()))?;
    let args = config.argv().to_vec();
    #[allow(unused_mut)]
    let mut plan = shell_launch_plan(&policy, helper, config.executable(), &args)
        .map_err(|sandbox| error(&sandbox.to_string()))?;
    if !plan.warnings.is_empty() {
        return Err(error("plugin sandbox produced a degradation warning"));
    }
    // Close the approval-to-exec replacement window at the final boundary.
    config.validate_executable_identity()?;
    let mut command = tokio::process::Command::new(&plan.program);
    command
        .args(&plan.args)
        .current_dir(config.cwd())
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
    command
        .env("HTTP_PROXY", proxy.url())
        .env("HTTPS_PROXY", proxy.url())
        .env("NO_PROXY", "");
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().map_err(|error| process_error(&error))?;
    #[cfg(target_os = "linux")]
    drop(plan.take_helper_pin());
    Ok((child, proxy))
}

fn attach_supervisor(
    mut child: Child,
    proxy: SupervisedEgressProxy,
    config: &PluginProcessConfig,
) -> Result<LaunchedPluginProcess, PluginProcessError> {
    let process_group = child.id();
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| error("plugin stdin is unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| error("plugin stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| error("plugin stderr is unavailable"))?;
    let denials = proxy.denials();
    let process = Arc::new(PluginChild {
        child: Mutex::new(child),
        process_group,
        violation: Arc::new(Mutex::new(None)),
        _proxy: proxy,
    });
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
    let process: Arc<dyn SupervisedPluginProcess> = process;
    Ok(LaunchedPluginProcess {
        stdin: Box::pin(stdin),
        stdout: Box::pin(BufReader::new(stdout)),
        stderr: Box::pin(BufReader::new(stderr.take(MAX_PLUGIN_STDERR_BYTES))),
        process,
        executable_identity: config.executable_identity().clone(),
    })
}

struct PluginChild {
    child: Mutex<Child>,
    process_group: Option<u32>,
    violation: Arc<Mutex<Option<String>>>,
    _proxy: SupervisedEgressProxy,
}

impl Drop for PluginChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self
            .process_group
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        #[cfg(not(unix))]
        if let Ok(child) = self.child.get_mut() {
            let _ = child.start_kill();
        }
    }
}

#[async_trait]
impl SupervisedPluginProcess for PluginChild {
    async fn settle_effects(&self) -> Result<(), PluginProcessError> {
        self.wait_for_exit().await?;
        rw_tools::terminate_and_wait_process_group(self.process_group)
            .await
            .map_err(|failure| error(&failure.to_string()))
    }

    fn mark_capability_violation(&self, violation: &CapabilityViolation) {
        if let Ok(mut value) = self.violation.lock() {
            *value = Some(violation.to_string().chars().take(512).collect());
        }
    }

    fn kill_tree(&self) -> Result<(), PluginProcessError> {
        #[cfg(unix)]
        if let Some(group) = self
            .process_group
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
        #[cfg(not(unix))]
        self.child
            .lock()
            .map_err(|_| error("plugin child lock was poisoned"))?
            .start_kill()
            .map_err(|error| process_error(&error))?;
        Ok(())
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
    async fn wait_for_exit(&self) -> Result<Option<i32>, PluginProcessError> {
        loop {
            let status = self
                .child
                .lock()
                .map_err(|_| error("plugin child lock was poisoned"))?
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
    config: &PluginProcessConfig,
    scratch: &Path,
) -> Result<Vec<PathBuf>, PluginProcessError> {
    let mut roots = vec![scratch.to_path_buf()];
    roots.push(config.executable().to_path_buf());
    if let Some(code_root) = config.code_root() {
        roots.push(code_root.canonical_path.clone());
    }
    roots.extend(
        config
            .attested_files()
            .iter()
            .map(|identity| identity.canonical_path.clone()),
    );
    for candidate in [
        "/System",
        "/Library/Apple",
        "/usr/lib",
        "/usr/share",
        "/lib",
        "/lib64",
        "/etc/ld.so.cache",
        "/dev",
        "/proc",
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

    use super::*;
    use rw_ext::{
        ApprovalStore, ApprovalStoreError, DenyPushHandler, LaunchedPluginProcess, PluginHost,
        PluginLauncher, SupervisedPluginProcess, approve_plugin_launch,
    };
    use rw_plugin_protocol::{
        METHOD_TOOL_CALL, PluginCapabilities, PluginManifest, PluginToolCapability,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn intrinsic_runtime_reads_do_not_require_fake_manifest_capability_and_network_fails_closed() {
        let scratch = tempfile::tempdir().expect("scratch");
        let helper = std::env::current_exe().expect("helper");
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

    struct RecordingProductionLauncher {
        inner: SandboxedPluginLauncher,
        process: StdMutex<Option<Arc<dyn SupervisedPluginProcess>>>,
    }

    #[async_trait]
    impl PluginLauncher for RecordingProductionLauncher {
        async fn launch(
            &self,
            config: &PluginProcessConfig,
            profile: &PluginSandboxProfile,
        ) -> Result<LaunchedPluginProcess, PluginProcessError> {
            let launched = self.inner.launch(config, profile).await?;
            *self.process.lock().expect("process lock") = Some(Arc::clone(&launched.process));
            Ok(launched)
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
            &approvals,
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
        let (bun, sdk) = bun_and_sdk();
        let scratch = tempfile::tempdir().expect("scratch");
        let workspace = tempfile::tempdir().expect("workspace");
        let package = workspace.path().join("plugin-code");
        std::fs::create_dir(&package).expect("package directory");
        let helper = std::env::current_exe().expect("helper");
        let Ok(launcher) = SandboxedPluginLauncher::new(scratch.path(), &helper) else {
            return;
        };
        let fixtures = [
            (
                "pre-tool-deny-custom-tool.ts",
                json!({
                    "name":"conformance-policy-tool", "version":"1.0.0", "protocol":2,
                    "capabilities": {
                        "tools":[{"name":"fixture_echo","description":"Echo bounded fixture input","schema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}}},"caps":[]}],
                        "hooks":[{"name":"pre_tool","failure_policy":"fail-closed"}]
                    }
                }),
            ),
            (
                "event-subscriber.ts",
                json!({
                    "name":"conformance-event-subscriber", "version":"1.0.0", "protocol":2,
                    "capabilities":{"event_subscriptions":["TurnFinished"],"push":["session/set_status"]}
                }),
            ),
            (
                "provider.ts",
                json!({
                    "name":"conformance-provider", "version":"1.0.0", "protocol":2,
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
    async fn omitted_network_outbound_is_killed_and_surfaces_terminal_violation() {
        let (bun, sdk) = bun_and_sdk();
        let scratch = tempfile::tempdir().expect("scratch");
        let workspace = tempfile::tempdir().expect("workspace");
        let package = workspace.path().join("plugin-code");
        std::fs::create_dir(&package).expect("package directory");
        let helper = std::env::current_exe().expect("helper");
        let Ok(inner) = SandboxedPluginLauncher::new(scratch.path(), &helper) else {
            return;
        };
        let launcher = RecordingProductionLauncher {
            inner,
            process: StdMutex::new(None),
        };
        let config = compiled_fixture_config(&bun, &sdk, &package, "network-without-capability.ts");
        let manifest: PluginManifest = serde_json::from_value(json!({
            "name":"network-without-capability", "version":"1.0.0", "protocol":2,
            "capabilities":{}
        }))
        .expect("adversarial manifest");
        let approvals = MemoryApproval::default();
        approve_plugin_launch(
            &approvals,
            &manifest,
            &config,
            "conformance:production:network-violation",
        )
        .expect("exact approval");
        let host = PluginHost::launch_approved(
            &launcher,
            &approvals,
            &config,
            "conformance:production:network-violation",
            &[workspace.path().to_path_buf()],
            manifest,
            Arc::new(DenyPushHandler),
            Arc::new(crate::extension_runtime::SharedPluginRedactor::new(
                rw_providers::FixtureRedactor::default(),
            )),
        )
        .await
        .expect("handshake before adversarial egress");
        let process = launcher
            .process
            .lock()
            .expect("process lock")
            .clone()
            .expect("production process recorded");
        let error = tokio::time::timeout(Duration::from_secs(3), process.wait())
            .await
            .expect("terminal violation deadline")
            .expect_err("network violation must be surfaced by the supervisor");
        assert!(
            error.message.contains("network egress"),
            "unexpected supervisor error: {}",
            error.message
        );
        drop(host);
    }

    #[tokio::test]
    async fn no_reads_plugin_cannot_read_sibling_workspace_secret() {
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
        let helper = std::env::current_exe().expect("helper");
        let Ok(launcher) = SandboxedPluginLauncher::new(scratch.path(), &helper) else {
            return;
        };
        let config =
            compiled_fixture_config(&bun, &sdk, &package, "read-sibling-without-capability.ts");
        let manifest: PluginManifest = serde_json::from_value(json!({
            "name":"read-sibling-without-capability", "version":"1.0.0", "protocol":2,
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
            &approvals,
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
            .request(
                METHOD_TOOL_CALL,
                json!({"name":"read_sibling_probe","input":{}}),
            )
            .await
            .expect("probe response");
        assert_eq!(response["content"], "denied");
        assert!(!response.to_string().contains("SIBLING_SECRET_CANARY"));
        host.shutdown().await.expect("fixture shutdown");
    }
}
