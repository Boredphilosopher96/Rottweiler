//! Explicit-authority, sandboxed plugin development supervisor.

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use miette::{IntoDiagnostic as _, Result, miette};
use rw_ext::{
    LaunchedPluginProcess, PluginCapabilities, PluginLauncher, PluginManifest, PluginProcessConfig,
    PluginSandboxMode, PluginSandboxProfile, PluginToolCapability, PluginToolEffect,
    SupervisedPluginProcess,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _},
    sync::watch,
    task::JoinHandle,
};

use rw_runtime::plugin::SandboxedPluginLauncher;

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_SOURCE_FILES: usize = 4_096;
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SOURCE_DEPTH: usize = 64;
const RPC_DEADLINE: Duration = Duration::from_secs(5);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

type Trace = Arc<dyn Fn(&'static str) + Send + Sync>;

struct DevTarget {
    root: PathBuf,
    config: PluginProcessConfig,
}

#[derive(Clone, Copy)]
struct SupervisorOptions {
    poll: Duration,
    debounce: Duration,
    max_launches: Option<usize>,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(200),
            debounce: Duration::from_millis(400),
            max_launches: None,
        }
    }
}

/// Runs a local plugin only after the CLI has checked `--allow-dev-exec`.
pub(crate) async fn run(path: &Path) -> Result<()> {
    let target = resolve_target(path)?;
    let scratch = DevScratch::create()?;
    let helper = fs::canonicalize(std::env::current_exe().into_diagnostic()?).into_diagnostic()?;
    let launcher = Arc::new(
        SandboxedPluginLauncher::new(scratch.path(), &helper)
            .map_err(|error| miette!("plugin dev sandbox is unavailable: {error}"))?,
    );
    let (stop_tx, stop_rx) = watch::channel(false);
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = stop_tx.send(true);
        }
    });
    let trace: Trace = Arc::new(|message| eprintln!("{message}"));
    let result = supervise(
        launcher,
        target,
        stop_rx,
        trace,
        SupervisorOptions::default(),
    )
    .await;
    signal.abort();
    result
}

async fn supervise(
    launcher: Arc<dyn PluginLauncher>,
    target: DevTarget,
    mut stop: watch::Receiver<bool>,
    trace: Trace,
    options: SupervisorOptions,
) -> Result<()> {
    let profile = read_only_profile(&target.root);
    let mut fingerprint = source_fingerprint(&target.root)?;
    let mut spawn_count = 0_usize;
    loop {
        let child = launcher
            .launch(&target.config, &profile)
            .await
            .map_err(|error| miette!("plugin dev launch failed closed: {error}"))?;
        spawn_count += 1;
        let mut running = initialize(child, Arc::clone(&trace)).await?;
        trace("plugin-dev lifecycle running");
        if options.max_launches == Some(spawn_count) {
            shutdown(&mut running, Arc::clone(&trace)).await?;
            return Ok(());
        }

        let mut pending: Option<(blake3::Hash, Instant)> = None;
        loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        shutdown(&mut running, Arc::clone(&trace)).await?;
                        return Ok(());
                    }
                }
                status = running.process.wait() => {
                    running.trace.abort();
                    status.map_err(|error| miette!("plugin dev child wait failed: {error}"))?;
                    running.process.reap().await
                        .map_err(|error| miette!("plugin dev child reap failed: {error}"))?;
                    return Err(miette!("plugin dev child exited before shutdown"));
                }
                trace = &mut running.trace => {
                    terminate_and_reap(running.process.as_ref()).await;
                    return match trace {
                        Ok(Ok(())) => Err(miette!("plugin dev protocol stream ended unexpectedly")),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(miette!("plugin dev protocol trace task failed")),
                    };
                }
                () = tokio::time::sleep(options.poll) => {
                    let current = match source_fingerprint(&target.root) {
                        Ok(current) => current,
                        Err(error) => {
                            shutdown(&mut running, Arc::clone(&trace)).await?;
                            return Err(error);
                        }
                    };
                    if current == fingerprint {
                        pending = None;
                        continue;
                    }
                    match pending {
                        Some((candidate, since)) if candidate == current && since.elapsed() >= options.debounce => {
                            fingerprint = current;
                            trace("plugin-dev lifecycle restart source-changed");
                            shutdown(&mut running, Arc::clone(&trace)).await?;
                            break;
                        }
                        Some((candidate, _)) if candidate == current => {}
                        _ => pending = Some((current, Instant::now())),
                    }
                }
            }
        }
    }
}

struct RunningPlugin {
    stdin: rw_ext::PluginStdin,
    process: Arc<dyn SupervisedPluginProcess>,
    trace: JoinHandle<Result<()>>,
}

async fn initialize(mut child: LaunchedPluginProcess, trace: Trace) -> Result<RunningPlugin> {
    let handshake = async {
        let initialize = json!({
            "jsonrpc":"2.0", "id":"rottweiler-dev-init", "method":"initialize",
            "params":{"host":"rottweiler-dev","protocol":1,"min_protocol":1,"max_frame_bytes":MAX_FRAME_BYTES}
        });
        write_frame(&mut child.stdin, &initialize).await?;
        let frame = tokio::time::timeout(RPC_DEADLINE, read_frame(&mut child.stdout))
            .await
            .map_err(|_| miette!("plugin dev initialize timed out"))??;
        let response: Value = serde_json::from_slice(&frame)
            .map_err(|_| miette!("plugin dev initialize response was invalid JSON"))?;
        let manifest_value = response
            .get("result")
            .cloned()
            .ok_or_else(|| miette!("plugin dev initialize was rejected"))?;
        let manifest: PluginManifest = serde_json::from_value(manifest_value)
            .map_err(|_| miette!("plugin dev initialize returned an invalid manifest"))?;
        manifest.validate().map_err(|error| {
            miette!("plugin dev initialize returned an invalid manifest: {error}")
        })?;
        Result::<()>::Ok(())
    }
    .await;
    if let Err(error) = handshake {
        terminate_and_reap(child.process.as_ref()).await;
        return Err(error);
    }
    // Do not print names, versions, payloads, IDs, params, or results: all are plugin-controlled.
    trace("plugin-rpc response initialize manifest-validated");
    let mut stdout = child.stdout;
    let trace_task = tokio::spawn(async move {
        loop {
            let frame = read_frame(&mut stdout).await?;
            let label = serde_json::from_slice::<Value>(&frame)
                .ok()
                .and_then(|value| value.get("method").and_then(Value::as_str).map(safe_method))
                .unwrap_or("response");
            trace(label);
        }
    });
    Ok(RunningPlugin {
        stdin: child.stdin,
        process: child.process,
        trace: trace_task,
    })
}

fn safe_method(method: &str) -> &'static str {
    match method {
        "tool/call" => "plugin-rpc request tool/call",
        "command/execute" => "plugin-rpc request command/execute",
        "hook/invoke" => "plugin-rpc request hook/invoke",
        "provider/complete" => "plugin-rpc request provider/complete",
        "event/publish" => "plugin-rpc notification event/publish",
        "session/inject_message" => "plugin-rpc request session/inject_message",
        "session/set_status" => "plugin-rpc request session/set_status",
        "ui/notify" => "plugin-rpc request ui/notify",
        "shutdown" => "plugin-rpc request shutdown",
        "exit" => "plugin-rpc notification exit",
        _ => "plugin-rpc unknown-method",
    }
}

async fn shutdown(running: &mut RunningPlugin, trace: Trace) -> Result<()> {
    trace("plugin-dev lifecycle shutdown");
    let _ = write_frame(
        &mut running.stdin,
        &json!({"jsonrpc":"2.0","id":"rottweiler-dev-shutdown","method":"shutdown"}),
    )
    .await;
    let _ = write_frame(
        &mut running.stdin,
        &json!({"jsonrpc":"2.0","method":"exit"}),
    )
    .await;
    if !matches!(
        tokio::time::timeout(Duration::from_millis(250), running.process.wait()).await,
        Ok(Ok(_))
    ) {
        running
            .process
            .kill_tree()
            .map_err(|error| miette!("plugin dev tree termination failed: {error}"))?;
    }
    tokio::time::timeout(SHUTDOWN_DEADLINE, running.process.reap())
        .await
        .map_err(|_| miette!("plugin dev child reap timed out"))?
        .map_err(|error| miette!("plugin dev child reap failed: {error}"))?;
    running.trace.abort();
    trace("plugin-dev lifecycle stopped");
    Ok(())
}

async fn terminate_and_reap(process: &dyn SupervisedPluginProcess) {
    let _ = process.kill_tree();
    let _ = tokio::time::timeout(SHUTDOWN_DEADLINE, process.reap()).await;
}

async fn write_frame(writer: &mut rw_ext::PluginStdin, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).into_diagnostic()?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(miette!("plugin dev host frame exceeded the protocol limit"));
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await.into_diagnostic()?;
    writer.flush().await.into_diagnostic()
}

async fn read_frame(reader: &mut rw_ext::PluginStdout) -> Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.into_diagnostic()?;
        if available.is_empty() {
            return Err(miette!("plugin dev protocol stream ended"));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if frame.len().saturating_add(take) > MAX_FRAME_BYTES + 1 {
            return Err(miette!("plugin dev frame exceeded the protocol limit"));
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take);
        if frame.last() == Some(&b'\n') {
            frame.pop();
            if frame.is_empty() {
                return Err(miette!("plugin dev returned an empty frame"));
            }
            return Ok(frame);
        }
    }
}

fn resolve_target(path: &Path) -> Result<DevTarget> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(miette!("plugin dev refuses a symlink target"));
    }
    let canonical = fs::canonicalize(path).into_diagnostic()?;
    let (root, entry) = if canonical.is_dir() {
        let entry = canonical.join("src/index.ts");
        (canonical, entry)
    } else if canonical.is_file() {
        let root = canonical
            .parent()
            .ok_or_else(|| miette!("plugin dev path has no parent"))?
            .to_path_buf();
        (root, canonical)
    } else {
        return Err(miette!("plugin dev target must be a file or directory"));
    };
    if !entry.is_file() {
        return Err(miette!("plugin dev entrypoint does not exist"));
    }
    let interpreted = matches!(
        entry.extension().and_then(|value| value.to_str()),
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")
    );
    let (executable, argv) = if interpreted {
        (find_executable("bun")?, vec![entry.into_os_string()])
    } else {
        ensure_executable(&entry)?;
        (entry, Vec::new())
    };
    let config = PluginProcessConfig::new(executable)
        .and_then(|config| config.with_argv(argv))
        .and_then(|config| config.with_cwd(&root))
        .map_err(|error| miette!("plugin dev process configuration is invalid: {error}"))?;
    Ok(DevTarget { root, config })
}

fn find_executable(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| miette!("PATH is unavailable"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            ensure_executable(&candidate)?;
            return fs::canonicalize(candidate).into_diagnostic();
        }
    }
    Err(miette!("{name} executable was not found"))
}

fn ensure_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if fs::metadata(path).into_diagnostic()?.permissions().mode() & 0o111 == 0 {
            return Err(miette!("plugin dev target is not executable"));
        }
    }
    Ok(())
}

fn read_only_profile(root: &Path) -> PluginSandboxProfile {
    PluginSandboxProfile {
        mode: PluginSandboxMode::Approved,
        capabilities: PluginCapabilities {
            tools: vec![PluginToolCapability {
                name: "dev-manifest-read".to_owned(),
                description: "Development manifest discovery with read-only filesystem access"
                    .to_owned(),
                schema: json!({"type":"object","additionalProperties":false}),
                caps: vec![PluginToolEffect::ReadsFilesystem],
            }],
            ..PluginCapabilities::default()
        },
        approved_roots: vec![root.to_path_buf()],
        allowed_domains: Vec::new(),
    }
}

fn source_fingerprint(root: &Path) -> Result<blake3::Hash> {
    let mut files = Vec::new();
    collect_sources(root, &mut files, 0)?;
    files.sort();
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_usize;
    for path in files {
        let metadata = fs::symlink_metadata(&path).into_diagnostic()?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(miette!(
                "plugin dev source identity changed during fingerprinting"
            ));
        }
        let length = usize::try_from(metadata.len())
            .map_err(|_| miette!("plugin dev source file size is unsupported"))?;
        total = total
            .checked_add(length)
            .ok_or_else(|| miette!("plugin dev source tree size overflowed"))?;
        if total > MAX_SOURCE_BYTES {
            return Err(miette!("plugin dev source tree exceeds 16 MiB"));
        }
        let bytes = read_source_nofollow(&path, length)?;
        hasher.update(
            path.strip_prefix(root)
                .unwrap_or(&path)
                .as_os_str()
                .as_encoded_bytes(),
        );
        hasher.update(&bytes);
    }
    Ok(hasher.finalize())
}

fn read_source_nofollow(path: &Path, expected: usize) -> Result<Vec<u8>> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags};
        let descriptor = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| miette!("plugin dev source could not be opened safely: {error}"))?;
        fs::File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = fs::File::open(path).into_diagnostic()?;
    let mut bytes = Vec::with_capacity(expected);
    let read_limit = u64::try_from(expected)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    if bytes.len() != expected {
        return Err(miette!(
            "plugin dev source identity changed during fingerprinting"
        ));
    }
    Ok(bytes)
}

fn collect_sources(current: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    if depth > MAX_SOURCE_DEPTH {
        return Err(miette!(
            "plugin dev source tree exceeds 64 directory levels"
        ));
    }
    for entry in fs::read_dir(current).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        if matches!(
            entry.file_name().to_str(),
            Some("node_modules" | "dist" | "target" | ".git")
        ) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).into_diagnostic()?;
        if metadata.file_type().is_symlink() {
            return Err(miette!("plugin dev refuses symlinked source entries"));
        }
        if metadata.is_dir() {
            collect_sources(&path, out, depth + 1)?;
        } else if metadata.is_file() {
            out.push(path);
        }
        if out.len() > MAX_SOURCE_FILES {
            return Err(miette!("plugin dev source tree exceeds 4096 files"));
        }
    }
    Ok(())
}

struct DevScratch(PathBuf);

impl DevScratch {
    fn create() -> Result<Self> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random)
            .map_err(|error| miette!("plugin dev entropy failed: {error}"))?;
        let path = std::env::temp_dir().join(format!(
            "rottweiler-plugin-dev-{}-{}",
            std::process::id(),
            u64::from_ne_bytes(random)
        ));
        fs::create_dir(&path).into_diagnostic()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).into_diagnostic()?;
        }
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for DevScratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]

    use std::{
        os::unix::fs::PermissionsExt as _,
        process::Stdio,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use rw_ext::{
        ApprovalStore, ApprovalStoreError, DenyPushHandler, ExecutableIdentity, HookDispatcher,
        HookEvent, PluginBoundaryRedactor, PluginHost, PluginProcessError, RpcHookHandler,
        RpcToolAdapter, approve_plugin_launch,
    };
    use rw_tools::{Tool as _, ToolContext};
    use serde_json::json;
    use tokio::{io::BufReader, process::Child};

    use super::*;

    struct DirectLauncher {
        launches: AtomicUsize,
    }

    #[async_trait]
    impl PluginLauncher for DirectLauncher {
        async fn launch(
            &self,
            config: &PluginProcessConfig,
            _profile: &PluginSandboxProfile,
        ) -> std::result::Result<LaunchedPluginProcess, PluginProcessError> {
            config.validate_executable_identity()?;
            let mut command = tokio::process::Command::new(config.executable());
            command
                .args(config.argv())
                .current_dir(config.cwd())
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .process_group(0);
            let mut child = command.spawn().map_err(|error| process_error(&error))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| process_error_message("stdin"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| process_error_message("stdout"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| process_error_message("stderr"))?;
            let group = child.id();
            self.launches.fetch_add(1, Ordering::SeqCst);
            Ok(LaunchedPluginProcess {
                stdin: Box::pin(stdin),
                stdout: Box::pin(BufReader::new(stdout)),
                stderr: Box::pin(BufReader::new(stderr)),
                process: Arc::new(DirectProcess {
                    child: Mutex::new(child),
                    group,
                }),
                executable_identity: ExecutableIdentity {
                    canonical_path: config.executable_identity().canonical_path.clone(),
                    device: config.executable_identity().device,
                    inode: config.executable_identity().inode,
                    length: config.executable_identity().length,
                    content_blake3: config.executable_identity().content_blake3.clone(),
                },
            })
        }
    }

    struct DirectProcess {
        child: Mutex<Child>,
        group: Option<u32>,
    }

    impl Drop for DirectProcess {
        fn drop(&mut self) {
            let _ = self.kill_tree();
        }
    }

    #[async_trait]
    impl SupervisedPluginProcess for DirectProcess {
        fn mark_capability_violation(&self, _violation: &rw_ext::CapabilityViolation) {}

        fn kill_tree(&self) -> std::result::Result<(), PluginProcessError> {
            if let Some(pid) = self
                .group
                .and_then(|value| i32::try_from(value).ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                rustix::process::kill_process_group(pid, rustix::process::Signal::KILL)
                    .or_else(|error| {
                        (error == rustix::io::Errno::SRCH)
                            .then_some(())
                            .ok_or(error)
                    })
                    .map_err(|error| process_error_message(&error.to_string()))?;
            }
            Ok(())
        }

        async fn wait(&self) -> std::result::Result<Option<i32>, PluginProcessError> {
            loop {
                if let Some(status) = self
                    .child
                    .lock()
                    .map_err(|_| process_error_message("lock"))?
                    .try_wait()
                    .map_err(|error| process_error(&error))?
                {
                    return Ok(status.code());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    fn process_error(error: &std::io::Error) -> PluginProcessError {
        process_error_message(&error.to_string())
    }

    fn process_error_message(message: &str) -> PluginProcessError {
        PluginProcessError {
            message: message.to_owned(),
        }
    }

    #[derive(Default)]
    struct MemoryApproval(Mutex<std::collections::BTreeMap<String, String>>);

    struct IdentityRedactor;

    impl PluginBoundaryRedactor for IdentityRedactor {
        fn redact(&self, value: serde_json::Value) -> serde_json::Value {
            value
        }
    }

    impl ApprovalStore for MemoryApproval {
        fn approved_fingerprint(
            &self,
            name: &str,
        ) -> std::result::Result<Option<String>, ApprovalStoreError> {
            Ok(self.0.lock().expect("approval lock").get(name).cloned())
        }

        fn record_approval(
            &self,
            name: &str,
            fingerprint: &str,
        ) -> std::result::Result<(), ApprovalStoreError> {
            self.0
                .lock()
                .expect("approval lock")
                .insert(name.to_owned(), fingerprint.to_owned());
            Ok(())
        }
    }

    fn stage_local_sdk(scaffold: &Path) {
        let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/plugin-sdk")
            .canonicalize()
            .expect("SDK root");
        let package = scaffold.join("node_modules/@rottweiler/plugin");
        fs::create_dir_all(&package).expect("package root");
        fs::write(
            package.join("package.json"),
            r#"{"name":"@rottweiler/plugin","type":"module","exports":{"types":"./src/index.ts","default":"./src/index.ts"}}"#,
        )
        .expect("local package manifest");
        std::os::unix::fs::symlink(sdk.join("src"), package.join("src")).expect("SDK source link");
        for dependency in ["@types", "bun-types", "typescript", "undici-types"] {
            let source = sdk.join("node_modules").join(dependency);
            let destination = scaffold.join("node_modules").join(dependency);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).expect("dependency parent");
            }
            std::os::unix::fs::symlink(source, destination).expect("dependency link");
        }
    }

    #[tokio::test]
    async fn cli_scaffold_typechecks_tests_and_crosses_the_rust_host() {
        let temporary = tempfile::tempdir().expect("scaffold root");
        let scaffold = temporary.path().join("fixture");
        crate::plugin_cli::scaffold_typescript(&scaffold, Some("fixture"), false)
            .expect("generate scaffold");
        stage_local_sdk(&scaffold);

        let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/plugin-sdk")
            .canonicalize()
            .expect("SDK root");
        let typecheck = std::process::Command::new(sdk.join("node_modules/.bin/tsc"))
            .args(["--project", "tsconfig.json"])
            .current_dir(&scaffold)
            .output()
            .expect("run typecheck");
        assert!(
            typecheck.status.success(),
            "generated typecheck failed: {}",
            String::from_utf8_lossy(&typecheck.stderr)
        );
        let bun = find_executable("bun").expect("Bun");
        let tests = std::process::Command::new(&bun)
            .args(["test"])
            .current_dir(&scaffold)
            .output()
            .expect("run generated tests");
        assert!(
            tests.status.success(),
            "generated tests failed: {}",
            String::from_utf8_lossy(&tests.stderr)
        );

        let manifest = PluginManifest::from_slice(
            &fs::read(scaffold.join("manifest.json")).expect("manifest bytes"),
        )
        .expect("trusted manifest");
        let process = PluginProcessConfig::new(bun)
            .and_then(|config| config.with_argv([scaffold.join("src/index.ts").into_os_string()]))
            .and_then(|config| config.with_cwd(&scaffold))
            .expect("plugin process config");
        let approvals = MemoryApproval::default();
        approve_plugin_launch(&approvals, &manifest, &process, "scaffold:fixture")
            .expect("approve exact scaffold");
        let launcher = DirectLauncher {
            launches: AtomicUsize::new(0),
        };
        let host = PluginHost::launch_approved(
            &launcher,
            &approvals,
            &process,
            "scaffold:fixture",
            std::slice::from_ref(&scaffold),
            manifest.clone(),
            Arc::new(DenyPushHandler),
            Arc::new(IdentityRedactor),
        )
        .await
        .expect("launch scaffold through Rust host");
        let tool = RpcToolAdapter::new(
            manifest.capabilities.tools[0].clone(),
            host.client(),
            host.enforcer(),
        )
        .expect("tool adapter");
        let output = tool
            .execute(
                &ToolContext::new(&scaffold).expect("tool context"),
                json!({"name":"Rottweiler"}),
            )
            .await
            .expect("custom tool");
        assert_eq!(output.content, "Hello, Rottweiler!");
        let hook = RpcHookHandler::new(host.client(), host.enforcer());
        let mut dispatcher = HookDispatcher::new();
        dispatcher
            .register(
                manifest.capabilities.hooks[0].registration("scaffold:pre-tool"),
                hook,
            )
            .expect("hook registration");
        assert!(matches!(
            dispatcher
                .dispatch(HookEvent::PreTool, json!({"name":"bash"}))
                .await
                .status(),
            rw_ext::HookDispatchStatus::Blocked { .. }
        ));
        host.shutdown().await.expect("scaffold shutdown");
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_post_init_stream_is_detected_and_child_is_reaped() {
        let project = tempfile::tempdir().expect("project");
        let script = project.path().join("oversized.sh");
        let pid_file = project.path().join("pid");
        fs::write(
            &script,
            r#"#!/bin/sh
printf '%s' "$$" > "$1"
while IFS= read -r frame; do
  case "$frame" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":"rottweiler-dev-init","result":{"name":"oversized","version":"1","protocol":1,"capabilities":{}}}'
      head -c 4194305 /dev/zero | tr '\000' x
      printf '\n'
      ;;
  esac
done
"#,
        )
        .expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("mode");
        let config = PluginProcessConfig::new(fs::canonicalize(&script).expect("script path"))
            .and_then(|config| config.with_argv([pid_file.clone().into_os_string()]))
            .and_then(|config| config.with_cwd(project.path()))
            .expect("process config");
        let target = DevTarget {
            root: fs::canonicalize(project.path()).expect("root"),
            config,
        };
        let launcher = Arc::new(DirectLauncher {
            launches: AtomicUsize::new(0),
        });
        let (_stop_tx, stop_rx) = watch::channel(false);
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            supervise(
                launcher,
                target,
                stop_rx,
                Arc::new(|_| {}),
                SupervisorOptions::default(),
            ),
        )
        .await
        .expect("supervisor deadline")
        .expect_err("oversized trace must fail");
        assert!(error.to_string().contains("frame exceeded"));
        let pid = fs::read_to_string(pid_file)
            .expect("pid")
            .parse::<i32>()
            .expect("numeric pid");
        let pid = rustix::process::Pid::from_raw(pid).expect("positive pid");
        assert_eq!(
            rustix::process::test_kill_process(pid).expect_err("child must be reaped"),
            rustix::io::Errno::SRCH
        );
    }

    #[tokio::test]
    async fn source_change_restarts_once_traces_are_redacted_and_children_are_reaped() {
        let project = tempfile::tempdir().expect("project");
        let state = tempfile::tempdir().expect("state");
        let script = project.path().join("fixture.sh");
        let source = project.path().join("source.txt");
        let pids = state.path().join("pids");
        fs::write(&source, "one").expect("source");
        fs::write(
            &script,
            r#"#!/bin/sh
printf '%s\n' "$$" >> "$1"
while IFS= read -r frame; do
  case "$frame" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":"rottweiler-dev-init","result":{"name":"fixture","version":"CANARY_SECRET","protocol":1,"capabilities":{}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":"push","method":"ui/notify","params":{"message":"CANARY_SECRET"}}'
      ;;
    *'"method":"shutdown"'*) exit 0 ;;
  esac
done
"#,
        )
        .expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("mode");
        let executable = fs::canonicalize(&script).expect("canonical script");
        let config = PluginProcessConfig::new(executable)
            .and_then(|config| config.with_argv([pids.clone().into_os_string()]))
            .and_then(|config| config.with_cwd(project.path()))
            .expect("config");
        let target = DevTarget {
            root: fs::canonicalize(project.path()).expect("root"),
            config,
        };
        let launcher = Arc::new(DirectLauncher {
            launches: AtomicUsize::new(0),
        });
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let trace_values = Arc::clone(&captured);
        let trace: Trace = Arc::new(move |line| {
            trace_values
                .lock()
                .expect("trace lock")
                .push(line.to_owned());
        });
        let (_stop_tx, stop_rx) = watch::channel(false);
        let edit = tokio::spawn({
            let pids = pids.clone();
            let source = source.clone();
            async move {
                loop {
                    if fs::read_to_string(&pids).is_ok_and(|contents| contents.lines().count() == 1)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                fs::write(&source, "two").expect("edit");
                tokio::time::sleep(Duration::from_millis(10)).await;
                fs::write(&source, "three").expect("coalesced edit");
            }
        });
        supervise(
            launcher.clone(),
            target,
            stop_rx,
            trace,
            SupervisorOptions {
                poll: Duration::from_millis(10),
                debounce: Duration::from_millis(40),
                max_launches: Some(2),
            },
        )
        .await
        .expect("supervise");
        edit.await.expect("edit task");
        assert_eq!(launcher.launches.load(Ordering::SeqCst), 2);
        let pid_values = fs::read_to_string(&pids).expect("pids");
        assert_eq!(pid_values.lines().count(), 2);
        for pid in pid_values.lines() {
            let status = std::process::Command::new("/bin/kill")
                .args(["-0", pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("kill probe");
            assert!(!status.success(), "child {pid} was not reaped");
        }
        let rendered = captured.lock().expect("trace lock").join("\n");
        assert!(!rendered.contains("CANARY_SECRET"));
        assert_eq!(rendered.matches("lifecycle restart").count(), 1);
    }
}
