//! Bounded JSON-RPC plugin protocol, manifest validation, and capability guards.

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use thiserror::Error;

use crate::{HookDirective, HookError, HookEvent, HookHandler, HookInvocation};

#[cfg(test)]
use rw_plugin_protocol::{
    FrameDecoder, FrameError, MAX_FRAME_BYTES, MAX_MANIFEST_BYTES, MAX_VERSION_BYTES,
    METHOD_PROVIDER_COMPLETE, METHOD_PROVIDER_EVENT, METHOD_PROVIDER_HTTP,
    METHOD_PROVIDER_HTTP_CANCEL, METHOD_PROVIDER_HTTP_EVENT, METHOD_PROVIDER_MODELS,
    METHOD_TOOL_CALL, METHOD_UI_NOTIFY, PROTOCOL_VERSION, PluginProviderCapability,
};
use rw_plugin_protocol::{
    HookInvokeParams, MAX_CAPABILITIES_PER_KIND, MAX_HOOK_PAYLOAD_BYTES, MAX_NAME_BYTES,
    MAX_RPC_MESSAGE_BYTES, METHOD_HOOK_INVOKE, ManifestError, PluginCapabilities, PluginHook,
    PluginHookCapability, PluginHookFailurePolicy, PluginManifest, PluginPush,
    PluginToolCapability, PluginToolEffect,
};

impl From<PluginHookFailurePolicy> for crate::HookFailurePolicy {
    fn from(policy: PluginHookFailurePolicy) -> Self {
        match policy {
            PluginHookFailurePolicy::FailOpen => Self::FailOpen,
            PluginHookFailurePolicy::FailClosed => Self::FailClosed,
        }
    }
}

impl From<PluginHook> for HookEvent {
    fn from(hook: PluginHook) -> Self {
        match hook {
            PluginHook::SessionStart => Self::SessionStart,
            PluginHook::SessionEnd => Self::SessionEnd,
            PluginHook::UserPromptSubmit => Self::UserPromptSubmit,
            PluginHook::PreTool => Self::PreTool,
            PluginHook::PostTool => Self::PostTool,
            PluginHook::PreCompact => Self::PreCompact,
            PluginHook::TurnEnd => Self::TurnEnd,
            PluginHook::PermissionCheck => Self::PermissionCheck,
        }
    }
}

impl From<HookEvent> for PluginHook {
    fn from(event: HookEvent) -> Self {
        match event {
            HookEvent::SessionStart => Self::SessionStart,
            HookEvent::SessionEnd => Self::SessionEnd,
            HookEvent::UserPromptSubmit => Self::UserPromptSubmit,
            HookEvent::PreTool => Self::PreTool,
            HookEvent::PostTool => Self::PostTool,
            HookEvent::PreCompact => Self::PreCompact,
            HookEvent::TurnEnd => Self::TurnEnd,
            HookEvent::PermissionCheck => Self::PermissionCheck,
        }
    }
}

/// Converts a protocol hook declaration into rw-ext's runtime registration.
#[must_use]
pub fn plugin_hook_registration(
    declaration: PluginHookCapability,
    id: impl Into<String>,
) -> crate::HookRegistration {
    crate::HookRegistration::new(id, declaration.name.into())
        .with_failure_policy(declaration.failure_policy.into())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("approval store failed: {message}")]
pub struct ApprovalStoreError {
    pub message: String,
}

pub trait ApprovalStore: Send + Sync {
    /// Loads the last explicitly approved fingerprint, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing store cannot be read.
    fn approved_fingerprint(&self, plugin_name: &str)
    -> Result<Option<String>, ApprovalStoreError>;
    /// Persists one explicit manifest approval.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing store cannot be written durably.
    fn record_approval(
        &self,
        plugin_name: &str,
        fingerprint: &str,
    ) -> Result<(), ApprovalStoreError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalRequirement {
    Approved,
    FirstLoad { fingerprint: String },
    ManifestChanged { previous: String, current: String },
}

/// Compares a manifest with the last explicit approval.
///
/// # Errors
///
/// Returns an error when validation, fingerprinting, or store access fails.
pub fn approval_requirement(
    store: &dyn ApprovalStore,
    manifest: &PluginManifest,
) -> Result<ApprovalRequirement, PluginApprovalError> {
    let current = manifest.fingerprint()?;
    match store.approved_fingerprint(&manifest.name)? {
        None => Ok(ApprovalRequirement::FirstLoad {
            fingerprint: current,
        }),
        Some(previous) if previous == current => Ok(ApprovalRequirement::Approved),
        Some(previous) => Ok(ApprovalRequirement::ManifestChanged { previous, current }),
    }
}

/// Records explicit approval for the manifest's current fingerprint.
///
/// # Errors
///
/// Returns an error when validation, fingerprinting, or persistence fails.
pub fn approve_manifest(
    store: &dyn ApprovalStore,
    manifest: &PluginManifest,
) -> Result<String, PluginApprovalError> {
    let fingerprint = manifest.fingerprint()?;
    store.record_approval(&manifest.name, &fingerprint)?;
    Ok(fingerprint)
}

#[derive(Debug, Error)]
pub enum PluginApprovalError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Store(#[from] ApprovalStoreError),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PluginProcessConfigError {
    #[error("plugin executable must resolve from an absolute path to a real executable file")]
    InvalidExecutable,
    #[error("plugin working directory is invalid: {0}")]
    InvalidCwd(String),
    #[error("plugin environment allowlist contains an invalid variable name")]
    InvalidEnvironmentName,
    #[error("plugin environment variable is not in the safe host allowlist")]
    UnsafeEnvironmentName,
    #[error("plugin argv contains an interior NUL byte")]
    InvalidArgument,
    #[error("plugin network allowlist contains an invalid public domain")]
    InvalidAllowedDomain,
    #[error("plugin content attestation contains an invalid regular file")]
    InvalidAttestedFile,
    #[error("plugin content attestation exceeds its file or byte limit")]
    AttestationLimit,
}

/// A direct-exec process description. Launchers must clear the environment and
/// restore only the named variables; no field is interpreted by a shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginProcessConfig {
    executable: PathBuf,
    argv: Vec<OsString>,
    cwd: PathBuf,
    environment_allowlist: BTreeSet<OsString>,
    allowed_domains: BTreeSet<String>,
    executable_identity: ExecutableIdentity,
    attested_files: Vec<ExecutableIdentity>,
    code_root: Option<CodeRootIdentity>,
    source_identity: Option<SourcePluginIdentity>,
}

/// Stable filesystem identity pinned when a plugin executable is configured.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExecutableIdentity {
    pub canonical_path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub length: u64,
    pub content_blake3: String,
}

/// Stable identity for the narrowly readable plugin package directory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CodeRootIdentity {
    pub canonical_path: PathBuf,
    pub device: u64,
    pub inode: u64,
}

/// Content identity of one host-prepared TypeScript source package.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SourcePluginIdentity {
    pub graph_blake3: String,
    pub lockfile_blake3: String,
    pub bundle_blake3: String,
    pub host_abi: u32,
    pub bundle_format: String,
}

impl PluginProcessConfig {
    /// Creates a direct-exec configuration using the validated current directory.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid executable or current directory.
    pub fn new(executable: impl Into<PathBuf>) -> Result<Self, PluginProcessConfigError> {
        let executable = validate_executable(&executable.into())?;
        let cwd = std::env::current_dir()
            .map_err(|error| PluginProcessConfigError::InvalidCwd(error.to_string()))?;
        let cwd = validate_cwd(&cwd)?;
        Ok(Self {
            executable_identity: executable_identity(&executable)?,
            executable,
            argv: Vec::new(),
            cwd,
            environment_allowlist: BTreeSet::new(),
            allowed_domains: BTreeSet::new(),
            attested_files: Vec::new(),
            code_root: None,
            source_identity: None,
        })
    }

    /// Replaces the literal argument vector.
    ///
    /// # Errors
    ///
    /// Returns an error when an argument contains an interior NUL byte.
    pub fn with_argv(
        mut self,
        argv: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Result<Self, PluginProcessConfigError> {
        self.argv = argv.into_iter().map(Into::into).collect();
        if self.argv.iter().any(|arg| os_contains_nul(arg)) {
            return Err(PluginProcessConfigError::InvalidArgument);
        }
        Ok(self)
    }

    /// Sets and canonicalizes the plugin working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the path does not resolve to a directory.
    pub fn with_cwd(mut self, cwd: impl AsRef<Path>) -> Result<Self, PluginProcessConfigError> {
        self.cwd = validate_cwd(cwd.as_ref())?;
        Ok(self)
    }

    /// Sets the environment variable names restored after clearing the environment.
    ///
    /// # Errors
    ///
    /// Returns an error when a variable name is not canonical uppercase ASCII.
    pub fn with_environment_allowlist(
        mut self,
        names: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Result<Self, PluginProcessConfigError> {
        self.environment_allowlist = names.into_iter().map(Into::into).collect();
        if self
            .environment_allowlist
            .iter()
            .any(|name| !valid_environment_name(name))
        {
            return Err(PluginProcessConfigError::InvalidEnvironmentName);
        }
        if self
            .environment_allowlist
            .iter()
            .any(|name| !safe_environment_name(name))
        {
            return Err(PluginProcessConfigError::UnsafeEnvironmentName);
        }
        Ok(self)
    }

    /// Sets the exact public DNS names approved for plugin network egress.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, local, or excessive domain entries.
    pub fn with_allowed_domains(
        mut self,
        domains: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, PluginProcessConfigError> {
        self.allowed_domains = domains.into_iter().map(Into::into).collect();
        if self.allowed_domains.len() > MAX_CAPABILITIES_PER_KIND
            || self
                .allowed_domains
                .iter()
                .any(|domain| !valid_public_domain(domain))
        {
            return Err(PluginProcessConfigError::InvalidAllowedDomain);
        }
        Ok(self)
    }

    /// Pins interpreter entrypoints and adjacent dependency descriptors whose
    /// contents affect the approved plugin process.
    ///
    /// # Errors
    ///
    /// Returns an error for non-regular files, duplicates, or excessive
    /// attestation work.
    pub fn with_attested_files(
        mut self,
        paths: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Result<Self, PluginProcessConfigError> {
        const MAX_ATTESTED_FILES: usize = 64;
        const MAX_ATTESTED_BYTES: u64 = 256 * 1024 * 1024;
        let mut canonical = paths
            .into_iter()
            .map(Into::into)
            .map(|path| {
                if std::fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(PluginProcessConfigError::InvalidAttestedFile);
                }
                let path = std::fs::canonicalize(path)
                    .map_err(|_| PluginProcessConfigError::InvalidAttestedFile)?;
                if !path.is_file() {
                    return Err(PluginProcessConfigError::InvalidAttestedFile);
                }
                Ok(path)
            })
            .collect::<Result<Vec<_>, _>>()?;
        canonical.sort();
        canonical.dedup();
        if canonical.len() > MAX_ATTESTED_FILES {
            return Err(PluginProcessConfigError::AttestationLimit);
        }
        let mut total = 0_u64;
        let mut identities = Vec::with_capacity(canonical.len());
        for path in canonical {
            let identity = executable_identity(&path)?;
            if let Some(root) = &self.code_root
                && identity.canonical_path != self.executable
                && !identity.canonical_path.starts_with(&root.canonical_path)
            {
                return Err(PluginProcessConfigError::InvalidAttestedFile);
            }
            total = total
                .checked_add(identity.length)
                .ok_or(PluginProcessConfigError::AttestationLimit)?;
            if total > MAX_ATTESTED_BYTES {
                return Err(PluginProcessConfigError::AttestationLimit);
            }
            identities.push(identity);
        }
        self.attested_files = identities;
        Ok(self)
    }

    /// Pins the only plugin-owned code/package directory readable without a
    /// `reads-fs` capability. It must be a real non-symlink directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the root is a symlink, is not a directory, or
    /// cannot be resolved to a stable canonical identity.
    pub fn with_code_root(
        mut self,
        root: impl AsRef<Path>,
    ) -> Result<Self, PluginProcessConfigError> {
        self.code_root = Some(directory_identity(root.as_ref())?);
        Ok(self)
    }

    /// Binds this process to the normalized source graph prepared by the private host.
    #[must_use]
    pub fn with_source_identity(mut self, identity: SourcePluginIdentity) -> Self {
        self.source_identity = Some(identity);
        self
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn environment_allowlist(&self) -> &BTreeSet<OsString> {
        &self.environment_allowlist
    }

    #[must_use]
    pub fn allowed_domains(&self) -> &BTreeSet<String> {
        &self.allowed_domains
    }

    #[must_use]
    pub const fn env_clear(&self) -> bool {
        true
    }

    #[must_use]
    pub const fn executable_identity(&self) -> &ExecutableIdentity {
        &self.executable_identity
    }

    #[must_use]
    pub fn attested_files(&self) -> &[ExecutableIdentity] {
        &self.attested_files
    }

    #[must_use]
    pub const fn code_root(&self) -> Option<&CodeRootIdentity> {
        self.code_root.as_ref()
    }

    #[must_use]
    pub const fn source_identity(&self) -> Option<&SourcePluginIdentity> {
        self.source_identity.as_ref()
    }

    /// Revalidates the executable immediately before a launcher calls `exec`.
    ///
    /// # Errors
    ///
    /// Returns an error if the path was substituted after approval.
    pub fn validate_executable_identity(&self) -> Result<(), PluginProcessError> {
        let current =
            executable_identity(&self.executable).map_err(|error| PluginProcessError {
                message: error.to_string(),
            })?;
        if current != self.executable_identity {
            return Err(PluginProcessError {
                message: "approved plugin executable identity changed before exec".to_owned(),
            });
        }
        for expected in &self.attested_files {
            let current = executable_identity(&expected.canonical_path).map_err(|error| {
                PluginProcessError {
                    message: error.to_string(),
                }
            })?;
            if current != *expected {
                return Err(PluginProcessError {
                    message: "approved plugin content identity changed before exec".to_owned(),
                });
            }
        }
        if let Some(expected) = &self.code_root {
            let current = directory_identity(&expected.canonical_path).map_err(|error| {
                PluginProcessError {
                    message: error.to_string(),
                }
            })?;
            if current != *expected {
                return Err(PluginProcessError {
                    message: "approved plugin code-root identity changed before exec".to_owned(),
                });
            }
        }
        Ok(())
    }
}

fn directory_identity(path: &Path) -> Result<CodeRootIdentity, PluginProcessConfigError> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(PluginProcessConfigError::InvalidAttestedFile);
    }
    let canonical =
        std::fs::canonicalize(path).map_err(|_| PluginProcessConfigError::InvalidAttestedFile)?;
    let metadata =
        std::fs::metadata(&canonical).map_err(|_| PluginProcessConfigError::InvalidAttestedFile)?;
    if !metadata.is_dir() {
        return Err(PluginProcessConfigError::InvalidAttestedFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(CodeRootIdentity {
            canonical_path: canonical,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(CodeRootIdentity {
            canonical_path: canonical,
            device: 0,
            inode: 0,
        })
    }
}

fn executable_identity(path: &Path) -> Result<ExecutableIdentity, PluginProcessConfigError> {
    const MAX_IDENTITY_FILE_BYTES: u64 = 256 * 1024 * 1024;
    let metadata =
        std::fs::metadata(path).map_err(|_| PluginProcessConfigError::InvalidExecutable)?;
    if !metadata.is_file() || metadata.len() > MAX_IDENTITY_FILE_BYTES {
        return Err(PluginProcessConfigError::AttestationLimit);
    }
    let file =
        std::fs::File::open(path).map_err(|_| PluginProcessConfigError::InvalidExecutable)?;
    let mut file = file.take(metadata.len().saturating_add(1));
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut read_bytes = 0_u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| PluginProcessConfigError::InvalidExecutable)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        read_bytes = read_bytes.saturating_add(count as u64);
    }
    if read_bytes != metadata.len() {
        return Err(PluginProcessConfigError::InvalidExecutable);
    }
    let content_blake3 = hasher.finalize().to_hex().to_string();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(ExecutableIdentity {
            canonical_path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            content_blake3,
        })
    }
    #[cfg(not(unix))]
    {
        Ok(ExecutableIdentity {
            canonical_path: path.to_path_buf(),
            device: 0,
            inode: 0,
            length: metadata.len(),
            content_blake3,
        })
    }
}

fn validate_executable(executable: &Path) -> Result<PathBuf, PluginProcessConfigError> {
    if !executable.is_absolute()
        || executable.as_os_str().is_empty()
        || os_contains_nul(executable.as_os_str())
    {
        return Err(PluginProcessConfigError::InvalidExecutable);
    }
    let canonical = std::fs::canonicalize(executable)
        .map_err(|_| PluginProcessConfigError::InvalidExecutable)?;
    if !canonical.is_file() || !is_executable(&canonical) {
        return Err(PluginProcessConfigError::InvalidExecutable);
    }
    Ok(canonical)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

fn validate_cwd(cwd: &Path) -> Result<PathBuf, PluginProcessConfigError> {
    let canonical = std::fs::canonicalize(cwd)
        .map_err(|error| PluginProcessConfigError::InvalidCwd(error.to_string()))?;
    if !canonical.is_dir() {
        return Err(PluginProcessConfigError::InvalidCwd(
            "path is not a directory".to_owned(),
        ));
    }
    Ok(canonical)
}

#[cfg(unix)]
fn os_contains_nul(value: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().contains(&0)
}

#[cfg(not(unix))]
fn os_contains_nul(value: &OsStr) -> bool {
    value.to_string_lossy().contains('\0')
}

fn valid_environment_name(value: &OsStr) -> bool {
    let value = value.to_string_lossy();
    !value.is_empty()
        && value.len() <= MAX_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn safe_environment_name(value: &OsStr) -> bool {
    let value = value.to_string_lossy();
    matches!(
        value.as_ref(),
        "LANG" | "LC_ALL" | "LC_CTYPE" | "TERM" | "TZ" | "NO_COLOR" | "FORCE_COLOR"
    ) && !value.contains("KEY")
        && !value.contains("TOKEN")
        && !value.contains("SECRET")
        && !value.contains("PASSWORD")
        && !matches!(
            value.as_ref(),
            "LD_PRELOAD"
                | "LD_LIBRARY_PATH"
                | "DYLD_INSERT_LIBRARIES"
                | "DYLD_LIBRARY_PATH"
                | "NODE_OPTIONS"
                | "BUN_OPTIONS"
                | "PYTHONPATH"
                | "RUSTC_WRAPPER"
        )
}

fn valid_public_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.is_ascii()
        && !domain.ends_with('.')
        && domain.split('.').count() >= 2
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        && !matches!(domain, "localhost" | "localhost.localdomain")
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("plugin process error: {message}")]
pub struct PluginProcessError {
    pub message: String,
}

#[async_trait]
pub trait SupervisedPluginProcess: Send + Sync {
    /// Records why the process is untrusted before termination.
    fn mark_capability_violation(&self, violation: &CapabilityViolation);
    /// Terminates the plugin and its complete descendant process tree.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor cannot terminate the process tree.
    fn kill_tree(&self) -> Result<(), PluginProcessError>;

    /// Waits until the direct child exits and returns its exit code when available.
    async fn wait(&self) -> Result<Option<i32>, PluginProcessError> {
        Ok(None)
    }

    /// Reaps the direct child after termination.
    async fn reap(&self) -> Result<(), PluginProcessError> {
        let _ = self.wait().await?;
        Ok(())
    }

    /// Reaps the child and proves no descendant can continue executing effects.
    async fn settle_effects(&self) -> Result<(), PluginProcessError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityKind {
    Tool,
    Command,
    Hook,
    Provider,
    ProviderCredential,
    Event,
    Push,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("plugin attempted undeclared {kind:?} capability `{name}`")]
pub struct CapabilityViolation {
    pub kind: CapabilityKind,
    pub name: String,
}

/// Immutable manifest snapshot used to police every message after initialization.
pub struct CapabilityEnforcer {
    capabilities: PluginCapabilities,
    process: Arc<dyn SupervisedPluginProcess>,
    violated: AtomicBool,
    violation: Mutex<Option<CapabilityEnforcementError>>,
    termination_retry: Mutex<()>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{violation}{termination_suffix}", termination_suffix = .termination_error.as_ref().map(|value| format!("; termination failed: {}", value.message)).unwrap_or_default())]
pub struct CapabilityEnforcementError {
    pub violation: CapabilityViolation,
    pub termination_error: Option<PluginProcessError>,
}

impl CapabilityEnforcer {
    /// Creates an immutable capability snapshot for a supervised process.
    ///
    pub fn new(manifest: &PluginManifest, process: Arc<dyn SupervisedPluginProcess>) -> Self {
        Self {
            capabilities: manifest.capabilities.clone(),
            process,
            violated: AtomicBool::new(false),
            violation: Mutex::new(None),
            termination_retry: Mutex::new(()),
        }
    }

    /// Verifies a tool declaration, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the tool was not declared.
    pub fn check_tool(&self, name: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Tool,
            name,
            self.capabilities.tools.iter().any(|tool| tool.name == name),
        )
    }

    #[must_use]
    pub fn tool_declaration_matches(&self, declaration: &PluginToolCapability) -> bool {
        self.capabilities
            .tools
            .iter()
            .any(|approved| approved == declaration)
    }

    /// Returns the effective authority held by the shared plugin process.
    ///
    /// A plugin process is one sandbox principal, so every tool adapter must
    /// present this union to the permission and checkpoint chokepoints rather
    /// than claiming only its handler's narrower declaration.
    #[must_use]
    pub fn process_tool_effects(&self) -> BTreeSet<PluginToolEffect> {
        let mut effects = self
            .capabilities
            .tools
            .iter()
            .flat_map(|tool| tool.caps.iter().copied())
            .collect::<BTreeSet<_>>();
        if !self.capabilities.providers.is_empty() {
            effects.insert(PluginToolEffect::Network);
        }
        effects
    }

    /// Verifies a command declaration, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the command was not declared.
    pub fn check_command(&self, name: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Command,
            name,
            self.capabilities
                .commands
                .iter()
                .any(|command| command.name == name),
        )
    }

    /// Verifies a hook declaration, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the hook was not declared.
    pub fn check_hook(&self, hook: PluginHook) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Hook,
            hook.as_str(),
            self.capabilities
                .hooks
                .iter()
                .any(|declaration| declaration.name == hook),
        )
    }

    /// Verifies that a model alias matches a declared provider prefix.
    ///
    /// # Errors
    ///
    /// Returns a violation when no provider prefix matches.
    pub fn check_provider(&self, alias: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Provider,
            alias,
            self.capabilities
                .providers
                .iter()
                .any(|provider| alias.starts_with(&provider.alias_prefix)),
        )
    }

    /// Verifies that a credential reference was approval-fingerprinted for the
    /// declared provider prefix serving `alias`.
    ///
    /// # Errors
    ///
    /// Returns a terminal capability violation for an undeclared alias/reference pair.
    pub fn check_provider_credential(
        &self,
        alias: &str,
        credential_reference: &str,
    ) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::ProviderCredential,
            credential_reference,
            self.capabilities.providers.iter().any(|provider| {
                alias.starts_with(&provider.alias_prefix)
                    && provider
                        .credential_references
                        .iter()
                        .any(|declared| declared == credential_reference)
            }),
        )
    }

    /// Verifies an event subscription, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the event was not declared.
    pub fn check_event(&self, event: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Event,
            event,
            self.capabilities
                .event_subscriptions
                .iter()
                .any(|declared| declared == event),
        )
    }

    /// Verifies a typed push capability, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the push method was not declared.
    pub fn check_push(&self, method: PluginPush) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Push,
            method.method(),
            self.capabilities.push.contains(&method),
        )
    }

    /// Verifies a wire push method, terminating the process on violation.
    ///
    /// # Errors
    ///
    /// Returns a violation when the push method was not declared.
    pub fn check_push_method(&self, method: &str) -> Result<(), CapabilityEnforcementError> {
        self.check(
            CapabilityKind::Push,
            method,
            self.capabilities
                .push
                .iter()
                .any(|declared| declared.method() == method),
        )
    }

    #[must_use]
    pub fn violated(&self) -> bool {
        self.violated.load(Ordering::Acquire)
    }

    fn check(
        &self,
        kind: CapabilityKind,
        name: &str,
        declared: bool,
    ) -> Result<(), CapabilityEnforcementError> {
        let cached_violation = {
            self.violation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        if let Some(mut error) = cached_violation {
            if error.termination_error.is_some() {
                let _retry = self
                    .termination_retry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                error = self
                    .violation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .unwrap_or(error);
                if error.termination_error.is_some() && self.process.kill_tree().is_ok() {
                    error.termination_error = None;
                    *self
                        .violation
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
                }
            }
            return Err(error);
        }
        if declared {
            return Ok(());
        }
        let violation = CapabilityViolation {
            kind,
            name: name.to_owned(),
        };
        let error = if self.violated.swap(true, Ordering::AcqRel) {
            CapabilityEnforcementError {
                violation,
                termination_error: None,
            }
        } else {
            self.process.mark_capability_violation(&violation);
            CapabilityEnforcementError {
                violation,
                termination_error: self.process.kill_tree().err(),
            }
        };
        *self
            .violation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
        Err(error)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("plugin RPC failed: {code}: {message}")]
pub struct PluginRpcError {
    pub code: String,
    pub message: String,
}

/// Incremental, request-scoped provider events received from a plugin. The
/// concrete transport owns cancellation and correlation cleanup when dropped.
pub type PluginProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<Value, PluginRpcError>> + Send + 'static>>;

#[async_trait]
pub trait PluginRpcClient: Send + Sync {
    /// Waits for teardown started by a cancelled or dropped request.
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, PluginRpcError>;

    async fn request_cancellable(
        &self,
        method: &str,
        params: Value,
        cancellation: &rw_tools::CancellationToken,
    ) -> Result<Value, PluginRpcError> {
        if cancellation.is_cancelled() {
            return Err(PluginRpcError {
                code: "cancelled".to_owned(),
                message: "plugin RPC request was cancelled".to_owned(),
            });
        }
        self.request(method, params).await
    }

    /// Calls a tool under host-owned total and idle deadlines.
    async fn call_tool(
        &self,
        _params: rw_plugin_protocol::ToolCallParams,
        _cancellation: &rw_tools::CancellationToken,
        _progress: Arc<dyn rw_tools::ToolProgressSink>,
    ) -> Result<Value, PluginRpcError> {
        Err(PluginRpcError {
            code: "unsupported".to_owned(),
            message: "RPC tool operations are unsupported".to_owned(),
        })
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), PluginRpcError> {
        let _ = (method, params);
        Err(PluginRpcError {
            code: "unsupported".to_owned(),
            message: "RPC notifications are unsupported".to_owned(),
        })
    }

    /// Starts a provider request whose events arrive incrementally over the
    /// protocol's correlated `provider/event` notification channel.
    async fn provider_stream(
        &self,
        _params: Value,
    ) -> Result<PluginProviderEventStream, PluginRpcError> {
        Err(PluginRpcError {
            code: "unsupported".to_owned(),
            message: "RPC provider streaming is unsupported".to_owned(),
        })
    }
}

/// Adapter that registers an out-of-process hook through the common dispatcher.
pub struct RpcHookHandler {
    client: Arc<dyn PluginRpcClient>,
    enforcer: Arc<CapabilityEnforcer>,
}

impl RpcHookHandler {
    #[must_use]
    pub fn new(client: Arc<dyn PluginRpcClient>, enforcer: Arc<CapabilityEnforcer>) -> Self {
        Self { client, enforcer }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum RpcHookResponse {
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
    Deny {
        message: String,
    },
    Replace {
        payload: Value,
    },
}

#[async_trait]
impl HookHandler for RpcHookHandler {
    async fn settle_effects(&self) -> std::result::Result<(), crate::HookError> {
        self.client
            .settle_effects()
            .await
            .map_err(|error| HookError::new("effects_unsettled", error.to_string()))
    }
    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        let hook = PluginHook::from(invocation.event());
        self.enforcer
            .check_hook(hook)
            .map_err(|error| HookError::new("capability_violation", error.to_string()))?;
        let result = self
            .client
            .request_cancellable(
                METHOD_HOOK_INVOKE,
                serde_json::to_value(HookInvokeParams {
                    hook,
                    payload: invocation.payload().clone(),
                })
                .map_err(|error| {
                    HookError::new("invalid_request", format!("hook request failed: {error}"))
                })?,
                invocation.cancellation(),
            )
            .await
            .map_err(|error| {
                if error.code.is_empty()
                    || error.code.len() > MAX_NAME_BYTES
                    || error.code.chars().any(char::is_control)
                    || error.message.is_empty()
                    || error.message.len() > MAX_RPC_MESSAGE_BYTES
                    || error.message.chars().any(char::is_control)
                {
                    HookError::new("invalid_rpc_error", "plugin returned an invalid RPC error")
                } else {
                    HookError::new(error.code, error.message)
                }
            })?;
        let mut result = result;
        if let Value::Object(object) = &mut result
            && !object.contains_key("decision")
            && let Some(action) = object.remove("action")
        {
            object.insert("decision".to_owned(), action);
        }
        let response: RpcHookResponse = serde_json::from_value(result)
            .map_err(|error| HookError::new("invalid_response", error.to_string()))?;
        match response {
            RpcHookResponse::Allow { payload: None } => Ok(HookDirective::Continue),
            RpcHookResponse::Allow {
                payload: Some(payload),
            }
            | RpcHookResponse::Replace { payload }
                if serde_json::to_vec(&payload)
                    .is_ok_and(|bytes| bytes.len() <= MAX_HOOK_PAYLOAD_BYTES) =>
            {
                Ok(HookDirective::Replace(payload))
            }
            RpcHookResponse::Allow { payload: Some(_) } | RpcHookResponse::Replace { .. } => {
                Err(HookError::new(
                    "invalid_response",
                    "hook replacement payload exceeds the limit",
                ))
            }
            RpcHookResponse::Deny { message }
                if !message.is_empty()
                    && message.len() <= MAX_RPC_MESSAGE_BYTES
                    && !message.chars().any(char::is_control) =>
            {
                Ok(HookDirective::Block { message })
            }
            RpcHookResponse::Deny { .. } => Err(HookError::new(
                "invalid_response",
                "hook denial message is invalid",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::{
        sync::{Barrier, Mutex, atomic::AtomicUsize},
        thread,
        time::Duration,
    };

    use super::*;
    use crate::{HookDispatchStatus, HookDispatcher, HookRegistration};

    fn manifest() -> PluginManifest {
        PluginManifest {
            name: "safe-plugin".to_owned(),
            version: "1.0.0".to_owned(),
            protocol: PROTOCOL_VERSION,
            capabilities: PluginCapabilities {
                tools: vec![PluginToolCapability {
                    name: "read_custom".to_owned(),
                    description: "Read custom data".to_owned(),
                    schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
                    caps: vec![PluginToolEffect::ReadsFilesystem],
                }],
                hooks: vec![PluginHookCapability {
                    name: PluginHook::PreTool,
                    failure_policy: PluginHookFailurePolicy::FailClosed,
                }],
                providers: vec![PluginProviderCapability {
                    alias_prefix: "custom/".to_owned(),
                    capabilities: vec!["models".to_owned()],
                    credential_references: vec!["provider-token".to_owned()],
                }],
                event_subscriptions: vec!["ToolCallFinished".to_owned()],
                push: vec![PluginPush::UiNotify],
                ..PluginCapabilities::default()
            },
        }
    }

    #[test]
    fn valid_manifest_has_order_independent_fingerprint() {
        let first = manifest();
        first.validate().expect("valid manifest");
        let mut second = first.clone();
        second.capabilities.hooks.reverse();
        second.capabilities.tools[0].caps.reverse();
        assert_eq!(
            first.fingerprint().expect("fingerprint"),
            second.fingerprint().expect("fingerprint")
        );
    }

    #[test]
    fn credential_reference_access_is_fingerprinted_and_enforced_per_provider() {
        let first = manifest();
        let mut widened = first.clone();
        widened.capabilities.providers[0]
            .credential_references
            .push("second-token".to_owned());
        assert_ne!(
            first.fingerprint().expect("first fingerprint"),
            widened.fingerprint().expect("widened fingerprint")
        );

        let process = Arc::new(ProcessState::default());
        let enforcer = CapabilityEnforcer::new(&first, process.clone());
        enforcer
            .check_provider_credential("custom/model", "provider-token")
            .expect("declared credential");
        assert!(
            enforcer
                .check_provider_credential("custom/model", "second-token")
                .is_err()
        );
        assert!(process.killed.load(Ordering::Acquire));
    }

    #[test]
    fn typescript_sdk_manifest_fixture_is_compatible() {
        let fixture = json!({
            "name": "sdk-fixture",
            "version": "1.0.0",
            "protocol": PROTOCOL_VERSION,
            "capabilities": {
                "tools": [{
                    "name": "hello",
                    "description": "Return a greeting",
                    "schema": {"type":"object","properties":{"name":{"type":"string"}}},
                    "caps": ["reads-fs", "network"]
                }],
                "commands": [{
                    "name": "greet",
                    "description": "Greet a user",
                    "argument_hint": "<name>",
                    "allowed_tools": ["hello"]
                }],
                "hooks": [{"name":"pre_tool","failure_policy":"fail-closed"}],
                "providers": [{"alias-prefix":"fixture/"}],
                "event_subscriptions": ["TurnFinished"]
            }
        });
        let bytes = serde_json::to_vec(&fixture).expect("fixture JSON");
        let manifest = PluginManifest::from_slice(&bytes).expect("TS SDK manifest");
        assert_eq!(manifest.capabilities.tools[0].caps.len(), 2);
        assert_eq!(
            manifest.capabilities.hooks[0].failure_policy,
            PluginHookFailurePolicy::FailClosed
        );
    }

    #[test]
    fn language_neutral_protocol_fixture_matches_rust_constants() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../packages/plugin-sdk/fixtures/wire/protocol-3.json"
        ))
        .expect("protocol fixture JSON");
        assert_eq!(fixture["protocol"], PROTOCOL_VERSION);
        assert_eq!(fixture["limits"]["max_frame_bytes"], MAX_FRAME_BYTES);
        assert_eq!(fixture["limits"]["max_manifest_bytes"], MAX_MANIFEST_BYTES);
        assert_eq!(fixture["limits"]["max_name_bytes"], MAX_NAME_BYTES);
        assert_eq!(fixture["limits"]["max_version_bytes"], MAX_VERSION_BYTES);
        assert_eq!(fixture["methods"]["toolCall"], METHOD_TOOL_CALL);
        assert_eq!(
            fixture["methods"]["providerComplete"],
            METHOD_PROVIDER_COMPLETE
        );
        assert_eq!(fixture["methods"]["providerEvent"], METHOD_PROVIDER_EVENT);
        assert_eq!(fixture["methods"]["notify"], METHOD_UI_NOTIFY);
        assert_eq!(fixture["methods"]["providerModels"], METHOD_PROVIDER_MODELS);
        assert_eq!(fixture["methods"]["providerHttp"], METHOD_PROVIDER_HTTP);
        assert_eq!(
            fixture["methods"]["providerHttpEvent"],
            METHOD_PROVIDER_HTTP_EVENT
        );
        assert_eq!(
            fixture["methods"]["providerHttpCancel"],
            METHOD_PROVIDER_HTTP_CANCEL
        );
    }

    #[test]
    fn decoder_rejects_oversized_and_malformed_frames() {
        let mut decoder = FrameDecoder::new(32);
        assert!(matches!(
            decoder.push(&[b'x'; 33]),
            Err(FrameError::TooLarge { limit: 32 })
        ));
        let mut decoder = FrameDecoder::default();
        assert!(matches!(
            decoder.push(b"{not-json}\n"),
            Err(FrameError::Malformed(_))
        ));
    }

    #[derive(Default)]
    struct MemoryApprovals(Mutex<BTreeMap<String, String>>);

    impl ApprovalStore for MemoryApprovals {
        fn approved_fingerprint(
            &self,
            plugin_name: &str,
        ) -> Result<Option<String>, ApprovalStoreError> {
            Ok(self.0.lock().expect("approvals").get(plugin_name).cloned())
        }

        fn record_approval(
            &self,
            plugin_name: &str,
            fingerprint: &str,
        ) -> Result<(), ApprovalStoreError> {
            self.0
                .lock()
                .expect("approvals")
                .insert(plugin_name.to_owned(), fingerprint.to_owned());
            Ok(())
        }
    }

    #[test]
    fn first_and_changed_manifests_require_approval() {
        let store = MemoryApprovals::default();
        let first = manifest();
        assert!(matches!(
            approval_requirement(&store, &first).expect("requirement"),
            ApprovalRequirement::FirstLoad { .. }
        ));
        approve_manifest(&store, &first).expect("approval");
        assert_eq!(
            approval_requirement(&store, &first).expect("requirement"),
            ApprovalRequirement::Approved
        );
        let mut changed = first;
        changed.capabilities.push.push(PluginPush::SessionSetStatus);
        assert!(matches!(
            approval_requirement(&store, &changed).expect("requirement"),
            ApprovalRequirement::ManifestChanged { .. }
        ));
    }

    #[derive(Default)]
    struct ProcessState {
        violations: Mutex<Vec<CapabilityViolation>>,
        killed: AtomicBool,
        kill_count: AtomicUsize,
    }

    #[async_trait]
    impl SupervisedPluginProcess for ProcessState {
        async fn settle_effects(&self) -> Result<(), PluginProcessError> {
            Ok(())
        }
        fn mark_capability_violation(&self, violation: &CapabilityViolation) {
            self.violations
                .lock()
                .expect("violations")
                .push(violation.clone());
        }

        fn kill_tree(&self) -> Result<(), PluginProcessError> {
            self.killed.store(true, Ordering::Release);
            self.kill_count.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    #[test]
    fn undeclared_capability_marks_and_kills_process() {
        let process = Arc::new(ProcessState::default());
        let enforcer = CapabilityEnforcer::new(&manifest(), process.clone());
        assert!(enforcer.check_tool("secret_tool").is_err());
        assert!(enforcer.violated());
        assert!(process.killed.load(Ordering::Acquire));
        assert_eq!(process.violations.lock().expect("violations").len(), 1);
    }

    #[test]
    fn cached_violation_does_not_repeat_successful_process_termination() {
        let process = Arc::new(ProcessState::default());
        let enforcer = CapabilityEnforcer::new(&manifest(), process.clone());
        let first = enforcer
            .check_tool("secret_tool")
            .expect_err("undeclared tool");
        let cached = enforcer
            .check_command("secret_command")
            .expect_err("cached violation");
        assert_eq!(cached, first);
        assert_eq!(process.kill_count.load(Ordering::Acquire), 1);
        assert_eq!(process.violations.lock().expect("violations").len(), 1);
    }

    #[derive(Default)]
    struct RetryProcess {
        kill_count: AtomicUsize,
    }

    #[async_trait]
    impl SupervisedPluginProcess for RetryProcess {
        async fn settle_effects(&self) -> Result<(), PluginProcessError> {
            Ok(())
        }
        fn mark_capability_violation(&self, _violation: &CapabilityViolation) {}

        fn kill_tree(&self) -> Result<(), PluginProcessError> {
            let attempt = self.kill_count.fetch_add(1, Ordering::AcqRel) + 1;
            if attempt == 1 {
                return Err(PluginProcessError {
                    message: "initial termination failed".to_owned(),
                });
            }
            if attempt == 2 {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(())
        }
    }

    #[test]
    fn concurrent_cached_violation_checks_share_one_successful_retry() {
        let process = Arc::new(RetryProcess::default());
        let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process.clone()));
        let initial = enforcer
            .check_tool("secret_tool")
            .expect_err("initial violation");
        assert!(initial.termination_error.is_some());

        let start = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let enforcer = enforcer.clone();
            let start = start.clone();
            workers.push(thread::spawn(move || {
                start.wait();
                enforcer
                    .check_command("secret_command")
                    .expect_err("cached violation")
            }));
        }
        start.wait();
        for worker in workers {
            assert!(
                worker
                    .join()
                    .expect("retry worker")
                    .termination_error
                    .is_none()
            );
        }
        assert_eq!(process.kill_count.load(Ordering::Acquire), 2);
    }

    struct DenyClient;

    #[async_trait]
    impl PluginRpcClient for DenyClient {
        async fn request(&self, method: &str, params: Value) -> Result<Value, PluginRpcError> {
            assert_eq!(method, METHOD_HOOK_INVOKE);
            assert_eq!(params["hook"], "pre_tool");
            Ok(json!({"decision":"deny","message":"blocked by plugin"}))
        }
    }

    #[tokio::test]
    async fn pre_tool_deny_uses_common_hook_dispatcher() {
        let process = Arc::new(ProcessState::default());
        let enforcer = Arc::new(CapabilityEnforcer::new(&manifest(), process));
        let handler = RpcHookHandler::new(Arc::new(DenyClient), enforcer);
        let mut dispatcher = HookDispatcher::new();
        dispatcher
            .register(
                HookRegistration::new("plugin:pre-tool", HookEvent::PreTool),
                handler,
            )
            .expect("hook registration");
        let result = dispatcher
            .dispatch(HookEvent::PreTool, json!({"name":"write"}))
            .await;
        assert_eq!(
            result.status(),
            &HookDispatchStatus::Blocked {
                hook_id: "plugin:pre-tool".to_owned(),
                message: "blocked by plugin".to_owned(),
            }
        );
    }
}
