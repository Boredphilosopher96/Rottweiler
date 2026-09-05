use super::*;

#[derive(Default)]
pub(super) struct SessionDevelopmentApprovalStore(Mutex<BTreeMap<String, String>>);

impl ApprovalStore for SessionDevelopmentApprovalStore {
    fn approved_fingerprint(
        &self,
        plugin_name: &str,
    ) -> std::result::Result<Option<String>, ApprovalStoreError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| ApprovalStoreError {
                message: "development approval state is unavailable".to_owned(),
            })?
            .get(plugin_name)
            .cloned())
    }

    fn record_approval(
        &self,
        plugin_name: &str,
        fingerprint: &str,
    ) -> std::result::Result<(), ApprovalStoreError> {
        self.0
            .lock()
            .map_err(|_| ApprovalStoreError {
                message: "development approval state is unavailable".to_owned(),
            })?
            .insert(plugin_name.to_owned(), fingerprint.to_owned());
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct DevelopmentExtensionState {
    base: Option<rw_core::SessionExtensionSnapshot>,
    ceiling: Option<rw_plugin_protocol::PluginCapabilities>,
    active: Option<PluginSessionRuntime>,
    revision: u64,
}

/// Sole owner of a session's temporary source-plugin generation.
pub(crate) struct RuntimeSessionExtensionController {
    private_root: PathBuf,
    helper: PathBuf,
    redactor: Arc<SharedPluginRedactor>,
    activation: Arc<PluginRuntimeBudget>,
    state: tokio::sync::Mutex<DevelopmentExtensionState>,
    operation: tokio::sync::Mutex<()>,
    failed: std::sync::atomic::AtomicBool,
}

impl RuntimeSessionExtensionController {
    pub(crate) fn new(
        private_root: PathBuf,
        helper: PathBuf,
        redactor: Arc<SharedPluginRedactor>,
        activation: Arc<PluginRuntimeBudget>,
    ) -> Self {
        Self {
            private_root,
            helper,
            redactor,
            activation,
            state: tokio::sync::Mutex::new(DevelopmentExtensionState::default()),
            operation: tokio::sync::Mutex::new(()),
            failed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn ensure_ready(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(rw_core::AgentLoopError::EffectsUnsettled(
                "development generation retirement is unproven".to_owned(),
            ));
        }
        Ok(())
    }

    async fn retire(
        &self,
        runtime: PluginSessionRuntime,
    ) -> std::result::Result<(), rw_core::AgentLoopError> {
        self.failed
            .store(true, std::sync::atomic::Ordering::Release);
        let resources =
            crate::session_resources::RuntimeSessionResources::new(None, Some(Arc::new(runtime)));
        rw_core::SessionResources::shutdown(resources.as_ref()).await?;
        self.failed
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn discovered(
        source: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<(crate::extension_config::DiscoveredPlugin, PluginManifest)> {
        if fs::symlink_metadata(source).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(miette!("development plugin source cannot be a symlink"));
        }
        let root = fs::canonicalize(source).into_diagnostic()?;
        if !root.is_dir()
            || !workspace_roots
                .iter()
                .any(|workspace| root.starts_with(workspace))
        {
            return Err(miette!(
                "development plugin source is outside this session's workspace roots"
            ));
        }
        let manifest_path = root.join("manifest.json");
        let manifest = PluginManifest::from_slice(&fs::read(&manifest_path).into_diagnostic()?)
            .map_err(|error| miette!(error.to_string()))?;
        manifest
            .validate()
            .map_err(|error| miette!(error.to_string()))?;
        if !manifest.capabilities.providers.is_empty()
            || !manifest.capabilities.event_subscriptions.is_empty()
            || !manifest.capabilities.push.is_empty()
            || manifest.capabilities.tools.iter().any(|tool| {
                tool.caps.iter().any(|effect| {
                    matches!(
                        effect,
                        rw_plugin_protocol::PluginToolEffect::WritesFilesystem
                            | rw_plugin_protocol::PluginToolEffect::Network
                            | rw_plugin_protocol::PluginToolEffect::Execute
                    )
                })
            })
        {
            return Err(miette!(
                "development attachment permits tools, hooks, commands, and read-only filesystem authority only"
            ));
        }
        let entry = root.join("src/index.ts");
        if !entry.is_file() {
            return Err(miette!(
                "development plugin entrypoint src/index.ts is unavailable"
            ));
        }
        let plugin = crate::extension_config::DiscoveredPlugin {
            name: manifest.name.clone(),
            enabled: true,
            target: crate::extension_config::DiscoveredPluginTarget::TypeScript {
                package_root: root,
                entry,
            },
            inherit_env: Vec::new(),
            manifest_path,
            allowed_domains: Vec::new(),
            origin: crate::extension_config::ExecutableConfigOrigin::TrustedProject(
                source.to_path_buf(),
            ),
        };
        Ok((plugin, manifest))
    }

    async fn prepare_candidate(
        &self,
        plugin: &crate::extension_config::DiscoveredPlugin,
        manifest: &PluginManifest,
        workspace_roots: &[PathBuf],
    ) -> std::result::Result<PluginSessionRuntime, rw_core::AgentLoopError> {
        let metadata =
            rw_ext::PluginEndpointMetadata::new(manifest.clone()).map_err(development_error)?;
        let push_handler = Arc::new(SessionPluginPushHandler::default());
        let endpoint: Arc<dyn rw_ext::PluginEndpoint> = Arc::new(
            activation::DormantPluginEndpoint::new(activation::ActivationRecipe {
                metadata,
                approval: activation::ActivationApproval::SessionDevelopment,
                config: plugin.clone(),
                private_root: self.private_root.clone(),
                workspace_roots: workspace_roots.to_vec(),
                helper: self.helper.clone(),
                redactor: Arc::clone(&self.redactor),
                push_handler: Arc::clone(&push_handler),
                budget: Arc::clone(&self.activation),
                #[cfg(test)]
                launcher: None,
            }),
        );
        let mut candidate = PluginSessionRuntime::new(&self.activation, &self.redactor);
        candidate
            .register_endpoint(plugin, manifest, Arc::clone(&endpoint), push_handler)
            .map_err(development_error)?;
        match endpoint.connect(&CancellationToken::default()).await {
            Ok(_) => Ok(candidate),
            Err(error) => {
                self.retire(candidate).await?;
                if error.code == "effects_unsettled" {
                    self.failed
                        .store(true, std::sync::atomic::Ordering::Release);
                    Err(rw_core::AgentLoopError::EffectsUnsettled(error.message))
                } else {
                    Err(development_error(error))
                }
            }
        }
    }

    fn compose_candidate(
        base: &rw_core::SessionExtensionSnapshot,
        candidate: &PluginSessionRuntime,
        revision: u64,
    ) -> std::result::Result<rw_core::SessionExtensionSnapshot, rw_core::AgentLoopError> {
        let mut tools = base.tools.as_ref().clone();
        for tool in &candidate.tools {
            tools.register(Arc::clone(tool)).map_err(|error| {
                development_error(format!("development plugin tool collision: {error}"))
            })?;
        }
        let mut hooks = base.hooks.as_ref().clone();
        for (registration, handler) in &candidate.hooks {
            hooks
                .register_shared(registration.clone(), Arc::clone(handler))
                .map_err(|error| {
                    development_error(format!("development plugin hook collision: {error}"))
                })?;
        }
        let mut commands = base.commands.as_ref().clone();
        for (descriptor, handler) in &candidate.commands {
            commands
                .register_shared(descriptor.clone(), Arc::clone(handler))
                .map_err(|error| {
                    development_error(format!("development plugin command collision: {error}"))
                })?;
        }
        Ok(rw_core::SessionExtensionSnapshot {
            revision,
            workspace_roots: Arc::clone(&base.workspace_roots),
            tools: Arc::new(tools),
            hooks: Arc::new(hooks),
            commands: Arc::new(commands),
        })
    }
}

pub(super) fn development_error(error: impl ToString) -> rw_core::AgentLoopError {
    let message = error.to_string();
    drop(error);
    rw_core::AgentLoopError::InvalidConfiguration(message)
}

#[async_trait]
impl rw_core::SessionExtensionController for RuntimeSessionExtensionController {
    async fn attach(
        &self,
        source: &Path,
        current: rw_core::SessionExtensionSnapshot,
    ) -> std::result::Result<rw_core::SessionExtensionSnapshot, rw_core::AgentLoopError> {
        let _operation = self.operation.lock().await;
        self.ensure_ready()?;
        let (plugin, manifest) =
            Self::discovered(source, &current.workspace_roots).map_err(development_error)?;
        let (base, ceiling, current_revision) = {
            let state = self.state.lock().await;
            (
                state.base.clone().unwrap_or_else(|| current.clone()),
                state.ceiling.clone(),
                state.revision.max(current.revision),
            )
        };
        if ceiling
            .as_ref()
            .is_some_and(|ceiling| ceiling != &manifest.capabilities)
        {
            return Err(rw_core::AgentLoopError::InvalidConfiguration(
                "development plugin capability expansion requires detach and a new explicit grant"
                    .to_owned(),
            ));
        }
        let candidate = self
            .prepare_candidate(&plugin, &manifest, &current.workspace_roots)
            .await?;
        let revision = current_revision.saturating_add(1);
        let snapshot = match Self::compose_candidate(&base, &candidate, revision) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.retire(candidate).await?;
                return Err(error);
            }
        };
        let retired = {
            let mut state = self.state.lock().await;
            if state.base.is_none() {
                state.base = Some(base);
            }
            if state.ceiling.is_none() {
                state.ceiling = Some(manifest.capabilities);
            }
            state.revision = revision;
            state.active.replace(candidate)
        };
        if let Some(retired) = retired {
            self.retire(retired).await?;
        }
        Ok(snapshot)
    }

    async fn detach(
        &self,
    ) -> std::result::Result<rw_core::SessionExtensionSnapshot, rw_core::AgentLoopError> {
        let _operation = self.operation.lock().await;
        self.ensure_ready()?;
        let (base, active) = {
            let mut state = self.state.lock().await;
            let base = state.base.take().ok_or_else(|| {
                rw_core::AgentLoopError::InvalidConfiguration(
                    "no development plugin is attached".to_owned(),
                )
            })?;
            state.ceiling = None;
            state.revision = state.revision.saturating_add(1);
            (base, state.active.take())
        };
        if let Some(active) = active {
            self.retire(active).await?;
        }
        Ok(base)
    }

    async fn rebase(
        &self,
        current: rw_core::SessionExtensionSnapshot,
    ) -> std::result::Result<(rw_core::SessionExtensionSnapshot, bool), rw_core::AgentLoopError>
    {
        let _operation = self.operation.lock().await;
        self.ensure_ready()?;
        let (snapshot, retired) = {
            let mut state = self.state.lock().await;
            let Some(active) = state.active.as_ref() else {
                return Ok((current, false));
            };
            let revision = state.revision.max(current.revision).saturating_add(1);
            if let Ok(snapshot) = Self::compose_candidate(&current, active, revision) {
                state.base = Some(current);
                state.revision = revision;
                (snapshot, None)
            } else {
                let retired = state.active.take();
                state.base = None;
                state.ceiling = None;
                state.revision = revision;
                (current, retired)
            }
        };
        if let Some(retired) = retired {
            self.retire(retired).await?;
            return Ok((snapshot, true));
        }
        Ok((snapshot, false))
    }

    async fn shutdown(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        let _operation = self.operation.lock().await;
        let earlier = self.ensure_ready();
        let active = self.state.lock().await.active.take();
        if let Some(active) = active {
            self.retire(active).await?;
        }
        earlier
    }
}
