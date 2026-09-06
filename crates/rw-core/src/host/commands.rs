use super::{
    Arc, BuiltinProviderId, ClientCommand, ClientRole, CommandOutcome, CreateSessionRequest,
    EngineEvent, EngineHost, HostError, ModeDescriptor, ModelAlias, Ordering,
    PROVIDER_AUTH_BEGIN_DEADLINE, Path, PendingProviderAuth, ProviderAuthOpeningGuard,
    ProviderAuthOwner, SessionId, SessionSlot, ack_meta, bounded_provider_auth_prompt,
    cancel_provider_auth_attempts, ensure_session_driver, overlay_model_catalog_current,
    pending_provider_auth_id, provider_auth_attempt_id, remove_provider_auth_reservation,
    sanitized_provider_auth_error, validate_provider_auth_name, watch, wire_command_catalog,
    wire_mode_catalog,
};

impl EngineHost {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn execute_inner(
        &self,
        command: ClientCommand,
    ) -> Result<(CommandOutcome, Option<SessionId>, Vec<EngineEvent>), HostError> {
        if self.shutting_down.load(Ordering::Acquire)
            && !matches!(command, ClientCommand::ShutdownHost { .. })
        {
            return Err(HostError::ShuttingDown);
        }
        match command {
            ClientCommand::GetSessionState { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let snapshot = session
                    .handle()
                    .live_state()
                    .await
                    .map_err(HostError::from)?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::SessionStateReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        snapshot,
                    }],
                ))
            }
            ClientCommand::GetSessionControls { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let snapshot = session.handle().controls().await.map_err(HostError::from)?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::SessionControlsReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        snapshot,
                    }],
                ))
            }

            ClientCommand::ReadFamilyControls {
                meta,
                session_id,
                after_revision: _,
            } => {
                let session = self.ready_session(&session_id).await?;
                let service = session
                    .subagents()
                    .ok_or_else(|| HostError::Query("family controls unavailable".into()))?;
                let snapshot = service.family_controls(&session_id).await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::FamilyControlsReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        snapshot,
                    }],
                ))
            }
            ClientCommand::ReadChildControls {
                meta,
                session_id,
                target,
            } => {
                let session = self.ready_session(&session_id).await?;
                let service = session
                    .subagents()
                    .ok_or_else(|| HostError::Query("family controls unavailable".into()))?;
                let snapshot = service.child_controls(&session_id, &target).await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::ChildControlsReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        target,
                        snapshot,
                    }],
                ))
            }
            ClientCommand::ResolveChildControl {
                meta,
                session_id,
                target,
                expected_revision,
                response,
            } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                let authority = session.handle().family_control_authority(&meta.client_id)?;
                let service = session
                    .subagents()
                    .ok_or_else(|| HostError::Query("family controls unavailable".into()))?;
                let outcome = service
                    .respond_control(
                        &session_id,
                        &target,
                        authority,
                        meta,
                        expected_revision,
                        response,
                    )
                    .await?;
                Ok((outcome, Some(session_id), Vec::new()))
            }
            ClientCommand::GetUiCatalog { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let catalog = session
                    .handle()
                    .ui_catalog()
                    .await
                    .map_err(HostError::from)?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::UiCatalogReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        catalog,
                    }],
                ))
            }
            ClientCommand::GetUiPanels { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let panels = session
                    .handle()
                    .ui_panels()
                    .await
                    .map_err(HostError::from)?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::UiPanelsReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        panels,
                    }],
                ))
            }
            ClientCommand::ReadSessionChildren {
                meta,
                session_id,
                scope,
            } => {
                let result = self.queries.session_children(&session_id, scope).await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::SessionChildrenReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        result,
                    }],
                ))
            }
            ClientCommand::GetTodos {
                meta,
                session_id,
                scope,
            } => {
                let result = self.queries.todos(&session_id, scope).await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::TodosRead {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        result,
                    }],
                ))
            }
            ClientCommand::ReadTranscriptTail {
                meta,
                session_id,
                scope,
                read,
            } => {
                let result = self
                    .queries
                    .read_transcript_tail(&session_id, scope, read)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::TranscriptTailReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        result,
                    }],
                ))
            }
            ClientCommand::ReadTranscript {
                meta,
                session_id,
                scope,
                read,
            } => {
                let result = self
                    .queries
                    .read_transcript(&session_id, scope, read)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::TranscriptPageReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        result,
                    }],
                ))
            }
            ClientCommand::ReadTranscriptContent {
                meta,
                session_id,
                scope,
                read,
            } => {
                let page = self
                    .queries
                    .read_transcript_content(&session_id, scope, read)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::TranscriptContentReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        page,
                    }],
                ))
            }
            ClientCommand::CreateSession { meta, cwd, model } => {
                let session_id = self.factory.allocate_session_id()?;
                let session = self
                    .create_session(CreateSessionRequest {
                        session_id: session_id.clone(),
                        workspace: cwd,
                        model,
                    })
                    .await?;
                let outcome = session
                    .handle()
                    .dispatch(ClientCommand::AttachSession {
                        meta: meta.clone(),
                        session_id: session_id.clone(),
                        last_seen_sequence: None,
                        role: ClientRole::Driver,
                    })
                    .await?;
                if matches!(outcome, CommandOutcome::Accepted {}) {
                    session.set_driver(Some(meta.client_id.clone()));
                }
                Ok((
                    outcome,
                    Some(session_id),
                    vec![EngineEvent::SessionsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        sessions: vec![session.descriptor()],
                    }],
                ))
            }
            ClientCommand::ResumeSession {
                meta,
                session_id,
                last_seen_sequence,
                role,
            } => {
                let session = self.resume_session(&session_id).await?;
                let _lifecycle_guard = match role {
                    ClientRole::Driver => Some(Arc::clone(&session.lifecycle).lock_owned().await),
                    ClientRole::Observer => None,
                };
                let outcome = session
                    .handle()
                    .dispatch(ClientCommand::AttachSession {
                        meta: meta.clone(),
                        session_id: session_id.clone(),
                        last_seen_sequence,
                        role: role.clone(),
                    })
                    .await?;
                if matches!(outcome, CommandOutcome::Accepted {}) && role == ClientRole::Driver {
                    session.set_driver(Some(meta.client_id.clone()));
                }
                Ok((
                    outcome,
                    Some(session_id),
                    vec![EngineEvent::SessionsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        sessions: vec![session.descriptor()],
                    }],
                ))
            }
            ClientCommand::ListSessions { meta } => {
                let mut sessions = self.factory.persisted_sessions().await?;
                let registry = self.registry.lock().await;
                for slot in registry.sessions.values() {
                    if let SessionSlot::Ready(session) = slot {
                        let descriptor = session.descriptor();
                        sessions.retain(|existing| existing.session_id != descriptor.session_id);
                        sessions.push(descriptor);
                    }
                }
                sessions.sort_by(|left, right| left.session_id.0.cmp(&right.session_id.0));
                Ok((
                    CommandOutcome::Accepted {},
                    None,
                    vec![EngineEvent::SessionsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        sessions,
                    }],
                ))
            }
            ClientCommand::SearchSessions { meta, query, limit } => {
                if query.trim().is_empty() || query.len() > 512 || !(1..=1_000).contains(&limit) {
                    return Err(HostError::Protocol(
                        "session search query or limit is invalid".to_owned(),
                    ));
                }
                let (sessions, truncated) = self
                    .factory
                    .search_persisted_sessions(&query, limit)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    None,
                    vec![EngineEvent::SessionsSearchReady {
                        meta: ack_meta(&meta, &*self.clock),
                        query,
                        sessions,
                        truncated,
                    }],
                ))
            }
            ClientCommand::RenameSession {
                meta,
                session_id,
                title,
            } => {
                // Resuming through this factory is the same storage-root and
                // workspace authorization boundary used by list/search. A
                // driver lease is deliberately not consulted: picker rename
                // applies to any session visible within that local scope.
                let session = self.resume_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                let tail = session.handle().last_sequence().await?;
                let mut events = session
                    .handle()
                    .subscribe_client(meta.client_id.clone(), tail)?;
                let request_id = meta.request_id.clone();
                let outcome = session
                    .handle()
                    .dispatch_durably(ClientCommand::RenameSession {
                        meta: meta.clone(),
                        session_id: session_id.clone(),
                        title,
                    })
                    .await?;
                let updated = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let event = events.recv().await.map_err(HostError::from)?;
                        if matches!(
                            &event,
                            EngineEvent::SessionTitleUpdated { meta, .. }
                                if meta.caused_by.as_ref() == Some(&request_id)
                        ) {
                            break Ok::<_, HostError>(event);
                        }
                    }
                })
                .await
                .map_err(|_| {
                    HostError::Persistence(
                        "committed session title update was not observable".to_owned(),
                    )
                })??;
                if let EngineEvent::SessionTitleUpdated { title, .. } = &updated {
                    session
                        .descriptor
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .title
                        .clone_from(title);
                }
                Ok((outcome, Some(session_id), vec![updated]))
            }
            ClientCommand::ListCommands { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let descriptors = session.handle().command_descriptors();
                let (commands, truncated) = wire_command_catalog(descriptors.iter().cloned());
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::CommandDescriptorsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        commands,
                        truncated,
                    }],
                ))
            }
            ClientCommand::ListModes { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let snapshot = session.handle().snapshot().await.map_err(HostError::from)?;
                let registry = session.handle().mode_registry();
                let active = registry.get(&snapshot.mode_id.0).ok_or_else(|| {
                    HostError::Persistence(format!(
                        "active mode {:?} is absent from the live registry",
                        snapshot.mode_id.0
                    ))
                })?;
                let active = ModeDescriptor {
                    id: active.id().clone(),
                    description: active.description().to_owned(),
                    current: true,
                };
                let (modes, truncated) = wire_mode_catalog(
                    active,
                    registry
                        .iter()
                        .filter(|definition| definition.id() != &snapshot.mode_id)
                        .map(|definition| ModeDescriptor {
                            id: definition.id().clone(),
                            description: definition.description().to_owned(),
                            current: false,
                        }),
                );
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::ModesListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        modes,
                        truncated,
                    }],
                ))
            }
            ClientCommand::ListModels {
                meta,
                session_id,
                refresh,
            } => {
                let (session_catalog, selected, resolved) = if let Some(session_id) = &session_id {
                    let session = self.ready_session(session_id).await?;
                    let snapshot = session.handle().snapshot().await.map_err(HostError::from)?;
                    let resolved = snapshot.resolved_model;
                    (
                        session.model_catalog(),
                        Some(snapshot.model_alias),
                        resolved,
                    )
                } else {
                    (None, None, None)
                };
                let mut catalog = if let Some(session_catalog) = session_catalog {
                    session_catalog
                        .get(refresh)
                        .await
                        .map_err(|error| HostError::Query(error.to_string()))?
                } else {
                    self.queries.model_catalog(refresh, None, None).await?
                };
                overlay_model_catalog_current(
                    &mut catalog,
                    selected.as_deref(),
                    resolved.as_deref(),
                );
                Ok((
                    CommandOutcome::Accepted {},
                    None,
                    vec![EngineEvent::ModelsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        models: catalog.models,
                        aliases: catalog.aliases,
                        providers: catalog.providers,
                        cached: catalog.cached,
                        truncated: catalog.truncated,
                    }],
                ))
            }
            ClientCommand::ListSettings { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let settings = self.queries.user_settings(&session.descriptor()).await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::SettingsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        settings,
                    }],
                ))
            }
            ClientCommand::SetSetting {
                meta,
                session_id,
                key,
                value,
            } => {
                let session = self.ready_session(&session_id).await?;
                let queries = Arc::clone(&self.queries);
                let actor = meta.client_id.clone();
                let settings = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may persist user settings".to_owned(),
                        ));
                    }
                    queries
                        .set_user_setting(&session.descriptor(), &key, &value)
                        .await
                })
                .await
                .map_err(|_| HostError::Query("user setting task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::SettingsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        settings,
                    }],
                ))
            }
            ClientCommand::ListMcpServers { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let servers = mcp.list().await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::ListRuntimeServices { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let services = match session.runtime_services() {
                    Some(services) => services.list().await?,
                    None => Vec::new(),
                };
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::RuntimeServicesListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        services,
                    }],
                ))
            }
            ClientCommand::AddMcpHttpServer {
                meta,
                session_id,
                name,
                endpoint,
            } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let actor = meta.client_id.clone();
                let servers = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may add MCP servers".to_owned(),
                        ));
                    }
                    mcp.add_http(&name, &endpoint).await
                })
                .await
                .map_err(|_| HostError::Query("MCP add task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::AddMcpStdioServer {
                meta,
                session_id,
                name,
                executable,
                args,
                environment,
            } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let actor = meta.client_id.clone();
                let servers = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may add MCP servers".to_owned(),
                        ));
                    }
                    mcp.add_stdio(&name, &executable, &args, &environment).await
                })
                .await
                .map_err(|_| HostError::Query("MCP add task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::RemoveMcpServer {
                meta,
                session_id,
                name,
            } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let actor = meta.client_id.clone();
                let servers = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may remove MCP servers".to_owned(),
                        ));
                    }
                    mcp.remove(&name).await
                })
                .await
                .map_err(|_| HostError::Query("MCP remove task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::ReviewMcpServer {
                meta,
                session_id,
                name,
            } => {
                let session = self.ready_session(&session_id).await?;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may review MCP configuration".to_owned(),
                    ));
                }
                let review = session
                    .mcp()
                    .ok_or_else(|| {
                        HostError::Query(
                            "live MCP management is unavailable for this session".to_owned(),
                        )
                    })?
                    .review(&name)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServerApprovalReviewed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        review,
                    }],
                ))
            }
            ClientCommand::ApproveMcpServer {
                meta,
                session_id,
                name,
                fingerprint,
            } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let actor = meta.client_id.clone();
                let servers = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may approve MCP servers".to_owned(),
                        ));
                    }
                    mcp.approve(&name, &fingerprint).await
                })
                .await
                .map_err(|_| HostError::Query("MCP approval task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::SetMcpServerEnabled {
                meta,
                session_id,
                name,
                enabled,
            } => {
                let session = self.ready_session(&session_id).await?;
                let mcp = session.mcp().ok_or_else(|| {
                    HostError::Query(
                        "live MCP management is unavailable for this session".to_owned(),
                    )
                })?;
                let actor = meta.client_id.clone();
                let servers = tokio::spawn(async move {
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may enable or disable MCP servers".to_owned(),
                        ));
                    }
                    mcp.set_enabled(&name, enabled).await
                })
                .await
                .map_err(|_| HostError::Query("MCP enablement task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::McpServersListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        servers,
                    }],
                ))
            }
            ClientCommand::BeginProviderAuth {
                meta,
                session_id,
                provider,
            } => {
                validate_provider_auth_name(&provider)?;
                let session = self.ready_session(&session_id).await?;
                let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may authenticate providers".to_owned(),
                    ));
                }
                let owner = ProviderAuthOwner {
                    client_id: meta.client_id.clone(),
                    session_id: session_id.clone(),
                    provider: provider.clone(),
                };
                let attempt_id = provider_auth_attempt_id(&meta, &session_id, &provider);
                {
                    let mut pending = self
                        .provider_auth
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if pending.keys().any(|active| active.provider == provider) {
                        return Err(HostError::Protocol(
                            "provider authentication is already in progress".to_owned(),
                        ));
                    }
                    pending.insert(
                        owner.clone(),
                        PendingProviderAuth::Opening {
                            attempt_id: attempt_id.clone(),
                        },
                    );
                }
                let mut opening_guard = ProviderAuthOpeningGuard {
                    pending: Arc::clone(&self.provider_auth),
                    owner: owner.clone(),
                    attempt_id: attempt_id.clone(),
                    armed: true,
                };
                drop(lifecycle_guard);
                let attempt = match tokio::time::timeout(
                    PROVIDER_AUTH_BEGIN_DEADLINE,
                    self.queries.begin_provider_auth(&provider),
                )
                .await
                {
                    Ok(Ok(attempt)) => attempt,
                    Ok(Err(error)) => {
                        remove_provider_auth_reservation(&self.provider_auth, &owner, &attempt_id);
                        return Err(HostError::Query(sanitized_provider_auth_error(&error)));
                    }
                    Err(_) => {
                        remove_provider_auth_reservation(&self.provider_auth, &owner, &attempt_id);
                        return Err(HostError::Query(
                            "provider authentication setup deadline exceeded".to_owned(),
                        ));
                    }
                };
                let (challenge, warnings) = bounded_provider_auth_prompt(&attempt)?;
                let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                let driver_unchanged = session.handle().snapshot().await?.driver_client_id.as_ref()
                    == Some(&meta.client_id);
                let mut attempt = Some(attempt);
                let retained = {
                    let mut pending = self
                        .provider_auth
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if driver_unchanged
                        && matches!(
                            pending.get(&owner),
                            Some(PendingProviderAuth::Opening { attempt_id: current }) if current == &attempt_id
                        )
                    {
                        if let Some(retained_attempt) = attempt.take() {
                            pending.insert(
                                owner,
                                PendingProviderAuth::Ready {
                                    attempt_id: attempt_id.clone(),
                                    attempt: retained_attempt,
                                },
                            );
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };
                drop(lifecycle_guard);
                if !retained {
                    if let Some(attempt) = attempt {
                        attempt.cancel();
                    }
                    return Err(HostError::Protocol(
                        "provider authentication was cancelled during setup".to_owned(),
                    ));
                }
                opening_guard.disarm();
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::ProviderAuthStarted {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        attempt_id,
                        provider,
                        challenge,
                        warnings,
                    }],
                ))
            }
            ClientCommand::ConfigureBuiltinProvider {
                meta,
                session_id,
                provider,
            } => {
                validate_provider_auth_name(&provider)?;
                let session = self.ready_session(&session_id).await?;
                let profile = BuiltinProviderId::parse(&provider)
                    .map(BuiltinProviderId::profile)
                    .filter(|profile| profile.setup_exposed())
                    .ok_or_else(|| {
                        HostError::Protocol(
                            "provider is not in the fixed built-in setup registry".to_owned(),
                        )
                    })?;
                let auth_kind = profile.onboarding_auth_kind();
                let queries = Arc::clone(&self.queries);
                let provider_mutation = Arc::clone(&self.provider_mutation);
                let actor = meta.client_id.clone();
                tokio::spawn(async move {
                    let _provider_mutation = provider_mutation.lock_owned().await;
                    let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                    let snapshot = session.handle().snapshot().await?;
                    if snapshot.driver_client_id.as_ref() != Some(&actor) {
                        return Err(HostError::Protocol(
                            "only the current driver may configure built-in providers".to_owned(),
                        ));
                    }
                    queries.configure_builtin_provider(profile).await
                })
                .await
                .map_err(|_| HostError::Query("provider configuration task failed".to_owned()))??;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::ProviderConfigured {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        provider,
                        auth_kind,
                    }],
                ))
            }
            ClientCommand::CompleteProviderAuth {
                meta,
                session_id,
                provider,
                attempt_id,
            } => {
                validate_provider_auth_name(&provider)?;
                let session = self.ready_session(&session_id).await?;
                let lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may complete provider authentication".to_owned(),
                    ));
                }
                let owner = ProviderAuthOwner {
                    client_id: meta.client_id.clone(),
                    session_id: session_id.clone(),
                    provider: provider.clone(),
                };
                let pending = {
                    let mut entries = self
                        .provider_auth
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    entries.remove(&owner)
                };
                let attempt = match pending {
                    Some(PendingProviderAuth::Ready {
                        attempt_id: current,
                        attempt,
                    }) if current == attempt_id => attempt,
                    Some(pending @ PendingProviderAuth::Completing { .. })
                        if pending_provider_auth_id(&pending) == &attempt_id =>
                    {
                        self.provider_auth
                            .entries
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(owner, pending);
                        // ProviderAuthStarted is durable and can be replayed after a
                        // transport reconnect. Treat the corresponding completion
                        // command as an idempotent subscription to the already-running
                        // poll/callback instead of turning a healthy login into a
                        // protocol error.
                        return Ok((CommandOutcome::Accepted {}, Some(session_id), Vec::new()));
                    }
                    Some(other) => {
                        self.provider_auth
                            .entries
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(owner, other);
                        return Err(HostError::Protocol(
                            "provider authentication attempt is not ready or does not match"
                                .to_owned(),
                        ));
                    }
                    None => {
                        return Err(HostError::Protocol(
                            "provider authentication attempt is no longer active".to_owned(),
                        ));
                    }
                };
                let cancellation = attempt.cancellation();
                let (cancelled, cancel_signal) = watch::channel(false);
                self.provider_auth
                    .entries
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        owner.clone(),
                        PendingProviderAuth::Completing {
                            attempt_id: attempt_id.clone(),
                            cancellation: Arc::clone(&cancellation),
                            cancelled,
                        },
                    );
                drop(lifecycle_guard);
                let host = self.clone();
                tokio::spawn(async move {
                    host.complete_provider_auth_task(
                        owner,
                        attempt_id,
                        attempt,
                        cancel_signal,
                        meta,
                    )
                    .await;
                });
                Ok((CommandOutcome::Accepted {}, Some(session_id), Vec::new()))
            }
            ClientCommand::CancelProviderAuth {
                meta,
                session_id,
                provider,
                attempt_id,
            } => {
                validate_provider_auth_name(&provider)?;
                let session = self.ready_session(&session_id).await?;
                let _lifecycle_guard = Arc::clone(&session.lifecycle).lock_owned().await;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may cancel provider authentication".to_owned(),
                    ));
                }
                let owner = ProviderAuthOwner {
                    client_id: meta.client_id.clone(),
                    session_id: session_id.clone(),
                    provider: provider.clone(),
                };
                let pending = {
                    let mut entries = self
                        .provider_auth
                        .entries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let pending = entries.remove(&owner);
                    match pending {
                        Some(pending) if pending_provider_auth_id(&pending) == &attempt_id => {
                            pending
                        }
                        Some(pending) => {
                            entries.insert(owner, pending);
                            return Err(HostError::Protocol(
                                "provider authentication attempt does not match".to_owned(),
                            ));
                        }
                        None => {
                            return Err(HostError::Protocol(
                                "provider authentication attempt is no longer active".to_owned(),
                            ));
                        }
                    }
                };
                cancel_provider_auth_attempts(vec![pending]);
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::ProviderAuthFinished {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        attempt_id,
                        provider,
                        success: false,
                        message: "provider authentication cancelled".to_owned(),
                        warnings: Vec::new(),
                    }],
                ))
            }
            ClientCommand::ExportSession {
                meta,
                session_id,
                format,
                output_path,
                force,
            } => {
                if !Path::new(&output_path).is_absolute() {
                    return Err(HostError::Protocol(
                        "export output path must be absolute".to_owned(),
                    ));
                }
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                let snapshot = session.handle().snapshot().await?;
                if snapshot.driver_client_id.as_ref() != Some(&meta.client_id) {
                    return Err(HostError::Protocol(
                        "only the current driver may export this session".to_owned(),
                    ));
                }
                let resolved_path = self
                    .queries
                    .export_session(&session.descriptor(), format, &output_path, force)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::SessionExported {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        output_path: resolved_path,
                    }],
                ))
            }
            ClientCommand::SearchWorkspaceFiles {
                meta,
                session_id,
                query,
                limit,
            } => {
                let session = self.ready_session(&session_id).await?;
                let (matches, truncated) = self
                    .queries
                    .search_workspace_files(&session.descriptor(), &query, limit)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::WorkspaceFilesFound {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        matches,
                        truncated,
                    }],
                ))
            }
            ClientCommand::PreviewWorkspaceFile {
                meta,
                session_id,
                path,
                max_bytes,
            } => {
                let session = self.ready_session(&session_id).await?;
                let preview = self
                    .queries
                    .preview_workspace_file(&session.descriptor(), &path, max_bytes)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::WorkspaceFilePreviewReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        preview,
                    }],
                ))
            }
            ClientCommand::GetWorkspaceStatus { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let status = self.queries.workspace_status(&session.descriptor()).await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::WorkspaceStatusReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        status,
                    }],
                ))
            }
            ClientCommand::GetWorkspaceDiff {
                meta,
                session_id,
                path,
                max_bytes,
            } => {
                let session = self.ready_session(&session_id).await?;
                let diff = self
                    .queries
                    .workspace_diff(&session.descriptor(), &path, max_bytes)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::WorkspaceDiffReady {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        diff,
                    }],
                ))
            }
            ClientCommand::ListSubagents { meta, session_id } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                let subagents = session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .list(&session_id)
                    .await?;
                Ok((
                    CommandOutcome::Accepted {},
                    Some(session_id.clone()),
                    vec![EngineEvent::SubagentsListed {
                        meta: ack_meta(&meta, &*self.clock),
                        session_id,
                        subagents,
                    }],
                ))
            }
            ClientCommand::ContinueSubagent {
                meta,
                session_id,
                subagent_id,
                content,
            } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .continue_child(&session_id, &subagent_id, content)
                    .await?;
                Ok((CommandOutcome::Accepted {}, Some(session_id), Vec::new()))
            }
            ClientCommand::InterruptSubagent {
                meta,
                session_id,
                subagent_id,
            } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .interrupt(&session_id, &subagent_id)
                    .await?;
                Ok((CommandOutcome::Accepted {}, Some(session_id), Vec::new()))
            }
            ClientCommand::CloseSubagent {
                meta,
                session_id,
                subagent_id,
            } => {
                let session = self.ready_session(&session_id).await?;
                let _lifecycle = Arc::clone(&session.lifecycle).lock_owned().await;
                ensure_session_driver(&session, &meta.client_id).await?;
                session
                    .subagents()
                    .ok_or_else(|| {
                        HostError::Query(
                            "child-agent control is unavailable for this session".to_owned(),
                        )
                    })?
                    .close(&session_id, &subagent_id)
                    .await?;
                Ok((CommandOutcome::Accepted {}, Some(session_id), Vec::new()))
            }
            ClientCommand::ShutdownHost { meta } => {
                self.shutdown_sessions().await?;
                Ok((
                    CommandOutcome::Accepted {},
                    None,
                    vec![EngineEvent::HostShutdown {
                        meta: ack_meta(&meta, &*self.clock),
                    }],
                ))
            }
            command => {
                let session_id = command
                    .session_id()
                    .cloned()
                    .ok_or_else(|| HostError::Protocol("command has no session id".to_owned()))?;
                let session = self.ready_session(&session_id).await?;
                let driver = match &command {
                    ClientCommand::TakeDriver { meta, .. } => Some(meta.client_id.clone()),
                    _ => None,
                };
                let persists_model = matches!(
                    command,
                    ClientCommand::SwitchModel { .. } | ClientCommand::AnswerQuestion { .. }
                );
                let lifecycle = (matches!(command, ClientCommand::TakeDriver { .. })
                    || persists_model)
                    .then(|| Arc::clone(&session.lifecycle));
                let _lifecycle = match lifecycle {
                    Some(lifecycle) => Some(lifecycle.lock_owned().await),
                    None => None,
                };
                let previous_driver = if driver.is_some() {
                    session.handle().snapshot().await?.driver_client_id
                } else {
                    None
                };
                let previous_model = if persists_model {
                    Some(session.handle().snapshot().await?.model_alias)
                } else {
                    None
                };
                let outcome = if persists_model {
                    session.handle().dispatch_durably(command).await?
                } else {
                    session.handle().dispatch(command).await?
                };
                if matches!(outcome, CommandOutcome::Accepted {}) {
                    // TakeDriver persists its lease before returning Accepted.
                    // Shell commands acknowledge before their durable event,
                    // so that descriptor field is updated by
                    // `project_durable_descriptor`. Model commands use the
                    // awaited durable path below so project preference
                    // persistence cannot be detached or silently ignored.
                    if let Some(driver) = driver {
                        if let Some(previous) =
                            previous_driver.filter(|previous| previous != &driver)
                        {
                            self.provider_auth
                                .cancel_session_client(&previous, &session_id);
                        }
                        session.set_driver(Some(driver));
                    }
                    if let Some(previous_model) = previous_model {
                        let committed_model = session.handle().snapshot().await?.model_alias;
                        if committed_model != previous_model {
                            let model = ModelAlias(committed_model);
                            let descriptor = {
                                let mut descriptor = session
                                    .descriptor
                                    .write()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                descriptor.model = model.clone();
                                descriptor.clone()
                            };
                            self.queries
                                .persist_project_model_selection(&descriptor, &model)
                                .await?;
                        }
                    }
                }
                Ok((outcome, Some(session_id), Vec::new()))
            }
        }
    }
}
