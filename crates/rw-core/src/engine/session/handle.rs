use crate::engine::AgentLoopError;
use crate::engine::MessageDisposition;
use crate::engine::RoutedEvent;
use crate::engine::SessionSnapshot;
use crate::engine::durability::SessionEventSink;
use crate::engine::model::ModelDriver;
use crate::engine::session::plugin_capability::PluginSessionCapability;
use crate::engine::session::plugin_capability::validate_plugin_id;
use crate::engine::session::state::ActorCommand;
use crate::engine::session::state::ProtocolCompletion;
use crate::engine::session::subscription::SessionSubscription;
use crate::engine::shutdown;
use crate::engine::wire_turn_id;
use rw_ext::CommandDescriptor;
use rw_ext::ModeRegistry;
use rw_tools::SubagentProgressEvent;
use rw_types::Answer;
use rw_types::ApprovalBinding;
use rw_types::ApprovalDecision;
use rw_types::ClientCommand;
use rw_types::ClientId;
use rw_types::ClientRole;
use rw_types::CommandMeta;
use rw_types::CommandOutcome;
use rw_types::ContextItemId;
use rw_types::ContextSnapshot;
use rw_types::CostSnapshot;
use rw_types::EngineEvent;
use rw_types::PROTOCOL_VERSION;
use rw_types::PlanDecision;
use rw_types::PromptDump;
use rw_types::QuestionId;
use rw_types::RequestId;
use rw_types::RewindTarget;
use rw_types::SequenceId;
use rw_types::SessionId;
use rw_types::ShellId;
use rw_types::SubagentId;
use rw_types::ToolCallId;
use rw_types::TurnId;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Cloneable command/event boundary for one session actor.
#[derive(Clone)]
pub struct SessionHandle {
    pub(super) child_progress: Arc<super::child_progress::HostedChildProgress>,
    pub(super) shutdown: shutdown::ActorShutdown,
    pub(in crate::engine) commands: mpsc::Sender<ActorCommand>,
    pub(super) events: broadcast::Sender<RoutedEvent>,
    pub(in crate::engine) active_turn: Arc<AtomicU64>,
    pub(super) session_id: SessionId,
    pub(in crate::engine) event_sink: Arc<dyn SessionEventSink>,
    pub(super) local_request_sequence: Arc<AtomicU64>,
    pub(super) local_attached: Arc<AtomicBool>,
    pub(super) local_last_seen: Option<SequenceId>,
    pub(super) command_descriptors: Arc<RwLock<Arc<[CommandDescriptor]>>>,
    pub(super) mode_registry: Arc<RwLock<Arc<ModeRegistry>>>,
    pub(super) model: Arc<dyn ModelDriver>,
}

impl SessionHandle {
    /// Closes admission and waits for the actor's owned effect settlement.
    ///
    /// # Errors
    /// Returns a sticky error while unproven resource owners remain quarantined.
    pub async fn close(&self) -> Result<(), AgentLoopError> {
        self.shutdown.close().await
    }
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

    /// Returns a weak namespace binding for future host-owned extension generations.
    #[must_use]
    pub fn plugin_binding(&self) -> super::plugin_capability::PluginSessionBinding {
        super::plugin_capability::PluginSessionBinding {
            commands: self.commands.downgrade(),
            session_id: self.session_id.clone(),
        }
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

    pub(super) fn local_meta(&self) -> CommandMeta {
        let request = self.local_request_sequence.fetch_add(1, Ordering::Relaxed);
        CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("local".to_owned()),
            request_id: RequestId(format!("local-{request}")),
        }
    }

    pub(in crate::engine) async fn ensure_local_driver(&self) -> Result<(), AgentLoopError> {
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
            CommandOutcome::Accepted {} => {
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
        if matches!(
            command,
            ClientCommand::GetContext { .. }
                | ClientCommand::DumpPrompt { .. }
                | ClientCommand::GetCost { .. }
        ) {
            return Err(AgentLoopError::InvalidConfiguration(
                "inspection requests require an owned read result".into(),
            ));
        }
        if let ClientCommand::Interrupt { meta, session_id } = &command {
            return Ok(self
                .shutdown
                .control
                .interrupt(meta, session_id, &self.events));
        }
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
        Ok(CommandOutcome::Accepted {})
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
        self.child_progress
            .register(&subagent_id, &child_session_id)?;
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
        let child = result.subagent_id.clone();
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::RecordSubagentFinished { result, respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)??;
        self.child_progress.finish(&child);
        Ok(())
    }

    /// Publishes a bounded child observation. Saturated display delivery is
    /// coalesced into a canonical source invalidation without delaying effects.
    ///
    /// # Errors
    /// Returns for invalid progress or a child without an active durable spawn.
    pub fn publish_subagent_progress(
        &self,
        progress: SubagentProgressEvent,
    ) -> Result<(), AgentLoopError> {
        self.child_progress.publish(progress, &self.commands)
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

    pub(super) async fn dispatch_wait(
        &self,
        command: ClientCommand,
    ) -> Result<ProtocolCompletion, AgentLoopError> {
        if matches!(command, ClientCommand::Interrupt { .. }) {
            return match self.dispatch(command).await? {
                CommandOutcome::Accepted {} => Ok(ProtocolCompletion::Unit),
                CommandOutcome::Rejected { error } => {
                    Err(AgentLoopError::InvalidConfiguration(error.message))
                }
            };
        }
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
            CommandOutcome::Accepted {} => completed.await.map_err(|_| AgentLoopError::Closed)?,
            CommandOutcome::Rejected { error } => {
                Err(AgentLoopError::InvalidConfiguration(error.message))
            }
        }
    }

    /// Complete one admitted host inspection without broadcasting its decoded payload.
    pub(crate) async fn read_inspection(
        &self,
        command: ClientCommand,
        meta: rw_types::CommandAckMeta,
    ) -> Result<crate::recovery::HistoryRead<EngineEvent>, AgentLoopError> {
        let session_id = self.session_id.clone();
        match self.dispatch_wait(command).await? {
            ProtocolCompletion::Context(snapshot) => {
                Ok(snapshot.map(|snapshot| EngineEvent::ContextSnapshotReady {
                    meta,
                    session_id,
                    snapshot,
                }))
            }
            ProtocolCompletion::Prompt(dump) => Ok(dump.map(|dump| EngineEvent::PromptDumpReady {
                meta,
                session_id,
                dump,
            })),
            ProtocolCompletion::Cost(snapshot) => Ok(crate::recovery::HistoryRead::new(
                EngineEvent::CostSnapshotReady {
                    meta,
                    session_id,
                    snapshot: *snapshot,
                },
                (),
            )),
            _ => Err(AgentLoopError::InvalidConfiguration(
                "request is not an actor inspection".into(),
            )),
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

    /// Subscribe after the source tail captured for this operation, excluding prior turns.
    /// # Errors
    /// Returns an invalid canonical source or unavailable event channel error.
    pub fn subscribe_live(&self) -> Result<SessionSubscription, AgentLoopError> {
        let receiver = self.events.subscribe();
        let source = self.event_sink.capture_read_view()?;
        let tail = source.last_sequence();
        Ok(SessionSubscription {
            client_id: ClientId("local".to_owned()),
            session_id: self.session_id.clone(),
            receiver,
            sink: Arc::clone(&self.event_sink),
            last_sequence: tail,
            initial_tail: tail,
            pending: VecDeque::new(),
            replay: None,
            needs_initial_replay: false,
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
            CommandOutcome::Accepted {}
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
        invocation_id: rw_types::ToolInvocationId,
        decision: ApprovalDecision,
    ) -> Result<bool, AgentLoopError> {
        self.approve_bound(request_id, invocation_id, decision, None)
            .await
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
        invocation_id: rw_types::ToolInvocationId,
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
                invocation_id,
                decision,
                binding,
            })
            .await?,
            CommandOutcome::Accepted {}
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
            CommandOutcome::Accepted {}
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
            CommandOutcome::Accepted {}
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
    pub async fn context_snapshot(
        &self,
    ) -> Result<crate::recovery::HistoryRead<ContextSnapshot>, AgentLoopError> {
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
    pub async fn dump_prompt(
        &self,
        turn_id: Option<TurnId>,
    ) -> Result<crate::recovery::HistoryRead<PromptDump>, AgentLoopError> {
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

impl SessionHandle {
    /// Reads a bounded live catalog without running plugin code.
    /// # Errors
    /// Returns actor closure or registry admission errors.
    pub async fn ui_catalog(&self) -> Result<rw_types::extension_ui::UiCatalog, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::UiCatalog { respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }
    /// Reads the coalesced panel surfaces without running plugin code.
    /// # Errors
    /// Returns actor closure or registry admission errors.
    pub async fn ui_panels(&self) -> Result<rw_types::extension_ui::UiPanels, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::UiPanels { respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }
}

impl SessionHandle {
    /// Snapshot admitted by the host's bounded direct-read owner. This never replays history.
    pub(crate) async fn controls(
        &self,
    ) -> Result<rw_types::session_controls::SessionControlsSnapshot, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::Controls { respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }
}

impl SessionHandle {
    /// This query requires the host's admitted direct-read owner.
    pub(crate) async fn live_state(
        &self,
    ) -> Result<rw_types::session_state::SessionStateSnapshot, AgentLoopError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(ActorCommand::LiveState { respond })
            .await
            .map_err(|_| AgentLoopError::Closed)?;
        receive.await.map_err(|_| AgentLoopError::Closed)?
    }
}
