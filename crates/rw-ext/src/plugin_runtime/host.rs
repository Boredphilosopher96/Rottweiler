use super::*;

/// Running plugin with an immutable manifest/capability snapshot.
pub struct PluginHost {
    manifest: PluginManifest,
    pub(super) client: Arc<JsonRpcPluginClient>,
    enforcer: Arc<CapabilityEnforcer>,
}

fn approved_plugin_profile(
    store: &dyn ApprovalStore,
    config: &PluginProcessConfig,
    origin: &str,
    approved_roots: &[PathBuf],
    expected_manifest: &PluginManifest,
) -> Result<PluginSandboxProfile, PluginHostError> {
    expected_manifest
        .validate()
        .map_err(PluginApprovalError::from)?;
    if plugin_launch_approval_requirement(store, expected_manifest, config, origin)?
        != ApprovalRequirement::Approved
    {
        return Err(PluginHostError::Approval(
            "executable, config, origin, or manifest requires explicit approval".to_owned(),
        ));
    }
    config.validate_executable_identity()?;
    let roots = canonical_roots(approved_roots)?;
    let cwd_authorized = if config.source_identity().is_some() {
        config
            .code_root()
            .is_some_and(|root| config.cwd().starts_with(&root.canonical_path))
    } else {
        roots.iter().any(|root| config.cwd().starts_with(root))
    };
    if !cwd_authorized {
        return Err(PluginHostError::Approval(
            "plugin cwd is outside its owned runtime root".to_owned(),
        ));
    }
    let reads_workspace = expected_manifest.capabilities.tools.iter().any(|tool| {
        tool.caps
            .contains(&rw_plugin_protocol::PluginToolEffect::ReadsFilesystem)
    });
    if !reads_workspace
        && config
            .code_root()
            .is_some_and(|code_root| roots.contains(&code_root.canonical_path))
    {
        return Err(PluginHostError::Approval(
            "plugin code root must be a strict workspace descendant unless reads-fs is declared"
                .to_owned(),
        ));
    }
    let requests_network = !expected_manifest.capabilities.providers.is_empty()
        || expected_manifest.capabilities.tools.iter().any(|tool| {
            tool.caps
                .contains(&rw_plugin_protocol::PluginToolEffect::Network)
        });
    if requests_network && config.allowed_domains().is_empty() {
        return Err(PluginHostError::Approval(
            "network-capable plugins require an explicit public-domain allowlist".to_owned(),
        ));
    }
    Ok(PluginSandboxProfile {
        mode: PluginSandboxMode::Approved,
        capabilities: expected_manifest.capabilities.clone(),
        approved_roots: roots,
        allowed_domains: config.allowed_domains().iter().cloned().collect(),
    })
}

impl PluginHost {
    /// Launches an approved plugin on a host surface that does not provide
    /// host-mediated provider HTTP.
    ///
    /// # Errors
    ///
    /// Returns the same approval, launch, handshake, or manifest error as the
    /// HTTP-capable launch boundary.
    #[allow(
        clippy::too_many_arguments,
        reason = "security-sensitive launch inputs remain explicit at the approval boundary"
    )]
    pub async fn launch_approved(
        launcher: &dyn PluginLauncher,
        store: &dyn ApprovalStore,
        config: &PluginProcessConfig,
        origin: &str,
        approved_roots: &[PathBuf],
        expected_manifest: PluginManifest,
        push_handler: Arc<dyn PushHandler>,
        redactor: Arc<dyn PluginBoundaryRedactor>,
    ) -> Result<Self, PluginHostError> {
        Self::launch_approved_with_http(
            launcher,
            store,
            config,
            origin,
            approved_roots,
            expected_manifest,
            push_handler,
            Arc::new(DenyPluginProviderHttpHandler),
            redactor,
        )
        .await
    }

    /// Launches only an exact approved executable/config/origin/manifest identity and completes
    /// the protocol handshake before exposing adapters.
    ///
    /// # Errors
    ///
    /// Returns an error for missing approval, identity drift, invalid roots, launch failure,
    /// handshake failure, or a manifest different from the approved snapshot.
    #[allow(
        clippy::too_many_arguments,
        reason = "security-sensitive launch inputs remain explicit at the approval boundary"
    )]
    pub async fn launch_approved_with_http(
        launcher: &dyn PluginLauncher,
        store: &dyn ApprovalStore,
        config: &PluginProcessConfig,
        origin: &str,
        approved_roots: &[PathBuf],
        expected_manifest: PluginManifest,
        push_handler: Arc<dyn PushHandler>,
        provider_http: Arc<dyn PluginProviderHttpHandler>,
        redactor: Arc<dyn PluginBoundaryRedactor>,
    ) -> Result<Self, PluginHostError> {
        let profile =
            approved_plugin_profile(store, config, origin, approved_roots, &expected_manifest)?;
        let child = launcher.launch(config, &profile).await?;
        if child.executable_identity != *config.executable_identity() {
            terminate_and_reap(child.process.as_ref()).await;
            return Err(PluginHostError::Approval(
                "launcher executable attestation differs from approved identity".to_owned(),
            ));
        }
        let process = Arc::clone(&child.process);
        let enforcer = Arc::new(CapabilityEnforcer::new(
            &expected_manifest,
            Arc::clone(&process),
        ));
        let client = JsonRpcPluginClient::start(
            child,
            Arc::clone(&enforcer),
            push_handler,
            provider_http,
            redactor,
            DEFAULT_REQUEST_TIMEOUT,
        );
        let initialize = serde_json::to_value(InitializeParams {
            host: rw_plugin_protocol::PLUGIN_HOST_ID.to_owned(),
            protocol: expected_manifest.protocol,
            max_frame_bytes: MAX_FRAME_BYTES,
            capabilities: vec!["provider-models".to_owned(), "provider-http".to_owned()],
        })
        .map_err(|error| PluginHostError::Rpc(rpc_error("invalid_request", &error.to_string())))?;
        let result = client.request(METHOD_INITIALIZE, initialize).await;
        let initialized: PluginManifest = match result.and_then(|value| {
            serde_json::from_value(value)
                .map_err(|error| rpc_error("invalid_manifest", &error.to_string()))
        }) {
            Ok(manifest) => manifest,
            Err(error) => {
                terminate_and_reap(process.as_ref()).await;
                return Err(PluginHostError::Rpc(error));
            }
        };
        if let Err(error) = initialized.validate() {
            terminate_and_reap(process.as_ref()).await;
            return Err(PluginHostError::ApprovalDetails(error.into()));
        }
        if initialized
            .fingerprint()
            .map_err(PluginApprovalError::from)?
            != expected_manifest
                .fingerprint()
                .map_err(PluginApprovalError::from)?
        {
            terminate_and_reap(process.as_ref()).await;
            return Err(PluginHostError::Approval(
                "initialized manifest differs from approved manifest".to_owned(),
            ));
        }
        Ok(Self {
            manifest: initialized,
            client,
            enforcer,
        })
    }

    #[must_use]
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    #[must_use]
    pub fn client(&self) -> Arc<dyn PluginRpcClient> {
        self.client.clone()
    }
    #[must_use]
    pub fn enforcer(&self) -> Arc<CapabilityEnforcer> {
        Arc::clone(&self.enforcer)
    }
    /// Gracefully shuts down and reaps the plugin process.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be terminated or reaped.
    pub async fn shutdown(&self) -> Result<(), PluginHostError> {
        self.client.shutdown(DEFAULT_SHUTDOWN_TIMEOUT).await
    }
}

/// Starts an initialization-only, zero-capability process to discover its manifest.
/// The child is always terminated and reaped; callers must separately approve and relaunch it.
///
/// # Errors
///
/// Returns an error for invalid roots or identity, launch/transport failure, malformed manifests,
/// or bounded shutdown failure.
#[cfg(test)]
pub(crate) async fn probe_plugin_manifest(
    launcher: &dyn PluginLauncher,
    config: &PluginProcessConfig,
    approved_roots: &[PathBuf],
    redactor: Arc<dyn PluginBoundaryRedactor>,
) -> Result<PluginManifest, PluginHostError> {
    config.validate_executable_identity()?;
    let roots = canonical_roots(approved_roots)?;
    if !roots.iter().any(|root| config.cwd().starts_with(root)) {
        return Err(PluginHostError::Approval(
            "plugin cwd is outside approved roots".to_owned(),
        ));
    }
    let child = launcher
        .launch(
            config,
            &PluginSandboxProfile {
                mode: PluginSandboxMode::ManifestProbe,
                capabilities: PluginCapabilities::default(),
                approved_roots: roots,
                allowed_domains: Vec::new(),
            },
        )
        .await?;
    if child.executable_identity != *config.executable_identity() {
        terminate_and_reap(child.process.as_ref()).await;
        return Err(PluginHostError::Approval(
            "launcher executable attestation differs from configured identity".to_owned(),
        ));
    }
    let process = Arc::clone(&child.process);
    let empty_manifest = PluginManifest {
        name: "manifest-probe".to_owned(),
        version: "0".to_owned(),
        protocol: rw_plugin_protocol::PROTOCOL_VERSION,
        capabilities: PluginCapabilities::default(),
    };
    let enforcer = Arc::new(CapabilityEnforcer::new(
        &empty_manifest,
        Arc::clone(&process),
    ));
    let client = JsonRpcPluginClient::start(
        child,
        enforcer,
        Arc::new(DenyPushHandler),
        Arc::new(DenyPluginProviderHttpHandler),
        redactor,
        DEFAULT_REQUEST_TIMEOUT,
    );
    let value = client
        .request(
            METHOD_INITIALIZE,
            serde_json::to_value(InitializeParams {
                host: rw_plugin_protocol::PLUGIN_HOST_ID.to_owned(),
                protocol: rw_plugin_protocol::PROTOCOL_VERSION,
                max_frame_bytes: MAX_FRAME_BYTES,
                capabilities: vec!["provider-models".to_owned(), "provider-http".to_owned()],
            })
            .map_err(|error| rpc_error("invalid_request", &error.to_string()))?,
        )
        .await;
    let manifest: PluginManifest = match value.and_then(|value| {
        serde_json::from_value(value)
            .map_err(|_| rpc_error("invalid_manifest", "plugin returned an invalid manifest"))
    }) {
        Ok(manifest) => manifest,
        Err(error) => {
            terminate_and_reap(process.as_ref()).await;
            return Err(error.into());
        }
    };
    if let Err(error) = manifest.validate() {
        terminate_and_reap(process.as_ref()).await;
        return Err(PluginApprovalError::from(error).into());
    }
    client.shutdown(DEFAULT_SHUTDOWN_TIMEOUT).await?;
    Ok(manifest)
}

fn canonical_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, PluginHostError> {
    if roots.is_empty() {
        return Err(PluginHostError::Approval(
            "at least one approved root is required".to_owned(),
        ));
    }
    roots
        .iter()
        .map(|root| {
            std::fs::canonicalize(root).map_err(|error| {
                PluginHostError::Approval(format!("invalid approved root: {error}"))
            })
        })
        .collect()
}
