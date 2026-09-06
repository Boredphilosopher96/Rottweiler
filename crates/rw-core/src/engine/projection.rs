#[allow(clippy::wildcard_imports)]
use super::*;

mod events;
pub(in crate::engine) mod repair;
pub(super) use events::recovered_pending_event;

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
    pub accounting: crate::engine::SessionAccountingState,
    pub latest_budget: Option<rw_types::session_state::SessionBudgetState>,
    pub budgeter: Budgeter,
    pub interrupted_compaction: bool,
    pub model_alias: Option<String>,
    pub provider: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub mode: SessionMode,
    pub mode_id: Option<ModeId>,
    pub permission_mode: Option<rw_types::PermissionModeDescriptor>,
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
    pub invocation_id: rw_types::ToolInvocationId,
    pub missing_start: Option<InterruptedToolStart>,
    pub output: ToolOutput,
}

/// Authoritative display fields needed only when a committed call never started.
#[derive(Clone, Debug, PartialEq)]
pub struct InterruptedToolStart {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug)]
struct ActiveToolStart {
    id: ToolCallId,
    invocation_id: rw_types::ToolInvocationId,
    index: usize,
}

/// A persisted event log cannot be projected safely.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionProjectionError {
    #[error("invalid durable attachment: {0}")]
    InvalidAttachment(String),
    #[error("invalid accepted input selector: {0}")]
    InvalidInput(&'static str),
    #[error("invalid plan: {0}")]
    InvalidPlan(&'static str),
    #[error("invalid durable question payload: {0}")]
    InvalidQuestion(&'static str),
    #[error("unsupported session event version {0}")]
    UnsupportedVersion(u16),
    #[error("session event sequence is not contiguous at {found}; expected {expected}")]
    NonContiguousSequence { expected: u64, found: u64 },
    #[error("event stream contains a non-durable event")]
    NonDurableEvent,
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

/// Projects an ordered durable event log into actor resume state.
///
/// User conversation resolves exact accepted-input or host-context selectors.
/// Uncommitted accepted messages remain available to interrupted-input repair.
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
        if recorded_fingerprint != &definition.semantic_fingerprint() {
            return Err(SessionProjectionError::ModeDefinitionChanged(
                mode.0.clone(),
            ));
        }
        Ok(mode_permission_base(definition))
    })
}

type ProjectedTurnMode = (
    SessionMode,
    Option<ModeId>,
    Option<PlanArtifact>,
    Option<PlanArtifact>,
    bool,
);
type ProjectedRewindArchive = (
    BTreeMap<u64, usize>,
    Vec<Turn>,
    Vec<u64>,
    Vec<ContextSurgeryAction>,
    BTreeMap<String, u64>,
    Budgeter,
);

/// Incremental core projection. Callers release each raw journal page after
/// folding it; this state owns semantic recovery data, not raw events.
#[derive(Clone, Debug)]
struct SessionProjector {
    conversation: Vec<Turn>,
    title: Option<String>,
    conversation_agent_turns: Vec<u64>,
    queued: VecDeque<(u64, String)>,
    uncommitted_users: BTreeMap<u64, Vec<(SequenceId, PreparedUserMessage)>>,
    completed_turns: u64,
    active_turn: Option<u64>,
    turn_ends: BTreeMap<u64, usize>,
    partial_assistant_blocks: Vec<Block>,
    partial_tool_blocks: Vec<Block>,
    next_turn: u64,
    last_sequence: Option<SequenceId>,
    driver_client_id: Option<ClientId>,
    session_id: Option<SessionId>,
    interrupted_tool_repairs: Vec<InterruptedToolRepair>,
    active_tool_starts: BTreeMap<rw_types::ToolInvocationId, ActiveToolStart>,
    interrupted_tool_turn: Option<Turn>,
    pending_questions: BTreeMap<String, RecoveredQuestion>,
    context_surgery: Vec<ContextSurgeryAction>,
    pruned_tool_outputs: BTreeMap<String, u64>,
    accounting: crate::engine::SessionAccountingState,
    latest_budget: Option<rw_types::session_state::SessionBudgetState>,
    model_alias: Option<String>,
    selected_provider: Option<String>,
    selected_thinking: Option<ThinkingLevel>,
    mode: SessionMode,
    mode_id: Option<ModeId>,
    permission_mode: Option<rw_types::PermissionModeDescriptor>,
    pending_plan: Option<PlanArtifact>,
    approved_plan: Option<PlanArtifact>,
    plan_gate_active: bool,
    turn_mode_states: BTreeMap<u64, ProjectedTurnMode>,
    active_shell: Option<RecoveredUserShell>,
    workspace_generation: u64,
    workspace_roots: Vec<rw_types::WorkspaceRootDescriptor>,
    compacted_conversation: Option<Vec<(u64, Turn)>>,
    compaction_surgery_start: Option<usize>,
    budgeter: Budgeter,
    rewind_archives: Vec<ProjectedRewindArchive>,
}

impl Default for SessionProjector {
    fn default() -> Self {
        Self {
            conversation: Vec::new(),
            title: None,
            conversation_agent_turns: Vec::new(),
            queued: VecDeque::new(),
            uncommitted_users: BTreeMap::new(),
            completed_turns: 0,
            active_turn: None,
            turn_ends: BTreeMap::new(),
            partial_assistant_blocks: Vec::new(),
            partial_tool_blocks: Vec::new(),
            next_turn: 1,
            last_sequence: None,
            driver_client_id: None,
            session_id: None,
            interrupted_tool_repairs: Vec::new(),
            active_tool_starts: BTreeMap::new(),
            interrupted_tool_turn: None,
            pending_questions: BTreeMap::new(),
            context_surgery: Vec::new(),
            pruned_tool_outputs: BTreeMap::new(),
            accounting: crate::engine::SessionAccountingState::default(),
            latest_budget: None,
            model_alias: None,
            selected_provider: None,
            selected_thinking: None,
            mode: SessionMode::Execute,
            mode_id: None,
            permission_mode: None,
            pending_plan: None,
            approved_plan: None,
            plan_gate_active: false,
            turn_mode_states: BTreeMap::new(),
            active_shell: None,
            workspace_generation: 0,
            workspace_roots: Vec::new(),
            compacted_conversation: None,
            compaction_surgery_start: None,
            budgeter: Budgeter::default(),
            rewind_archives: Vec::new(),
        }
    }
}

impl SessionProjector {
    #[allow(clippy::match_same_arms, clippy::too_many_lines)]
    fn push_resolving(
        self,
        event: &EngineEvent,
        resolved: &EngineEvent,
        mut resolve_mode: impl FnMut(&ModeId, &String) -> Result<SessionMode, SessionProjectionError>,
    ) -> Result<Self, SessionProjectionError> {
        let Self {
            mut conversation,
            mut title,
            mut conversation_agent_turns,
            mut queued,
            mut uncommitted_users,
            mut completed_turns,
            mut active_turn,
            mut turn_ends,
            mut partial_assistant_blocks,
            mut partial_tool_blocks,
            mut next_turn,
            mut last_sequence,
            mut driver_client_id,
            mut session_id,
            interrupted_tool_repairs,
            mut active_tool_starts,
            interrupted_tool_turn,
            mut pending_questions,
            mut context_surgery,
            mut pruned_tool_outputs,
            mut accounting,
            mut latest_budget,
            mut model_alias,
            mut selected_provider,
            mut selected_thinking,
            mut mode,
            mut mode_id,
            mut permission_mode,
            mut pending_plan,
            mut approved_plan,
            mut plan_gate_active,
            mut turn_mode_states,
            mut active_shell,
            mut workspace_generation,
            mut workspace_roots,
            mut compacted_conversation,
            mut compaction_surgery_start,
            mut budgeter,
            mut rewind_archives,
        } = self;
        'event: {
            let meta = event
                .meta()
                .ok_or(SessionProjectionError::NonDurableEvent)?;
            if meta.protocol_version != SESSION_EVENT_VERSION {
                return Err(SessionProjectionError::UnsupportedVersion(
                    meta.protocol_version,
                ));
            }
            if let Some(expected) = &session_id {
                if expected != &meta.session_id {
                    return Err(SessionProjectionError::SessionChanged {
                        expected: expected.0.clone(),
                        found: meta.session_id.0.clone(),
                    });
                }
            } else {
                session_id = Some(meta.session_id.clone());
            }
            let expected =
                last_sequence.map_or(0, |sequence: SequenceId| sequence.0.saturating_add(1));
            if meta.sequence_id.0 != expected {
                return Err(SessionProjectionError::NonContiguousSequence {
                    expected,
                    found: meta.sequence_id.0,
                });
            }
            last_sequence = Some(meta.sequence_id);
            if let EngineEvent::ConversationInputCommitted {
                agent_turn,
                accepted_source,
                ..
            } = event
            {
                if !uncommitted_users.get(agent_turn).is_some_and(|pending| {
                    pending.iter().any(|(source, _)| source == accepted_source)
                }) {
                    return Err(SessionProjectionError::InvalidInput(
                        "input is not pending in this turn",
                    ));
                }
            }
            let Some(kind) = recovered_pending_event(resolved)? else {
                break 'event;
            };
            let input_source = if let EngineEvent::ConversationInputCommitted {
                accepted_source,
                ..
            } = event
            {
                Some(*accepted_source)
            } else {
                None
            };
            match &kind {
                PendingEvent::ConversationInputCommitted { .. }
                | PendingEvent::ConversationContextCommitted { .. } => {
                    return Err(SessionProjectionError::InvalidInput("unresolved input"));
                }
                PendingEvent::ProviderCallAccounted { .. } => {}
                PendingEvent::TurnStarted { turn } => {
                    active_tool_starts.clear();
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
                PendingEvent::UserMessageAccepted {
                    turn,
                    content,
                    attachments,
                } => {
                    let message =
                        crate::engine::dispatch::recover_user_message(content, attachments)
                            .map_err(SessionProjectionError::InvalidAttachment)?;
                    if let Some(position) = queued
                        .iter()
                        .position(|(_, queued_content)| queued_content == content)
                    {
                        queued.remove(position);
                    }
                    uncommitted_users
                        .entry(*turn)
                        .or_default()
                        .push((meta.sequence_id, message));
                }
                PendingEvent::SessionTitleUpdated {
                    title: updated,
                    usage,
                    cost,
                } => {
                    title = Some(updated.clone());
                    if let (Some(usage), Some(cost)) = (usage, cost) {
                        accounting.record(&TurnAccounting {
                            turn_id: TurnId("title".to_owned()),
                            attribution: AccountingAttribution::Title,
                            usage: (*usage).into(),
                            cost: cost.clone(),
                        });
                    }
                }
                PendingEvent::PluginMessageInjected { .. }
                | PendingEvent::PluginStatusChanged { .. }
                | PendingEvent::ExtensionStateCommitted { .. }
                | PendingEvent::TodoStateCommitted { .. }
                | PendingEvent::UiNotification { .. } => {}
                PendingEvent::ConversationTurnCommitted { agent_turn, turn } => {
                    if let Some(compacted) = &mut compacted_conversation {
                        compacted.push((*agent_turn, turn.clone()));
                        break 'event;
                    }
                    if turn.role == Role::User
                        && let Some(pending) = uncommitted_users.get_mut(agent_turn)
                        && let Some(index) = pending
                            .iter()
                            .position(|(source, _)| input_source == Some(*source))
                    {
                        pending.remove(index);
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
                        conversation_agent_turns =
                            restored_turns.into_iter().take(retained).collect();
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
                    pending_questions.retain(|_, question: &mut RecoveredQuestion| {
                        question.agent_turn <= *to_turn
                    });
                    context_surgery.retain(|action: &ContextSurgeryAction| {
                        action.effective_after_agent_turn <= *to_turn
                    });
                }
                PendingEvent::TurnFinished {
                    turn, usage, cost, ..
                } => {
                    uncommitted_users.remove(turn);
                    if active_turn == Some(*turn) {
                        active_tool_starts.clear();
                        active_turn = None;
                    }
                    completed_turns = completed_turns.saturating_add(1);
                    next_turn = next_turn.max(turn.saturating_add(1));
                    turn_ends.insert(*turn, conversation.len());
                    pending_questions
                        .retain(|_, question: &mut RecoveredQuestion| question.agent_turn != *turn);
                    accounting.record(&TurnAccounting {
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
                PendingEvent::ToolCallStarted {
                    turn,
                    id,
                    invocation_id,
                    index,
                    ..
                } if active_turn == Some(*turn) => {
                    active_tool_starts.insert(
                        invocation_id.clone(),
                        ActiveToolStart {
                            id: ToolCallId(id.clone()),
                            invocation_id: invocation_id.clone(),
                            index: *index,
                        },
                    );
                }
                PendingEvent::ToolCallFinished {
                    turn,
                    id,
                    invocation_id,
                    output,
                    is_error,
                    ..
                } if active_turn == Some(*turn) => {
                    active_tool_starts.remove(invocation_id);
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
                | PendingEvent::GuardTriggered { .. } => {}
                PendingEvent::BudgetStatus {
                    turn,
                    level,
                    scope,
                    unit,
                    current,
                    limit,
                } => {
                    latest_budget = Some(rw_types::session_state::SessionBudgetState {
                        turn_id: wire_turn_id(*turn),
                        level: level.clone(),
                        scope: scope.clone(),
                        unit: unit.clone(),
                        current: *current,
                        limit: *limit,
                    });
                }
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
                    accounting.record(&TurnAccounting {
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
                    accounting.record(&TurnAccounting {
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
                    source,
                    reclaimed_tokens,
                } => {
                    pruned_tool_outputs.insert(source.key(), *reclaimed_tokens);
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
                    rw_types::question_admission::validate_questions(questions)
                        .map_err(SessionProjectionError::InvalidQuestion)?;
                    if !pending_questions.contains_key(&question_id.0)
                        && pending_questions.len()
                            >= rw_types::question_admission::MAX_PENDING_QUESTION_REQUESTS
                    {
                        return Err(SessionProjectionError::InvalidQuestion(
                            "pending request count exceeds admission",
                        ));
                    }
                    pending_questions.insert(
                        question_id.0.clone(),
                        RecoveredQuestion {
                            agent_turn: *turn,
                            question_id: question_id.clone(),
                            questions: questions.clone(),
                        },
                    );
                }
                PendingEvent::ToolApprovalResolved { .. } => {}
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
                    mode = resolve_mode(changed, definition_fingerprint)?;
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
                    rw_types::session_controls::validate_plan(artifact)
                        .map_err(SessionProjectionError::InvalidPlan)?;
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
        Ok(Self {
            conversation,
            title,
            conversation_agent_turns,
            queued,
            uncommitted_users,
            completed_turns,
            active_turn,
            turn_ends,
            partial_assistant_blocks,
            partial_tool_blocks,
            next_turn,
            last_sequence,
            driver_client_id,
            session_id,
            interrupted_tool_repairs,
            active_tool_starts,
            interrupted_tool_turn,
            pending_questions,
            context_surgery,
            pruned_tool_outputs,
            accounting,
            latest_budget,
            model_alias,
            selected_provider,
            selected_thinking,
            mode,
            mode_id,
            permission_mode,
            pending_plan,
            approved_plan,
            plan_gate_active,
            turn_mode_states,
            active_shell,
            workspace_generation,
            workspace_roots,
            compacted_conversation,
            compaction_surgery_start,
            budgeter,
            rewind_archives,
        })
    }

    /// Finishes recovery, including deterministic repair of interrupted work.
    ///
    /// # Errors
    /// Returns a projection failure when terminal repair cannot be represented.
    #[allow(clippy::too_many_lines)]
    pub fn finish(self) -> Result<SessionRecoveredState, SessionProjectionError> {
        let Self {
            mut conversation,
            title,
            conversation_agent_turns,
            queued,
            uncommitted_users,
            completed_turns,
            active_turn,
            turn_ends,
            partial_assistant_blocks,
            partial_tool_blocks,
            next_turn,
            last_sequence,
            driver_client_id,
            session_id: _,
            mut interrupted_tool_repairs,
            active_tool_starts,
            mut interrupted_tool_turn,
            pending_questions,
            context_surgery,
            pruned_tool_outputs,
            accounting,
            latest_budget,
            model_alias,
            selected_provider,
            selected_thinking,
            mode,
            mode_id,
            permission_mode,
            pending_plan,
            approved_plan,
            plan_gate_active,
            turn_mode_states: _,
            active_shell,
            workspace_generation,
            workspace_roots,
            compacted_conversation,
            compaction_surgery_start: _,
            budgeter,
            rewind_archives: _,
        } = self;
        for messages in uncommitted_users.into_values() {
            for (_, message) in messages {
                conversation.push(message.turn(message.content.clone()));
            }
        }
        if let Some(interrupted_turn) = active_turn {
            let repair = repair::repair_tools(
                interrupted_turn,
                conversation_agent_turns
                    .iter()
                    .zip(&conversation)
                    .filter_map(|(turn, value)| (*turn == interrupted_turn).then_some(value)),
                active_tool_starts
                    .into_values()
                    .map(|start| crate::engine::recovery::ToolStartIdentity {
                        invocation_id: start.invocation_id,
                        tool_call_id: start.id,
                        index: start.index,
                    })
                    .collect(),
                partial_tool_blocks,
            );
            interrupted_tool_repairs.extend(repair.tools);
            if let Some(tool_turn) = repair.tool_turn {
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
            latest_budget,
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
}

fn project_session_events_resolving_mode(
    events: &[EngineEvent],
    mut resolve_mode: impl FnMut(&ModeId, &String) -> Result<SessionMode, SessionProjectionError>,
) -> Result<SessionRecoveredState, SessionProjectionError> {
    let mut projector = SessionProjector::default();
    for event in events {
        let resolved = crate::engine::recovery::input::materialize_audit_event(events, event)
            .map_err(|_| SessionProjectionError::InvalidInput("conversation source"))?;
        projector = projector.push_resolving(event, &resolved, &mut resolve_mode)?;
    }
    projector.finish()
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
