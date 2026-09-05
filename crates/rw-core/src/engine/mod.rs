mod accounting_state;
pub use accounting_state::SessionAccountingState;
use rw_types::hook_contract::{HookClass, HookInput};
mod redaction;
pub use redaction::NoopSecretRedactor;
pub use redaction::SecretRedactor;
mod model;
pub use model::ModelContextMetadata;
pub use model::ModelDriver;
pub use model::ModelSource;
mod durability;
pub use durability::{
    AdmittedEventBatch, EventBatchPlan, EventBatchReservation, ExtensionStateView,
    NoopSessionEventSink,
};
pub use durability::{CompletedTurn, SessionEventSink, commit_session_events};
mod mutation_checkpoints;
pub use mutation_checkpoints::MutationCheckpoint;
pub use mutation_checkpoints::MutationCheckpointCoordinator;
pub use mutation_checkpoints::MutationCheckpointOutcome;
pub use mutation_checkpoints::NoopMutationCheckpointCoordinator;
pub use mutation_checkpoints::RewindCheckpoint;
mod event_clock;
pub use event_clock::BudgetLedgerQuery;
pub use event_clock::BudgetLedgerTotals;
pub use event_clock::EventClock;
pub use event_clock::SystemEventClock;
mod pending_event;
mod plugin_state;
use pending_event::PendingEvent;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use rw_context::Budgeter;
use rw_ext::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
    HookDirective, HookDispatcher, HookEvent, HookFailurePolicy, HookHandler, HookInvocation,
    HookRegistration, ModeDefinition, ModeRegistry,
};
use rw_providers::TokenUsage;
use rw_tools::{ApprovalPreview, CancellationToken, ToolRegistry};
use rw_types::config::ThinkingLevel;
use rw_types::config::{PermissionDecision, PermissionRule};
use rw_types::{
    AccountingAttribution, Answer, ApprovalBinding, Block, ClientId, ContextItemId,
    ContextItemKind, ContextSnapshot, Cost, CostSnapshot, EngineEvent, ModeId, ModelAlias,
    ModelContextTransfer, ModelSwitchQuestion, PROTOCOL_VERSION, PlanArtifact, PlanDecision,
    Question, QuestionId, QuestionOption, QuestionResponseKind, ReviewFileStatus, Role, SequenceId,
    SessionId, SessionMode, SessionReview, ShellId, StoredAttachment, ToolCallId, ToolOutput,
    ToolOutputStream, Turn, TurnAccounting, TurnId, TurnMeta, TurnStatus, UnifiedDiff, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::broadcast;

use crate::{InitDepth, PermissionGate, PermissionRequest};

mod commands;
mod dispatch;
mod projection;
#[cfg(unix)]
pub mod recovery;
mod replay;
pub use replay::{SessionEventReadView, SessionReplayLimits};
mod runtime_publication;
mod session;
mod session_extension;
mod session_resources;
mod shutdown;
pub use runtime_publication::{PreparedRuntimePublication, RuntimePublication};
pub use session_resources::{NoopSessionResources, SessionResources};
mod task_ownership;
mod turn;

pub use commands::{
    CommandToolCall, CommandToolOutputKind, FolderTrustController, FolderTrustOperation,
    NoopFolderTrustController, NoopWorkspaceRootController, SessionCommandAction,
    SessionCommandContext, SessionCommandOutput, WorkspaceRootController, WorkspaceRootRequest,
    WorkspaceRuntimeGeneration, builtin_command_registry,
};
use projection::approved_plan_context_item;
pub use projection::{
    ContextSurgeryAction, InterruptedToolRepair, RecoveredQuestion, RecoveredUserShell,
    SessionProjectionError, SessionProjector, SessionRecoveredState, project_session_events,
    project_session_events_with_modes, project_session_read_view,
};
use session::ActorState;
pub use session::{
    PluginSessionBinding, PluginSessionCapability, SessionActor, SessionActorConfig, SessionHandle,
    SessionSubscription, StartupNotification,
};
pub use session_extension::{
    NoopSessionExtensionController, SessionExtensionController, SessionExtensionSnapshot,
};
use turn::{TurnSignal, append_text, append_thinking, emit_batch, hook_event_name, persist_event};

const SESSION_TITLE_TIMEOUT: Duration = Duration::from_secs(4);
const SESSION_TITLE_PROMPT_CHARS: usize = 1_024;
const SESSION_TITLE_OUTPUT_CHARS: usize = 160;
const SESSION_TITLE_MAX_CHARS: usize = 72;

mod decimal_u64 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<SerializerType>(
        value: &u64,
        serializer: SerializerType,
    ) -> Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, DeserializerType>(
        deserializer: DeserializerType,
    ) -> Result<u64, DeserializerType::Error>
    where
        DeserializerType: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(DeserializerType::Error::custom)
    }
}

/// Schema version for the headless Rust session event stream.
pub const SESSION_EVENT_VERSION: u16 = PROTOCOL_VERSION;

/// Maximum time core waits for a cancelled tool to finish its own cleanup.
pub const TOOL_CANCELLATION_GRACE: Duration = Duration::from_secs(2);

const TEXT_DELTA_COALESCE_WINDOW: Duration = Duration::from_millis(2);
const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_LIVE_TOOL_OUTPUT_CHUNKS: usize = 1024;
const MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS: usize = 32;
const MAX_TOOL_EXECUTION_WINDOW: usize = 16;
const MAX_CAPTURED_SHELL_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_APPROVAL_DIFF_BYTES: usize = 256 * 1024;
const MAX_COMMAND_TOOL_FRAME_BYTES: usize = 256 * 1024;
const MAX_PLUGIN_ID_BYTES: usize = 64;
const MAX_PLUGIN_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_PLUGIN_STATUS_BYTES: usize = 16 * 1024;
const MAX_PLUGIN_NOTIFICATION_TITLE_BYTES: usize = 64;
const MAX_PLUGIN_NOTIFICATION_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_PERMISSION_RULES_PER_SCOPE: usize = 128;
const MAX_PERMISSION_APPROVALS: usize = 256;
const MAX_PERMISSION_PATTERN_BYTES: usize = 2 * 1024;
const MAX_PERMISSION_ID_BYTES: usize = 192;
const MAX_PERMISSION_LABEL_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq)]
struct PreparedUserMessage {
    content: String,
    stored_attachments: Vec<StoredAttachment>,
}

impl PreparedUserMessage {
    fn turn(&self, content: String) -> Turn {
        let mut blocks = Vec::with_capacity(self.stored_attachments.len().saturating_add(1));
        if !content.is_empty() {
            blocks.push(Block::Text { text: content });
        }
        blocks.extend(
            self.stored_attachments
                .iter()
                .map(|attachment| match &attachment.data {
                    rw_types::AttachmentData::Text { content } => {
                        let label = attachment
                            .source_path
                            .as_deref()
                            .unwrap_or(&attachment.name);
                        Block::Text {
                            text: format!(
                                "Attached file {label:?} ({}):\n{content}",
                                attachment.media_type
                            ),
                        }
                    }
                    rw_types::AttachmentData::InlineBase64 { data } => Block::Image {
                        media_type: attachment.media_type.clone(),
                        data: rw_types::ImageRef::InlineBase64 { data: data.clone() },
                    },
                }),
        );
        Turn {
            role: Role::User,
            blocks,
            meta: TurnMeta::default(),
        }
    }

    fn redact(self, redactor: &dyn SecretRedactor) -> Result<Self, String> {
        dispatch::redact_prepared_message(self, redactor)
    }
}

fn approval_diff(request: &PermissionRequest, preview: &ApprovalPreview) -> Option<UnifiedDiff> {
    let path = preview.path.to_str()?.to_owned();
    let before = match &preview.before {
        Some(bytes) => std::str::from_utf8(bytes).ok()?,
        None => "",
    };
    let after = std::str::from_utf8(&preview.after).ok()?;
    let before_label = if preview.before.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_owned()
    };
    let full_diff = format!(
        "--- {before_label}\n+++ b/{path}\n@@ approved full-file change @@\n-{}\n+{}\n",
        before.replace('\n', "\n-"),
        after.replace('\n', "\n+")
    );
    let arguments = serde_json::to_vec(&request.arguments).ok()?;
    let arguments_hash = blake3::hash(&arguments).to_hex().to_string();
    let mut base_hasher = blake3::Hasher::new();
    base_hasher.update(b"rottweiler-approval-base-v1\0");
    match &preview.before {
        Some(bytes) => {
            base_hasher.update(b"present\0");
            base_hasher.update(bytes);
        }
        None => {
            base_hasher.update(b"missing\0");
        }
    }
    let base_hash = base_hasher.finalize().to_hex().to_string();
    let diff_hash = blake3::hash(full_diff.as_bytes()).to_hex().to_string();
    let mut proposal_hasher = blake3::Hasher::new();
    proposal_hasher.update(b"rottweiler-approval-proposal-v1\0");
    proposal_hasher.update(request.id.as_bytes());
    proposal_hasher.update(b"\0");
    proposal_hasher.update(request.tool_name.as_bytes());
    proposal_hasher.update(b"\0");
    proposal_hasher.update(path.as_bytes());
    proposal_hasher.update(b"\0");
    proposal_hasher.update(arguments_hash.as_bytes());
    proposal_hasher.update(base_hash.as_bytes());
    proposal_hasher.update(diff_hash.as_bytes());
    let proposal_id = proposal_hasher.finalize().to_hex().to_string();
    let truncated = full_diff.len() > MAX_APPROVAL_DIFF_BYTES;
    let unified_diff = if truncated {
        let boundary = full_diff
            .char_indices()
            .take_while(|(index, _)| *index <= MAX_APPROVAL_DIFF_BYTES)
            .last()
            .map_or(0, |(index, _)| index);
        full_diff.get(..boundary).unwrap_or_default().to_owned()
    } else {
        full_diff.clone()
    };
    Some(UnifiedDiff {
        proposal_id,
        path,
        unified_diff,
        arguments_hash,
        base_hash,
        diff_hash,
        truncated,
    })
}

fn diff_binding(diff: &UnifiedDiff) -> ApprovalBinding {
    ApprovalBinding {
        proposal_id: diff.proposal_id.clone(),
        arguments_hash: diff.arguments_hash.clone(),
        base_hash: diff.base_hash.clone(),
        diff_hash: diff.diff_hash.clone(),
    }
}

/// Stable turn-loop construction or runtime failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AgentLoopError {
    /// Owned effects could not be proven settled.
    #[error("effect settlement is unproven: {0}")]
    EffectsUnsettled(String),
    /// A replay cursor is beyond its captured committed prefix.
    #[error("replay cursor is ahead of the captured journal")]
    ReplayCursorAhead,
    /// Configuration would make termination impossible or invalid.
    #[error("invalid agent-loop configuration: {0}")]
    InvalidConfiguration(String),
    /// Provider/router failed before or during streaming.
    #[error("provider failure: {0}")]
    Provider(String),
    /// Extension registry or hook setup failed.
    #[error("extension failure: {0}")]
    Extension(String),
    /// Workspace tool context could not be created.
    #[error("tool context failure: {0}")]
    ToolContext(String),
    /// Durable session log rejected an append.
    #[error("session persistence failure: {0}")]
    Persistence(String),
    /// The session actor is no longer available.
    #[error("session actor is closed")]
    Closed,
}

/// Why one user-facing turn stopped.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnStatus {
    /// Provider completed without requesting more tools.
    Completed,
    /// Active client interrupted the provider or a tool.
    Interrupted,
    /// Provider/tool/hook failure prevented completion.
    Failed,
    /// Configured provider-iteration limit was reached.
    MaxTurns,
    /// Identical failing tool invocations reached the configured threshold.
    DoomLoop,
    /// A configured session or daily spend cap stopped the turn.
    BudgetExceeded,
}

fn wire_turn_id(turn: u64) -> TurnId {
    TurnId(turn.to_string())
}

fn session_mode_name(mode: SessionMode) -> &'static str {
    mode.as_str()
}

fn parse_session_mode(mode: &str) -> Option<SessionMode> {
    match mode {
        "discuss" => Some(SessionMode::Discuss),
        "plan" => Some(SessionMode::Plan),
        "execute" => Some(SessionMode::Execute),
        _ => None,
    }
}

fn mode_permission_base(mode: &ModeDefinition) -> SessionMode {
    mode.permission()
}

fn permission_mode_name(mode: rw_types::PermissionModeDescriptor) -> &'static str {
    mode.as_str()
}

fn parse_permission_mode(
    mode: &str,
) -> Result<rw_types::PermissionModeDescriptor, SessionProjectionError> {
    mode.parse()
        .map_err(|_| SessionProjectionError::InvalidPermissionMode(mode.to_owned()))
}

fn model_context_transfer_value(strategy: ModelContextTransfer) -> &'static str {
    match strategy {
        ModelContextTransfer::PassSummary => "pass_summary",
        ModelContextTransfer::PassFullContext => "pass_full_context",
        ModelContextTransfer::StartWithoutContext => "start_without_context",
    }
}

fn parse_model_context_transfer(value: &str) -> Option<ModelContextTransfer> {
    match value {
        "pass_summary" => Some(ModelContextTransfer::PassSummary),
        "pass_full_context" => Some(ModelContextTransfer::PassFullContext),
        "start_without_context" => Some(ModelContextTransfer::StartWithoutContext),
        _ => None,
    }
}

fn model_switch_question(
    question_id: QuestionId,
    model: ModelAlias,
    provider: Option<String>,
) -> Question {
    let option = |strategy, label: &str, description: &str| QuestionOption {
        value: model_context_transfer_value(strategy).to_owned(),
        label: label.to_owned(),
        description: Some(description.to_owned()),
        model_context_transfer: Some(strategy),
    };
    Question {
        id: question_id,
        prompt: "How should the new model receive this conversation?".to_owned(),
        response_kind: QuestionResponseKind::SelectOne,
        // The client highlights the first choice by default. Summary is the
        // safe, economical default and never silently replays full history.
        options: vec![
            option(
                ModelContextTransfer::PassSummary,
                "Pass summary",
                "Compact this conversation, then switch models",
            ),
            option(
                ModelContextTransfer::PassFullContext,
                "Pass full context",
                "Switch models with the complete current history",
            ),
            option(
                ModelContextTransfer::StartWithoutContext,
                "Start without context",
                "Keep project instructions but start a fresh conversation",
            ),
        ],
        model_switch: Some(ModelSwitchQuestion { model, provider }),
    }
}

fn model_switch_answer(
    answers: &[Answer],
    question_id: &QuestionId,
) -> Option<ModelContextTransfer> {
    let values = &answers
        .iter()
        .find(|answer| answer.question_id == *question_id)?
        .values;
    (values.len() == 1)
        .then(|| parse_model_context_transfer(&values[0]))
        .flatten()
}

fn unavailable_cost() -> Cost {
    Cost::Unavailable {
        reason: "provider accounting is unavailable".to_owned(),
    }
}

impl From<AgentTurnStatus> for TurnStatus {
    fn from(value: AgentTurnStatus) -> Self {
        match value {
            AgentTurnStatus::Completed => Self::Completed,
            AgentTurnStatus::Interrupted => Self::Interrupted,
            AgentTurnStatus::Failed => Self::Failed,
            AgentTurnStatus::MaxTurns => Self::MaxTurns,
            AgentTurnStatus::DoomLoop => Self::DoomLoop,
            AgentTurnStatus::BudgetExceeded => Self::BudgetExceeded,
        }
    }
}

/// Provider usage accumulated across all model iterations in a turn.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionUsage {
    #[serde(with = "decimal_u64")]
    pub input_tokens: u64,
    #[serde(with = "decimal_u64")]
    pub output_tokens: u64,
    #[serde(with = "decimal_u64")]
    pub cache_read_tokens: u64,
    #[serde(with = "decimal_u64")]
    pub cache_write_tokens: u64,
    #[serde(with = "decimal_u64")]
    pub reasoning_tokens: u64,
}

impl SessionUsage {
    fn update(&mut self, usage: TokenUsage) {
        self.input_tokens = usage.input_tokens;
        self.output_tokens = usage.output_tokens;
        self.cache_read_tokens = usage.cache_read_tokens;
        self.cache_write_tokens = usage.cache_write_tokens;
        self.reasoning_tokens = usage.reasoning_tokens;
    }

    fn add(&mut self, usage: Self) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(usage.reasoning_tokens);
    }
}

impl From<SessionUsage> for Usage {
    fn from(value: SessionUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cache_read_tokens: value.cache_read_tokens,
            cache_write_tokens: value.cache_write_tokens,
            reasoning_tokens: value.reasoning_tokens,
        }
    }
}

impl From<SessionUsage> for TokenUsage {
    fn from(value: SessionUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cache_read_tokens: value.cache_read_tokens,
            cache_write_tokens: value.cache_write_tokens,
            reasoning_tokens: value.reasoning_tokens,
        }
    }
}

/// Immediate disposition of a submitted user message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageDisposition {
    Started,
    Queued,
    Command,
}

/// Read-only actor state for tests and future persistence adapters.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub conversation_turns: u64,
    pub resolved_model: Option<String>,
    pub queued_messages: Vec<String>,
    pub running: bool,
    pub completed_turns: u64,
    pub model_alias: String,
    pub provider: Option<String>,
    pub thinking: ThinkingLevel,
    pub mode: SessionMode,
    pub mode_id: ModeId,
    pub permission_mode: Option<rw_types::PermissionModeDescriptor>,
    pub pending_plan: Option<PlanArtifact>,
    pub approved_plan: Option<PlanArtifact>,
    pub plan_gate_active: bool,
    pub active_shell: Option<RecoveredUserShell>,
    pub active_background: bool,
    pub workspace_generation: u64,
    pub workspace_roots: Vec<rw_types::WorkspaceRootDescriptor>,
    pub driver_client_id: Option<ClientId>,
}

async fn apply_mode_change(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    sink: &Arc<dyn SessionEventSink>,
    mode_id: ModeId,
    modes: &ModeRegistry,
) -> Result<(), AgentLoopError> {
    let definition = modes.get(&mode_id.0).ok_or_else(|| {
        AgentLoopError::InvalidConfiguration(format!("unknown mode {:?}", mode_id.0))
    })?;
    let mode = mode_permission_base(definition);
    let evicted = (mode == SessionMode::Plan)
        .then(|| approved_plan_context_item(&state.conversation))
        .flatten();
    let mut durable = Vec::with_capacity(usize::from(evicted.is_some()) + 1);
    if let Some(item_id) = &evicted {
        durable.push(PendingEvent::ContextItemEvicted {
            item_id: item_id.clone(),
            effective_after_agent_turn: state.completed_turns,
        });
    }
    durable.push(PendingEvent::ModeChanged {
        mode: mode_id.clone(),
        definition_fingerprint: definition.semantic_fingerprint(),
    });
    emit_batch(state, events, sink, durable).await?;
    if let Some(item_id) = evicted {
        state.context_surgery.push(ContextSurgeryAction {
            item_id,
            pinned: false,
            effective_after_agent_turn: state.completed_turns,
        });
    }
    state.mode = mode;
    state.mode_id = mode_id;
    if mode == SessionMode::Plan {
        state.pending_plan = None;
        state.approved_plan = None;
        state.plan_gate_active = true;
    }
    Ok(())
}

async fn apply_permission_mode_change(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    config: &SessionActorConfig,
    mode: Option<rw_types::PermissionModeDescriptor>,
) -> Result<(), AgentLoopError> {
    let previous = config.permissions.snapshot().runtime_mode;
    config
        .permissions
        .set_runtime_mode(mode)
        .map_err(AgentLoopError::InvalidConfiguration)?;
    if let Err(error) = emit_batch(
        state,
        events,
        &config.event_sink,
        vec![PendingEvent::PermissionModeChanged { mode }],
    )
    .await
    {
        let _ = config.permissions.set_runtime_mode(previous);
        return Err(error);
    }
    Ok(())
}

struct ValidateToolHook;

#[async_trait]
impl HookHandler for ValidateToolHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(
        &self,
        invocation: HookInvocation<'_>,
    ) -> Result<HookDirective, rw_ext::HookError> {
        let valid = matches!(invocation.input(), HookInput::PreTool(input) if !input.name.trim().is_empty());
        if valid {
            Ok(HookDirective::Continue {})
        } else {
            Ok(HookDirective::Block {
                message: "tool name must not be empty".to_owned(),
            })
        }
    }
}

/// Registers core hooks through rw-ext's public dispatcher API.
///
/// # Errors
///
/// Returns an extension error if any built-in registration is invalid.
pub fn builtin_hook_dispatcher() -> Result<HookDispatcher, AgentLoopError> {
    let mut dispatcher = HookDispatcher::new();
    dispatcher
        .register(
            HookRegistration::new("core.validate-tool", HookEvent::PreTool, HookClass::Policy)
                .with_priority(i32::MIN)
                .with_failure_policy(HookFailurePolicy::FailClosed),
            ValidateToolHook,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    Ok(dispatcher)
}

#[derive(Clone, Debug)]
struct RoutedEvent {
    target: Option<ClientId>,
    event: EngineEvent,
}

#[cfg(test)]
mod tests;
