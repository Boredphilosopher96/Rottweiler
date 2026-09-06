use super::{
    Arc, CommandDescriptor, EditableSettingKey, HostError, HostQueryService, MAX_PREVIEW_BYTES,
    MAX_SEARCH_RESULTS, ModelAlias, ModelCatalogSnapshot, PathBuf, ProviderAuthAttempt,
    ProviderAuthChallenge, ProviderAuthCompletion, ProviderLogin, ProviderLoginCancellation,
    QUERY_DEADLINE, RuntimeSessionFactory, SESSION_EXPORT_DEADLINE, SessionDescriptor,
    TranscriptFormat, UserSettingDescriptor, WORKSPACE_DIFF_DEADLINE, WORKSPACE_STATUS_DEADLINE,
    WorkspaceDiff, WorkspaceFileMatch, WorkspaceFilePreview, WorkspaceStatus, async_trait,
    begin_provider_login, builtin_command_registry, overlay_catalog_current, preview_file,
    read_workspace_diff, read_workspace_status, search_workspaces, split_virtual_path,
};

#[async_trait]
impl HostQueryService for RuntimeSessionFactory {
    async fn session_children(
        &self,
        session: &rw_types::SessionId,
        scope: rw_types::session_read::SessionReadScope,
    ) -> Result<rw_types::session_children::SessionChildrenResult, HostError> {
        let factory = self.clone();
        let session = session.clone();
        let root = scope
            .root(&session)
            .map_err(|message| HostError::Protocol(message.into()))?
            .clone();
        let order = self
            .journal_service
            .child_projection_order(&session.0)
            .map_err(|error| HostError::Query(error.to_string()))?
            .acquire()
            .await
            .map_err(|error| HostError::Query(error.to_string()))?;
        self.transcripts
            .blocking(move |reader| {
                let metadata =
                    super::load_session_metadata_any(&factory.options.storage_root, &root.0)
                        .map_err(|_| {
                            HostError::Persistence("session metadata is unavailable".into())
                        })?;
                factory.authorize_workspace_path(&metadata.workspace)?;
                reader.read_children(&session, &scope, &order)
            })
            .await
    }

    async fn todos(
        &self,
        session: &rw_types::SessionId,
        scope: rw_types::session_read::SessionReadScope,
    ) -> Result<rw_types::todo::TodoReadResult, HostError> {
        let factory = self.clone();
        let requested = session.clone();
        let root = scope
            .root(session)
            .map_err(|message| HostError::Protocol(message.into()))?
            .clone();
        crate::todo_service::read_todos(
            Arc::clone(&self.journal_service),
            session.clone(),
            move |budget| {
                let metadata =
                    super::load_session_metadata_any(&factory.options.storage_root, &root.0)
                        .map_err(|_| {
                            HostError::Persistence("session metadata is unavailable".into())
                        })?;
                factory.authorize_workspace_path(&metadata.workspace)?;
                factory
                    .transcripts
                    .authorize_scope(&requested, &scope, budget)
            },
        )
        .await
    }

    async fn read_transcript_tail(
        &self,
        session: &rw_types::SessionId,
        scope: rw_types::session_read::SessionReadScope,
        read: rw_types::transcript_tail::TranscriptTailRead,
    ) -> Result<rw_types::transcript_tail::TranscriptTailResult, HostError> {
        let factory = self.clone();
        let session = session.clone();
        let root = scope
            .root(&session)
            .map_err(|message| HostError::Protocol(message.into()))?
            .clone();
        self.transcripts
            .blocking(move |transcripts| {
                let metadata =
                    super::load_session_metadata_any(&factory.options.storage_root, &root.0)
                        .map_err(|_| {
                            HostError::Persistence("session metadata is unavailable".into())
                        })?;
                factory.authorize_workspace_path(&metadata.workspace)?;
                transcripts.read_tail(&session, &scope, &read)
            })
            .await
    }

    async fn read_transcript(
        &self,
        session: &rw_types::SessionId,
        scope: rw_types::session_read::SessionReadScope,
        read: rw_types::transcript::TranscriptRead,
    ) -> Result<rw_types::transcript::TranscriptReadResult, HostError> {
        let factory = self.clone();
        let session = session.clone();
        let root = scope
            .root(&session)
            .map_err(|message| HostError::Protocol(message.into()))?
            .clone();
        self.transcripts
            .blocking(move |transcripts| {
                let metadata =
                    super::load_session_metadata_any(&factory.options.storage_root, &root.0)
                        .map_err(|_| {
                            HostError::Persistence("session metadata is unavailable".into())
                        })?;
                factory.authorize_workspace_path(&metadata.workspace)?;
                transcripts.read(&session, &scope, &read)
            })
            .await
    }

    async fn read_transcript_content(
        &self,
        session: &rw_types::SessionId,
        scope: rw_types::session_read::SessionReadScope,
        read: rw_types::transcript::TranscriptContentRead,
    ) -> Result<rw_types::transcript::TranscriptContentPage, HostError> {
        let factory = self.clone();
        let session = session.clone();
        let root = scope
            .root(&session)
            .map_err(|message| HostError::Protocol(message.into()))?
            .clone();
        self.transcripts
            .blocking(move |transcripts| {
                let metadata =
                    super::load_session_metadata_any(&factory.options.storage_root, &root.0)
                        .map_err(|_| {
                            HostError::Persistence("session metadata is unavailable".into())
                        })?;
                factory.authorize_workspace_path(&metadata.workspace)?;
                transcripts.read_content(&session, &scope, &read)
            })
            .await
    }

    async fn command_descriptors(&self) -> Result<Vec<CommandDescriptor>, HostError> {
        let registry = builtin_command_registry().map_err(HostError::from)?;
        Ok(registry
            .descriptors()
            .map(|descriptor| CommandDescriptor {
                name: descriptor.name().to_owned(),
                description: descriptor.description().to_owned(),
                usage: descriptor.argument_hint().unwrap_or_default().to_owned(),
                source: rw_core::CommandSource::default(),
            })
            .collect())
    }

    async fn model_catalog(
        &self,
        refresh: bool,
        selected_model: Option<&str>,
        resolved_model: Option<&str>,
    ) -> Result<ModelCatalogSnapshot, HostError> {
        let mut catalog = self
            .model_catalog
            .get(refresh)
            .await
            .map_err(|error| HostError::Query(error.to_string()))?;
        overlay_catalog_current(&mut catalog, selected_model, resolved_model);
        Ok(catalog)
    }

    async fn user_settings(
        &self,
        session: &SessionDescriptor,
    ) -> Result<Vec<UserSettingDescriptor>, HostError> {
        let workspace = self.workspace_for_session(session)?;
        let config_loader = self.settings_loader_for(&workspace);
        let project_loader = config_loader.clone();
        let effective =
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                config_loader.load()
            })
            .await
            .map_err(|_| HostError::Query("user settings worker failed".to_owned()))?
            .map_err(|error| HostError::Query(error.to_string()))?;
        let project_model = project_loader
            .tui_project_model()
            .map_err(|error| HostError::Query(error.to_string()))?;
        let keybinding_preset = project_loader
            .tui_keybinding_preset()
            .map_err(|error| HostError::Query(error.to_string()))?;
        let mcp_servers = project_loader
            .tui_mcp_servers()
            .map_err(|error| HostError::Query(error.to_string()))?;
        Ok(Self::setting_descriptors(
            &effective,
            session,
            project_model.as_deref(),
            &keybinding_preset,
            &mcp_servers,
        ))
    }

    async fn set_user_setting(
        &self,
        session: &SessionDescriptor,
        key: &str,
        value: &str,
    ) -> Result<Vec<UserSettingDescriptor>, HostError> {
        let workspace = self.workspace_for_session(session)?;
        let config_loader = self.settings_loader_for(&workspace);
        let project_loader = config_loader.clone();
        let setting_key = EditableSettingKey::parse(key).ok_or_else(|| {
            HostError::Persistence(
                rw_store::config::ConfigError::InvalidUserSetting {
                    key: key.to_owned(),
                    reason: "key or value is outside the safe TUI settings allowlist".to_owned(),
                }
                .to_string(),
            )
        })?;
        let rendered_key = setting_key.render();
        let value = value.to_owned();
        let project_model_write = matches!(&setting_key, EditableSettingKey::ProjectDefaultModel);
        let persisted_project_model = project_model_write.then(|| value.clone());
        let effective =
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                match setting_key {
                    EditableSettingKey::ProjectDefaultModel => {
                        config_loader.persist_tui_project_model(&value)
                    }
                    EditableSettingKey::KeybindingPreset => {
                        config_loader.persist_tui_keybinding_preset(&value)?;
                        config_loader.load()
                    }
                    EditableSettingKey::McpServerEnabled(server) => {
                        let enabled = match value.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => {
                                return Err(rw_store::config::ConfigError::InvalidUserSetting {
                                    key: rendered_key,
                                    reason: "MCP enablement must be true or false".to_owned(),
                                });
                            }
                        };
                        config_loader.persist_tui_mcp_enabled(&server, enabled)?;
                        config_loader.load()
                    }
                    EditableSettingKey::McpAddHttp(server) => {
                        config_loader.persist_tui_mcp_http_server(&server, &value)?;
                        config_loader.load()
                    }
                    _ => config_loader.persist_tui_setting(&rendered_key, &value),
                }
            })
            .await
            .map_err(|_| HostError::Persistence("user setting worker failed".to_owned()))?
            .map_err(|error| HostError::Persistence(error.to_string()))?;
        let project_model = if let Some(model) = persisted_project_model {
            Some(model)
        } else {
            project_loader
                .tui_project_model()
                .map_err(|error| HostError::Query(error.to_string()))?
        };
        let keybinding_preset = project_loader
            .tui_keybinding_preset()
            .map_err(|error| HostError::Query(error.to_string()))?;
        let mcp_servers = project_loader
            .tui_mcp_servers()
            .map_err(|error| HostError::Query(error.to_string()))?;
        Ok(Self::setting_descriptors(
            &effective,
            session,
            project_model.as_deref(),
            &keybinding_preset,
            &mcp_servers,
        ))
    }

    async fn persist_project_model_selection(
        &self,
        session: &SessionDescriptor,
        model: &ModelAlias,
    ) -> Result<(), HostError> {
        let workspace = self.workspace_for_session(session)?;
        let loader = self.settings_loader_for(&workspace);
        let model = model.0.clone();
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            loader.persist_tui_project_model(&model)
        })
        .await
        .map_err(|_| HostError::Persistence("project model worker failed".to_owned()))?
        .map_err(|error| HostError::Persistence(error.to_string()))?;
        Ok(())
    }

    async fn begin_provider_auth(&self, provider: &str) -> Result<ProviderAuthAttempt, HostError> {
        match begin_provider_login(provider)
            .await
            .map_err(|error| HostError::Query(error.to_string()))?
        {
            ProviderLogin::OAuth(login) => {
                let challenge = ProviderAuthChallenge::Oauth {
                    authorization_url: login.authorization_url().to_owned(),
                    redirect_uri: login.redirect_uri().to_owned(),
                };
                let warnings = login.warnings().to_vec();
                let provider = provider.to_owned();
                let completion = Box::pin(async move {
                    let prepared = login
                        .prepare()
                        .await
                        .map_err(|error| HostError::Query(error.to_string()))?;
                    Ok(ProviderAuthCompletion::new(
                        provider,
                        "provider authentication completed".to_owned(),
                        Vec::new(),
                    )
                    .with_persistence(move || {
                        prepared
                            .persist()
                            .map(|result| result.warnings)
                            .map_err(|_| {
                                HostError::Persistence(
                                    "provider credential storage failed".to_owned(),
                                )
                            })
                    }))
                });
                Ok(ProviderAuthAttempt::new(
                    challenge,
                    warnings,
                    completion,
                    Arc::new(|| {}),
                ))
            }
            ProviderLogin::GitHubCopilot(login) => {
                let challenge = ProviderAuthChallenge::DeviceFlow {
                    verification_uri: login.verification_uri().to_owned(),
                    user_code: login.user_code().to_owned(),
                };
                let warnings = login.warnings().to_vec();
                let cancellation = ProviderLoginCancellation::default();
                let poll_cancellation = cancellation.clone();
                let provider = provider.to_owned();
                let completion = Box::pin(async move {
                    let prepared = login
                        .prepare(&poll_cancellation)
                        .await
                        .map_err(|error| HostError::Query(error.to_string()))?;
                    Ok(ProviderAuthCompletion::new(
                        provider,
                        "provider authentication completed".to_owned(),
                        Vec::new(),
                    )
                    .with_persistence(move || {
                        prepared
                            .persist()
                            .map(|result| result.warnings)
                            .map_err(|_| {
                                HostError::Persistence(
                                    "provider credential storage failed".to_owned(),
                                )
                            })
                    }))
                });
                Ok(ProviderAuthAttempt::new(
                    challenge,
                    warnings,
                    completion,
                    Arc::new(move || cancellation.cancel()),
                ))
            }
        }
    }

    async fn configure_builtin_provider(
        &self,
        profile: rw_core::BuiltinProviderProfile,
    ) -> Result<(), HostError> {
        let config_loader = self.settings_loader();
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            config_loader.configure_provider_profile(profile.canonical_id(), profile.config_kind())
        })
        .await
        .map_err(|_| HostError::Persistence("built-in provider setup worker failed".to_owned()))?
        .map_err(|error| HostError::Persistence(error.to_string()))?;
        Ok(())
    }

    async fn search_workspace_files(
        &self,
        session: &SessionDescriptor,
        query: &str,
        limit: u32,
    ) -> Result<(Vec<WorkspaceFileMatch>, bool), HostError> {
        let workspaces = self.workspace_roots_for_session(session).await?;
        let query = query.to_owned();
        let limit = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .clamp(1, MAX_SEARCH_RESULTS);
        tokio::time::timeout(
            QUERY_DEADLINE,
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                search_workspaces(&workspaces, &query, limit)
            }),
        )
        .await
        .map_err(|_| HostError::Query("workspace search deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace search worker failed".to_owned()))?
    }

    async fn preview_workspace_file(
        &self,
        session: &SessionDescriptor,
        path: &str,
        max_bytes: u32,
    ) -> Result<WorkspaceFilePreview, HostError> {
        let workspaces = self.workspace_roots_for_session(session).await?;
        let (root_index, relative) = split_virtual_path(path)?;
        let workspace = workspaces
            .get(root_index)
            .cloned()
            .ok_or_else(|| HostError::Query("workspace root index is not authorized".to_owned()))?;
        let rendered_path = path.to_owned();
        let maximum = usize::try_from(max_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_PREVIEW_BYTES);
        if maximum == 0 {
            return Err(HostError::Query(
                "preview byte limit must not be zero".to_owned(),
            ));
        }
        tokio::time::timeout(
            QUERY_DEADLINE,
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                let mut preview = preview_file(&workspace, &relative, maximum)?;
                preview.path = rendered_path;
                Ok(preview)
            }),
        )
        .await
        .map_err(|_| HostError::Query("workspace preview deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace preview worker failed".to_owned()))?
    }

    async fn workspace_status(
        &self,
        session: &SessionDescriptor,
    ) -> Result<WorkspaceStatus, HostError> {
        let workspace = self.workspace_for_session(session)?;
        let name = session.workspace_name.clone();
        tokio::time::timeout(
            WORKSPACE_STATUS_DEADLINE,
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                read_workspace_status(&workspace, name)
            }),
        )
        .await
        .map_err(|_| HostError::Query("workspace status deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace status worker failed".to_owned()))?
    }

    async fn workspace_diff(
        &self,
        session: &SessionDescriptor,
        path: &str,
        max_bytes: u32,
    ) -> Result<WorkspaceDiff, HostError> {
        let workspaces = self.workspace_roots_for_session(session).await?;
        let (root_index, relative) = split_virtual_path(path)?;
        let workspace = workspaces
            .get(root_index)
            .cloned()
            .ok_or_else(|| HostError::Query("workspace root index is not authorized".to_owned()))?;
        let rendered_path = path.to_owned();
        let maximum = usize::try_from(max_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_PREVIEW_BYTES);
        if maximum == 0 {
            return Err(HostError::Query(
                "workspace diff byte limit must not be zero".to_owned(),
            ));
        }
        tokio::time::timeout(
            WORKSPACE_DIFF_DEADLINE,
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                let mut diff = read_workspace_diff(&workspace, &relative, maximum)?;
                diff.path = rendered_path;
                Ok(diff)
            }),
        )
        .await
        .map_err(|_| HostError::Query("workspace diff deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("workspace diff worker failed".to_owned()))?
    }

    async fn export_session(
        &self,
        session: &SessionDescriptor,
        format: TranscriptFormat,
        output_path: &str,
        force: bool,
    ) -> Result<String, HostError> {
        let output_path = PathBuf::from(output_path);
        if !output_path.is_absolute() {
            return Err(HostError::Query(
                "export output path must be absolute".to_owned(),
            ));
        }
        let factory = self.clone();
        let session = session.clone();
        tokio::time::timeout(
            SESSION_EXPORT_DEADLINE,
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                factory.export_session_blocking(&session, format, &output_path, force)
            }),
        )
        .await
        .map_err(|_| HostError::Query("session export deadline exceeded".to_owned()))?
        .map_err(|_| HostError::Query("session export worker failed".to_owned()))?
    }
}
