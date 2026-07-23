#[allow(clippy::wildcard_imports)]
use super::*;

/// Persisted actor state supplied when resuming a session from its event log.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRecoveredState {
    pub title: Option<String>,
    pub conversation: Vec<Turn>,
    pub queued_messages: Vec<String>,
    pub queued_message_positions: Vec<u64>,
    pub completed_turns: u64,
    pub next_turn: u64,
    pub last_sequence: Option<SequenceId>,
    pub interrupted_turn: Option<u64>,
    pub turn_ends: BTreeMap<u64, usize>,
    pub driver_client_id: Option<ClientId>,
    pub interrupted_tool_repairs: Vec<InterruptedToolRepair>,
    pub interrupted_tool_turn: Option<Turn>,
    pub pending_questions: BTreeMap<String, RecoveredQuestion>,
    pub context_surgery: Vec<ContextSurgeryAction>,
    pub pruned_tool_outputs: BTreeMap<String, u64>,
    pub accounting: Vec<TurnAccounting>,
    pub budgeter: Budgeter,
    pub interrupted_compaction: bool,
    pub model_alias: Option<String>,
    pub provider: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub mode: SessionMode,
    pub mode_id: Option<ModeId>,
    pub permission_mode: Option<crate::HeadlessPermissionMode>,
    pub pending_plan: Option<PlanArtifact>,
    pub approved_plan: Option<PlanArtifact>,
    pub plan_gate_active: bool,
    pub active_shell: Option<RecoveredUserShell>,
    pub workspace_generation: u64,
    pub workspace_roots: Vec<rw_types::WorkspaceRootDescriptor>,
}

/// Durable foreground-shell gate reconstructed from the session log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveredUserShell {
    pub shell_id: ShellId,
    pub command: String,
}

/// Durable context surgery projected from pin/evict events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSurgeryAction {
    pub item_id: ContextItemId,
    pub pinned: bool,
    pub effective_after_agent_turn: u64,
}

/// Durable interactive question state reconstructed from `QuestionAsked` and
/// `QuestionAnswered` events.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveredQuestion {
    pub agent_turn: u64,
    pub question_id: QuestionId,
    pub questions: Vec<Question>,
}

/// Deterministic durable repair for a tool call that was committed by the
/// provider but had no terminal result when the process died.
#[derive(Clone, Debug, PartialEq)]
pub struct InterruptedToolRepair {
    pub agent_turn: u64,
    pub call_index: usize,
    pub tool_call_id: ToolCallId,
    pub output: ToolOutput,
}

/// A persisted event log cannot be projected safely.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionProjectionError {
    #[error("unsupported session event version {0}")]
    UnsupportedVersion(u16),
    #[error("session event sequence is not contiguous at {found}; expected {expected}")]
    NonContiguousSequence { expected: u64, found: u64 },
    #[error("event stream contains a connection-scoped command acknowledgement")]
    ConnectionScopedEvent,
    #[error("event session changed from {expected} to {found}")]
    SessionChanged { expected: String, found: String },
    #[error("invalid decimal turn id `{0}`")]
    InvalidTurnId(String),
    #[error("invalid durable user-shell transition: {0}")]
    InvalidShellTransition(String),
    #[error("unknown permission mode id `{0}` in durable session")]
    InvalidPermissionMode(String),
    #[error("unknown mode id `{0}` in durable session")]
    InvalidMode(String),
    #[error("durable mode definition `{0}` is missing its fingerprint or changed")]
    ModeDefinitionChanged(String),
    #[error("invalid durable workspace-root generation")]
    InvalidWorkspaceGeneration,
}

pub(super) fn parse_turn_id(turn_id: &TurnId) -> Result<u64, SessionProjectionError> {
    turn_id
        .0
        .parse()
        .map_err(|_| SessionProjectionError::InvalidTurnId(turn_id.0.clone()))
}

pub(super) fn review_hash_is_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(super) fn review_path_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4_096
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[allow(clippy::match_same_arms, clippy::too_many_lines)]
pub(super) fn recovered_pending_event(
    event: &EngineEvent,
) -> Result<Option<PendingEvent>, SessionProjectionError> {
    let pending = match event {
        EngineEvent::CommandAcknowledged { .. }
        | EngineEvent::SubagentProgress { .. }
        | EngineEvent::CompactionAttemptStarted { .. }
        | EngineEvent::CompactionTextDelta { .. }
        | EngineEvent::CompactionThinkingDelta { .. } => {
            return Err(SessionProjectionError::ConnectionScopedEvent);
        }
        EngineEvent::ContextSnapshotReady { .. }
        | EngineEvent::CostSnapshotReady { .. }
        | EngineEvent::PromptDumpReady { .. }
        | EngineEvent::SessionReplayCompleted { .. }
        | EngineEvent::SessionForked { .. }
        | EngineEvent::SessionExported { .. }
        | EngineEvent::SessionsListed { .. }
        | EngineEvent::SubagentsListed { .. }
        | EngineEvent::SubagentReplayBatch { .. }
        | EngineEvent::SubagentReplayCompleted { .. }
        | EngineEvent::SessionsSearchReady { .. }
        | EngineEvent::SessionReviewReady { .. }
        | EngineEvent::SessionReviewUpdated { .. }
        | EngineEvent::CommandDescriptorsListed { .. }
        | EngineEvent::ModesListed { .. }
        | EngineEvent::ModelsListed { .. }
        | EngineEvent::SettingsListed { .. }
        | EngineEvent::McpServersListed { .. }
        | EngineEvent::RuntimeServicesListed { .. }
        | EngineEvent::McpServerApprovalReviewed { .. }
        | EngineEvent::PermissionsListed { .. }
        | EngineEvent::ProviderAuthStarted { .. }
        | EngineEvent::ProviderConfigured { .. }
        | EngineEvent::ProviderAuthFinished { .. }
        | EngineEvent::ProviderActivationFinished { .. }
        | EngineEvent::WorkspaceFilesFound { .. }
        | EngineEvent::WorkspaceFilePreviewReady { .. }
        | EngineEvent::WorkspaceStatusReady { .. }
        | EngineEvent::WorkspaceDiffReady { .. }
        | EngineEvent::HostShutdown { .. } => {
            return Err(SessionProjectionError::ConnectionScopedEvent);
        }
        EngineEvent::TurnStarted { turn_id, .. } => PendingEvent::TurnStarted {
            turn: parse_turn_id(turn_id)?,
        },
        EngineEvent::MessageQueued {
            position,
            content,
            attachments,
            ..
        } => PendingEvent::MessageQueued {
            position: *position,
            content: content.clone(),
            attachments: attachments.clone(),
        },
        EngineEvent::QueuedMessageRemoved { position, .. } => PendingEvent::QueuedMessageRemoved {
            position: *position,
        },
        EngineEvent::QueuedMessagesCleared { .. } => PendingEvent::QueuedMessagesCleared,
        EngineEvent::UserMessageAccepted {
            agent_turn,
            content,
            attachments,
            ..
        } => PendingEvent::UserMessageAccepted {
            turn: *agent_turn,
            content: content.clone(),
            attachments: attachments.clone(),
        },
        EngineEvent::SessionTitleUpdated {
            title, usage, cost, ..
        } => PendingEvent::SessionTitleUpdated {
            title: title.clone(),
            usage: usage.as_ref().map(|usage| SessionUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            }),
            cost: cost.clone(),
        },
        EngineEvent::PluginMessageInjected {
            plugin_id,
            content,
            queued,
            ..
        } => PendingEvent::PluginMessageInjected {
            plugin_id: plugin_id.clone(),
            content: content.clone(),
            queued: *queued,
        },
        EngineEvent::PluginStatusChanged {
            plugin_id, status, ..
        } => PendingEvent::PluginStatusChanged {
            plugin_id: plugin_id.clone(),
            status: status.clone(),
        },
        EngineEvent::UiNotification {
            plugin_id,
            title,
            message,
            ..
        } => PendingEvent::UiNotification {
            plugin_id: plugin_id.clone(),
            title: title.clone(),
            message: message.clone(),
        },
        EngineEvent::ConversationTurnCommitted {
            agent_turn, turn, ..
        } => PendingEvent::ConversationTurnCommitted {
            agent_turn: *agent_turn,
            turn: turn.clone(),
        },
        EngineEvent::ConversationRewound {
            to_agent_turn,
            operation_id,
            unrestorable_paths,
            ..
        } => PendingEvent::ConversationRewound {
            to_turn: *to_agent_turn,
            operation_id: operation_id.clone(),
            unrestorable_paths: unrestorable_paths.clone(),
        },
        EngineEvent::TextDelta { turn_id, text, .. } => PendingEvent::TextDelta {
            turn: parse_turn_id(turn_id)?,
            text: text.clone(),
        },
        EngineEvent::ThinkingDelta {
            turn_id,
            text,
            signature,
            ..
        } => PendingEvent::ThinkingDelta {
            turn: parse_turn_id(turn_id)?,
            content: text.clone(),
            signature: signature.clone(),
        },
        EngineEvent::CitationDelta {
            turn_id,
            uri,
            title,
            ..
        } => PendingEvent::CitationDelta {
            turn: parse_turn_id(turn_id)?,
            uri: uri.clone(),
            title: title.clone(),
        },
        EngineEvent::ToolCallStarted {
            turn_id,
            tool_call_id,
            name,
            args,
            call_index,
            ..
        } => PendingEvent::ToolCallStarted {
            turn: parse_turn_id(turn_id)?,
            id: tool_call_id.0.clone(),
            name: name.clone(),
            arguments: args.clone(),
            index: usize::try_from(*call_index).unwrap_or(usize::MAX),
        },
        EngineEvent::ToolOutputDelta {
            turn_id,
            tool_call_id,
            stream,
            chunk,
            ..
        } => PendingEvent::ToolOutput {
            turn: parse_turn_id(turn_id)?,
            id: tool_call_id.0.clone(),
            stream: match stream {
                ToolOutputStream::Stdout => "stdout",
                ToolOutputStream::Stderr => "stderr",
            }
            .to_owned(),
            chunk: chunk.clone(),
        },
        EngineEvent::ToolDiffReady {
            turn_id,
            tool_call_id,
            diff,
            ..
        } => PendingEvent::ToolDiffReady {
            turn: parse_turn_id(turn_id)?,
            id: tool_call_id.0.clone(),
            diff: diff.clone(),
        },
        EngineEvent::ToolCallFinished {
            turn_id,
            tool_call_id,
            output,
            is_error,
            call_index,
            ..
        } => PendingEvent::ToolCallFinished {
            turn: parse_turn_id(turn_id)?,
            id: tool_call_id.0.clone(),
            output: output.clone(),
            is_error: *is_error,
            index: usize::try_from(*call_index).unwrap_or(usize::MAX),
        },
        EngineEvent::ToolApprovalNeeded {
            turn_id,
            tool_call_id,
            name,
            args,
            capabilities,
            diff,
            ..
        } => PendingEvent::PermissionRequested {
            turn: parse_turn_id(turn_id)?,
            request: PermissionRequest {
                id: tool_call_id.0.clone(),
                tool_name: name.clone(),
                arguments: args.clone(),
                capabilities: capabilities.clone(),
                approval_diff: diff.clone(),
            },
        },
        EngineEvent::QuestionAnswered {
            turn_id,
            question_id,
            answers,
            ..
        } => PendingEvent::QuestionAnswered {
            turn: parse_turn_id(turn_id)?,
            question_id: question_id.clone(),
            answers: answers.clone(),
        },
        EngineEvent::HookFailed {
            event,
            hook_id,
            fail_closed,
            message,
            ..
        } => PendingEvent::HookFailure {
            event: event.clone(),
            hook_id: hook_id.clone(),
            fail_closed: *fail_closed,
            message: message.clone(),
        },
        EngineEvent::CommandFinished {
            name,
            message,
            unrestorable_paths,
            ..
        } => PendingEvent::CommandFinished {
            name: name.clone(),
            message: message.clone(),
            unrestorable_paths: unrestorable_paths.clone(),
        },
        EngineEvent::GuardTriggered {
            turn_id,
            guard,
            message,
            ..
        } => PendingEvent::GuardTriggered {
            turn: parse_turn_id(turn_id)?,
            guard: guard.clone(),
            message: message.clone(),
        },
        EngineEvent::TurnFinished {
            turn_id,
            status,
            usage,
            cost,
            ..
        } => PendingEvent::TurnFinished {
            turn: parse_turn_id(turn_id)?,
            status: match status {
                TurnStatus::Completed => AgentTurnStatus::Completed,
                TurnStatus::Interrupted => AgentTurnStatus::Interrupted,
                TurnStatus::Failed => AgentTurnStatus::Failed,
                TurnStatus::MaxTurns => AgentTurnStatus::MaxTurns,
                TurnStatus::DoomLoop => AgentTurnStatus::DoomLoop,
                TurnStatus::BudgetExceeded => AgentTurnStatus::BudgetExceeded,
            },
            usage: SessionUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            },
            cost: cost.clone(),
        },
        EngineEvent::ContextUsageUpdated {
            turn_id,
            used_tokens,
            usable_tokens,
            reserved_tokens,
            context_window_known,
            context_window_reason,
            stable_prefix_hash,
            cache_hit_basis_points,
            estimated_input_tokens,
            provider_input_tokens,
            correction_millionths,
            ..
        } => PendingEvent::ContextUsage {
            turn: parse_turn_id(turn_id)?,
            used_tokens: *used_tokens,
            usable_tokens: *usable_tokens,
            reserved_tokens: *reserved_tokens,
            context_window_known: *context_window_known,
            context_window_reason: context_window_reason.clone(),
            stable_prefix_hash: stable_prefix_hash.clone(),
            cache_hit_basis_points: *cache_hit_basis_points,
            estimated_input_tokens: *estimated_input_tokens,
            provider_input_tokens: *provider_input_tokens,
            correction_millionths: *correction_millionths,
        },
        EngineEvent::BudgetStatusChanged {
            turn_id,
            level,
            scope,
            unit,
            current,
            limit,
            ..
        } => PendingEvent::BudgetStatus {
            turn: parse_turn_id(turn_id)?,
            level: level.clone(),
            scope: scope.clone(),
            unit: unit.clone(),
            current: *current,
            limit: *limit,
        },
        EngineEvent::CompactionStarted { reason, .. } => PendingEvent::CompactionStarted {
            reason: reason.clone(),
        },
        EngineEvent::CompactionAttemptFinished {
            summary_turn_id,
            usage,
            cost,
            ..
        } => PendingEvent::CompactionAttemptFinished {
            summary_turn: parse_turn_id(summary_turn_id)?,
            usage: SessionUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            },
            cost: cost.clone(),
        },
        EngineEvent::CompactionFinished {
            summary_turn_id,
            reclaimed_tokens,
            usage,
            cost,
            ..
        } => PendingEvent::CompactionFinished {
            summary_turn: parse_turn_id(summary_turn_id)?,
            reclaimed_tokens: *reclaimed_tokens,
            usage: usage.as_ref().map(|usage| SessionUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            }),
            cost: cost.clone(),
        },
        EngineEvent::CompactionFailed {
            summary_turn_id, ..
        } => PendingEvent::CompactionFailed {
            summary_turn: parse_turn_id(summary_turn_id)?,
        },
        EngineEvent::ToolOutputPruned {
            tool_call_id,
            reclaimed_tokens,
            ..
        } => PendingEvent::ToolOutputPruned {
            tool_call_id: tool_call_id.0.clone(),
            reclaimed_tokens: *reclaimed_tokens,
        },
        EngineEvent::ContextItemPinned {
            item_id,
            effective_after_agent_turn,
            ..
        } => PendingEvent::ContextItemPinned {
            item_id: item_id.clone(),
            effective_after_agent_turn: *effective_after_agent_turn,
        },
        EngineEvent::ContextItemEvicted {
            item_id,
            effective_after_agent_turn,
            ..
        } => PendingEvent::ContextItemEvicted {
            item_id: item_id.clone(),
            effective_after_agent_turn: *effective_after_agent_turn,
        },
        EngineEvent::DriverChanged {
            driver_client_id, ..
        } => PendingEvent::DriverChanged {
            driver_client_id: driver_client_id.clone(),
        },
        EngineEvent::SessionCreated {
            driver_client_id, ..
        } => PendingEvent::SessionCreated {
            driver_client_id: driver_client_id.clone(),
        },
        EngineEvent::WorkspaceRootsChanged {
            generation,
            effective_from_turn,
            roots,
            ..
        } => PendingEvent::WorkspaceRootsChanged {
            generation: *generation,
            effective_from_turn: *effective_from_turn,
            roots: roots.clone(),
        },
        EngineEvent::ModelChanged {
            model,
            provider,
            thinking,
            ..
        } => PendingEvent::ModelChanged {
            model: model.clone(),
            provider: provider.clone(),
            thinking: config_thinking_to_provider(thinking.unwrap_or_default()),
        },
        EngineEvent::ModelContextCleared { strategy, .. } => PendingEvent::ModelContextCleared {
            strategy: *strategy,
        },
        EngineEvent::ModeChanged {
            mode,
            definition_fingerprint,
            ..
        } => PendingEvent::ModeChanged {
            mode: mode.clone(),
            definition_fingerprint: definition_fingerprint.clone(),
        },
        EngineEvent::PermissionModeChanged { mode, .. } => PendingEvent::PermissionModeChanged {
            mode: mode.as_deref().map(parse_permission_mode).transpose()?,
        },
        EngineEvent::PlanSubmitted { artifact, .. } => PendingEvent::PlanSubmitted {
            artifact: artifact.clone(),
        },
        EngineEvent::PlanReviewed {
            artifact,
            decision,
            revisions,
            ..
        } => PendingEvent::PlanReviewed {
            artifact: artifact.clone(),
            decision: *decision,
            revisions: revisions.clone(),
        },
        EngineEvent::UserShellStateChanged {
            shell_id,
            command,
            active,
            status,
            captured_output,
            ..
        } => PendingEvent::UserShellStateChanged {
            shell_id: shell_id.clone(),
            command: command.clone().unwrap_or_default(),
            active: *active,
            status: *status,
            captured_output: captured_output.clone(),
        },
        EngineEvent::QuestionAsked {
            turn_id,
            question_id,
            questions,
            ..
        } => PendingEvent::QuestionAsked {
            turn: parse_turn_id(turn_id)?,
            question_id: question_id.clone(),
            questions: questions.clone(),
        },
        EngineEvent::Error { error, .. } => PendingEvent::Error {
            message: error.message.clone(),
        },
        EngineEvent::SubagentSpawned { .. } | EngineEvent::SubagentFinished { .. } => {
            return Ok(None);
        }
    };
    Ok(Some(pending))
}

/// Projects an ordered durable event log into actor resume state.
///
/// Full conversation commit events are authoritative. Accepted user messages
/// are retained as a fallback only when a crash occurred before their commit.
///
/// # Errors
///
/// Returns an error for unsupported versions or a non-contiguous sequence.
#[allow(clippy::match_same_arms, clippy::too_many_lines)]
pub fn project_session_events(
    events: &[EngineEvent],
) -> Result<SessionRecoveredState, SessionProjectionError> {
    project_session_events_resolving_mode(events, |mode, _recorded_fingerprint| {
        Ok(parse_session_mode(&mode.0).unwrap_or(SessionMode::Execute))
    })
}

/// Projects a session log with the exact runtime mode registry. Unlike the
/// protocol-only projector, this preserves custom permission floors and fails
/// closed when a durable custom mode is no longer registered.
///
/// # Errors
///
/// Returns the normal projection errors or [`SessionProjectionError::InvalidMode`]
/// when a durable mode id is absent from the supplied registry.
pub fn project_session_events_with_modes(
    events: &[EngineEvent],
    modes: &ModeRegistry,
) -> Result<SessionRecoveredState, SessionProjectionError> {
    project_session_events_resolving_mode(events, |mode, recorded_fingerprint| {
        let definition = modes
            .get(&mode.0)
            .ok_or_else(|| SessionProjectionError::InvalidMode(mode.0.clone()))?;
        match recorded_fingerprint {
            Some(recorded) if recorded == &definition.semantic_fingerprint() => {}
            None if matches!(mode.0.as_str(), "discuss" | "plan" | "execute") => {}
            _ => {
                return Err(SessionProjectionError::ModeDefinitionChanged(
                    mode.0.clone(),
                ));
            }
        }
        Ok(mode_permission_base(definition))
    })
}

#[allow(clippy::match_same_arms, clippy::too_many_lines)]
fn project_session_events_resolving_mode(
    events: &[EngineEvent],
    mut resolve_mode: impl FnMut(
        &ModeId,
        Option<&String>,
    ) -> Result<SessionMode, SessionProjectionError>,
) -> Result<SessionRecoveredState, SessionProjectionError> {
    let mut conversation = Vec::new();
    let mut title = None;
    let mut conversation_agent_turns = Vec::new();
    let mut queued = VecDeque::<(u64, String)>::new();
    let mut uncommitted_users = BTreeMap::<u64, Vec<String>>::new();
    let mut completed_turns = 0_u64;
    let mut active_turn = None;
    let mut turn_ends = BTreeMap::new();
    let mut partial_assistant_blocks = Vec::<Block>::new();
    let mut partial_tool_blocks = Vec::<Block>::new();
    let mut next_turn = 1_u64;
    let mut last_sequence = None;
    let mut driver_client_id = None;
    let mut session_id: Option<&SessionId> = None;
    let mut interrupted_tool_repairs = Vec::new();
    let mut interrupted_tool_turn = None;
    let mut pending_questions = BTreeMap::new();
    let mut context_surgery = Vec::new();
    let mut pruned_tool_outputs = BTreeMap::new();
    let mut accounting = Vec::new();
    let mut model_alias = None;
    let mut selected_provider = None;
    let mut selected_thinking = None;
    let mut mode = SessionMode::Execute;
    let mut mode_id = None;
    let mut permission_mode = None;
    let mut pending_plan = None;
    let mut approved_plan = None;
    let mut plan_gate_active = false;
    let mut turn_mode_states = BTreeMap::<
        u64,
        (
            SessionMode,
            Option<ModeId>,
            Option<PlanArtifact>,
            Option<PlanArtifact>,
            bool,
        ),
    >::new();
    let mut active_shell = None::<RecoveredUserShell>;
    let mut workspace_generation = 0_u64;
    let mut workspace_roots = Vec::new();
    let mut compacted_conversation = None::<Vec<(u64, Turn)>>;
    let mut compaction_surgery_start = None::<usize>;
    let mut budgeter = Budgeter::default();
    let mut rewind_archives = Vec::<(
        BTreeMap<u64, usize>,
        Vec<Turn>,
        Vec<u64>,
        Vec<ContextSurgeryAction>,
        BTreeMap<String, u64>,
        Budgeter,
    )>::new();
    for event in events {
        let meta = event_meta(event).ok_or(SessionProjectionError::ConnectionScopedEvent)?;
        if meta.protocol_version != SESSION_EVENT_VERSION {
            return Err(SessionProjectionError::UnsupportedVersion(
                meta.protocol_version,
            ));
        }
        if let Some(expected) = session_id {
            if expected != &meta.session_id {
                return Err(SessionProjectionError::SessionChanged {
                    expected: expected.0.clone(),
                    found: meta.session_id.0.clone(),
                });
            }
        } else {
            session_id = Some(&meta.session_id);
        }
        let expected = last_sequence.map_or(0, |sequence: SequenceId| sequence.0.saturating_add(1));
        if meta.sequence_id.0 != expected {
            return Err(SessionProjectionError::NonContiguousSequence {
                expected,
                found: meta.sequence_id.0,
            });
        }
        last_sequence = Some(meta.sequence_id);
        let Some(kind) = recovered_pending_event(event)? else {
            continue;
        };
        match &kind {
            PendingEvent::TurnStarted { turn } => {
                active_turn = Some(*turn);
                partial_assistant_blocks.clear();
                partial_tool_blocks.clear();
                next_turn = next_turn.max(turn.saturating_add(1));
            }
            PendingEvent::MessageQueued {
                position, content, ..
            } => queued.push_back((*position, content.clone())),
            PendingEvent::QueuedMessageRemoved { position } => {
                if let Some(index) = queued
                    .iter()
                    .position(|(queued_position, _)| queued_position == position)
                {
                    queued.remove(index);
                }
            }
            PendingEvent::QueuedMessagesCleared => queued.clear(),
            PendingEvent::UserMessageAccepted { turn, content, .. } => {
                if let Some(position) = queued
                    .iter()
                    .position(|(_, queued_content)| queued_content == content)
                {
                    queued.remove(position);
                }
                uncommitted_users
                    .entry(*turn)
                    .or_default()
                    .push(content.clone());
            }
            PendingEvent::SessionTitleUpdated {
                title: updated,
                usage,
                cost,
            } => {
                title = Some(updated.clone());
                if let (Some(usage), Some(cost)) = (usage, cost) {
                    accounting.push(TurnAccounting {
                        turn_id: TurnId("title".to_owned()),
                        attribution: AccountingAttribution::Title,
                        usage: (*usage).into(),
                        cost: cost.clone(),
                    });
                }
            }
            PendingEvent::PluginMessageInjected { .. }
            | PendingEvent::PluginStatusChanged { .. }
            | PendingEvent::UiNotification { .. } => {}
            PendingEvent::ConversationTurnCommitted { agent_turn, turn } => {
                if let Some(compacted) = &mut compacted_conversation {
                    compacted.push((*agent_turn, turn.clone()));
                    continue;
                }
                if turn.role == Role::User
                    && let Some(pending) = uncommitted_users.get_mut(agent_turn)
                    && !pending.is_empty()
                {
                    pending.remove(0);
                }
                conversation.push(turn.clone());
                conversation_agent_turns.push(*agent_turn);
                if turn.role == Role::Assistant {
                    partial_assistant_blocks.clear();
                } else if turn.role == Role::Tool {
                    partial_tool_blocks.clear();
                }
            }
            PendingEvent::ConversationRewound { to_turn, .. } => {
                if let Some((
                    ends,
                    restored,
                    restored_turns,
                    restored_surgery,
                    restored_pruned,
                    restored_budgeter,
                )) = rewind_archives
                    .iter()
                    .find(|(ends, ..)| ends.contains_key(to_turn))
                    .cloned()
                {
                    let retained = ends.get(to_turn).copied().unwrap_or_default();
                    conversation = restored.into_iter().take(retained).collect();
                    conversation_agent_turns = restored_turns.into_iter().take(retained).collect();
                    context_surgery = restored_surgery
                        .into_iter()
                        .filter(|action| action.effective_after_agent_turn <= *to_turn)
                        .collect();
                    pruned_tool_outputs = restored_pruned;
                    budgeter = restored_budgeter;
                } else {
                    let retained = conversation_agent_turns
                        .iter()
                        .take_while(|turn| **turn <= *to_turn)
                        .count();
                    conversation.truncate(retained);
                    conversation_agent_turns.truncate(retained);
                }
                turn_ends.retain(|turn, _| *turn <= *to_turn);
                queued.clear();
                uncommitted_users.retain(|turn, _| *turn <= *to_turn);
                if active_turn.is_some_and(|turn| turn > *to_turn) {
                    active_turn = None;
                    partial_assistant_blocks.clear();
                    partial_tool_blocks.clear();
                }
                completed_turns = u64::try_from(turn_ends.len()).unwrap_or(u64::MAX);
                if let Some((
                    restored_mode,
                    restored_mode_id,
                    restored_pending_plan,
                    restored_approved_plan,
                    restored_plan_gate,
                )) = turn_mode_states.get(to_turn).cloned()
                {
                    mode = restored_mode;
                    mode_id = restored_mode_id;
                    pending_plan = restored_pending_plan;
                    approved_plan = restored_approved_plan;
                    plan_gate_active = restored_plan_gate;
                }
                turn_mode_states.retain(|turn, _| *turn <= *to_turn);
                pending_questions
                    .retain(|_, question: &mut RecoveredQuestion| question.agent_turn <= *to_turn);
                context_surgery.retain(|action: &ContextSurgeryAction| {
                    action.effective_after_agent_turn <= *to_turn
                });
            }
            PendingEvent::TurnFinished {
                turn, usage, cost, ..
            } => {
                if active_turn == Some(*turn) {
                    active_turn = None;
                }
                completed_turns = completed_turns.saturating_add(1);
                next_turn = next_turn.max(turn.saturating_add(1));
                turn_ends.insert(*turn, conversation.len());
                pending_questions
                    .retain(|_, question: &mut RecoveredQuestion| question.agent_turn != *turn);
                accounting.push(TurnAccounting {
                    turn_id: wire_turn_id(*turn),
                    attribution: AccountingAttribution::Main,
                    usage: (*usage).into(),
                    cost: cost.clone(),
                });
                turn_mode_states.insert(
                    *turn,
                    (
                        mode,
                        mode_id.clone(),
                        pending_plan.clone(),
                        approved_plan.clone(),
                        plan_gate_active,
                    ),
                );
            }
            PendingEvent::TextDelta { turn, text } if active_turn == Some(*turn) => {
                append_text(&mut partial_assistant_blocks, text);
            }
            PendingEvent::ThinkingDelta {
                turn,
                content,
                signature,
            } if active_turn == Some(*turn) => {
                append_thinking(&mut partial_assistant_blocks, content, signature.clone());
            }
            PendingEvent::CitationDelta { turn, uri, title } if active_turn == Some(*turn) => {
                partial_assistant_blocks.push(Block::Citation {
                    uri: uri.clone(),
                    title: title.clone(),
                    excerpt: None,
                });
            }
            PendingEvent::ToolCallFinished {
                turn,
                id,
                output,
                is_error,
                ..
            } if active_turn == Some(*turn) => {
                partial_tool_blocks.push(Block::ToolResult {
                    id: ToolCallId(id.clone()),
                    output: output.clone(),
                    is_error: *is_error,
                });
            }
            PendingEvent::TextDelta { .. }
            | PendingEvent::ThinkingDelta { .. }
            | PendingEvent::CitationDelta { .. }
            | PendingEvent::ToolCallStarted { .. }
            | PendingEvent::PermissionRequested { .. }
            | PendingEvent::ToolDiffReady { .. }
            | PendingEvent::ToolOutput { .. }
            | PendingEvent::ToolCallFinished { .. }
            | PendingEvent::SubagentSpawned { .. }
            | PendingEvent::SubagentFinished { .. }
            | PendingEvent::HookFailure { .. }
            | PendingEvent::CommandFinished { .. }
            | PendingEvent::GuardTriggered { .. }
            | PendingEvent::BudgetStatus { .. } => {}
            PendingEvent::Error { .. } | PendingEvent::CompactionFailed { .. } => {
                compacted_conversation = None;
                if let Some(start) = compaction_surgery_start.take() {
                    context_surgery.truncate(start);
                }
            }
            PendingEvent::ContextUsage {
                estimated_input_tokens,
                provider_input_tokens,
                ..
            } if *estimated_input_tokens > 0 && *provider_input_tokens > 0 => {
                budgeter.reconcile(
                    *estimated_input_tokens,
                    TokenUsage {
                        input_tokens: *provider_input_tokens,
                        ..TokenUsage::default()
                    },
                );
            }
            PendingEvent::ContextUsage { .. } => {}
            PendingEvent::CompactionStarted { .. } => {
                rewind_archives.push((
                    turn_ends.clone(),
                    conversation.clone(),
                    conversation_agent_turns.clone(),
                    context_surgery.clone(),
                    pruned_tool_outputs.clone(),
                    budgeter,
                ));
                compacted_conversation = Some(Vec::new());
                compaction_surgery_start = Some(context_surgery.len());
            }
            PendingEvent::CompactionAttemptFinished {
                summary_turn,
                usage,
                cost,
            } => {
                accounting.push(TurnAccounting {
                    turn_id: wire_turn_id(*summary_turn),
                    attribution: AccountingAttribution::Compaction,
                    usage: (*usage).into(),
                    cost: cost.clone(),
                });
            }
            PendingEvent::CompactionFinished {
                summary_turn,
                usage: Some(usage),
                cost: Some(cost),
                ..
            } => {
                if let Some(compacted) = compacted_conversation.take() {
                    conversation = compacted.iter().map(|(_, turn)| turn.clone()).collect();
                    conversation_agent_turns = compacted
                        .iter()
                        .map(|(agent_turn, _)| *agent_turn)
                        .collect();
                }
                if let Some(start) = compaction_surgery_start.take() {
                    context_surgery.drain(..start);
                }
                accounting.push(TurnAccounting {
                    turn_id: wire_turn_id(*summary_turn),
                    attribution: AccountingAttribution::Compaction,
                    usage: (*usage).into(),
                    cost: cost.clone(),
                });
            }
            PendingEvent::CompactionFinished { .. } => {
                if let Some(compacted) = compacted_conversation.take() {
                    conversation = compacted.iter().map(|(_, turn)| turn.clone()).collect();
                    conversation_agent_turns = compacted
                        .iter()
                        .map(|(agent_turn, _)| *agent_turn)
                        .collect();
                }
                if let Some(start) = compaction_surgery_start.take() {
                    context_surgery.drain(..start);
                }
            }
            PendingEvent::ToolOutputPruned {
                tool_call_id,
                reclaimed_tokens,
            } => {
                pruned_tool_outputs.insert(tool_call_id.clone(), *reclaimed_tokens);
            }
            PendingEvent::ContextItemPinned {
                item_id,
                effective_after_agent_turn,
            } => context_surgery.push(ContextSurgeryAction {
                item_id: item_id.clone(),
                pinned: true,
                effective_after_agent_turn: *effective_after_agent_turn,
            }),
            PendingEvent::ContextItemEvicted {
                item_id,
                effective_after_agent_turn,
            } => context_surgery.push(ContextSurgeryAction {
                item_id: item_id.clone(),
                pinned: false,
                effective_after_agent_turn: *effective_after_agent_turn,
            }),
            PendingEvent::QuestionAsked {
                turn,
                question_id,
                questions,
            } => {
                pending_questions.insert(
                    question_id.0.clone(),
                    RecoveredQuestion {
                        agent_turn: *turn,
                        question_id: question_id.clone(),
                        questions: questions.clone(),
                    },
                );
            }
            PendingEvent::QuestionAnswered { question_id, .. } => {
                pending_questions.remove(&question_id.0);
            }
            PendingEvent::WorkspaceRootsChanged {
                generation, roots, ..
            } => {
                if *generation != workspace_generation.saturating_add(1)
                    || roots.is_empty()
                    || roots.iter().enumerate().any(|(index, root)| {
                        root.index != u32::try_from(index).unwrap_or(u32::MAX)
                            || root.machine_local
                            || root.path != format!("@root/{index}")
                    })
                    || (!workspace_roots.is_empty()
                        && roots
                            .iter()
                            .take(workspace_roots.len())
                            .ne(workspace_roots.iter()))
                    || (!workspace_roots.is_empty() && roots.len() != workspace_roots.len() + 1)
                {
                    return Err(SessionProjectionError::InvalidWorkspaceGeneration);
                }
                workspace_generation = *generation;
                workspace_roots.clone_from(roots);
            }
            PendingEvent::SessionCreated {
                driver_client_id: driver,
            }
            | PendingEvent::DriverChanged {
                driver_client_id: driver,
            } => {
                driver_client_id = Some(driver.clone());
            }
            PendingEvent::ModelChanged {
                model,
                provider,
                thinking,
            } => {
                model_alias = Some(model.0.clone());
                selected_provider.clone_from(provider);
                selected_thinking = Some(*thinking);
            }
            PendingEvent::ModelContextCleared { .. } => {
                let retained = conversation
                    .iter()
                    .zip(conversation_agent_turns.iter().copied())
                    .filter(|(turn, _)| turn.role == Role::System)
                    .map(|(turn, agent_turn)| (turn.clone(), agent_turn))
                    .collect::<Vec<_>>();
                conversation = retained.iter().map(|(turn, _)| turn.clone()).collect();
                conversation_agent_turns =
                    retained.iter().map(|(_, agent_turn)| *agent_turn).collect();
                context_surgery.clear();
                pruned_tool_outputs.clear();
            }
            PendingEvent::ModeChanged {
                mode: changed,
                definition_fingerprint,
            } => {
                mode_id = Some(changed.clone());
                mode = resolve_mode(changed, definition_fingerprint.as_ref())?;
                if mode == SessionMode::Plan {
                    pending_plan = None;
                    approved_plan = None;
                    plan_gate_active = true;
                }
            }
            PendingEvent::PermissionModeChanged { mode: changed } => {
                permission_mode = *changed;
            }
            PendingEvent::PlanSubmitted { artifact } => {
                pending_plan = Some(artifact.clone());
            }
            PendingEvent::PlanReviewed {
                artifact, decision, ..
            } => {
                pending_plan = None;
                if *decision == PlanDecision::Approve {
                    approved_plan = Some(artifact.clone());
                    plan_gate_active = false;
                }
            }
            PendingEvent::UserShellStateChanged {
                shell_id,
                command,
                active: true,
                status: None,
                captured_output: None,
            } => {
                if active_shell.is_some() {
                    return Err(SessionProjectionError::InvalidShellTransition(
                        "a second shell started while one was already active".to_owned(),
                    ));
                }
                active_shell = Some(RecoveredUserShell {
                    shell_id: shell_id.clone(),
                    command: command.clone(),
                });
            }
            PendingEvent::UserShellStateChanged {
                shell_id,
                command,
                active: false,
                status: Some(status),
                captured_output,
            } => {
                if active_shell.as_ref().map(|shell| &shell.shell_id) != Some(shell_id) {
                    return Err(SessionProjectionError::InvalidShellTransition(
                        "shell end did not match the active shell id".to_owned(),
                    ));
                }
                conversation.push(shell_context_turn(
                    command,
                    *status,
                    captured_output.as_deref(),
                ));
                active_shell = None;
            }
            PendingEvent::UserShellStateChanged { .. } => {
                return Err(SessionProjectionError::InvalidShellTransition(
                    "shell start must not carry terminal fields".to_owned(),
                ));
            }
        }
    }
    for messages in uncommitted_users.into_values() {
        for content in messages {
            conversation.push(Turn {
                role: Role::User,
                blocks: vec![Block::Text { text: content }],
                meta: TurnMeta::default(),
            });
        }
    }
    if let Some(interrupted_turn) = active_turn {
        let mut requested = Vec::<ToolCallId>::new();
        let mut finished = Vec::<ToolCallId>::new();
        for (turn, conversation_turn) in conversation_agent_turns.iter().zip(&conversation) {
            if *turn != interrupted_turn {
                continue;
            }
            for block in &conversation_turn.blocks {
                match block {
                    Block::ToolCall { id, .. } => requested.push(id.clone()),
                    Block::ToolResult { id, .. } => finished.push(id.clone()),
                    _ => {}
                }
            }
        }
        for block in &partial_tool_blocks {
            if let Block::ToolResult { id, .. } = block {
                finished.push(id.clone());
            }
        }
        for (call_index, id) in requested.into_iter().enumerate() {
            if !finished.contains(&id) {
                let output = ToolOutput::Text {
                    text: "tool call was interrupted before a result was persisted".to_owned(),
                };
                interrupted_tool_repairs.push(InterruptedToolRepair {
                    agent_turn: interrupted_turn,
                    call_index,
                    tool_call_id: id.clone(),
                    output: output.clone(),
                });
                partial_tool_blocks.push(Block::ToolResult {
                    id,
                    output,
                    is_error: true,
                });
            }
        }
        if !partial_tool_blocks.is_empty() {
            let tool_turn = Turn {
                role: Role::Tool,
                blocks: partial_tool_blocks,
                meta: TurnMeta::default(),
            };
            conversation.push(tool_turn.clone());
            interrupted_tool_turn = Some(tool_turn);
        }
        if !partial_assistant_blocks.is_empty() {
            conversation.push(Turn {
                role: Role::Assistant,
                blocks: partial_assistant_blocks,
                meta: TurnMeta::default(),
            });
        }
    }
    let interrupted_compaction = compacted_conversation.is_some();
    Ok(SessionRecoveredState {
        title,
        conversation,
        queued_messages: queued.iter().map(|(_, content)| content.clone()).collect(),
        queued_message_positions: queued.iter().map(|(position, _)| *position).collect(),
        completed_turns,
        next_turn,
        last_sequence,
        interrupted_turn: active_turn,
        turn_ends,
        driver_client_id,
        interrupted_tool_repairs,
        interrupted_tool_turn,
        pending_questions,
        context_surgery,
        pruned_tool_outputs,
        accounting,
        budgeter,
        interrupted_compaction,
        model_alias,
        provider: selected_provider,
        thinking: selected_thinking,
        mode,
        mode_id,
        permission_mode,
        pending_plan,
        approved_plan,
        plan_gate_active,
        active_shell,
        workspace_generation,
        workspace_roots,
    })
}

pub(super) fn shell_context_turn(
    command: &str,
    status: i32,
    captured_output: Option<&str>,
) -> Turn {
    let mut text = format!(
        "Foreground shell command (user-provided context):\n$ {command}\nExit status: {status}"
    );
    if let Some(output) = captured_output.filter(|output| !output.is_empty()) {
        text.push_str("\nOutput:\n");
        text.push_str(output);
    }
    Turn {
        role: Role::User,
        blocks: vec![Block::Text { text }],
        meta: TurnMeta::default(),
    }
}

pub(super) fn plan_review_context_turn(
    artifact: &PlanArtifact,
    decision: PlanDecision,
    revisions: Option<&str>,
) -> Option<Turn> {
    if decision == PlanDecision::Reject && revisions.is_none_or(|text| text.trim().is_empty()) {
        return None;
    }
    let text = if decision == PlanDecision::Approve {
        let serialized = serde_json::to_string_pretty(artifact)
            .unwrap_or_else(|_| "{\"error\":\"plan serialization failed\"}".to_owned());
        format!(
            "Approved plan artifact (authoritative for Execute mode; keep through compaction):\n{serialized}"
        )
    } else {
        format!(
            "Plan rejected. Requested revisions:\n{}",
            revisions.unwrap_or_default().trim()
        )
    };
    Some(Turn {
        role: Role::User,
        blocks: vec![Block::Text { text }],
        meta: TurnMeta::default(),
    })
}

pub(super) fn approved_plan_context_item(conversation: &[Turn]) -> Option<ContextItemId> {
    conversation
        .iter()
        .enumerate()
        .rev()
        .find(|(_, turn)| {
            turn.blocks.iter().any(|block| {
                matches!(block, Block::Text { text } if text.starts_with("Approved plan artifact (authoritative"))
            })
        })
        .map(|(index, _)| ContextItemId(format!("conversation:{index}")))
}
