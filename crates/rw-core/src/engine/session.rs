#[allow(clippy::wildcard_imports)]
use super::*;

/// Dependencies and guardrails for one headless session actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupNotification {
    pub plugin_id: String,
    pub status: String,
    pub title: String,
    pub message: String,
}

pub struct SessionActorConfig {
    pub session_id: SessionId,
    pub workspace_root: PathBuf,
    pub additional_workspace_roots: Vec<PathBuf>,
    pub workspace_generation: u64,
    pub initial_session_context: Vec<Turn>,
    pub startup_notifications: Vec<StartupNotification>,
    pub model_alias: String,
    pub model: Arc<dyn ModelDriver>,
    pub tools: Arc<ToolRegistry>,
    pub permissions: Arc<PermissionGate>,
    pub hooks: Arc<HookDispatcher>,
    pub commands: Arc<CommandRegistry<SessionCommandContext, SessionCommandOutput>>,
    pub modes: Arc<ModeRegistry>,
    pub event_sink: Arc<dyn SessionEventSink>,
    pub event_clock: Arc<dyn EventClock>,
    pub secret_redactor: Arc<dyn SecretRedactor>,
    pub checkpoints: Arc<dyn MutationCheckpointCoordinator>,
    pub folder_trust: Arc<dyn FolderTrustController>,
    pub workspace_roots: Arc<dyn WorkspaceRootController>,
    pub extension_development: Arc<dyn SessionExtensionController>,
    pub recovered: SessionRecoveredState,
    pub max_turns: usize,
    pub identical_tool_failure_limit: usize,
    pub max_output_tokens: u32,
    pub thinking: ThinkingLevel,
    pub event_capacity: usize,
}

impl fmt::Debug for SessionActorConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionActorConfig")
            .field("session_id", &self.session_id)
            .field("workspace_root", &self.workspace_root)
            .field(
                "additional_workspace_roots",
                &self.additional_workspace_roots,
            )
            .field("workspace_generation", &self.workspace_generation)
            .field("initial_session_context", &self.initial_session_context)
            .field("startup_notifications", &self.startup_notifications)
            .field("model_alias", &self.model_alias)
            .field("recovered", &self.recovered)
            .field("max_turns", &self.max_turns)
            .field(
                "identical_tool_failure_limit",
                &self.identical_tool_failure_limit,
            )
            .field("max_output_tokens", &self.max_output_tokens)
            .field("thinking", &self.thinking)
            .field("event_capacity", &self.event_capacity)
            .finish_non_exhaustive()
    }
}

impl SessionActorConfig {
    pub(super) fn with_model_alias(&self, model_alias: String) -> Self {
        Self {
            session_id: self.session_id.clone(),
            workspace_root: self.workspace_root.clone(),
            additional_workspace_roots: self.additional_workspace_roots.clone(),
            workspace_generation: self.workspace_generation,
            initial_session_context: self.initial_session_context.clone(),
            startup_notifications: self.startup_notifications.clone(),
            model_alias,
            model: Arc::clone(&self.model),
            tools: Arc::clone(&self.tools),
            permissions: Arc::clone(&self.permissions),
            hooks: Arc::clone(&self.hooks),
            commands: Arc::clone(&self.commands),
            modes: Arc::clone(&self.modes),
            event_sink: Arc::clone(&self.event_sink),
            event_clock: Arc::clone(&self.event_clock),
            secret_redactor: Arc::clone(&self.secret_redactor),
            checkpoints: Arc::clone(&self.checkpoints),
            folder_trust: Arc::clone(&self.folder_trust),
            workspace_roots: Arc::clone(&self.workspace_roots),
            extension_development: Arc::clone(&self.extension_development),
            recovered: self.recovered.clone(),
            max_turns: self.max_turns,
            identical_tool_failure_limit: self.identical_tool_failure_limit,
            max_output_tokens: self.max_output_tokens,
            thinking: self.thinking,
            event_capacity: self.event_capacity,
        }
    }

    pub(super) fn with_workspace_generation(
        &self,
        generation: &WorkspaceRuntimeGeneration,
        active_mode: &ModeId,
    ) -> Self {
        let mut configured = self.with_model_alias(self.model_alias.clone());
        configured.workspace_root.clone_from(&generation.roots[0]);
        configured.additional_workspace_roots = generation.roots.iter().skip(1).cloned().collect();
        configured.workspace_generation = generation.generation;
        configured.tools = Arc::new(
            generation
                .tools
                .as_ref()
                .clone()
                .with_mcp_tool_policy(self.tools.mcp_tool_policy().clone()),
        );
        configured.hooks = Arc::clone(&generation.hooks);
        configured.commands = Arc::clone(&generation.commands);
        configured.modes = self.modes.get(&active_mode.0).map_or_else(
            || Arc::clone(&generation.modes),
            |definition| Arc::new(generation.modes.with_pinned(definition.clone())),
        );
        configured.permissions = Arc::clone(&generation.permissions);
        configured.checkpoints = Arc::clone(&generation.checkpoints);
        configured.folder_trust = Arc::clone(&generation.folder_trust);
        configured
            .initial_session_context
            .extend(generation.supplemental_context.iter().cloned());
        configured
    }

    pub(super) fn with_extension_snapshot(&self, snapshot: &SessionExtensionSnapshot) -> Self {
        let mut configured = self.with_model_alias(self.model_alias.clone());
        configured.tools = Arc::clone(&snapshot.tools);
        configured.hooks = Arc::clone(&snapshot.hooks);
        configured.commands = Arc::clone(&snapshot.commands);
        configured
    }

    fn with_model_alias_and_mode(&self, model_alias: String, mode_id: &ModeId) -> Self {
        let mut configured = self.with_model_alias(model_alias);
        let Some(mode) = configured.modes.get(&mode_id.0) else {
            return configured;
        };
        // Execute is the base policy already present in the canonical system
        // prompt. Preserve that stable cache prefix for the embedded default;
        // an extension overriding `execute` still contributes its fragment.
        if mode.id().0 == "execute" && matches!(mode.source(), ModeSource::Embedded { .. }) {
            return configured;
        }
        if let Some(system) = configured
            .initial_session_context
            .iter_mut()
            .find(|turn| turn.role == Role::System)
        {
            system.blocks.push(Block::Text {
                text: mode.prompt().to_owned(),
            });
        } else {
            configured.initial_session_context.insert(
                0,
                Turn {
                    role: Role::System,
                    blocks: vec![Block::Text {
                        text: mode.prompt().to_owned(),
                    }],
                    meta: TurnMeta::default(),
                },
            );
        }
        configured
    }

    pub(super) fn with_model_route_and_mode(
        &self,
        model_alias: String,
        provider: Option<String>,
        mode_id: &ModeId,
    ) -> Self {
        let mut configured = self.with_model_alias_and_mode(model_alias, mode_id);
        configured.recovered.provider = provider;
        configured
    }
}

/// Starts one single-writer session actor.
pub struct SessionActor;

impl SessionActor {
    /// Spawns the actor and returns its provider/UI-neutral handle.
    ///
    /// # Errors
    ///
    /// Rejects zero guardrails, empty aliases, or an unusable workspace root.
    pub fn spawn(config: SessionActorConfig) -> Result<SessionHandle, AgentLoopError> {
        if SessionId::validate(&config.session_id.0).is_err() {
            return Err(AgentLoopError::InvalidConfiguration(
                "session id must satisfy the canonical session identifier grammar".to_owned(),
            ));
        }
        if config.model_alias.trim().is_empty() {
            return Err(AgentLoopError::InvalidConfiguration(
                "model alias must not be empty".to_owned(),
            ));
        }
        let recovered_mode = config
            .recovered
            .mode_id
            .as_ref()
            .map_or("execute", |mode| mode.0.as_str());
        if config.modes.get(recovered_mode).is_none() {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "recovered mode {recovered_mode:?} is not registered"
            )));
        }
        if config.recovered.permission_mode.is_some() {
            config
                .permissions
                .set_runtime_mode(config.recovered.permission_mode)
                .map_err(AgentLoopError::InvalidConfiguration)?;
        }
        if config.max_turns == 0
            || config.identical_tool_failure_limit == 0
            || config.max_output_tokens == 0
            || config.event_capacity == 0
        {
            return Err(AgentLoopError::InvalidConfiguration(
                "turn, doom-loop, output, and event limits must be greater than zero".to_owned(),
            ));
        }
        let tool_context = ToolContext::from_workspace_roots(
            std::iter::once(&config.workspace_root).chain(&config.additional_workspace_roots),
        )
        .map_err(|error| AgentLoopError::ToolContext(error.to_string()))?
        .with_session_id(config.session_id.clone())
        .with_mcp_tool_policy(config.tools.mcp_tool_policy().clone());
        let (command_tx, command_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(config.event_capacity);
        let active_turn = Arc::new(AtomicU64::new(0));
        let command_descriptors = Arc::new(RwLock::new(Arc::from(
            config.commands.descriptors().cloned().collect::<Vec<_>>(),
        )));
        let mode_registry = Arc::new(RwLock::new(Arc::clone(&config.modes)));
        let handle = SessionHandle {
            commands: command_tx,
            events: event_tx.clone(),
            active_turn: active_turn.clone(),
            session_id: config.session_id.clone(),
            event_sink: Arc::clone(&config.event_sink),
            local_request_sequence: Arc::new(AtomicU64::new(0)),
            local_attached: Arc::new(AtomicBool::new(false)),
            local_last_seen: config.recovered.last_sequence,
            command_descriptors: Arc::clone(&command_descriptors),
            mode_registry: Arc::clone(&mode_registry),
            model: Arc::clone(&config.model),
        };
        tokio::spawn(run_actor(
            config,
            tool_context,
            command_rx,
            event_tx,
            active_turn,
            Arc::clone(&command_descriptors),
            mode_registry,
        ));
        Ok(handle)
    }
}
/// One client-filtered view of the single engine event channel. A lagged live
/// receiver catches up from the durable source and suppresses duplicate live
/// deliveries by sequence id.
pub struct SessionSubscription {
    pub(super) client_id: ClientId,
    session_id: SessionId,
    pub(super) receiver: broadcast::Receiver<RoutedEvent>,
    sink: Arc<dyn SessionEventSink>,
    last_sequence: Option<SequenceId>,
    initial_tail: Option<SequenceId>,
    pending: VecDeque<EngineEvent>,
    replay: Option<Arc<dyn SessionEventReadView>>,
    needs_initial_replay: bool,
}

impl SessionSubscription {
    /// Durable tail captured before this subscription was returned to its caller.
    #[must_use]
    pub const fn initial_tail(&self) -> Option<SequenceId> {
        self.initial_tail
    }

    /// Loads and validates the first page of the prefix captured at subscription
    /// creation. Callers can validate storage before sending a protocol command.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the durable replay is invalid.
    pub async fn prime(&mut self) -> Result<(), AgentLoopError> {
        if self.needs_initial_replay {
            self.refill_replay().await?;
            self.needs_initial_replay = false;
        }
        Ok(())
    }

    async fn refill_replay(&mut self) -> Result<(), AgentLoopError> {
        let Some(view) = &self.replay else {
            return Ok(());
        };
        if self.last_sequence == view.last_sequence() {
            self.replay = None;
            return Ok(());
        }
        let page = view
            .read_page(self.last_sequence, SessionReplayLimits::default())
            .await?;
        validate_gap(self.last_sequence, &page, &self.session_id)?;
        if page.is_empty()
            || page.last().and_then(EngineEvent::meta).is_some_and(|meta| {
                view.last_sequence()
                    .is_none_or(|tail| meta.sequence_id > tail)
            })
        {
            return Err(AgentLoopError::Persistence(
                "replay page does not advance inside its captured prefix".to_owned(),
            ));
        }
        self.pending.extend(page);
        Ok(())
    }

    /// Receives the next protocol event for this client.
    ///
    /// # Errors
    ///
    /// Returns a persistence error if a broadcast gap cannot be replayed, or
    /// [`AgentLoopError::Closed`] after the actor event channel closes.
    pub async fn recv(&mut self) -> Result<EngineEvent, AgentLoopError> {
        loop {
            self.prime().await?;
            if self.pending.is_empty() {
                self.refill_replay().await?;
            }
            if let Some(event) = self.pending.pop_front() {
                self.observe(&event);
                return Ok(event);
            }
            match self.receiver.recv().await {
                Ok(routed) => {
                    if routed
                        .target
                        .as_ref()
                        .is_some_and(|target| target != &self.client_id)
                    {
                        continue;
                    }
                    if let Some(meta) = routed.event.meta()
                        && self
                            .last_sequence
                            .is_some_and(|last| meta.sequence_id <= last)
                    {
                        continue;
                    }
                    self.observe(&routed.event);
                    return Ok(routed.event);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.replay = Some(self.sink.capture_read_view()?);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(AgentLoopError::Closed);
                }
            }
        }
    }

    fn observe(&mut self, event: &EngineEvent) {
        if let Some(meta) = event.meta() {
            self.last_sequence = Some(meta.sequence_id);
        }
    }
}

pub(super) fn validate_gap(
    last_seen: Option<SequenceId>,
    gap: &[EngineEvent],
    session_id: &SessionId,
) -> Result<(), AgentLoopError> {
    let mut expected = last_seen.map_or(0, |sequence| sequence.0.saturating_add(1));
    for event in gap {
        let meta = event.meta().ok_or_else(|| {
            AgentLoopError::Persistence(
                "durable gap contained a connection-scoped acknowledgement".to_owned(),
            )
        })?;
        if meta.protocol_version != PROTOCOL_VERSION {
            return Err(AgentLoopError::Persistence(format!(
                "durable gap returned protocol version {}, expected {PROTOCOL_VERSION}",
                meta.protocol_version
            )));
        }
        if &meta.session_id != session_id {
            return Err(AgentLoopError::Persistence(
                "durable gap returned an event for a different session".to_owned(),
            ));
        }
        if meta.sequence_id.0 != expected {
            return Err(AgentLoopError::Persistence(format!(
                "durable gap returned sequence {}, expected {expected}",
                meta.sequence_id.0
            )));
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
    }
    Ok(())
}

/// Cloneable command/event boundary for one session actor.
#[derive(Clone)]
pub struct SessionHandle {
    pub(super) commands: mpsc::Sender<ActorCommand>,
    events: broadcast::Sender<RoutedEvent>,
    pub(super) active_turn: Arc<AtomicU64>,
    session_id: SessionId,
    pub(super) event_sink: Arc<dyn SessionEventSink>,
    local_request_sequence: Arc<AtomicU64>,
    local_attached: Arc<AtomicBool>,
    local_last_seen: Option<SequenceId>,
    command_descriptors: Arc<RwLock<Arc<[CommandDescriptor]>>>,
    mode_registry: Arc<RwLock<Arc<ModeRegistry>>>,
    model: Arc<dyn ModelDriver>,
}

/// Opaque, plugin-scoped machine capability for one session actor.
///
/// This capability deliberately exposes only the three approved plugin push
/// operations. It cannot dispatch client commands, acquire the driver lease,
/// answer permissions, or interrupt a turn.
#[derive(Clone)]
pub struct PluginSessionCapability {
    commands: mpsc::Sender<ActorCommand>,
    plugin_id: String,
}

impl fmt::Debug for PluginSessionCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginSessionCapability")
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

impl PluginSessionCapability {
    /// Injects one plain user message through normal actor sequencing.
    /// Slash-prefixed content remains a message and is never command-dispatched.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-bearing input and a closed actor.
    pub async fn inject_message(
        &self,
        content: impl Into<String>,
    ) -> Result<MessageDisposition, AgentLoopError> {
        let content = content.into();
        validate_plugin_text("injected message", &content, MAX_PLUGIN_MESSAGE_BYTES)?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginInjectMessage {
                plugin_id: self.plugin_id.clone(),
                content,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Publishes bounded session status text without taking the driver lease.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-bearing input, persistence failure,
    /// and a closed actor.
    pub async fn set_status(&self, status: impl Into<String>) -> Result<(), AgentLoopError> {
        let status = status.into();
        validate_plugin_text("plugin status", &status, MAX_PLUGIN_STATUS_BYTES)?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginSetStatus {
                plugin_id: self.plugin_id.clone(),
                status,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Publishes a bounded session-local UI notification.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or control-bearing input, persistence failure,
    /// and a closed actor.
    pub async fn notify(
        &self,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<(), AgentLoopError> {
        let title = title.into();
        let message = message.into();
        validate_plugin_text(
            "notification title",
            &title,
            MAX_PLUGIN_NOTIFICATION_TITLE_BYTES,
        )?;
        validate_plugin_text(
            "notification message",
            &message,
            MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES,
        )?;
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PluginNotify {
                plugin_id: self.plugin_id.clone(),
                title,
                message,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }
}

pub(super) fn validate_plugin_text(
    label: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), AgentLoopError> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(AgentLoopError::InvalidConfiguration(format!(
            "{label} is empty, exceeds its byte limit, or contains control characters"
        )));
    }
    Ok(())
}

pub(super) fn validate_plugin_id(plugin_id: &str) -> Result<(), AgentLoopError> {
    if plugin_id.is_empty()
        || plugin_id.len() > MAX_PLUGIN_ID_BYTES
        || !plugin_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(AgentLoopError::InvalidConfiguration(
            "plugin id must be a bounded canonical name".to_owned(),
        ));
    }
    Ok(())
}

impl SessionHandle {
    /// Stable id of the session routed by this handle.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Activates newly stored provider credentials in the running model
    /// runtime without restarting the actor or application.
    ///
    /// # Errors
    ///
    /// Returns a sanitized model-runtime error if activation fails.
    pub async fn activate_provider(
        &self,
        provider: &str,
        selected_model: Option<&str>,
    ) -> Result<(), AgentLoopError> {
        self.model.activate_provider(provider, selected_model).await
    }

    /// Mints the narrow machine capability for one approved logical plugin.
    /// The capability cannot access protocol dispatch or the driver lease.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical plugin id.
    pub fn plugin_session_capability(
        &self,
        plugin_id: impl Into<String>,
    ) -> Result<PluginSessionCapability, AgentLoopError> {
        let plugin_id = plugin_id.into();
        validate_plugin_id(&plugin_id)?;
        Ok(PluginSessionCapability {
            commands: self.commands.clone(),
            plugin_id,
        })
    }

    /// Current durable event-log tail used by host reconnect completion.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the durable sink cannot read its tail.
    pub async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.event_sink.last_sequence().await
    }

    /// Returns the exact slash-command catalog assembled for this live
    /// session, including project commands, skills, MCP prompts, and plugins.
    ///
    #[must_use]
    pub fn command_descriptors(&self) -> Arc<[CommandDescriptor]> {
        self.command_descriptors
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Current mode registry, including trusted declarative modes activated by
    /// workspace-generation changes.
    #[must_use]
    pub fn mode_registry(&self) -> Arc<ModeRegistry> {
        self.mode_registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn local_meta(&self) -> CommandMeta {
        let request = self.local_request_sequence.fetch_add(1, Ordering::Relaxed);
        CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("local".to_owned()),
            request_id: RequestId(format!("local-{request}")),
        }
    }

    pub(super) async fn ensure_local_driver(&self) -> Result<(), AgentLoopError> {
        if self.local_attached.load(Ordering::Acquire) {
            return Ok(());
        }
        let outcome = self
            .dispatch(ClientCommand::AttachSession {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                last_seen_sequence: self.local_last_seen,
                role: ClientRole::Driver,
            })
            .await?;
        match outcome {
            CommandOutcome::Accepted => {
                self.local_attached.store(true, Ordering::Release);
                Ok(())
            }
            CommandOutcome::Rejected { error }
                if matches!(
                    error.code.as_str(),
                    "session_persistence_failure" | "gap_replay_failed" | "invalid_gap_replay"
                ) =>
            {
                Err(AgentLoopError::Persistence(error.message))
            }
            CommandOutcome::Rejected { error } => Err(AgentLoopError::InvalidConfiguration(
                format!("local driver attach rejected: {}", error.message),
            )),
        }
    }

    /// Dispatches the canonical protocol command to this session actor. The
    /// returned outcome is also emitted as a targeted, connection-scoped
    /// [`EngineEvent::CommandAcknowledged`] on this handle's event channel.
    ///
    /// # Errors
    ///
    /// Returns [`AgentLoopError::Closed`] if the actor is unavailable.
    pub async fn dispatch(&self, command: ClientCommand) -> Result<CommandOutcome, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::Protocol {
                command,
                respond,
                completion: None,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)
    }

    /// Dispatches one protocol command and waits for its durable work to
    /// finish after the immediate acknowledgement. Host-level projections
    /// use this boundary when a command's committed state must be persisted
    /// elsewhere before the command is reported complete.
    pub(crate) async fn dispatch_durably(
        &self,
        command: ClientCommand,
    ) -> Result<CommandOutcome, AgentLoopError> {
        self.dispatch_wait(command).await?;
        Ok(CommandOutcome::Accepted)
    }

    /// Persists a parent-owned child invocation through the parent actor's
    /// single-writer journal.
    ///
    /// # Errors
    ///
    /// Returns when the parent actor is closed or its journal append fails.
    pub async fn record_subagent_spawned(
        &self,
        subagent_id: SubagentId,
        child_session_id: SessionId,
        task: String,
    ) -> Result<(), AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::RecordSubagentSpawned {
                subagent_id,
                child_session_id,
                task,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Persists a terminal parent-owned child invocation through the parent
    /// actor's single-writer journal.
    ///
    /// # Errors
    ///
    /// Returns when the parent actor is closed or its journal append fails.
    pub async fn record_subagent_finished(
        &self,
        result: rw_types::SubagentResult,
    ) -> Result<(), AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::RecordSubagentFinished { result, respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Broadcasts display-only child progress without appending it to the
    /// parent journal.
    ///
    /// # Errors
    ///
    /// Returns when the parent actor is closed.
    pub async fn publish_subagent_progress_batch(
        &self,
        progress: Vec<SubagentProgressEvent>,
    ) -> Result<(), AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::PublishSubagentProgressBatch { progress, respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    /// Completes a foreground shell on behalf of the trusted CLI TTY broker.
    ///
    /// This is deliberately not a client protocol dispatch: the broker owns
    /// the real terminal but never takes the interactive driver's lease. The
    /// actor still validates the engine-generated shell id and persists the
    /// inactive event before releasing the turn gate.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor is closed, the shell id is stale, the
    /// captured output exceeds the durable limit, or persistence fails.
    pub async fn complete_user_shell(
        &self,
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
    ) -> Result<(), AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::CompleteUserShell {
                shell_id,
                status,
                captured_output,
                respond,
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }

    async fn dispatch_wait(
        &self,
        command: ClientCommand,
    ) -> Result<ProtocolCompletion, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        let (complete, completed) = oneshot::channel();
        self.commands
            .send(ActorCommand::Protocol {
                command,
                respond,
                completion: Some(complete),
            })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        match receive.await.map_err(|_| AgentLoopError::Closed)? {
            CommandOutcome::Accepted => completed.await.map_err(|_| AgentLoopError::Closed)?,
            CommandOutcome::Rejected { error } => {
                Err(AgentLoopError::InvalidConfiguration(error.message))
            }
        }
    }

    /// Subscribes to sequenced actor events from a captured durable prefix.
    ///
    /// # Errors
    /// Rejects an unavailable read view or a cursor beyond its committed tail.
    pub fn subscribe(&self) -> Result<SessionSubscription, AgentLoopError> {
        self.subscribe_client(ClientId("local".to_owned()), self.local_last_seen)
    }

    /// Subscribes one protocol client, optionally starting after a previously
    /// observed durable sequence. Captures the prefix before returning.
    ///
    /// # Errors
    /// Rejects an unavailable read view or a cursor beyond its committed tail.
    pub fn subscribe_client(
        &self,
        client_id: ClientId,
        last_sequence: Option<SequenceId>,
    ) -> Result<SessionSubscription, AgentLoopError> {
        let receiver = self.events.subscribe();
        let replay = self.event_sink.capture_read_view()?;
        let initial_tail = replay.last_sequence();
        if last_sequence.is_some_and(|last| initial_tail.is_none_or(|tail| last > tail)) {
            return Err(AgentLoopError::ReplayCursorAhead);
        }
        Ok(SessionSubscription {
            client_id,
            session_id: self.session_id.clone(),
            receiver,
            sink: Arc::clone(&self.event_sink),
            last_sequence,
            initial_tail,
            pending: VecDeque::new(),
            replay: Some(replay),
            needs_initial_replay: true,
        })
    }

    /// Starts a turn, queues a mid-turn message, or dispatches a slash command.
    ///
    /// # Errors
    ///
    /// Returns actor, extension, or persistence failures.
    pub async fn send_message(
        &self,
        content: impl Into<String>,
    ) -> Result<MessageDisposition, AgentLoopError> {
        self.ensure_local_driver().await?;
        let content = content.into();
        match self
            .dispatch_wait(ClientCommand::SendMessage {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                content,
                attachments: Vec::new(),
            })
            .await?
        {
            ProtocolCompletion::Message(disposition) => Ok(disposition),
            _ => Err(AgentLoopError::Closed),
        }
    }

    /// Cooperatively interrupts the active provider/tool future.
    ///
    /// # Errors
    ///
    /// Returns [`AgentLoopError::Closed`] if the actor is unavailable.
    pub async fn interrupt(&self) -> Result<bool, AgentLoopError> {
        self.ensure_local_driver().await?;
        let target_turn = self.active_turn.load(Ordering::Acquire);
        if target_turn == 0 {
            return Ok(false);
        }
        Ok(matches!(
            self.dispatch(ClientCommand::Interrupt {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
            })
            .await?,
            CommandOutcome::Accepted
        ))
    }

    /// Answers one pending ask-tier permission request.
    ///
    /// # Errors
    ///
    /// Returns [`AgentLoopError::Closed`] if the actor is unavailable.
    pub async fn approve(
        &self,
        request_id: impl Into<String>,
        decision: ApprovalDecision,
    ) -> Result<bool, AgentLoopError> {
        self.approve_bound(request_id, decision, None).await
    }

    /// Answers one pending ask-tier permission request with the exact binding
    /// displayed by the client. Diff approvals require this method; generic
    /// approvals continue to use [`Self::approve`].
    ///
    /// # Errors
    ///
    /// Returns [`AgentLoopError::Closed`] if the actor is unavailable.
    pub async fn approve_bound(
        &self,
        request_id: impl Into<String>,
        decision: ApprovalDecision,
        binding: Option<ApprovalBinding>,
    ) -> Result<bool, AgentLoopError> {
        self.ensure_local_driver().await?;
        let target_turn = self.active_turn.load(Ordering::Acquire);
        if target_turn == 0 {
            return Ok(false);
        }
        Ok(matches!(
            self.dispatch(ClientCommand::ApproveTool {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                tool_call_id: ToolCallId(request_id.into()),
                decision,
                binding,
            })
            .await?,
            CommandOutcome::Accepted
        ))
    }

    /// Reviews the pending plan as the active local driver.
    ///
    /// # Errors
    ///
    /// Returns an actor or protocol error when the session is closed, the
    /// caller cannot acquire the driver lease, or no plan is pending.
    pub async fn review_plan(
        &self,
        decision: PlanDecision,
        revisions: Option<String>,
    ) -> Result<bool, AgentLoopError> {
        self.ensure_local_driver().await?;
        Ok(matches!(
            self.dispatch(ClientCommand::ApprovePlan {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                decision,
                revisions,
            })
            .await?,
            CommandOutcome::Accepted
        ))
    }

    /// Answers one pending protocol-routed `ask_user` question as the local
    /// driver.
    ///
    /// # Errors
    ///
    /// Returns actor or protocol rejection failures.
    pub async fn answer_question(
        &self,
        question_id: QuestionId,
        values: Vec<String>,
    ) -> Result<bool, AgentLoopError> {
        self.ensure_local_driver().await?;
        Ok(matches!(
            self.dispatch(ClientCommand::AnswerQuestion {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                question_id: question_id.clone(),
                answers: vec![Answer {
                    question_id,
                    values,
                }],
            })
            .await?,
            CommandOutcome::Accepted
        ))
    }

    /// Returns an actor-consistent snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`AgentLoopError::Closed`] if the actor is unavailable.
    pub async fn snapshot(&self) -> Result<SessionSnapshot, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::Snapshot { respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)
    }

    /// Returns the exact actor-consistent context inventory.
    ///
    /// # Errors
    ///
    /// Returns actor, persistence, or assembly failures.
    pub async fn context_snapshot(&self) -> Result<ContextSnapshot, AgentLoopError> {
        match self
            .dispatch_wait(ClientCommand::GetContext {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
            })
            .await?
        {
            ProtocolCompletion::Context(snapshot) => Ok(snapshot),
            _ => Err(AgentLoopError::Closed),
        }
    }

    /// Returns reconciled usage, cost, and budget state without requiring a provider.
    ///
    /// # Errors
    ///
    /// Returns actor or accounting-ledger failures.
    pub async fn cost_snapshot(&self) -> Result<CostSnapshot, AgentLoopError> {
        match self
            .dispatch_wait(ClientCommand::GetCost {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
            })
            .await?
        {
            ProtocolCompletion::Cost(snapshot) => Ok(*snapshot),
            _ => Err(AgentLoopError::Closed),
        }
    }

    /// Returns the exact provider-neutral assembled prompt for offline inspection.
    ///
    /// # Errors
    ///
    /// Returns actor, historical projection, or assembly failures.
    pub async fn dump_prompt(&self, turn_id: Option<TurnId>) -> Result<PromptDump, AgentLoopError> {
        match self
            .dispatch_wait(ClientCommand::DumpPrompt {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                turn_id,
            })
            .await?
        {
            ProtocolCompletion::Prompt(dump) => Ok(dump),
            _ => Err(AgentLoopError::Closed),
        }
    }

    /// Pins an assembled context item for future provider turns.
    ///
    /// # Errors
    ///
    /// Returns actor, validation, or persistence failures.
    pub async fn pin_context(&self, item_id: ContextItemId) -> Result<(), AgentLoopError> {
        self.ensure_local_driver().await?;
        match self
            .dispatch_wait(ClientCommand::PinContext {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                item_id,
            })
            .await?
        {
            ProtocolCompletion::Unit => Ok(()),
            _ => Err(AgentLoopError::Closed),
        }
    }

    /// Evicts an assembled context item from future provider turns.
    ///
    /// # Errors
    ///
    /// Returns actor, validation, or persistence failures.
    pub async fn evict_context(&self, item_id: ContextItemId) -> Result<(), AgentLoopError> {
        self.ensure_local_driver().await?;
        match self
            .dispatch_wait(ClientCommand::EvictContext {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                item_id,
            })
            .await?
        {
            ProtocolCompletion::Unit => Ok(()),
            _ => Err(AgentLoopError::Closed),
        }
    }

    /// Runs manual compaction while the session is idle.
    ///
    /// # Errors
    ///
    /// Returns actor, budget, provider, hook, or persistence failures.
    pub async fn compact(&self, instructions: Option<String>) -> Result<(), AgentLoopError> {
        self.ensure_local_driver().await?;
        match self
            .dispatch_wait(ClientCommand::Compact {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                instructions,
            })
            .await?
        {
            ProtocolCompletion::Unit => Ok(()),
            _ => Err(AgentLoopError::Closed),
        }
    }

    /// Rewinds workspace and conversation state to a completed agent turn.
    ///
    /// # Errors
    ///
    /// Returns an error for an active turn, unknown target, checkpoint failure,
    /// persistence failure, or a closed actor.
    pub async fn rewind(&self, to_turn: u64) -> Result<(), AgentLoopError> {
        self.ensure_local_driver().await?;
        match self
            .dispatch_wait(ClientCommand::Rewind {
                meta: self.local_meta(),
                session_id: self.session_id.clone(),
                target: RewindTarget::Turn {
                    turn_id: wire_turn_id(to_turn),
                },
            })
            .await?
        {
            ProtocolCompletion::Rewind(_unrestorable) => Ok(()),
            _ => Err(AgentLoopError::Closed),
        }
    }
}
fn interrupted_turn_recovery_events(recovered: &SessionRecoveredState) -> Vec<PendingEvent> {
    let Some(turn) = recovered.interrupted_turn else {
        return Vec::new();
    };
    let mut events = recovered
        .interrupted_tool_repairs
        .iter()
        .map(|repair| PendingEvent::ToolCallFinished {
            turn: repair.agent_turn,
            id: repair.tool_call_id.0.clone(),
            output: repair.output.clone(),
            is_error: true,
            index: repair.call_index,
        })
        .collect::<Vec<_>>();
    if let Some(tool_turn) = &recovered.interrupted_tool_turn {
        events.push(PendingEvent::ConversationTurnCommitted {
            agent_turn: turn,
            turn: tool_turn.clone(),
        });
    }
    events.push(PendingEvent::TurnFinished {
        turn,
        status: AgentTurnStatus::Interrupted,
        usage: SessionUsage::default(),
        cost: unavailable_cost(),
    });
    events
}

/// Rebuilds all mutable actor state from the authoritative journal after an
/// append error. A sink's default batch implementation may have committed a
/// prefix before returning an error, so retaining any in-memory mutations is
/// unsafe. The interrupted turn is durably closed before the actor accepts
/// more work.
pub(super) async fn recover_actor_from_journal(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    events: &broadcast::Sender<RoutedEvent>,
    active_turn: &Arc<AtomicU64>,
) -> Result<(), AgentLoopError> {
    if let Some(running) = &state.running {
        running.cancellation.cancel();
    }
    active_turn.store(0, Ordering::Release);
    for (_, pending) in std::mem::take(&mut state.pending_approvals) {
        let _ = pending.respond.send(ApprovalDecision::Deny);
    }
    for (_, pending) in std::mem::take(&mut state.pending_questions) {
        let _ = pending.respond.send(String::new());
    }

    let recovered = project_session_read_view(
        config.event_sink.capture_read_view()?,
        &config.session_id,
        &config.modes,
    )
    .await?;
    let client_roles = std::mem::take(&mut state.client_roles);
    *state = ActorState::recover(
        config.session_id.clone(),
        Arc::clone(&config.event_clock),
        &config.model_alias,
        config.thinking,
        &config.modes,
        &recovered,
    );
    state.client_roles = client_roles;

    if recovered.interrupted_compaction {
        emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::Error {
                message: "interrupted compaction was aborted during recovery".to_owned(),
            },
        )
        .await?;
    }
    if let Some(turn) = recovered.interrupted_turn {
        emit_batch(
            state,
            events,
            &config.event_sink,
            interrupted_turn_recovery_events(&recovered),
        )
        .await?;
        state.accounting.push(TurnAccounting {
            turn_id: wire_turn_id(turn),
            attribution: AccountingAttribution::Main,
            usage: SessionUsage::default().into(),
            cost: unavailable_cost(),
        });
        state.completed_turns = state.completed_turns.saturating_add(1);
        state.turn_ends.insert(turn, state.conversation.len());
    }
    Ok(())
}

async fn dispatch_lifecycle_hook(
    event: HookEvent,
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
) -> bool {
    let result = config
        .hooks
        .dispatch(
            event,
            json!({
                "session_id": config.session_id.0,
                "workspace": config.workspace_root,
            }),
        )
        .await;
    for failure in result.failures() {
        if emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::HookFailure {
                event: hook_event_name(event).to_owned(),
                hook_id: failure.hook_id().to_owned(),
                fail_closed: failure.policy() == HookFailurePolicy::FailClosed,
                message: config.secret_redactor.redact(&failure.error().to_string()),
            },
        )
        .await
        .is_err()
        {
            return false;
        }
    }
    result.completed()
}

async fn end_actor_session(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
) {
    let _ = dispatch_lifecycle_hook(HookEvent::SessionEnd, state, config, events).await;
    if let Err(error) = config.tools.end_session(&config.session_id).await {
        let _ = emit(
            state,
            events,
            &config.event_sink,
            PendingEvent::Error {
                message: config
                    .secret_redactor
                    .redact(&format!("session resource cleanup failed: {error}")),
            },
        )
        .await;
    }
}

#[allow(clippy::too_many_lines)]
async fn run_actor(
    config: SessionActorConfig,
    mut tool_context: ToolContext,
    mut commands: mpsc::Receiver<ActorCommand>,
    events: broadcast::Sender<RoutedEvent>,
    active_turn: Arc<AtomicU64>,
    command_descriptors: Arc<RwLock<Arc<[CommandDescriptor]>>>,
    mode_registry: Arc<RwLock<Arc<ModeRegistry>>>,
) {
    let mut state = ActorState::recover(
        config.session_id.clone(),
        Arc::clone(&config.event_clock),
        &config.model_alias,
        config.thinking,
        &config.modes,
        &config.recovered,
    );
    let interrupted_turn = config.recovered.interrupted_turn;
    let mut config = Arc::new(config);
    let (turn_signals, mut signals) = mpsc::unbounded_channel();
    if !config.startup_notifications.is_empty() {
        let startup_events = config.startup_notifications.iter().flat_map(|notice| {
            [
                PendingEvent::PluginStatusChanged {
                    plugin_id: notice.plugin_id.clone(),
                    status: notice.status.clone(),
                },
                PendingEvent::UiNotification {
                    plugin_id: notice.plugin_id.clone(),
                    title: notice.title.clone(),
                    message: notice.message.clone(),
                },
            ]
        });
        if emit_batch(
            &mut state,
            &events,
            &config.event_sink,
            startup_events.collect(),
        )
        .await
        .is_err()
        {
            end_actor_session(&mut state, &config, &events).await;
            return;
        }
    }
    if !dispatch_lifecycle_hook(HookEvent::SessionStart, &mut state, &config, &events).await {
        end_actor_session(&mut state, &config, &events).await;
        return;
    }
    if config.recovered.interrupted_compaction
        && emit(
            &mut state,
            &events,
            &config.event_sink,
            PendingEvent::Error {
                message: "interrupted compaction was aborted during recovery".to_owned(),
            },
        )
        .await
        .is_err()
    {
        end_actor_session(&mut state, &config, &events).await;
        return;
    }
    if let Some(turn) = interrupted_turn {
        let mut recovery_events = config
            .recovered
            .interrupted_tool_repairs
            .iter()
            .map(|repair| PendingEvent::ToolCallFinished {
                turn: repair.agent_turn,
                id: repair.tool_call_id.0.clone(),
                output: repair.output.clone(),
                is_error: true,
                index: repair.call_index,
            })
            .collect::<Vec<_>>();
        if let Some(tool_turn) = &config.recovered.interrupted_tool_turn {
            recovery_events.push(PendingEvent::ConversationTurnCommitted {
                agent_turn: turn,
                turn: tool_turn.clone(),
            });
        }
        recovery_events.push(PendingEvent::TurnFinished {
            turn,
            status: AgentTurnStatus::Interrupted,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
        });
        if emit_batch(&mut state, &events, &config.event_sink, recovery_events)
            .await
            .is_err()
        {
            end_actor_session(&mut state, &config, &events).await;
            return;
        }
        state.accounting.push(TurnAccounting {
            turn_id: wire_turn_id(turn),
            attribution: AccountingAttribution::Main,
            usage: SessionUsage::default().into(),
            cost: unavailable_cost(),
        });
        state.completed_turns = state.completed_turns.saturating_add(1);
        state.turn_ends.insert(turn, state.conversation.len());
    }
    if !state.queued.is_empty() {
        state.queued_positions.clear();
        let messages = state
            .queued
            .drain(..)
            .map(|content| (content, Vec::new()))
            .collect();
        if start_turn(
            &mut state,
            &config,
            &tool_context,
            &turn_signals,
            &events,
            messages,
            &active_turn,
        )
        .await
        .is_err()
        {
            end_actor_session(&mut state, &config, &events).await;
            return;
        }
    }
    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else {
                    if let Some(running) = &state.running {
                        running.cancellation.cancel();
                    }
                    break;
                };
                handle_actor_command(
                    command,
                    &mut state,
                    &mut config,
                    &mut tool_context,
                    &turn_signals,
                    &events,
                    &active_turn,
                    &command_descriptors,
                    &mode_registry,
                ).await;
            }
            signal = signals.recv() => {
                let Some(signal) = signal else { break; };
                if handle_turn_signal(
                    signal,
                    &mut state,
                    &config,
                    &tool_context,
                    &turn_signals,
                    &events,
                    &active_turn,
                ).await.is_err() {
                    // Signals already queued by the cancelled provider task
                    // describe the pre-recovery turn and must not be appended
                    // to the rebuilt journal state.
                    while signals.try_recv().is_ok() {}
                    if recover_actor_from_journal(&mut state, &config, &events, &active_turn)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }
    end_actor_session(&mut state, &config, &events).await;
}
pub(super) enum ActorCommand {
    Protocol {
        command: ClientCommand,
        respond: oneshot::Sender<CommandOutcome>,
        completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
    },
    CompleteUserShell {
        shell_id: ShellId,
        status: i32,
        captured_output: Option<String>,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    RecordSubagentSpawned {
        subagent_id: SubagentId,
        child_session_id: SessionId,
        task: String,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    RecordSubagentFinished {
        result: rw_types::SubagentResult,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    PublishSubagentProgressBatch {
        progress: Vec<SubagentProgressEvent>,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    PluginInjectMessage {
        plugin_id: String,
        content: String,
        respond: oneshot::Sender<Result<MessageDisposition, AgentLoopError>>,
    },
    PluginSetStatus {
        plugin_id: String,
        status: String,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    PluginNotify {
        plugin_id: String,
        title: String,
        message: String,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    SendMessage {
        command_meta: CommandMeta,
        content: String,
        attachments: Vec<Attachment>,
        observed_turn: u64,
        respond: oneshot::Sender<Result<MessageDisposition, AgentLoopError>>,
    },
    #[cfg(test)]
    Interrupt {
        target_turn: u64,
        respond: oneshot::Sender<bool>,
    },
    Snapshot {
        respond: oneshot::Sender<SessionSnapshot>,
    },
}

pub(super) enum ProtocolCompletion {
    Message(MessageDisposition),
    Rewind(Vec<UnrestorablePath>),
    Context(ContextSnapshot),
    Cost(Box<CostSnapshot>),
    Prompt(PromptDump),
    Unit,
}

#[allow(clippy::struct_excessive_bools)]
pub(super) struct ActorState {
    pub(super) session_id: SessionId,
    pub(super) session_title: Option<String>,
    pub(super) title_generation_started: bool,
    pub(super) event_clock: Arc<dyn EventClock>,
    pub(super) conversation: Vec<Turn>,
    pub(super) queued: VecDeque<String>,
    pub(super) queued_positions: VecDeque<u64>,
    pub(super) running: Option<RunningTurn>,
    pub(super) pending_approvals: BTreeMap<String, PendingApproval>,
    pub(super) next_turn: u64,
    pub(super) completed_turns: u64,
    pub(super) turn_ends: BTreeMap<u64, usize>,
    pub(super) sequence: Option<u64>,
    pub(super) pending_rewind: Option<(u64, RewindCheckpoint)>,
    pub(super) transient_cause: Option<RequestId>,
    pub(super) poisoned: bool,
    pub(super) driver_client_id: Option<ClientId>,
    pub(super) client_roles: BTreeMap<String, ClientRole>,
    pub(super) pending_questions: BTreeMap<String, PendingQuestion>,
    pub(super) pending_model_switches: BTreeMap<String, PendingModelSwitch>,
    pub(super) next_question: u64,
    pub(super) context_surgery: Vec<ContextSurgeryAction>,
    pub(super) pruned_tool_outputs: BTreeMap<String, u64>,
    pub(super) accounting: Vec<TurnAccounting>,
    pub(super) budgeter: Budgeter,
    pub(super) model_alias: String,
    pub(super) provider: Option<String>,
    pub(super) thinking: ThinkingLevel,
    pub(super) mode: SessionMode,
    pub(super) mode_id: ModeId,
    pub(super) pending_plan: Option<PlanArtifact>,
    pub(super) approved_plan: Option<PlanArtifact>,
    pub(super) plan_gate_active: bool,
    pub(super) active_shell: Option<RecoveredUserShell>,
    pub(super) initialization_running: bool,
}

pub(super) struct PendingQuestion {
    pub(super) turn: u64,
    pub(super) respond: oneshot::Sender<String>,
}

pub(super) enum PrecommittedAnswer {
    Turn(PendingQuestion, String),
    Model(PendingModelSwitch, ModelContextTransfer),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingModelSwitch {
    pub(super) turn: u64,
    pub(super) model: ModelAlias,
    pub(super) provider: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PreparedModelSwitch {
    pub(super) model: ModelAlias,
    pub(super) provider: Option<String>,
    pub(super) thinking: ThinkingLevel,
}

pub(super) struct PendingApproval {
    pub(super) respond: oneshot::Sender<ApprovalDecision>,
    pub(super) binding: Option<ApprovalBinding>,
    pub(super) request: PermissionRequest,
    pub(super) turn: u64,
}

impl ActorState {
    fn recover(
        session_id: SessionId,
        event_clock: Arc<dyn EventClock>,
        default_model_alias: &str,
        default_thinking: ThinkingLevel,
        modes: &ModeRegistry,
        recovered: &SessionRecoveredState,
    ) -> Self {
        let pending_model_switches = recovered
            .pending_questions
            .iter()
            .filter_map(|(question_id, recovered)| {
                recovered
                    .questions
                    .iter()
                    .find_map(|question| question.model_switch.as_ref())
                    .map(|target| {
                        (
                            question_id.clone(),
                            PendingModelSwitch {
                                turn: recovered.agent_turn,
                                model: target.model.clone(),
                                provider: target.provider.clone(),
                            },
                        )
                    })
            })
            .collect();
        let queued_positions = recovered
            .queued_messages
            .iter()
            .enumerate()
            .map(|(index, _)| {
                recovered
                    .queued_message_positions
                    .get(index)
                    .copied()
                    .unwrap_or_else(|| u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1))
            })
            .collect();
        let mode_id = recovered
            .mode_id
            .clone()
            .unwrap_or_else(|| ModeId(session_mode_name(recovered.mode).to_owned()));
        let mode = modes
            .get(&mode_id.0)
            .map_or(recovered.mode, mode_permission_base);
        Self {
            session_id,
            session_title: recovered.title.clone(),
            title_generation_started: recovered.title.is_some(),
            event_clock,
            conversation: recovered.conversation.clone(),
            queued: recovered.queued_messages.iter().cloned().collect(),
            queued_positions,
            running: None,
            pending_approvals: BTreeMap::new(),
            next_turn: recovered
                .next_turn
                .max(recovered.completed_turns.saturating_add(1))
                .max(1),
            completed_turns: recovered.completed_turns,
            turn_ends: recovered.turn_ends.clone(),
            sequence: recovered.last_sequence.map(|sequence| sequence.0),
            pending_rewind: None,
            transient_cause: None,
            poisoned: false,
            driver_client_id: recovered.driver_client_id.clone(),
            client_roles: BTreeMap::new(),
            pending_questions: BTreeMap::new(),
            pending_model_switches,
            next_question: 0,
            context_surgery: recovered.context_surgery.clone(),
            pruned_tool_outputs: recovered.pruned_tool_outputs.clone(),
            accounting: recovered.accounting.clone(),
            budgeter: recovered.budgeter,
            model_alias: recovered
                .model_alias
                .clone()
                .unwrap_or_else(|| default_model_alias.to_owned()),
            provider: recovered.provider.clone(),
            thinking: recovered.thinking.unwrap_or(default_thinking),
            mode,
            mode_id,
            pending_plan: recovered.pending_plan.clone(),
            approved_plan: recovered.approved_plan.clone(),
            plan_gate_active: recovered.plan_gate_active,
            active_shell: recovered.active_shell.clone(),
            initialization_running: false,
        }
    }

    pub(super) fn caused_by(&self) -> Option<RequestId> {
        self.transient_cause.clone().or_else(|| {
            self.running
                .as_ref()
                .and_then(|running| running.caused_by.clone())
        })
    }
}
