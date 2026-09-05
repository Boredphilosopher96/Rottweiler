use super::*;

/// Immutable sandbox input. Launchers translate this into their platform profile.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginSandboxProfile {
    pub mode: PluginSandboxMode,
    pub capabilities: PluginCapabilities,
    pub approved_roots: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginSandboxMode {
    /// Initialization-only launch: deny network/writes and expose only runtime/entrypoint reads.
    #[cfg(test)]
    ManifestProbe,
    /// Release-owned source graph discovery and sealed-bundle preparation.
    Preparation {
        #[cfg(target_os = "linux")]
        filesystem: Box<rw_tools::PreparationFilesystem>,
    },
    Approved,
}

impl PluginSandboxProfile {
    #[must_use]
    pub fn allows_workspace_reads(&self) -> bool {
        self.capabilities.tools.iter().any(|tool| {
            tool.caps
                .contains(&rw_plugin_protocol::PluginToolEffect::ReadsFilesystem)
        })
    }

    #[must_use]
    pub fn allows_workspace_writes(&self) -> bool {
        self.capabilities.tools.iter().any(|tool| {
            tool.caps
                .contains(&rw_plugin_protocol::PluginToolEffect::WritesFilesystem)
        })
    }

    #[must_use]
    pub fn requests_network(&self) -> bool {
        !self.capabilities.providers.is_empty()
            || self.capabilities.tools.iter().any(|tool| {
                tool.caps
                    .contains(&rw_plugin_protocol::PluginToolEffect::Network)
            })
    }

    #[must_use]
    pub fn allows_network(&self) -> bool {
        self.requests_network() && !self.allowed_domains.is_empty()
    }
}

/// A launched child with exclusive stdio ownership and a supervised process handle.
pub struct LaunchedPluginProcess {
    pub stdin: PluginStdin,
    pub stdout: PluginStdout,
    pub stderr: PluginStdout,
    pub process: Arc<dyn SupervisedPluginProcess>,
    /// Identity re-attested by the launcher at its final pre-spawn boundary.
    pub executable_identity: ExecutableIdentity,
}

/// Mandatory host boundary for removing known secrets before any value reaches a plugin.
pub trait PluginBoundaryRedactor: Send + Sync {
    fn redact(&self, value: Value) -> Value;

    /// Redacts known credential bytes before an HTTP response chunk is encoded
    /// onto the plugin wire.
    fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        value.to_vec()
    }

    /// Redacts the safely-emittable prefix while returning the original tail
    /// needed to detect a credential completed by the next transport chunk.
    fn redact_streaming_prefix(&self, value: &[u8], retain: usize) -> (Vec<u8>, Vec<u8>) {
        if retain == 0 {
            (self.redact_bytes(value), Vec::new())
        } else {
            (Vec::new(), value.to_vec())
        }
    }

    /// Longest registered credential, used to retain an exact cross-chunk overlap.
    fn maximum_secret_bytes(&self) -> usize {
        0
    }
}

/// Test-only identity boundary. Production composition must inject the shared redactor.
#[cfg(test)]
pub(crate) struct NoopPluginBoundaryRedactor;

#[cfg(test)]
impl PluginBoundaryRedactor for NoopPluginBoundaryRedactor {
    fn redact(&self, value: Value) -> Value {
        value
    }

    fn redact_bytes(&self, value: &[u8]) -> Vec<u8> {
        value.to_vec()
    }

    fn maximum_secret_bytes(&self) -> usize {
        0
    }
}

pub type PluginHttpByteStream =
    Pin<Box<dyn Stream<Item = Result<Vec<u8>, PluginRpcError>> + Send + 'static>>;

/// Host-owned response to one authenticated plugin-provider HTTP request.
pub struct PluginHttpStreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: PluginHttpByteStream,
}

/// Trusted host boundary that resolves credentials and owns the provider socket.
#[async_trait]
pub trait PluginProviderHttpHandler: Send + Sync {
    async fn request(
        &self,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<PluginHttpStreamResponse, PluginRpcError>;
}

pub struct DenyPluginProviderHttpHandler;

#[async_trait]
impl PluginProviderHttpHandler for DenyPluginProviderHttpHandler {
    async fn request(
        &self,
        _params: Value,
        _cancellation: &CancellationToken,
    ) -> Result<PluginHttpStreamResponse, PluginRpcError> {
        Err(rpc_error(
            "provider_http_unavailable",
            "host-mediated provider HTTP is unavailable on this host surface",
        ))
    }
}

/// Injected process launcher. Production launchers must sandbox before direct exec.
#[async_trait]
pub trait PluginLauncher: Send + Sync {
    /// Launches by direct exec. Implementations must revalidate and return the exact executable
    /// identity at the final spawn boundary, clear the environment, create a killable process
    /// group, and enforce every absent profile effect at syscall level. Manifest probes may read
    /// only their runtime/entrypoint; approved launches may read/write/network only when the
    /// corresponding helper above permits it. Network must traverse the policy proxy and exact
    /// public-domain allowlist.
    async fn launch(
        &self,
        config: &PluginProcessConfig,
        profile: &PluginSandboxProfile,
    ) -> Result<LaunchedPluginProcess, PluginProcessError>;
}

/// Host-owned handler for declared plugin-to-host push requests.
/// The returned future must await completion of admitted effects, including any
/// delegated actor command. Teardown drains this future before releasing callers.
#[async_trait]
pub trait PushHandler: Send + Sync {
    async fn handle_push(&self, method: &str, params: Value) -> Result<Value, PluginRpcError>;
}

/// Rejects every push. Useful when a host surface has no interactive session attached.
pub struct DenyPushHandler;

#[async_trait]
impl PushHandler for DenyPushHandler {
    async fn handle_push(&self, _method: &str, _params: Value) -> Result<Value, PluginRpcError> {
        Err(PluginRpcError {
            code: "push_unavailable".to_owned(),
            message: "plugin push is unavailable on this host surface".to_owned(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PluginApprovalIdentity {
    pub plugin_name: String,
    pub manifest_fingerprint: String,
    pub executable: ExecutableIdentity,
    pub config_fingerprint: String,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::SourcePluginIdentity>,
}

impl PluginApprovalIdentity {
    pub(super) fn fingerprint(&self) -> Result<String, PluginHostError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| PluginHostError::Protocol(error.to_string()))?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }
}

pub(super) fn approval_identity(
    manifest: &PluginManifest,
    config: &PluginProcessConfig,
    origin: &str,
) -> Result<PluginApprovalIdentity, PluginHostError> {
    if origin.is_empty() || origin.len() > 4096 || origin.chars().any(char::is_control) {
        return Err(PluginHostError::Approval(
            "plugin origin is invalid".to_owned(),
        ));
    }
    let mut config_value = json!({
        "argv": config.argv().iter().map(|value| os_fingerprint_bytes(value)).collect::<Vec<_>>(),
        "cwd": config.cwd(),
        "environment": config.environment_allowlist().iter().map(|value| os_fingerprint_bytes(value)).collect::<Vec<_>>(),
        "allowed_domains": config.allowed_domains(),
        "attested_files": config.attested_files(),
        "code_root": config.code_root(),
    });
    if let Some(source) = config.source_identity() {
        config_value["source"] = serde_json::to_value(source)
            .map_err(|error| PluginHostError::Protocol(error.to_string()))?;
    }
    let config_bytes = serde_json::to_vec(&config_value)
        .map_err(|error| PluginHostError::Protocol(error.to_string()))?;
    Ok(PluginApprovalIdentity {
        plugin_name: manifest.name.clone(),
        manifest_fingerprint: manifest.fingerprint().map_err(PluginApprovalError::from)?,
        executable: config.executable_identity().clone(),
        config_fingerprint: blake3::hash(&config_bytes).to_hex().to_string(),
        origin: origin.to_owned(),
        source: config.source_identity().cloned(),
    })
}

#[cfg(unix)]
fn os_fingerprint_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_fingerprint_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

/// Compares the exact executable/config/origin/manifest identity with durable approval.
///
/// # Errors
///
/// Returns an error if the identity cannot be validated, fingerprinted, or loaded.
pub fn plugin_launch_approval_requirement(
    store: &dyn ApprovalStore,
    manifest: &PluginManifest,
    config: &PluginProcessConfig,
    origin: &str,
) -> Result<ApprovalRequirement, PluginHostError> {
    let identity = approval_identity(manifest, config, origin)?;
    let current = identity.fingerprint()?;
    match store.approved_fingerprint(&manifest.name)? {
        None => Ok(ApprovalRequirement::FirstLoad {
            fingerprint: current,
        }),
        Some(previous) if previous == current => Ok(ApprovalRequirement::Approved),
        Some(previous) => Ok(ApprovalRequirement::ManifestChanged { previous, current }),
    }
}

/// Records explicit approval for an exact executable/config/origin/manifest identity.
///
/// # Errors
///
/// Returns an error if the identity cannot be validated, fingerprinted, or persisted.
pub fn approve_plugin_launch(
    store: &dyn ApprovalStore,
    manifest: &PluginManifest,
    config: &PluginProcessConfig,
    origin: &str,
) -> Result<String, PluginHostError> {
    let fingerprint = approval_identity(manifest, config, origin)?.fingerprint()?;
    store.record_approval(&manifest.name, &fingerprint)?;
    Ok(fingerprint)
}

#[derive(Debug, Error)]
pub enum PluginHostError {
    #[error(transparent)]
    ApprovalStore(#[from] crate::plugin::ApprovalStoreError),
    #[error(transparent)]
    ApprovalDetails(#[from] PluginApprovalError),
    #[error("plugin launch is not approved: {0}")]
    Approval(String),
    #[error(transparent)]
    Process(#[from] PluginProcessError),
    #[error("plugin protocol failed: {0}")]
    Protocol(String),
    #[error(transparent)]
    Rpc(#[from] PluginRpcError),
}
