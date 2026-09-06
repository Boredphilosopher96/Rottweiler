use std::{
    collections::BTreeSet,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use async_trait::async_trait;
use rw_sandbox::{
    EgressPolicy, LaunchPlan, NetworkPolicy, SandboxPolicy, SandboxSupport, SupervisedEgressProxy,
    normalize_egress_domain, shell_launch_plan,
};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Exact argv request for a long-lived, line/framed protocol child.
#[derive(Clone, Debug)]
pub struct ProtocolChildRequest {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    pub environment: Vec<(String, String)>,
    /// Explicit filesystem and public-domain authority for this child.
    pub sandbox: ProtocolSandboxPolicy,
}

/// Bounded authority requested by a long-lived protocol child.
///
/// Empty lists are the fail-closed default: only intrinsic runtime reads,
/// scratch writes, and no network are granted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProtocolSandboxPolicy {
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
}

const MAX_PROTOCOL_ROOTS: usize = 64;
const MAX_PROTOCOL_DOMAINS: usize = 32;

/// Process-tree ownership retained independently from the protocol transport.
#[async_trait]
pub trait ProtocolProcessHandle: Send {
    /// Briefly observe a natural direct-child exit without terminating it.
    async fn observe_exit(&mut self, deadline: Duration) -> io::Result<Option<ExitStatus>>;

    /// Kill the whole process group and synchronously reap the direct child.
    async fn terminate_and_reap(&mut self, deadline: Duration) -> io::Result<()>;
}

/// Piped protocol endpoints plus explicit process-tree ownership.
pub struct SpawnedProtocolChild {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub handle: Box<dyn ProtocolProcessHandle>,
}

/// Injectable launcher boundary used by protocol clients such as MCP.
#[async_trait]
pub trait ProtocolChildLauncher: Send + Sync {
    async fn spawn(&self, request: &ProtocolChildRequest) -> io::Result<SpawnedProtocolChild>;
}

/// OS-sandboxed launcher for long-lived protocol children.
///
/// Executables and cwd values are canonicalized and checked against the
/// launcher authority roots. The child receives only the fixed HOME/TMPDIR
/// values and explicitly approved environment keys.
pub struct SandboxedProtocolLauncher {
    workspace_roots: Vec<PathBuf>,
    scratch: PathBuf,
    helper_executable: rw_sandbox::SandboxHelper,
    allowed_environment: BTreeSet<String>,
    sandbox_unavailable: Option<String>,
}

impl SandboxedProtocolLauncher {
    /// Validate and pin the launcher's workspace, scratch, helper, and env authority.
    ///
    /// # Errors
    ///
    /// Returns an error when an authority path cannot be canonicalized or is
    /// writable/untrusted, or when the scratch has symlink provenance.
    pub fn new(
        workspace_roots: &[PathBuf],
        scratch: impl AsRef<Path>,
        helper_executable: &rw_sandbox::SandboxHelper,
        allowed_environment: impl IntoIterator<Item = String>,
    ) -> io::Result<Self> {
        let workspace_roots = workspace_roots
            .iter()
            .map(std::fs::canonicalize)
            .collect::<io::Result<Vec<_>>>()?;
        let scratch = std::fs::canonicalize(scratch)?;
        if !scratch.is_dir() || has_symlink_provenance(&scratch) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe protocol scratch directory",
            ));
        }
        let helper_executable = helper_executable.clone();
        let capability = rw_sandbox::probe();
        Ok(Self {
            workspace_roots,
            scratch,
            helper_executable,
            allowed_environment: allowed_environment.into_iter().collect(),
            sandbox_unavailable: (capability.support == SandboxSupport::Unavailable).then(|| {
                capability.warning.unwrap_or_else(|| {
                    "the operating system has no supported sandbox backend".to_owned()
                })
            }),
        })
    }

    fn cwd(&self, requested: Option<&Path>) -> io::Result<PathBuf> {
        let requested = requested
            .or_else(|| self.workspace_roots.first().map(PathBuf::as_path))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "protocol cwd is required")
            })?;
        let canonical = std::fs::canonicalize(requested)?;
        if !canonical.is_dir()
            || !self
                .workspace_roots
                .iter()
                .any(|root| canonical.starts_with(root))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "protocol cwd is outside approved workspace roots",
            ));
        }
        Ok(canonical)
    }
}

#[async_trait]
impl ProtocolChildLauncher for SandboxedProtocolLauncher {
    async fn spawn(&self, request: &ProtocolChildRequest) -> io::Result<SpawnedProtocolChild> {
        validate_request(request, &self.allowed_environment)?;
        if let Some(reason) = &self.sandbox_unavailable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("sandboxed protocol execution is unavailable: {reason}"),
            ));
        }
        let executable = trusted_executable(&request.executable, &self.workspace_roots)?;
        let identity = executable_identity(&executable)?;
        let cwd = self.cwd(request.working_directory.as_deref())?;
        let (policy, proxy) = self.sandbox_policy(request, &executable)?;
        let args = request.args.iter().map(OsString::from).collect::<Vec<_>>();
        let mut plan = shell_launch_plan(&policy, &self.helper_executable, &executable, &args)
            .map_err(|error| io::Error::other(error.to_string()))?;
        // Close the canonicalization-to-spawn replacement window as far as the
        // platform API permits. Linux additionally pins the sandbox helper in LaunchPlan.
        if executable_identity(&executable)? != identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "protocol executable changed during launch",
            ));
        }
        let mut command = tokio::process::Command::new(&plan.program);
        command
            .args(&plan.args)
            .current_dir(cwd)
            .env_clear()
            .env("HOME", &self.scratch)
            .env("TMPDIR", &self.scratch)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in &request.environment {
            command.env(key, value);
        }
        if let Some(proxy) = &proxy {
            command
                .env("HTTP_PROXY", proxy.url())
                .env("HTTPS_PROXY", proxy.url())
                .env("ALL_PROXY", proxy.url())
                .env("NO_PROXY", "");
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        release_helper_pin(&mut plan);
        let process_group = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("protocol stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("protocol stdout unavailable"))?;
        Ok(SpawnedProtocolChild {
            stdin,
            stdout,
            handle: Box::new(TokioProtocolHandle {
                child,
                process_group,
                _proxy: proxy,
            }),
        })
    }
}

impl SandboxedProtocolLauncher {
    fn sandbox_policy(
        &self,
        request: &ProtocolChildRequest,
        executable: &Path,
    ) -> io::Result<(SandboxPolicy, Option<SupervisedEgressProxy>)> {
        let mut write_roots = vec![self.scratch.clone()];
        write_roots.extend(self.approved_roots(&request.sandbox.write_roots)?);
        let mut read_roots = intrinsic_protocol_read_roots(executable, &self.scratch);
        read_roots.extend(self.approved_roots(&request.sandbox.read_roots)?);
        read_roots.extend(write_roots.iter().cloned());
        read_roots.sort();
        read_roots.dedup();

        let domains = validated_domains(&request.sandbox.allowed_domains)?;
        let proxy = if domains.is_empty() {
            None
        } else {
            Some(
                SupervisedEgressProxy::start(EgressPolicy::new(&domains))
                    .map_err(|error| io::Error::other(error.to_string()))?,
            )
        };
        let network =
            proxy
                .as_ref()
                .map_or(NetworkPolicy::Deny, |proxy| NetworkPolicy::PolicyProxy {
                    port: proxy.address().port(),
                    relay_path: proxy.relay_path().map(Path::to_path_buf),
                });
        let policy = SandboxPolicy::new(&write_roots, network)
            .and_then(|policy| policy.with_read_roots(read_roots))
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok((policy, proxy))
    }

    fn approved_roots(&self, supplied: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
        if supplied.len() > MAX_PROTOCOL_ROOTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "protocol filesystem authority exceeds its root limit",
            ));
        }
        supplied
            .iter()
            .map(|root| {
                let canonical = std::fs::canonicalize(root)?;
                if !self
                    .workspace_roots
                    .iter()
                    .any(|workspace| canonical.starts_with(workspace))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "protocol filesystem authority is outside approved workspace roots",
                    ));
                }
                Ok(canonical)
            })
            .collect()
    }
}

fn intrinsic_protocol_read_roots(executable: &Path, scratch: &Path) -> Vec<PathBuf> {
    let mut roots = vec![executable.to_path_buf(), scratch.to_path_buf()];
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
    roots
}

fn validated_domains(domains: &[String]) -> io::Result<Vec<String>> {
    if domains.len() > MAX_PROTOCOL_DOMAINS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "protocol network authority exceeds its domain limit",
        ));
    }
    let normalized = domains
        .iter()
        .map(|domain| {
            normalize_egress_domain(domain).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid protocol egress domain",
                )
            })
        })
        .collect::<io::Result<BTreeSet<_>>>()?;
    Ok(normalized.into_iter().collect())
}

fn validate_request(request: &ProtocolChildRequest, allowed: &BTreeSet<String>) -> io::Result<()> {
    if request.executable.as_os_str().is_empty()
        || request.args.iter().any(|arg| arg.contains('\0'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid protocol executable or argument",
        ));
    }
    for (key, value) in &request.environment {
        if !allowed.contains(key)
            || key.is_empty()
            || key.contains(['=', '\0'])
            || value.contains('\0')
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "protocol environment key is not approved",
            ));
        }
    }
    Ok(())
}

fn trusted_executable(candidate: &Path, workspace_roots: &[PathBuf]) -> io::Result<PathBuf> {
    if !candidate.is_absolute() || has_symlink_provenance(candidate) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "protocol executable must be an absolute path without symlink provenance",
        ));
    }
    let canonical = std::fs::canonicalize(candidate)?;
    let metadata = std::fs::metadata(&canonical)?;
    if !metadata.is_file()
        || workspace_roots
            .iter()
            .any(|root| canonical.starts_with(root))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "protocol executable is not trusted",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "protocol executable is writable by group or other",
            ));
        }
    }
    Ok(canonical)
}

#[cfg(unix)]
fn executable_identity(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn executable_identity(path: &Path) -> io::Result<(u64, u64)> {
    let metadata = std::fs::metadata(path)?;
    Ok((
        metadata.len(),
        metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
    ))
}

fn has_symlink_provenance(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return true;
        }
    }
    false
}

struct TokioProtocolHandle {
    child: Child,
    process_group: Option<u32>,
    _proxy: Option<SupervisedEgressProxy>,
}

#[async_trait]
impl ProtocolProcessHandle for TokioProtocolHandle {
    async fn observe_exit(&mut self, deadline: Duration) -> io::Result<Option<ExitStatus>> {
        match tokio::time::timeout(deadline, self.child.wait()).await {
            Ok(status) => status.map(Some),
            Err(_) => Ok(None),
        }
    }

    async fn terminate_and_reap(&mut self, deadline: Duration) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(group) = self
            .process_group
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        #[cfg(not(unix))]
        self.child.start_kill()?;
        tokio::time::timeout(deadline, self.child.wait())
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "protocol child did not exit")
            })??;
        #[cfg(unix)]
        if let Some(group) = self
            .process_group
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let end = tokio::time::Instant::now() + deadline;
            while rustix::process::test_kill_process_group(group).is_ok() {
                if tokio::time::Instant::now() >= end {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "protocol process group did not exit",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        self.process_group = None;
        Ok(())
    }
}

impl Drop for TokioProtocolHandle {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self
            .process_group
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            // Async cancellation can drop a connector while rmcp initialization
            // is pending. Signal the complete group synchronously so descendants
            // cannot survive merely because the async reap path was cancelled.
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = self.child.start_kill();
    }
}

fn release_helper_pin(plan: &mut LaunchPlan) {
    #[cfg(target_os = "linux")]
    drop(plan.take_helper_pin());
    #[cfg(not(target_os = "linux"))]
    let _ = plan;
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn request(sandbox: ProtocolSandboxPolicy) -> ProtocolChildRequest {
        ProtocolChildRequest {
            executable: PathBuf::from("/usr/bin/true"),
            args: Vec::new(),
            working_directory: None,
            environment: Vec::new(),
            sandbox,
        }
    }

    #[tokio::test]
    async fn protocol_launcher_rejects_environment_outside_allowlist_before_spawn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scratch = tempfile::tempdir().expect("scratch");
        let helper = std::fs::canonicalize(std::env::current_exe().expect("test executable"))
            .expect("canonical helper executable");
        let launcher = SandboxedProtocolLauncher::new(
            &[workspace.path().to_path_buf()],
            scratch.path(),
            &rw_sandbox::SandboxHelper::from_running(&helper).expect("running helper"),
            Vec::<String>::new(),
        )
        .expect("launcher");
        let error = launcher
            .spawn(&ProtocolChildRequest {
                executable: PathBuf::from("/usr/bin/true"),
                args: Vec::new(),
                working_directory: Some(workspace.path().to_path_buf()),
                environment: vec![("UNAPPROVED_SECRET".to_owned(), "canary".to_owned())],
                sandbox: ProtocolSandboxPolicy::default(),
            })
            .await
            .err()
            .expect("environment must fail");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!error.to_string().contains("canary"));
    }

    #[tokio::test]
    async fn protocol_launcher_rejects_an_unavailable_sandbox_before_spawn() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scratch = tempfile::tempdir().expect("scratch");
        let helper = std::fs::canonicalize(std::env::current_exe().expect("test executable"))
            .expect("canonical helper executable");
        let mut launcher = SandboxedProtocolLauncher::new(
            &[workspace.path().to_path_buf()],
            scratch.path(),
            &rw_sandbox::SandboxHelper::from_running(&helper).expect("running helper"),
            Vec::<String>::new(),
        )
        .expect("launcher");
        launcher.sandbox_unavailable = Some("test sandbox backend is blocked".to_owned());

        let error = launcher
            .spawn(&request(ProtocolSandboxPolicy::default()))
            .await
            .err()
            .expect("unsupported sandbox must fail closed");

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            "sandboxed protocol execution is unavailable: test sandbox backend is blocked"
        );
    }

    #[test]
    fn protocol_authority_is_workspace_bounded_and_defaults_fail_closed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scratch = tempfile::tempdir().expect("scratch");
        let helper = std::fs::canonicalize(std::env::current_exe().expect("test executable"))
            .expect("canonical helper executable");
        let allowed = workspace.path().join("allowed");
        std::fs::create_dir(&allowed).expect("allowed root");
        let launcher = SandboxedProtocolLauncher::new(
            &[workspace.path().to_path_buf()],
            scratch.path(),
            &rw_sandbox::SandboxHelper::from_running(&helper).expect("running helper"),
            Vec::<String>::new(),
        )
        .expect("launcher");
        let executable = PathBuf::from("/usr/bin/true");

        let (default_policy, proxy) = launcher
            .sandbox_policy(&request(ProtocolSandboxPolicy::default()), &executable)
            .expect("default policy");
        assert!(matches!(default_policy.network(), NetworkPolicy::Deny));
        assert!(proxy.is_none());
        assert_eq!(
            default_policy.write_roots(),
            &[scratch.path().canonicalize().expect("canonical scratch")]
        );
        assert!(default_policy.read_roots().is_some());

        let scoped = request(ProtocolSandboxPolicy {
            read_roots: vec![allowed.clone()],
            write_roots: vec![allowed],
            allowed_domains: vec!["api.example.com".to_owned()],
        });
        let (policy, proxy) = launcher
            .sandbox_policy(&scoped, &executable)
            .expect("scoped policy");
        assert!(matches!(
            policy.network(),
            NetworkPolicy::PolicyProxy { .. }
        ));
        assert!(proxy.is_some());

        let outside = tempfile::tempdir().expect("outside");
        let outside_request = request(ProtocolSandboxPolicy {
            read_roots: vec![outside.path().to_path_buf()],
            ..ProtocolSandboxPolicy::default()
        });
        assert!(
            launcher
                .sandbox_policy(&outside_request, &executable)
                .is_err()
        );
    }
}
