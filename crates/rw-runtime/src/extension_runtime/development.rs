use super::{
    ApprovalStore, ApprovalStoreError, Arc, BTreeMap, IntoDiagnostic, Mutex, Path, PathBuf,
    PluginManifest, Result, async_trait, fs, miette,
};

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

/// The controller owns one source recipe for configured and development plugins.
pub(crate) struct RuntimeSessionExtensionController {
    owner: Arc<super::generations::PluginGenerationOwner>,
    recipe: crate::session_runtime::native_registry_recipe::NativeRegistryRecipe,
    state: tokio::sync::Mutex<DevelopmentExtensionState>,
}
#[derive(Default)]
struct DevelopmentExtensionState {
    development: Option<crate::extension_config::DiscoveredPlugin>,
    ceiling: Option<rw_plugin_protocol::PluginCapabilities>,
    revision: u64,
}
struct NativePreparation<'a> {
    built: &'a mut crate::session_runtime::tool_composition::BuiltTools,
    catalog: &'a Arc<rw_ext::ExtensionCatalog>,
    roots: &'a [PathBuf],
    alias: &'a str,
    revision: u64,
    development: Option<&'a crate::extension_config::DiscoveredPlugin>,
}
impl RuntimeSessionExtensionController {
    pub(crate) fn new(
        owner: Arc<super::generations::PluginGenerationOwner>,
        recipe: crate::session_runtime::native_registry_recipe::NativeRegistryRecipe,
    ) -> Self {
        Self {
            owner,
            recipe,
            state: tokio::sync::Mutex::new(DevelopmentExtensionState::default()),
        }
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

    async fn prepare(
        &self,
        state: &DevelopmentExtensionState,
        input: NativePreparation<'_>,
    ) -> std::result::Result<rw_core::SessionExtensionSnapshot, rw_core::AgentLoopError> {
        let NativePreparation {
            built,
            catalog,
            roots,
            alias,
            revision,
            development,
        } = input;
        let root_owner = self.recipe.root_owner()?;
        let configured = root_owner.native_configs(roots)?;
        let prepared = self.owner.prepare(&configured, development, roots).await?;
        let candidate = prepared.runtime.clone();
        let mut tools = built.registry.as_ref().clone();
        for tool in &candidate.tools {
            tools
                .register(tool.clone())
                .map_err(|error| closed_generation(&error))?;
        }
        self.recipe
            .add_tools(&mut tools, catalog)
            .map_err(|error| closed_generation(&error))?;
        built.registry = Arc::new(tools);
        let extensions = root_owner
            .prepare_extensions(catalog, roots, built)
            .map_err(|error| closed_generation(&error))?;
        let mut hooks = extensions.hooks.as_ref().clone();
        for (registration, handler) in &candidate.hooks {
            hooks
                .register_shared(registration.clone(), handler.clone())
                .map_err(|error| closed_generation(&error))?;
        }
        let mut commands = extensions.commands.as_ref().clone();
        if let Some(mcp) = &self.recipe.mcp {
            super::register_mcp_command(
                &mut commands,
                mcp.manager.clone(),
                Some(mcp.approvals.clone()),
            )
            .await
            .map_err(|error| closed_generation(&error))?;
        }
        for (descriptor, handler) in &candidate.commands {
            commands
                .register_shared(descriptor.clone(), handler.clone())
                .map_err(|error| closed_generation(&error))?;
        }
        let publication = prepared.with_model(
            crate::session_runtime::native_model_generations::NativeModelInput {
                providers: Vec::new(),
                tools: built.registry.clone(),
                roots: roots.to_vec(),
                alias: alias.to_owned(),
                websearch: built.websearch.clone(),
            },
        )?;
        let revision = state
            .revision
            .max(revision)
            .checked_add(1)
            .ok_or_else(|| closed_generation("extension revision exhausted"))?;
        let model = publication.model();
        for agent in rw_ext::compose_agent_registry(catalog)
            .map_err(|error| closed_generation(&error))?
            .definitions()
        {
            if let Some(alias) = agent.model()
                && !model.has_model_alias(alias)
            {
                return Err(closed_generation(
                    "agent references an unavailable model in the candidate generation",
                ));
            }
        }
        Ok(rw_core::SessionExtensionSnapshot {
            model,
            model_alias: alias.to_owned(),
            publication: publication
                .publication(self.recipe.orchestrator.clone(), built.registry.clone()),
            ui: candidate.ui.clone(),
            revision,
            workspace_roots: Arc::from(roots.to_vec()),
            tools: built.registry.clone(),
            hooks: Arc::new(hooks),
            commands: Arc::new(commands),
        })
    }
    pub(crate) async fn prepare_workspace(
        &self,
        root: &mut crate::session_runtime::workspace_roots::PreparedRootGeneration,
        alias: &str,
        revision: u64,
    ) -> std::result::Result<rw_core::SessionExtensionSnapshot, rw_core::AgentLoopError> {
        let mut state = self.state.lock().await;
        let snapshot = self
            .prepare(
                &state,
                NativePreparation {
                    built: &mut root.built,
                    catalog: &root.catalog,
                    roots: &root.roots,
                    alias,
                    revision,
                    development: state.development.as_ref(),
                },
            )
            .await?;
        state.revision = snapshot.revision;
        root.extensions.hooks = snapshot.hooks.clone();
        root.extensions.commands = snapshot.commands.clone();
        Ok(snapshot)
    }
}

pub(super) fn development_error(error: &(impl ToString + ?Sized)) -> rw_core::AgentLoopError {
    rw_core::AgentLoopError::InvalidConfiguration(error.to_string())
}
fn closed_generation(error: &(impl ToString + ?Sized)) -> rw_core::AgentLoopError {
    rw_core::AgentLoopError::EffectsUnsettled(error.to_string())
}
#[async_trait]
impl rw_core::SessionExtensionController for RuntimeSessionExtensionController {
    async fn attach(
        &self,
        source: &Path,
        current: rw_core::SessionExtensionSnapshot,
    ) -> std::result::Result<rw_core::SessionExtensionSnapshot, rw_core::AgentLoopError> {
        let mut state = self.state.lock().await;
        let (plugin, manifest) = Self::discovered(source, &current.workspace_roots)
            .map_err(|error| development_error(&error))?;
        if state
            .ceiling
            .as_ref()
            .is_some_and(|ceiling| ceiling != &manifest.capabilities)
        {
            return Err(development_error(
                "development capability change requires detach and a fresh explicit grant",
            ));
        }
        let root_owner = self.recipe.root_owner()?;
        let catalog = root_owner.extension_catalog(&current.workspace_roots)?;
        let mut built = root_owner.prepare_tools(&current.workspace_roots)?;
        built.registry = Arc::new(
            built
                .registry
                .as_ref()
                .clone()
                .with_mcp_tool_policy(current.tools.mcp_tool_policy().clone()),
        );
        let snapshot = self
            .prepare(
                &state,
                NativePreparation {
                    built: &mut built,
                    catalog: &catalog,
                    roots: &current.workspace_roots,
                    alias: &current.model_alias,
                    revision: current.revision,
                    development: Some(&plugin),
                },
            )
            .await?;
        state.ceiling = Some(manifest.capabilities);
        state.development = Some(plugin);
        state.revision = snapshot.revision;
        Ok(snapshot)
    }
    async fn detach(
        &self,
        current: rw_core::SessionExtensionSnapshot,
    ) -> std::result::Result<rw_core::SessionExtensionSnapshot, rw_core::AgentLoopError> {
        let mut state = self.state.lock().await;
        if state.development.is_none() {
            return Err(development_error("no development plugin is attached"));
        }
        let root_owner = self.recipe.root_owner()?;
        let catalog = root_owner.extension_catalog(&current.workspace_roots)?;
        let mut built = root_owner.prepare_tools(&current.workspace_roots)?;
        built.registry = Arc::new(
            built
                .registry
                .as_ref()
                .clone()
                .with_mcp_tool_policy(current.tools.mcp_tool_policy().clone()),
        );
        let snapshot = self
            .prepare(
                &state,
                NativePreparation {
                    built: &mut built,
                    catalog: &catalog,
                    roots: &current.workspace_roots,
                    alias: &current.model_alias,
                    revision: current.revision,
                    development: None,
                },
            )
            .await?;
        state.development = None;
        state.ceiling = None;
        state.revision = snapshot.revision;
        Ok(snapshot)
    }
    async fn shutdown(&self) -> std::result::Result<(), rw_core::AgentLoopError> {
        self.owner
            .shutdown()
            .await
            .map_err(|error| closed_generation(&error))
    }
}
