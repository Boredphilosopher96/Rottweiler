use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    panic::AssertUnwindSafe,
    path::{Component, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::{FutureExt, StreamExt};
use rw_context::{
    AssembledContext, AssemblyInput, Budgeter, CompactionInput,
    CompactionReason as ContextCompactionReason, Compactor, ContextAssembler,
    ContextItem as AssemblyContextItem, ContextItemId as AssemblyContextItemId,
    ContextItemKind as AssemblyContextItemKind, ContextProvenance, ConversationPin,
    LocalTokenEstimator, OverflowPolicy, PRUNED_TOOL_OUTPUT_REPLACEMENT, PreCompactHook,
    PruneConfig, PruneRecord, PruneRecordKind, Pruner, ToonPromptEncoder,
};
use rw_ext::{
    CommandDescriptor, CommandExecutionError, CommandHandler, CommandInvocation, CommandRegistry,
    HookDirective, HookDispatchResult, HookDispatchStatus, HookDispatcher, HookEvent, HookFailure,
    HookFailurePolicy, HookHandler, HookInvocation, HookRegistration,
};
use rw_providers::{
    BoxEventStream, CacheBreakpointSupport, CacheHint, FinishReason, ProviderEvent,
    ProviderRequest, ThinkingLevel, TokenUsage, ToolChoice, ToolDefinition,
};
use rw_tools::{
    AskUserInput, CancellationToken, MutationScope, QuestionAsker, ToolContext, ToolDescriptor,
    ToolError, ToolOutputChunk, ToolOutputSink, ToolRegistry, ToolResult,
};
use rw_types::config::{BudgetConfig, CompactionConfig};
use rw_types::{
    AccountingAttribution, Answer, ApprovalDecision, Attachment, AttachmentData, Block,
    BudgetLevel, BudgetScope, BudgetUnit, CacheBreakpoint, ClientCommand, ClientId, ClientRole,
    CommandAckMeta, CommandMeta, CommandOutcome, CompactionReason, ContextItemId, ContextItemKind,
    ContextItemSnapshot, ContextItemState, ContextSnapshot, Cost, CostSnapshot, EngineError,
    EngineErrorCategory, EngineEvent, EventMeta, ImageRef, ModelAlias, PROTOCOL_VERSION,
    PromptDump, PromptTool, Question, QuestionId, QuestionOption, QuestionResponseKind, RequestId,
    RewindTarget, Role, SequenceId, SessionId, ShellId, StoredAttachment, ToolCallId, ToolOutput,
    ToolOutputPart, ToolOutputStream, Turn, TurnAccounting, TurnId, TurnMeta, TurnStatus,
    UnifiedDiff, UnrestorablePath, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc, oneshot};

use crate::{
    PermissionApprover, PermissionGate, PermissionOutcome, PermissionRequest, ProviderRuntime,
};

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
const MAX_ATTACHMENTS: usize = 16;
const MAX_TEXT_ATTACHMENT_BYTES: usize = 1024 * 1024;
const MAX_IMAGE_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_CAPTURED_SHELL_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_APPROVAL_DIFF_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
struct PreparedUserMessage {
    content: String,
    stored_attachments: Vec<StoredAttachment>,
    attachment_blocks: Vec<Block>,
}

impl PreparedUserMessage {
    fn turn(&self, content: String) -> Turn {
        let mut blocks = Vec::with_capacity(self.attachment_blocks.len().saturating_add(1));
        if !content.is_empty() {
            blocks.push(Block::Text { text: content });
        }
        blocks.extend(self.attachment_blocks.clone());
        Turn {
            role: Role::User,
            blocks,
            meta: TurnMeta::default(),
        }
    }
}

fn permission_diff(request: &PermissionRequest) -> Option<UnifiedDiff> {
    let path = request.arguments.get("path")?.as_str()?.to_owned();
    let (before, after) = match request.tool_name.as_str() {
        "edit" => (
            request.arguments.get("old")?.as_str()?.to_owned(),
            request.arguments.get("new")?.as_str()?.to_owned(),
        ),
        "write" => (
            String::new(),
            request.arguments.get("content")?.as_str()?.to_owned(),
        ),
        _ => return None,
    };
    let full_diff = format!(
        "--- a/{path}\n+++ b/{path}\n@@ proposed change @@\n-{}\n+{}\n",
        before.replace('\n', "\n-"),
        after.replace('\n', "\n+")
    );
    let arguments = serde_json::to_vec(&request.arguments).ok()?;
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
        proposal_id: request.id.clone(),
        path,
        unified_diff,
        arguments_hash: blake3::hash(&arguments).to_hex().to_string(),
        base_hash: blake3::hash(before.as_bytes()).to_hex().to_string(),
        diff_hash: blake3::hash(full_diff.as_bytes()).to_hex().to_string(),
        truncated,
    })
}

/// Shared redaction hook applied before tool text enters persistence,
/// broadcast, or the next provider request.
pub trait SecretRedactor: Send + Sync {
    fn redact(&self, text: &str) -> String;
}

#[derive(Debug, Default)]
pub struct NoopSecretRedactor;

impl SecretRedactor for NoopSecretRedactor {
    fn redact(&self, text: &str) -> String {
        text.to_owned()
    }
}

/// Provider-neutral model streaming boundary used by the actor loop.
pub trait ModelDriver: Send + Sync {
    /// Starts one provider iteration for an already-resolved model alias.
    ///
    /// # Errors
    ///
    /// Returns an error when alias resolution or stream construction fails.
    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
    ) -> Result<BoxEventStream, AgentLoopError>;

    /// Context/cache metadata known without a network call. Unknown context
    /// windows conservatively disable estimate-triggered auto-compaction.
    fn context_metadata(&self, _alias: &str) -> ModelContextMetadata {
        ModelContextMetadata::default()
    }

    /// Whether an alias is configured without making a provider request.
    fn has_model_alias(&self, alias: &str) -> bool {
        !alias.trim().is_empty()
    }

    /// Whether an alias accepts provider-neutral image blocks.
    fn supports_vision(&self, _alias: &str) -> bool {
        false
    }

    /// Validated compaction settings associated with this model runtime.
    fn compaction_config(&self) -> CompactionConfig {
        CompactionConfig::default()
    }

    /// Validated spend guardrails associated with this model runtime.
    fn budget_config(&self) -> BudgetConfig {
        BudgetConfig::default()
    }

    /// Billing disposition for normalized usage.
    fn cost(&self, _alias: &str, _usage: TokenUsage) -> Cost {
        Cost::Unavailable {
            reason: "provider accounting is unavailable".to_owned(),
        }
    }

    /// Billing disposition when the normalized stream reported the concrete
    /// model that served a failover-capable alias.
    fn cost_for_reported_model(
        &self,
        alias: &str,
        _reported_model: Option<&str>,
        usage: TokenUsage,
    ) -> Cost {
        self.cost(alias, usage)
    }

    /// Billing disposition keyed by an opaque router-owned candidate identity.
    fn cost_for_route(
        &self,
        alias: &str,
        _route: Option<&str>,
        reported_model: Option<&str>,
        usage: TokenUsage,
    ) -> Cost {
        self.cost_for_reported_model(alias, reported_model, usage)
    }
}

/// Synchronous context metadata consumed by the provider-neutral assembler.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelContextMetadata {
    pub max_context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub cache_breakpoints: Option<CacheBreakpointSupport>,
}

impl ModelDriver for ProviderRuntime {
    fn stream(
        &self,
        alias: &str,
        request: ProviderRequest,
    ) -> Result<BoxEventStream, AgentLoopError> {
        self.stream_alias(alias, request)
            .map_err(|error| AgentLoopError::Provider(error.to_string()))
    }

    fn context_metadata(&self, alias: &str) -> ModelContextMetadata {
        self.resolved_alias_capabilities(alias).map_or_else(
            ModelContextMetadata::default,
            |capabilities| ModelContextMetadata {
                max_context_tokens: capabilities.max_context_tokens,
                max_output_tokens: capabilities.max_output_tokens,
                cache_breakpoints: Some(capabilities.cache_breakpoints),
            },
        )
    }

    fn has_model_alias(&self, alias: &str) -> bool {
        self.resolved_alias_capabilities(alias).is_some()
    }

    fn supports_vision(&self, alias: &str) -> bool {
        self.resolved_alias_capabilities(alias)
            .is_some_and(|capabilities| capabilities.vision)
    }

    fn compaction_config(&self) -> CompactionConfig {
        ProviderRuntime::compaction_config(self).clone()
    }

    fn budget_config(&self) -> BudgetConfig {
        ProviderRuntime::budget_config(self).clone()
    }

    fn cost(&self, alias: &str, usage: TokenUsage) -> Cost {
        self.accounting_for_alias(alias, usage)
    }

    fn cost_for_reported_model(
        &self,
        alias: &str,
        reported_model: Option<&str>,
        usage: TokenUsage,
    ) -> Cost {
        self.accounting_for_reported_model(alias, reported_model, usage)
    }

    fn cost_for_route(
        &self,
        _alias: &str,
        route: Option<&str>,
        _reported_model: Option<&str>,
        usage: TokenUsage,
    ) -> Cost {
        self.accounting_for_route(route, usage)
    }
}

/// Stable turn-loop construction or runtime failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AgentLoopError {
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

/// Unstamped event assembled inside the single-writer actor. The only public,
/// persisted, or streamed representation is [`EngineEvent`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PendingEvent {
    SessionCreated {
        driver_client_id: ClientId,
    },
    TurnStarted {
        turn: u64,
    },
    UserMessageAccepted {
        turn: u64,
        content: String,
        attachments: Vec<StoredAttachment>,
    },
    ConversationTurnCommitted {
        agent_turn: u64,
        turn: Turn,
    },
    ConversationRewound {
        to_turn: u64,
        operation_id: String,
        unrestorable_paths: Vec<UnrestorablePath>,
    },
    MessageQueued {
        position: usize,
        content: String,
        attachments: Vec<StoredAttachment>,
    },
    TextDelta {
        turn: u64,
        text: String,
    },
    ThinkingDelta {
        turn: u64,
        content: String,
        signature: Option<String>,
    },
    CitationDelta {
        turn: u64,
        uri: String,
        title: Option<String>,
    },
    ToolCallStarted {
        turn: u64,
        id: String,
        name: String,
        arguments: Value,
        index: usize,
    },
    PermissionRequested {
        turn: u64,
        request: PermissionRequest,
    },
    ToolOutput {
        turn: u64,
        id: String,
        stream: String,
        chunk: String,
    },
    ToolCallFinished {
        turn: u64,
        id: String,
        output: ToolOutput,
        is_error: bool,
        index: usize,
    },
    HookFailure {
        event: String,
        hook_id: String,
        fail_closed: bool,
        message: String,
    },
    CommandFinished {
        name: String,
        message: String,
        unrestorable_paths: Vec<UnrestorablePath>,
    },
    GuardTriggered {
        turn: u64,
        guard: String,
        message: String,
    },
    TurnFinished {
        turn: u64,
        status: AgentTurnStatus,
        usage: SessionUsage,
        cost: Cost,
    },
    ContextUsage {
        turn: u64,
        used_tokens: u64,
        usable_tokens: u64,
        reserved_tokens: u64,
        stable_prefix_hash: String,
        cache_hit_basis_points: u16,
        estimated_input_tokens: u64,
        provider_input_tokens: u64,
        correction_millionths: u64,
    },
    BudgetStatus {
        turn: u64,
        level: BudgetLevel,
        scope: BudgetScope,
        unit: BudgetUnit,
        current: u64,
        limit: u64,
    },
    CompactionStarted {
        reason: CompactionReason,
    },
    CompactionAttemptFinished {
        summary_turn: u64,
        usage: SessionUsage,
        cost: Cost,
    },
    CompactionFinished {
        summary_turn: u64,
        reclaimed_tokens: u64,
        usage: Option<SessionUsage>,
        cost: Option<Cost>,
    },
    ToolOutputPruned {
        tool_call_id: String,
        reclaimed_tokens: u64,
    },
    ContextItemPinned {
        item_id: ContextItemId,
        effective_after_agent_turn: u64,
    },
    ContextItemEvicted {
        item_id: ContextItemId,
        effective_after_agent_turn: u64,
    },
    Error {
        message: String,
    },
    DriverChanged {
        driver_client_id: ClientId,
    },
    ModelChanged {
        model: ModelAlias,
    },
    UserShellStateChanged {
        shell_id: ShellId,
        command: String,
        active: bool,
        status: Option<i32>,
        captured_output: Option<String>,
    },
    QuestionAsked {
        turn: u64,
        question_id: QuestionId,
        questions: Vec<Question>,
    },
    QuestionAnswered {
        turn: u64,
        question_id: QuestionId,
        answers: Vec<Answer>,
    },
}

fn wire_turn_id(turn: u64) -> TurnId {
    TurnId(turn.to_string())
}

fn unavailable_cost() -> Cost {
    Cost::Unavailable {
        reason: "provider accounting is unavailable".to_owned(),
    }
}

/// Replay-injectable timestamp source for event metadata.
pub trait EventClock: Send + Sync {
    fn emitted_at(&self) -> String;

    /// Milliseconds since the Unix epoch used for deterministic budget windows.
    fn unix_time_millis(&self) -> u64 {
        0
    }
}

/// UTC wall-clock timestamps for production sessions.
#[derive(Debug, Default)]
pub struct SystemEventClock;

impl EventClock for SystemEventClock {
    fn emitted_at(&self) -> String {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        format_unix_rfc3339(elapsed.as_secs(), elapsed.subsec_millis())
    }

    fn unix_time_millis(&self) -> u64 {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    }
}

/// Time bounds for a storage-neutral cross-session accounting query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetLedgerQuery {
    pub now_unix_ms: u64,
    pub utc_day_start_unix_ms: u64,
    pub trailing_minute_start_unix_ms: u64,
}

/// Reconciled totals supplied by durable storage for budget enforcement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetLedgerTotals {
    /// True when totals came from a durable reconciled cross-session ledger.
    pub authoritative: bool,
    pub session_cost_micros_usd: u64,
    pub session_ai_credit_micros: u64,
    pub daily_cost_micros_usd: u64,
    pub daily_ai_credit_micros: u64,
    pub trailing_minute_cost_micros_usd: u64,
    pub trailing_minute_ai_credit_micros: u64,
    pub session_subscription_quota_entries: u64,
    pub session_cost_unavailable_entries: u64,
    pub session_non_usd_monetary_entries: u64,
    pub daily_subscription_quota_entries: u64,
    pub daily_cost_unavailable_entries: u64,
    pub daily_non_usd_monetary_entries: u64,
}

fn format_unix_rfc3339(seconds: u64, millis: u32) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch.saturating_add(719_468);
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn event_meta(event: &EngineEvent) -> Option<&EventMeta> {
    event.meta()
}

impl PendingEvent {
    #[allow(clippy::too_many_lines)]
    fn stamp(self, meta: EventMeta) -> EngineEvent {
        match self {
            Self::SessionCreated { driver_client_id } => EngineEvent::SessionCreated {
                meta,
                driver_client_id,
            },
            Self::TurnStarted { turn } => EngineEvent::TurnStarted {
                meta,
                turn_id: wire_turn_id(turn),
            },
            Self::UserMessageAccepted {
                turn,
                content,
                attachments,
            } => EngineEvent::UserMessageAccepted {
                meta,
                agent_turn: turn,
                content,
                attachments,
            },
            Self::ConversationTurnCommitted { agent_turn, turn } => {
                EngineEvent::ConversationTurnCommitted {
                    meta,
                    agent_turn,
                    turn,
                }
            }
            Self::ConversationRewound {
                to_turn,
                operation_id,
                unrestorable_paths,
            } => EngineEvent::ConversationRewound {
                meta,
                to_agent_turn: to_turn,
                operation_id,
                unrestorable_paths,
            },
            Self::MessageQueued {
                position,
                content,
                attachments,
            } => EngineEvent::MessageQueued {
                meta,
                position: u64::try_from(position).unwrap_or(u64::MAX),
                content,
                attachments,
            },
            Self::TextDelta { turn, text } => EngineEvent::TextDelta {
                meta,
                turn_id: wire_turn_id(turn),
                text,
            },
            Self::ThinkingDelta {
                turn,
                content,
                signature,
            } => EngineEvent::ThinkingDelta {
                meta,
                turn_id: wire_turn_id(turn),
                text: content,
                signature,
            },
            Self::CitationDelta { turn, uri, title } => EngineEvent::CitationDelta {
                meta,
                turn_id: wire_turn_id(turn),
                uri,
                title,
            },
            Self::ToolCallStarted {
                turn,
                id,
                name,
                arguments,
                index,
            } => EngineEvent::ToolCallStarted {
                meta,
                turn_id: wire_turn_id(turn),
                tool_call_id: ToolCallId(id),
                name,
                args: arguments,
                call_index: u32::try_from(index).unwrap_or(u32::MAX),
            },
            Self::PermissionRequested { turn, request } => {
                let diff = permission_diff(&request);
                EngineEvent::ToolApprovalNeeded {
                    meta,
                    turn_id: wire_turn_id(turn),
                    tool_call_id: ToolCallId(request.id),
                    name: request.tool_name.clone(),
                    args: request.arguments,
                    capabilities: request.capabilities,
                    rationale: format!("permission required for tool `{}`", request.tool_name),
                    diff,
                }
            }
            Self::ToolOutput {
                turn,
                id,
                stream,
                chunk,
            } => EngineEvent::ToolOutputDelta {
                meta,
                turn_id: wire_turn_id(turn),
                tool_call_id: ToolCallId(id),
                stream: if stream == "stderr" {
                    ToolOutputStream::Stderr
                } else {
                    ToolOutputStream::Stdout
                },
                chunk,
            },
            Self::ToolCallFinished {
                turn,
                id,
                output,
                is_error,
                index,
            } => EngineEvent::ToolCallFinished {
                meta,
                turn_id: wire_turn_id(turn),
                tool_call_id: ToolCallId(id),
                output,
                is_error,
                call_index: u32::try_from(index).unwrap_or(u32::MAX),
            },
            Self::HookFailure {
                event,
                hook_id,
                fail_closed,
                message,
            } => EngineEvent::HookFailed {
                meta,
                event,
                hook_id,
                fail_closed,
                message,
            },
            Self::CommandFinished {
                name,
                message,
                unrestorable_paths,
            } => EngineEvent::CommandFinished {
                meta,
                name,
                message,
                unrestorable_paths,
            },
            Self::GuardTriggered {
                turn,
                guard,
                message,
            } => EngineEvent::GuardTriggered {
                meta,
                turn_id: wire_turn_id(turn),
                guard,
                message,
            },
            Self::TurnFinished {
                turn,
                status,
                usage,
                cost,
            } => EngineEvent::TurnFinished {
                meta,
                turn_id: wire_turn_id(turn),
                status: status.into(),
                usage: usage.into(),
                cost,
            },
            Self::ContextUsage {
                turn,
                used_tokens,
                usable_tokens,
                reserved_tokens,
                stable_prefix_hash,
                cache_hit_basis_points,
                estimated_input_tokens,
                provider_input_tokens,
                correction_millionths,
            } => EngineEvent::ContextUsageUpdated {
                meta,
                turn_id: wire_turn_id(turn),
                used_tokens,
                usable_tokens,
                reserved_tokens,
                stable_prefix_hash,
                cache_hit_basis_points,
                estimated_input_tokens,
                provider_input_tokens,
                correction_millionths,
            },
            Self::BudgetStatus {
                turn,
                level,
                scope,
                unit,
                current,
                limit,
            } => EngineEvent::BudgetStatusChanged {
                meta,
                turn_id: wire_turn_id(turn),
                level,
                scope,
                unit,
                current,
                limit,
            },
            Self::CompactionStarted { reason } => EngineEvent::CompactionStarted { meta, reason },
            Self::CompactionAttemptFinished {
                summary_turn,
                usage,
                cost,
            } => EngineEvent::CompactionAttemptFinished {
                meta,
                summary_turn_id: wire_turn_id(summary_turn),
                usage: usage.into(),
                cost,
            },
            Self::CompactionFinished {
                summary_turn,
                reclaimed_tokens,
                usage,
                cost,
            } => EngineEvent::CompactionFinished {
                meta,
                summary_turn_id: wire_turn_id(summary_turn),
                reclaimed_tokens,
                usage: usage.map(Into::into),
                cost,
            },
            Self::ToolOutputPruned {
                tool_call_id,
                reclaimed_tokens,
            } => EngineEvent::ToolOutputPruned {
                meta,
                tool_call_id: ToolCallId(tool_call_id),
                reclaimed_tokens,
            },
            Self::ContextItemPinned {
                item_id,
                effective_after_agent_turn,
            } => EngineEvent::ContextItemPinned {
                meta,
                item_id,
                effective_after_agent_turn,
            },
            Self::ContextItemEvicted {
                item_id,
                effective_after_agent_turn,
            } => EngineEvent::ContextItemEvicted {
                meta,
                item_id,
                effective_after_agent_turn,
            },
            Self::Error { message } => EngineEvent::Error {
                meta,
                error: EngineError {
                    category: EngineErrorCategory::Internal,
                    code: "agent_loop".to_owned(),
                    message,
                    retryable: false,
                    details: None,
                },
            },
            Self::DriverChanged { driver_client_id } => EngineEvent::DriverChanged {
                meta,
                driver_client_id,
            },
            Self::ModelChanged { model } => EngineEvent::ModelChanged { meta, model },
            Self::UserShellStateChanged {
                shell_id,
                command,
                active,
                status,
                captured_output,
            } => EngineEvent::UserShellStateChanged {
                meta,
                shell_id,
                command: Some(command),
                active,
                status,
                captured_output,
            },
            Self::QuestionAsked {
                turn,
                question_id,
                questions,
            } => EngineEvent::QuestionAsked {
                meta,
                turn_id: wire_turn_id(turn),
                question_id,
                questions,
            },
            Self::QuestionAnswered {
                turn,
                question_id,
                answers,
            } => EngineEvent::QuestionAnswered {
                meta,
                turn_id: wire_turn_id(turn),
                question_id,
                answers,
            },
        }
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
    pub conversation: Vec<Turn>,
    pub queued_messages: Vec<String>,
    pub running: bool,
    pub completed_turns: u64,
    pub model_alias: String,
    pub active_shell: Option<RecoveredUserShell>,
}

/// Persisted actor state supplied when resuming a session from its event log.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRecoveredState {
    pub conversation: Vec<Turn>,
    pub queued_messages: Vec<String>,
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
    pub active_shell: Option<RecoveredUserShell>,
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
}

fn parse_turn_id(turn_id: &TurnId) -> Result<u64, SessionProjectionError> {
    turn_id
        .0
        .parse()
        .map_err(|_| SessionProjectionError::InvalidTurnId(turn_id.0.clone()))
}

#[allow(clippy::match_same_arms, clippy::too_many_lines)]
fn recovered_pending_event(
    event: &EngineEvent,
) -> Result<Option<PendingEvent>, SessionProjectionError> {
    let pending = match event {
        EngineEvent::CommandAcknowledged { .. } => {
            return Err(SessionProjectionError::ConnectionScopedEvent);
        }
        EngineEvent::ContextSnapshotReady { .. }
        | EngineEvent::CostSnapshotReady { .. }
        | EngineEvent::PromptDumpReady { .. }
        | EngineEvent::SessionReplayCompleted { .. }
        | EngineEvent::SessionsListed { .. }
        | EngineEvent::CommandDescriptorsListed { .. }
        | EngineEvent::ModelsListed { .. }
        | EngineEvent::WorkspaceFilesFound { .. }
        | EngineEvent::WorkspaceFilePreviewReady { .. }
        | EngineEvent::WorkspaceStatusReady { .. }
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
            position: usize::try_from(*position).unwrap_or(usize::MAX),
            content: content.clone(),
            attachments: attachments.clone(),
        },
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
            ..
        } => PendingEvent::PermissionRequested {
            turn: parse_turn_id(turn_id)?,
            request: PermissionRequest {
                id: tool_call_id.0.clone(),
                tool_name: name.clone(),
                arguments: args.clone(),
                capabilities: capabilities.clone(),
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
        EngineEvent::ModelChanged { model, .. } => PendingEvent::ModelChanged {
            model: model.clone(),
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
        EngineEvent::SubagentSpawned { .. }
        | EngineEvent::SubagentFinished { .. }
        | EngineEvent::ModeChanged { .. } => return Ok(None),
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
    let mut conversation = Vec::new();
    let mut conversation_agent_turns = Vec::new();
    let mut queued = VecDeque::new();
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
    let mut active_shell = None::<RecoveredUserShell>;
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
            PendingEvent::MessageQueued { content, .. } => queued.push_back(content.clone()),
            PendingEvent::UserMessageAccepted { turn, content, .. } => {
                if let Some(position) = queued.iter().position(|queued| queued == content) {
                    queued.remove(position);
                }
                uncommitted_users
                    .entry(*turn)
                    .or_default()
                    .push(content.clone());
            }
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
            }
            PendingEvent::TextDelta { turn, text } if active_turn == Some(*turn) => {
                append_text(&mut partial_assistant_blocks, text);
            }
            PendingEvent::ThinkingDelta {
                turn,
                content,
                signature,
            } if active_turn == Some(*turn) => {
                partial_assistant_blocks.push(Block::Thinking {
                    content: content.clone(),
                    signature: signature.clone(),
                });
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
            | PendingEvent::ToolOutput { .. }
            | PendingEvent::ToolCallFinished { .. }
            | PendingEvent::HookFailure { .. }
            | PendingEvent::CommandFinished { .. }
            | PendingEvent::GuardTriggered { .. }
            | PendingEvent::BudgetStatus { .. } => {}
            PendingEvent::Error { .. } => {
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
            PendingEvent::SessionCreated {
                driver_client_id: driver,
            }
            | PendingEvent::DriverChanged {
                driver_client_id: driver,
            } => {
                driver_client_id = Some(driver.clone());
            }
            PendingEvent::ModelChanged { model } => {
                model_alias = Some(model.0.clone());
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
                active: false,
                ..
            } => {
                if active_shell.as_ref().map(|shell| &shell.shell_id) != Some(shell_id) {
                    return Err(SessionProjectionError::InvalidShellTransition(
                        "shell end did not match the active shell id".to_owned(),
                    ));
                }
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
        conversation,
        queued_messages: queued.into_iter().collect(),
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
        active_shell,
    })
}

/// Provider/UI-neutral durability boundary for the sequenced session log.
///
/// Implementations must not return until the event is durably appended. The
/// actor invokes this boundary before making the event visible to subscribers.
#[async_trait]
pub trait SessionEventSink: Send + Sync {
    /// Durably append exactly the fully stamped protocol event supplied by the
    /// actor and return that same event after persistence completes.
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError>;

    /// Appends an ordered event batch.
    ///
    /// The extensible default appends sequentially and may leave a recoverable
    /// persisted prefix if a later append fails. Implementations with a native
    /// batch primitive should override this to share one durable sync.
    async fn append_batch(
        &self,
        batch: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        let mut events = Vec::with_capacity(batch.len());
        for event in batch {
            events.push(self.append(event).await?);
        }
        Ok(events)
    }

    /// Reads every persisted event strictly after `last_seen`, or the whole
    /// log when `last_seen` is `None`. Implementations must return a contiguous
    /// sequence and never synthesize connection-scoped acknowledgements.
    async fn read_after(
        &self,
        last_seen: Option<SequenceId>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError>;

    /// Returns the current durable tail without relying on the finite live
    /// broadcast buffer.
    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        Ok(self
            .read_after(None)
            .await?
            .last()
            .and_then(EngineEvent::meta)
            .map(|meta| meta.sequence_id))
    }

    /// Returns reconciled session, UTC-day, and trailing-minute spend totals.
    /// Ephemeral sinks have no cross-session ledger and therefore return zero.
    async fn budget_totals(
        &self,
        _query: BudgetLedgerQuery,
    ) -> Result<BudgetLedgerTotals, AgentLoopError> {
        Ok(BudgetLedgerTotals::default())
    }
}

/// Event sink for ephemeral sessions and deterministic unit tests.
#[derive(Debug, Default)]
pub struct NoopSessionEventSink {
    next_sequence: Mutex<u64>,
    events: Mutex<Vec<EngineEvent>>,
}

impl NoopSessionEventSink {
    #[must_use]
    pub fn new(last_sequence: Option<SequenceId>) -> Self {
        Self {
            next_sequence: Mutex::new(
                last_sequence.map_or(0, |sequence| sequence.0.saturating_add(1)),
            ),
            events: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl SessionEventSink for NoopSessionEventSink {
    async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
        let mut next = self
            .next_sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let meta = event_meta(&event).ok_or_else(|| {
            AgentLoopError::Persistence(
                "connection-scoped acknowledgement cannot enter a session log".to_owned(),
            )
        })?;
        if meta.sequence_id.0 != *next {
            return Err(AgentLoopError::Persistence(format!(
                "event sequence {} does not match expected {}",
                meta.sequence_id.0, *next
            )));
        }
        *next = next
            .checked_add(1)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
        Ok(event)
    }

    async fn append_batch(
        &self,
        batch: Vec<EngineEvent>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        let count = u64::try_from(batch.len())
            .map_err(|_| AgentLoopError::Persistence("event batch length overflow".to_owned()))?;
        let mut next = self
            .next_sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let advanced = next
            .checked_add(count)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
        for (offset, event) in batch.iter().enumerate() {
            let offset = u64::try_from(offset)
                .map_err(|_| AgentLoopError::Persistence("event batch overflow".to_owned()))?;
            let sequence = next
                .checked_add(offset)
                .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
            let meta = event_meta(event).ok_or_else(|| {
                AgentLoopError::Persistence(
                    "connection-scoped acknowledgement cannot enter a session log".to_owned(),
                )
            })?;
            if meta.sequence_id.0 != sequence {
                return Err(AgentLoopError::Persistence(format!(
                    "event sequence {} does not match expected {sequence}",
                    meta.sequence_id.0
                )));
            }
        }
        *next = advanced;
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(batch.iter().cloned());
        Ok(batch)
    }

    async fn read_after(
        &self,
        last_seen: Option<SequenceId>,
    ) -> Result<Vec<EngineEvent>, AgentLoopError> {
        let first = last_seen.map_or(0, |sequence| sequence.0.saturating_add(1));
        Ok(self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|event| event_meta(event).is_some_and(|meta| meta.sequence_id.0 >= first))
            .cloned()
            .collect())
    }

    async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        let next = *self
            .next_sequence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(next.checked_sub(1).map(SequenceId))
    }
}

/// Opaque checkpoint handle returned before a mutating tool starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationCheckpoint {
    pub id: Option<String>,
}

/// Terminal disposition reported after a checkpointed tool attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationCheckpointOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// Opaque handle for a prepared and applied rewind transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindCheckpoint {
    pub id: String,
    pub unrestorable_paths: Vec<UnrestorablePath>,
}

/// Storage-neutral boundary used around every mutating tool execution.
#[async_trait]
pub trait MutationCheckpointCoordinator: Send + Sync {
    async fn begin(
        &self,
        session_id: &SessionId,
        agent_turn: u64,
        tool_call_id: &str,
        scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError>;

    async fn finish(
        &self,
        checkpoint: &MutationCheckpoint,
        outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError>;

    async fn prepare_apply_rewind(
        &self,
        session_id: &SessionId,
        to_turn: u64,
        operation_id: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError>;

    async fn acknowledge_rewind(&self, checkpoint: &RewindCheckpoint)
    -> Result<(), AgentLoopError>;
}

/// Checkpoint coordinator for read-only or ephemeral sessions.
#[derive(Debug, Default)]
pub struct NoopMutationCheckpointCoordinator;

#[async_trait]
impl MutationCheckpointCoordinator for NoopMutationCheckpointCoordinator {
    async fn begin(
        &self,
        _session_id: &SessionId,
        _agent_turn: u64,
        _tool_call_id: &str,
        _scope: &MutationScope,
    ) -> Result<MutationCheckpoint, AgentLoopError> {
        Ok(MutationCheckpoint { id: None })
    }

    async fn finish(
        &self,
        _checkpoint: &MutationCheckpoint,
        _outcome: MutationCheckpointOutcome,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }

    async fn prepare_apply_rewind(
        &self,
        _session_id: &SessionId,
        _to_turn: u64,
        operation_id: &str,
    ) -> Result<RewindCheckpoint, AgentLoopError> {
        Ok(RewindCheckpoint {
            id: operation_id.to_owned(),
            unrestorable_paths: Vec::new(),
        })
    }

    async fn acknowledge_rewind(
        &self,
        _checkpoint: &RewindCheckpoint,
    ) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

/// Engine-owned slash-command context. Public handlers use this exact type.
#[derive(Clone, Debug, Default)]
pub struct SessionCommandContext {
    running: bool,
    queued_messages: usize,
}

impl SessionCommandContext {
    #[must_use]
    pub const fn running(&self) -> bool {
        self.running
    }

    #[must_use]
    pub const fn queued_messages(&self) -> usize {
        self.queued_messages
    }
}

/// Command result interpreted by the actor after common registry dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCommandOutput {
    pub message: String,
    pub action: SessionCommandAction,
}

/// Actor action requested by a command handler.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SessionCommandAction {
    #[default]
    None,
    Interrupt,
    Rewind {
        to_turn: u64,
    },
    Context,
    PinContext {
        item_id: ContextItemId,
    },
    EvictContext {
        item_id: ContextItemId,
    },
    Cost,
    Compact {
        instructions: Option<String>,
    },
}

struct StatusCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for StatusCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        _invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: format!(
                "running={}, queued_messages={}",
                context.running, context.queued_messages
            ),
            action: SessionCommandAction::None,
        })
    }
}

struct InterruptCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for InterruptCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        _invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: "interrupt requested".to_owned(),
            action: SessionCommandAction::Interrupt,
        })
    }
}

struct HelpCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for HelpCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        _invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        Ok(SessionCommandOutput {
            message: "/help, /status, /interrupt, /rewind <turn>, /context [pin|evict <item-id>], /cost, /compact [instructions]".to_owned(),
            action: SessionCommandAction::None,
        })
    }
}

struct ContextCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for ContextCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        let arguments = invocation.arguments().trim();
        let action = if arguments.is_empty() {
            SessionCommandAction::Context
        } else {
            if context.running() {
                return Err(CommandExecutionError::new(
                    "turn_running",
                    "context surgery requires an idle session",
                ));
            }
            let Some((operation, item_id)) = arguments.split_once(char::is_whitespace) else {
                return Err(CommandExecutionError::new(
                    "invalid_context_command",
                    "usage: /context [pin <item-id> | evict <item-id>]",
                ));
            };
            let item_id = item_id.trim();
            if item_id.is_empty() {
                return Err(CommandExecutionError::new(
                    "invalid_context_command",
                    "usage: /context [pin <item-id> | evict <item-id>]",
                ));
            }
            match operation {
                "pin" => SessionCommandAction::PinContext {
                    item_id: ContextItemId(item_id.to_owned()),
                },
                "evict" => SessionCommandAction::EvictContext {
                    item_id: ContextItemId(item_id.to_owned()),
                },
                _ => {
                    return Err(CommandExecutionError::new(
                        "invalid_context_command",
                        "usage: /context [pin <item-id> | evict <item-id>]",
                    ));
                }
            }
        };
        Ok(SessionCommandOutput {
            message: String::new(),
            action,
        })
    }
}

struct CostCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for CostCommand {
    async fn execute(
        &self,
        _context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        if !invocation.arguments().trim().is_empty() {
            return Err(CommandExecutionError::new(
                "invalid_cost_command",
                "usage: /cost",
            ));
        }
        Ok(SessionCommandOutput {
            message: String::new(),
            action: SessionCommandAction::Cost,
        })
    }
}

struct CompactCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for CompactCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        if context.running() {
            return Err(CommandExecutionError::new(
                "turn_running",
                "manual compaction requires an idle session",
            ));
        }
        let instructions = invocation.arguments().trim();
        Ok(SessionCommandOutput {
            message: "compaction started".to_owned(),
            action: SessionCommandAction::Compact {
                instructions: (!instructions.is_empty()).then(|| instructions.to_owned()),
            },
        })
    }
}

struct RewindCommand;

#[async_trait]
impl CommandHandler<SessionCommandContext, SessionCommandOutput> for RewindCommand {
    async fn execute(
        &self,
        context: &mut SessionCommandContext,
        invocation: CommandInvocation,
    ) -> Result<SessionCommandOutput, CommandExecutionError> {
        if context.running() {
            return Err(CommandExecutionError::new(
                "turn_running",
                "interrupt the active turn before rewinding",
            ));
        }
        let to_turn = invocation
            .arguments()
            .parse::<u64>()
            .map_err(|_| CommandExecutionError::new("invalid_turn", "usage: /rewind <turn>"))?;
        Ok(SessionCommandOutput {
            message: format!("rewound to turn {to_turn}"),
            action: SessionCommandAction::Rewind { to_turn },
        })
    }
}

/// Registers core commands through rw-ext's public registry API.
///
/// # Errors
///
/// Returns an extension error if any built-in registration is invalid.
pub fn builtin_command_registry()
-> Result<CommandRegistry<SessionCommandContext, SessionCommandOutput>, AgentLoopError> {
    let mut registry = CommandRegistry::new();
    registry
        .register(
            CommandDescriptor::new("help", "List available commands"),
            HelpCommand,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    registry
        .register(
            CommandDescriptor::new("status", "Show actor running and queue state"),
            StatusCommand,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    registry
        .register(
            CommandDescriptor::new("rewind", "Restore a completed turn checkpoint")
                .with_argument_hint("<turn>"),
            RewindCommand,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    registry
        .register(
            CommandDescriptor::new("interrupt", "Interrupt the active turn"),
            InterruptCommand,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    registry
        .register(
            CommandDescriptor::new("context", "Inspect, pin, or evict context items")
                .with_argument_hint("[pin|evict <item-id>]"),
            ContextCommand,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    registry
        .register(
            CommandDescriptor::new("cost", "Show usage, cost, and budget accounting"),
            CostCommand,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    registry
        .register(
            CommandDescriptor::new("compact", "Compact conversation context")
                .with_argument_hint("[instructions]"),
            CompactCommand,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    Ok(registry)
}

struct ValidateToolHook;

#[async_trait]
impl HookHandler for ValidateToolHook {
    async fn invoke(
        &self,
        invocation: HookInvocation<'_>,
    ) -> Result<HookDirective, rw_ext::HookError> {
        let valid = invocation
            .payload()
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.trim().is_empty());
        if valid {
            Ok(HookDirective::Continue)
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
            HookRegistration::new("core.validate-tool", HookEvent::PreTool)
                .with_priority(i32::MIN)
                .with_failure_policy(HookFailurePolicy::FailClosed),
            ValidateToolHook,
        )
        .map_err(|error| AgentLoopError::Extension(error.to_string()))?;
    Ok(dispatcher)
}

/// Dependencies and guardrails for one headless session actor.
pub struct SessionActorConfig {
    pub session_id: SessionId,
    pub workspace_root: PathBuf,
    pub initial_session_context: Vec<Turn>,
    pub model_alias: String,
    pub model: Arc<dyn ModelDriver>,
    pub tools: Arc<ToolRegistry>,
    pub permissions: Arc<PermissionGate>,
    pub hooks: Arc<HookDispatcher>,
    pub commands: Arc<CommandRegistry<SessionCommandContext, SessionCommandOutput>>,
    pub event_sink: Arc<dyn SessionEventSink>,
    pub event_clock: Arc<dyn EventClock>,
    pub secret_redactor: Arc<dyn SecretRedactor>,
    pub checkpoints: Arc<dyn MutationCheckpointCoordinator>,
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
            .field("initial_session_context", &self.initial_session_context)
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
    fn with_model_alias(&self, model_alias: String) -> Self {
        Self {
            session_id: self.session_id.clone(),
            workspace_root: self.workspace_root.clone(),
            initial_session_context: self.initial_session_context.clone(),
            model_alias,
            model: Arc::clone(&self.model),
            tools: Arc::clone(&self.tools),
            permissions: Arc::clone(&self.permissions),
            hooks: Arc::clone(&self.hooks),
            commands: Arc::clone(&self.commands),
            event_sink: Arc::clone(&self.event_sink),
            event_clock: Arc::clone(&self.event_clock),
            secret_redactor: Arc::clone(&self.secret_redactor),
            checkpoints: Arc::clone(&self.checkpoints),
            recovered: self.recovered.clone(),
            max_turns: self.max_turns,
            identical_tool_failure_limit: self.identical_tool_failure_limit,
            max_output_tokens: self.max_output_tokens,
            thinking: self.thinking,
            event_capacity: self.event_capacity,
        }
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
        if config.session_id.0.is_empty()
            || config.session_id.0.len() > 128
            || !config
                .session_id
                .0
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(AgentLoopError::InvalidConfiguration(
                "session id must be 1-128 ASCII letters, digits, '-', '_', or '.'".to_owned(),
            ));
        }
        if config.model_alias.trim().is_empty() {
            return Err(AgentLoopError::InvalidConfiguration(
                "model alias must not be empty".to_owned(),
            ));
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
        let tool_context = ToolContext::new(&config.workspace_root)
            .map_err(|error| AgentLoopError::ToolContext(error.to_string()))?
            .with_session_id(config.session_id.clone());
        let (command_tx, command_rx) = mpsc::channel(64);
        let (event_tx, _) = broadcast::channel(config.event_capacity);
        let active_turn = Arc::new(AtomicU64::new(0));
        let handle = SessionHandle {
            commands: command_tx,
            events: event_tx.clone(),
            active_turn: active_turn.clone(),
            session_id: config.session_id.clone(),
            event_sink: Arc::clone(&config.event_sink),
            local_request_sequence: Arc::new(AtomicU64::new(0)),
            local_attached: Arc::new(AtomicBool::new(false)),
            local_last_seen: config.recovered.last_sequence,
        };
        tokio::spawn(run_actor(
            config,
            tool_context,
            command_rx,
            event_tx,
            active_turn,
        ));
        Ok(handle)
    }
}

#[derive(Clone, Debug)]
struct RoutedEvent {
    target: Option<ClientId>,
    event: EngineEvent,
}

/// One client-filtered view of the single engine event channel. A lagged live
/// receiver catches up from the durable source and suppresses duplicate live
/// deliveries by sequence id.
pub struct SessionSubscription {
    client_id: ClientId,
    session_id: SessionId,
    receiver: broadcast::Receiver<RoutedEvent>,
    sink: Arc<dyn SessionEventSink>,
    last_sequence: Option<SequenceId>,
    pending: VecDeque<EngineEvent>,
    needs_initial_replay: bool,
}

impl SessionSubscription {
    /// Receives the next protocol event for this client.
    ///
    /// # Errors
    ///
    /// Returns a persistence error if a broadcast gap cannot be replayed, or
    /// [`AgentLoopError::Closed`] after the actor event channel closes.
    pub async fn recv(&mut self) -> Result<EngineEvent, AgentLoopError> {
        loop {
            if self.needs_initial_replay {
                self.needs_initial_replay = false;
                let gap = self.sink.read_after(self.last_sequence).await?;
                validate_gap(self.last_sequence, &gap, &self.session_id)?;
                self.pending.extend(gap);
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
                    if let Some(meta) = event_meta(&routed.event)
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
                    let gap = self.sink.read_after(self.last_sequence).await?;
                    validate_gap(self.last_sequence, &gap, &self.session_id)?;
                    self.pending.extend(gap);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(AgentLoopError::Closed);
                }
            }
        }
    }

    fn observe(&mut self, event: &EngineEvent) {
        if let Some(meta) = event_meta(event) {
            self.last_sequence = Some(meta.sequence_id);
        }
    }
}

fn validate_gap(
    last_seen: Option<SequenceId>,
    gap: &[EngineEvent],
    session_id: &SessionId,
) -> Result<(), AgentLoopError> {
    let mut expected = last_seen.map_or(0, |sequence| sequence.0.saturating_add(1));
    for event in gap {
        let meta = event_meta(event).ok_or_else(|| {
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
    commands: mpsc::Sender<ActorCommand>,
    events: broadcast::Sender<RoutedEvent>,
    active_turn: Arc<AtomicU64>,
    session_id: SessionId,
    event_sink: Arc<dyn SessionEventSink>,
    local_request_sequence: Arc<AtomicU64>,
    local_attached: Arc<AtomicBool>,
    local_last_seen: Option<SequenceId>,
}

impl SessionHandle {
    /// Stable id of the session routed by this handle.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Current durable event-log tail used by host reconnect completion.
    ///
    /// # Errors
    ///
    /// Returns a persistence error when the durable sink cannot read its tail.
    pub async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
        self.event_sink.last_sequence().await
    }

    fn local_meta(&self) -> CommandMeta {
        let request = self.local_request_sequence.fetch_add(1, Ordering::Relaxed);
        CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId("local".to_owned()),
            request_id: RequestId(format!("local-{request}")),
        }
    }

    async fn ensure_local_driver(&self) -> Result<(), AgentLoopError> {
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

    /// Subscribes to sequenced actor events.
    #[must_use]
    pub fn subscribe(&self) -> SessionSubscription {
        self.subscribe_client(ClientId("local".to_owned()), self.local_last_seen)
    }

    /// Subscribes one protocol client, optionally starting after a previously
    /// observed durable sequence.
    #[must_use]
    pub fn subscribe_client(
        &self,
        client_id: ClientId,
        last_sequence: Option<SequenceId>,
    ) -> SessionSubscription {
        SessionSubscription {
            client_id,
            session_id: self.session_id.clone(),
            receiver: self.events.subscribe(),
            sink: Arc::clone(&self.event_sink),
            last_sequence,
            pending: VecDeque::new(),
            needs_initial_replay: true,
        }
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
            ProtocolCompletion::Cost(snapshot) => Ok(snapshot),
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

enum ActorCommand {
    Protocol {
        command: ClientCommand,
        respond: oneshot::Sender<CommandOutcome>,
        completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
    },
    SendMessage {
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

enum ProtocolCompletion {
    Message(MessageDisposition),
    Rewind(Vec<UnrestorablePath>),
    Context(ContextSnapshot),
    Cost(CostSnapshot),
    Prompt(PromptDump),
    Unit,
}

struct RunningTurn {
    id: u64,
    cancellation: CancellationToken,
    caused_by: Option<RequestId>,
}

enum TurnSignal {
    Event(PendingEvent),
    ToolOutput {
        event: PendingEvent,
        _permit: OwnedSemaphorePermit,
    },
    DurableEvent {
        kind: PendingEvent,
        respond: oneshot::Sender<Result<(), AgentLoopError>>,
    },
    Approval {
        request: PermissionRequest,
        respond: oneshot::Sender<ApprovalDecision>,
    },
    Question {
        request: AskUserInput,
        respond: oneshot::Sender<String>,
    },
    Complete(TurnOutcome),
    ManualCompactionComplete {
        turn: u64,
        conversation: Vec<Turn>,
        context_surgery: Vec<ContextSurgeryAction>,
        result: Result<(), AgentLoopError>,
        completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
    },
}

struct TurnOutcome {
    turn: u64,
    conversation: Vec<Turn>,
    status: AgentTurnStatus,
    usage: SessionUsage,
    cost: Cost,
    deferred_terminal_delta: Option<String>,
    deferred_terminal_turn: Option<Turn>,
    context_surgery: Vec<ContextSurgeryAction>,
    pruned_tool_outputs: BTreeMap<String, u64>,
    budgeter: Budgeter,
}

struct ActorState {
    session_id: SessionId,
    event_clock: Arc<dyn EventClock>,
    conversation: Vec<Turn>,
    queued: VecDeque<String>,
    running: Option<RunningTurn>,
    pending_approvals: BTreeMap<String, oneshot::Sender<ApprovalDecision>>,
    next_turn: u64,
    completed_turns: u64,
    turn_ends: BTreeMap<u64, usize>,
    sequence: Option<u64>,
    pending_rewind: Option<(u64, RewindCheckpoint)>,
    transient_cause: Option<RequestId>,
    poisoned: bool,
    driver_client_id: Option<ClientId>,
    client_roles: BTreeMap<String, ClientRole>,
    pending_questions: BTreeMap<String, PendingQuestion>,
    next_question: u64,
    context_surgery: Vec<ContextSurgeryAction>,
    pruned_tool_outputs: BTreeMap<String, u64>,
    accounting: Vec<TurnAccounting>,
    budgeter: Budgeter,
    model_alias: String,
    active_shell: Option<RecoveredUserShell>,
}

struct PendingQuestion {
    turn: u64,
    respond: oneshot::Sender<String>,
}

impl ActorState {
    fn recover(
        session_id: SessionId,
        event_clock: Arc<dyn EventClock>,
        default_model_alias: &str,
        recovered: &SessionRecoveredState,
    ) -> Self {
        Self {
            session_id,
            event_clock,
            conversation: recovered.conversation.clone(),
            queued: recovered.queued_messages.iter().cloned().collect(),
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
            next_question: 0,
            context_surgery: recovered.context_surgery.clone(),
            pruned_tool_outputs: recovered.pruned_tool_outputs.clone(),
            accounting: recovered.accounting.clone(),
            budgeter: recovered.budgeter,
            model_alias: recovered
                .model_alias
                .clone()
                .unwrap_or_else(|| default_model_alias.to_owned()),
            active_shell: recovered.active_shell.clone(),
        }
    }

    fn caused_by(&self) -> Option<RequestId> {
        self.transient_cause.clone().or_else(|| {
            self.running
                .as_ref()
                .and_then(|running| running.caused_by.clone())
        })
    }
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
                message: failure.error().to_string(),
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

#[allow(clippy::too_many_lines)]
async fn run_actor(
    config: SessionActorConfig,
    tool_context: ToolContext,
    mut commands: mpsc::Receiver<ActorCommand>,
    events: broadcast::Sender<RoutedEvent>,
    active_turn: Arc<AtomicU64>,
) {
    let mut state = ActorState::recover(
        config.session_id.clone(),
        Arc::clone(&config.event_clock),
        &config.model_alias,
        &config.recovered,
    );
    let interrupted_turn = config.recovered.interrupted_turn;
    let config = Arc::new(config);
    let (turn_signals, mut signals) = mpsc::unbounded_channel();
    if !dispatch_lifecycle_hook(HookEvent::SessionStart, &mut state, &config, &events).await {
        let _ = dispatch_lifecycle_hook(HookEvent::SessionEnd, &mut state, &config, &events).await;
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
        let _ = dispatch_lifecycle_hook(HookEvent::SessionEnd, &mut state, &config, &events).await;
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
            let _ =
                dispatch_lifecycle_hook(HookEvent::SessionEnd, &mut state, &config, &events).await;
            return;
        }
        state.completed_turns = state.completed_turns.saturating_add(1);
        state.turn_ends.insert(turn, state.conversation.len());
    }
    if !state.queued.is_empty() {
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
            let _ =
                dispatch_lifecycle_hook(HookEvent::SessionEnd, &mut state, &config, &events).await;
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
                    &config,
                    &tool_context,
                    &turn_signals,
                    &events,
                    &active_turn,
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
                    if let Some(running) = &state.running {
                        running.cancellation.cancel();
                    }
                    break;
                }
            }
        }
    }
    let _ = dispatch_lifecycle_hook(HookEvent::SessionEnd, &mut state, &config, &events).await;
}

fn client_command_meta(command: &ClientCommand) -> &CommandMeta {
    match command {
        ClientCommand::CreateSession { meta, .. }
        | ClientCommand::ResumeSession { meta, .. }
        | ClientCommand::AttachSession { meta, .. }
        | ClientCommand::SendMessage { meta, .. }
        | ClientCommand::Interrupt { meta, .. }
        | ClientCommand::ApproveTool { meta, .. }
        | ClientCommand::AnswerQuestion { meta, .. }
        | ClientCommand::SwitchMode { meta, .. }
        | ClientCommand::SwitchModel { meta, .. }
        | ClientCommand::Compact { meta, .. }
        | ClientCommand::Fork { meta, .. }
        | ClientCommand::Rewind { meta, .. }
        | ClientCommand::TakeDriver { meta, .. }
        | ClientCommand::UserShellStarted { meta, .. }
        | ClientCommand::UserShellEnded { meta, .. }
        | ClientCommand::PinContext { meta, .. }
        | ClientCommand::EvictContext { meta, .. }
        | ClientCommand::GetContext { meta, .. }
        | ClientCommand::GetCost { meta, .. }
        | ClientCommand::DumpPrompt { meta, .. }
        | ClientCommand::ListSessions { meta, .. }
        | ClientCommand::ListCommands { meta, .. }
        | ClientCommand::ListModels { meta, .. }
        | ClientCommand::SearchWorkspaceFiles { meta, .. }
        | ClientCommand::PreviewWorkspaceFile { meta, .. }
        | ClientCommand::GetWorkspaceStatus { meta, .. }
        | ClientCommand::ShutdownHost { meta, .. } => meta,
    }
}

fn client_command_session(command: &ClientCommand) -> Option<&SessionId> {
    match command {
        ClientCommand::CreateSession { .. }
        | ClientCommand::ListSessions { .. }
        | ClientCommand::ListCommands { .. }
        | ClientCommand::ListModels { .. }
        | ClientCommand::ShutdownHost { .. } => None,
        ClientCommand::ResumeSession { session_id, .. }
        | ClientCommand::AttachSession { session_id, .. }
        | ClientCommand::SendMessage { session_id, .. }
        | ClientCommand::Interrupt { session_id, .. }
        | ClientCommand::ApproveTool { session_id, .. }
        | ClientCommand::AnswerQuestion { session_id, .. }
        | ClientCommand::SwitchMode { session_id, .. }
        | ClientCommand::SwitchModel { session_id, .. }
        | ClientCommand::Compact { session_id, .. }
        | ClientCommand::Fork { session_id, .. }
        | ClientCommand::Rewind { session_id, .. }
        | ClientCommand::TakeDriver { session_id, .. }
        | ClientCommand::UserShellStarted { session_id, .. }
        | ClientCommand::UserShellEnded { session_id, .. }
        | ClientCommand::PinContext { session_id, .. }
        | ClientCommand::EvictContext { session_id, .. }
        | ClientCommand::GetContext { session_id, .. }
        | ClientCommand::GetCost { session_id, .. }
        | ClientCommand::DumpPrompt { session_id, .. }
        | ClientCommand::SearchWorkspaceFiles { session_id, .. }
        | ClientCommand::PreviewWorkspaceFile { session_id, .. }
        | ClientCommand::GetWorkspaceStatus { session_id, .. } => Some(session_id),
    }
}

fn protocol_rejection(code: &str, message: impl Into<String>) -> CommandOutcome {
    CommandOutcome::Rejected {
        error: EngineError {
            category: EngineErrorCategory::Protocol,
            code: code.to_owned(),
            message: message.into(),
            retryable: false,
            details: None,
        },
    }
}

#[allow(clippy::too_many_lines)]
fn prepare_user_message(
    content: &str,
    attachments: &[Attachment],
    model_alias: &str,
    model: &dyn ModelDriver,
) -> Result<PreparedUserMessage, String> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(format!("at most {MAX_ATTACHMENTS} attachments are allowed"));
    }
    let mut total_bytes = 0_usize;
    let mut stored_attachments = Vec::with_capacity(attachments.len());
    let mut attachment_blocks = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        if attachment.name.is_empty()
            || attachment.name.len() > 255
            || attachment.name == "."
            || attachment.name == ".."
            || attachment
                .name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err("attachment names must be safe single path components".to_owned());
        }
        if attachment.media_type.trim() != attachment.media_type
            || attachment.media_type.to_ascii_lowercase() != attachment.media_type
        {
            return Err(
                "attachment media types must be canonical lowercase MIME values".to_owned(),
            );
        }
        let (byte_len, content_hash, block) = match (
            &attachment.data,
            attachment.media_type.as_str(),
        ) {
            (AttachmentData::Text { content }, media_type)
                if media_type.starts_with("text/") || media_type == "application/json" =>
            {
                if content.len() > MAX_TEXT_ATTACHMENT_BYTES {
                    return Err(format!(
                        "text attachment {:?} exceeds {MAX_TEXT_ATTACHMENT_BYTES} bytes",
                        attachment.name
                    ));
                }
                let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
                let text = format!(
                    "Attached file {:?} ({media_type}):\n{content}",
                    attachment.name
                );
                (content.len(), hash, Block::Text { text })
            }
            (AttachmentData::InlineBase64 { data }, media_type)
                if matches!(
                    media_type,
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) =>
            {
                if !model.supports_vision(model_alias) {
                    return Err(format!(
                        "model alias {model_alias:?} does not support image attachments"
                    ));
                }
                let decoded_len = canonical_base64_decoded_len(data).ok_or_else(|| {
                    format!(
                        "image attachment {:?} is not canonical base64",
                        attachment.name
                    )
                })?;
                if decoded_len > MAX_IMAGE_ATTACHMENT_BYTES {
                    return Err(format!(
                        "image attachment {:?} exceeds {MAX_IMAGE_ATTACHMENT_BYTES} decoded bytes",
                        attachment.name
                    ));
                }
                let hash = blake3::hash(data.as_bytes()).to_hex().to_string();
                (
                    decoded_len,
                    hash,
                    Block::Image {
                        media_type: media_type.to_owned(),
                        data: ImageRef::InlineBase64 { data: data.clone() },
                    },
                )
            }
            _ => {
                return Err(format!(
                    "attachment {:?} has unsupported data for media type {:?}",
                    attachment.name, attachment.media_type
                ));
            }
        };
        total_bytes = total_bytes.saturating_add(byte_len);
        if total_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err(format!(
                "attachments exceed the {MAX_TOTAL_ATTACHMENT_BYTES}-byte total limit"
            ));
        }
        stored_attachments.push(StoredAttachment {
            name: attachment.name.clone(),
            media_type: attachment.media_type.clone(),
            content_hash,
            byte_len: u64::try_from(byte_len).unwrap_or(u64::MAX),
        });
        attachment_blocks.push(block);
    }
    if content.is_empty() && attachment_blocks.is_empty() {
        return Err("message content and attachments cannot both be empty".to_owned());
    }
    Ok(PreparedUserMessage {
        content: content.to_owned(),
        stored_attachments,
        attachment_blocks,
    })
}

fn canonical_base64_decoded_len(data: &str) -> Option<usize> {
    if data.is_empty() || !data.len().is_multiple_of(4) || !data.is_ascii() {
        return None;
    }
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    if padding > 2 {
        return None;
    }
    let payload_len = data.len().checked_sub(padding)?;
    if data
        .bytes()
        .take(payload_len)
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')))
        || data.bytes().skip(payload_len).any(|byte| byte != b'=')
    {
        return None;
    }
    data.len()
        .checked_div(4)?
        .checked_mul(3)?
        .checked_sub(padding)
}

fn send_ack(
    state: &ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    meta: &CommandMeta,
    session_id: Option<SessionId>,
    outcome: CommandOutcome,
) {
    let _ = events.send(RoutedEvent {
        target: Some(meta.client_id.clone()),
        event: EngineEvent::CommandAcknowledged {
            meta: CommandAckMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: meta.client_id.clone(),
                request_id: meta.request_id.clone(),
                emitted_at: state.event_clock.emitted_at(),
            },
            session_id,
            outcome,
        },
    });
}

fn send_connection_event(
    events: &broadcast::Sender<RoutedEvent>,
    client_id: &ClientId,
    event: EngineEvent,
) {
    let _ = events.send(RoutedEvent {
        target: Some(client_id.clone()),
        event,
    });
}

fn query_meta(state: &ActorState, meta: &CommandMeta) -> CommandAckMeta {
    CommandAckMeta {
        protocol_version: PROTOCOL_VERSION,
        client_id: meta.client_id.clone(),
        request_id: meta.request_id.clone(),
        emitted_at: state.event_clock.emitted_at(),
    }
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(usage.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(usage.cache_write_tokens);
    total.reasoning_tokens = total
        .reasoning_tokens
        .saturating_add(usage.reasoning_tokens);
}

fn cost_units(cost: &Cost) -> (u64, u64) {
    match cost {
        Cost::Monetary {
            amount_micros,
            currency,
        } if currency.eq_ignore_ascii_case("USD") => (*amount_micros, 0),
        Cost::AiCredits { credits_micros, .. } => (0, *credits_micros),
        Cost::Monetary { .. } | Cost::SubscriptionQuota { .. } | Cost::Unavailable { .. } => (0, 0),
    }
}

fn dollar_accounting_complete(cost: &Cost) -> bool {
    matches!(cost, Cost::Monetary { currency, .. } if currency.eq_ignore_ascii_case("USD"))
}

async fn persist_incomplete_dollar_caps(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    budget: &BudgetConfig,
    current: u64,
) -> Result<bool, AgentLoopError> {
    let mut hard_stop = false;
    for (scope, limit) in [
        (BudgetScope::Session, budget.session_cost_cap_micros_usd),
        (BudgetScope::Daily, budget.daily_cost_cap_micros_usd),
    ] {
        let Some(limit) = limit else {
            continue;
        };
        persist_event(
            signals,
            PendingEvent::BudgetStatus {
                turn,
                level: BudgetLevel::HardCap,
                scope,
                unit: BudgetUnit::MicrosUsd,
                current,
                limit,
            },
        )
        .await?;
        hard_stop = true;
    }
    Ok(hard_stop)
}

async fn persist_incomplete_budget_caps(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    budget: &BudgetConfig,
    cost: &Cost,
    current_cost_micros: u64,
    current_credit_micros: u64,
) -> Result<bool, AgentLoopError> {
    let mut hard_stop = false;
    if !dollar_accounting_complete(cost) {
        hard_stop |=
            persist_incomplete_dollar_caps(signals, turn, budget, current_cost_micros).await?;
    }
    if matches!(cost, Cost::Unavailable { .. }) {
        for (scope, limit) in [
            (BudgetScope::Session, budget.session_ai_credit_cap_micros),
            (BudgetScope::Daily, budget.daily_ai_credit_cap_micros),
        ] {
            let Some(limit) = limit else {
                continue;
            };
            persist_event(
                signals,
                PendingEvent::BudgetStatus {
                    turn,
                    level: BudgetLevel::HardCap,
                    scope,
                    unit: BudgetUnit::AiCreditMicros,
                    current: current_credit_micros,
                    limit,
                },
            )
            .await?;
            hard_stop = true;
        }
    }
    Ok(hard_stop)
}

fn combine_cost(total: Option<Cost>, next: Cost) -> Cost {
    let Some(total) = total else {
        return next;
    };
    match (total, next) {
        (
            Cost::Monetary {
                amount_micros: left,
                currency: left_currency,
            },
            Cost::Monetary {
                amount_micros: right,
                currency: right_currency,
            },
        ) if left_currency == right_currency => Cost::Monetary {
            amount_micros: left.saturating_add(right),
            currency: left_currency,
        },
        (
            Cost::AiCredits {
                credits_micros: left,
                nominal_amount_micros: left_nominal,
                currency: left_currency,
            },
            Cost::AiCredits {
                credits_micros: right,
                nominal_amount_micros: right_nominal,
                currency: right_currency,
            },
        ) if left_currency == right_currency => Cost::AiCredits {
            credits_micros: left.saturating_add(right),
            nominal_amount_micros: left_nominal
                .and_then(|value| value.parse::<u64>().ok())
                .zip(right_nominal.and_then(|value| value.parse::<u64>().ok()))
                .map(|(left, right)| left.saturating_add(right).to_string()),
            currency: left_currency,
        },
        (
            Cost::SubscriptionQuota {
                used: left,
                unit: left_unit,
            },
            Cost::SubscriptionQuota {
                used: right,
                unit: right_unit,
            },
        ) if left_unit == right_unit => Cost::SubscriptionQuota {
            used: left
                .and_then(|value| value.parse::<u64>().ok())
                .zip(right.and_then(|value| value.parse::<u64>().ok()))
                .map(|(left, right)| left.saturating_add(right).to_string()),
            unit: left_unit,
        },
        (Cost::Unavailable { reason }, _) | (_, Cost::Unavailable { reason }) => {
            Cost::Unavailable { reason }
        }
        _ => Cost::Unavailable {
            reason: "mixed accounting units cannot be aggregated".to_owned(),
        },
    }
}

struct BudgetCheck {
    events: Vec<PendingEvent>,
    hard_stop: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SessionAccountingFallback {
    cost_micros_usd: u64,
    ai_credit_micros: u64,
    subscription_quota_entries: u64,
    cost_unavailable_entries: u64,
    non_usd_monetary_entries: u64,
}

fn session_accounting_fallback(accounting: &[TurnAccounting]) -> SessionAccountingFallback {
    let mut fallback = SessionAccountingFallback::default();
    for turn in accounting {
        match &turn.cost {
            Cost::Monetary {
                amount_micros,
                currency,
            } if currency.eq_ignore_ascii_case("USD") => {
                fallback.cost_micros_usd = fallback.cost_micros_usd.saturating_add(*amount_micros);
            }
            Cost::AiCredits { credits_micros, .. } => {
                fallback.ai_credit_micros =
                    fallback.ai_credit_micros.saturating_add(*credits_micros);
            }
            Cost::SubscriptionQuota { .. } => {
                fallback.subscription_quota_entries =
                    fallback.subscription_quota_entries.saturating_add(1);
            }
            Cost::Unavailable { .. } => {
                fallback.cost_unavailable_entries =
                    fallback.cost_unavailable_entries.saturating_add(1);
            }
            Cost::Monetary { .. } => {
                fallback.non_usd_monetary_entries =
                    fallback.non_usd_monetary_entries.saturating_add(1);
            }
        }
    }
    fallback
}

fn push_cap_event(
    events: &mut Vec<PendingEvent>,
    turn: u64,
    scope: BudgetScope,
    unit: BudgetUnit,
    current: u64,
    limit: Option<u64>,
    warn_at_percent: u8,
) -> bool {
    let Some(limit) = limit else {
        return false;
    };
    if current >= limit {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope,
            unit,
            current,
            limit,
        });
        return true;
    }
    let warning = u128::from(limit)
        .saturating_mul(u128::from(warn_at_percent))
        .div_ceil(100);
    if u128::from(current) >= warning {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::Warning,
            scope,
            unit,
            current,
            limit,
        });
    }
    false
}

#[allow(clippy::too_many_lines)]
async fn evaluate_budget(
    turn: u64,
    state_clock: &dyn EventClock,
    sink: &Arc<dyn SessionEventSink>,
    budget: &BudgetConfig,
    local_session: SessionAccountingFallback,
    current_turn_cost: u64,
    current_turn_credits: u64,
) -> Result<BudgetCheck, AgentLoopError> {
    if budget.session_cost_cap_micros_usd.is_none()
        && budget.daily_cost_cap_micros_usd.is_none()
        && budget.session_ai_credit_cap_micros.is_none()
        && budget.daily_ai_credit_cap_micros.is_none()
        && budget.spend_rate_alarm_micros_usd_per_minute.is_none()
        && budget.ai_credit_rate_alarm_micros_per_minute.is_none()
    {
        return Ok(BudgetCheck {
            events: Vec::new(),
            hard_stop: false,
        });
    }
    let now = state_clock.unix_time_millis();
    let ledger = sink
        .budget_totals(BudgetLedgerQuery {
            now_unix_ms: now,
            utc_day_start_unix_ms: now.saturating_sub(now % 86_400_000),
            trailing_minute_start_unix_ms: now.saturating_sub(60_000),
        })
        .await?;
    let session_cost = if ledger.authoritative {
        ledger.session_cost_micros_usd
    } else {
        local_session.cost_micros_usd
    }
    .saturating_add(current_turn_cost);
    let session_credits = if ledger.authoritative {
        ledger.session_ai_credit_micros
    } else {
        local_session.ai_credit_micros
    }
    .saturating_add(current_turn_credits);
    let daily_cost = ledger
        .daily_cost_micros_usd
        .saturating_add(current_turn_cost);
    let daily_credits = ledger
        .daily_ai_credit_micros
        .saturating_add(current_turn_credits);
    let trailing_cost = ledger
        .trailing_minute_cost_micros_usd
        .saturating_add(current_turn_cost);
    let trailing_credits = ledger
        .trailing_minute_ai_credit_micros
        .saturating_add(current_turn_credits);
    let mut events = Vec::new();
    let mut hard_stop = false;
    if !ledger.authoritative {
        for (unit, current, limit) in [
            (
                BudgetUnit::MicrosUsd,
                daily_cost,
                budget.daily_cost_cap_micros_usd,
            ),
            (
                BudgetUnit::AiCreditMicros,
                daily_credits,
                budget.daily_ai_credit_cap_micros,
            ),
        ] {
            if let Some(limit) = limit {
                events.push(PendingEvent::BudgetStatus {
                    turn,
                    level: BudgetLevel::HardCap,
                    scope: BudgetScope::Daily,
                    unit,
                    current,
                    limit,
                });
                hard_stop = true;
            }
        }
    }
    let session_accounting_incomplete = if ledger.authoritative {
        ledger.session_subscription_quota_entries > 0
            || ledger.session_cost_unavailable_entries > 0
            || ledger.session_non_usd_monetary_entries > 0
    } else {
        local_session.subscription_quota_entries > 0
            || local_session.cost_unavailable_entries > 0
            || local_session.non_usd_monetary_entries > 0
    };
    if let Some(limit) = budget.session_cost_cap_micros_usd
        && session_accounting_incomplete
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Session,
            unit: BudgetUnit::MicrosUsd,
            current: session_cost,
            limit,
        });
        hard_stop = true;
    }
    let session_credit_accounting_incomplete = if ledger.authoritative {
        ledger.session_cost_unavailable_entries > 0
    } else {
        local_session.cost_unavailable_entries > 0
    };
    if let Some(limit) = budget.session_ai_credit_cap_micros
        && session_credit_accounting_incomplete
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Session,
            unit: BudgetUnit::AiCreditMicros,
            current: session_credits,
            limit,
        });
        hard_stop = true;
    }
    let daily_accounting_incomplete = ledger.daily_subscription_quota_entries > 0
        || ledger.daily_cost_unavailable_entries > 0
        || ledger.daily_non_usd_monetary_entries > 0;
    if ledger.authoritative
        && let Some(limit) = budget.daily_cost_cap_micros_usd
        && daily_accounting_incomplete
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Daily,
            unit: BudgetUnit::MicrosUsd,
            current: daily_cost,
            limit,
        });
        hard_stop = true;
    }
    if ledger.authoritative
        && let Some(limit) = budget.daily_ai_credit_cap_micros
        && ledger.daily_cost_unavailable_entries > 0
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::HardCap,
            scope: BudgetScope::Daily,
            unit: BudgetUnit::AiCreditMicros,
            current: daily_credits,
            limit,
        });
        hard_stop = true;
    }
    hard_stop |= push_cap_event(
        &mut events,
        turn,
        BudgetScope::Session,
        BudgetUnit::MicrosUsd,
        session_cost,
        budget.session_cost_cap_micros_usd,
        budget.warn_at_percent,
    );
    if ledger.authoritative {
        hard_stop |= push_cap_event(
            &mut events,
            turn,
            BudgetScope::Daily,
            BudgetUnit::MicrosUsd,
            daily_cost,
            budget.daily_cost_cap_micros_usd,
            budget.warn_at_percent,
        );
    }
    hard_stop |= push_cap_event(
        &mut events,
        turn,
        BudgetScope::Session,
        BudgetUnit::AiCreditMicros,
        session_credits,
        budget.session_ai_credit_cap_micros,
        budget.warn_at_percent,
    );
    if ledger.authoritative {
        hard_stop |= push_cap_event(
            &mut events,
            turn,
            BudgetScope::Daily,
            BudgetUnit::AiCreditMicros,
            daily_credits,
            budget.daily_ai_credit_cap_micros,
            budget.warn_at_percent,
        );
    }
    if budget
        .spend_rate_alarm_micros_usd_per_minute
        .is_some_and(|limit| trailing_cost >= limit)
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::SpendRateAlarm,
            scope: BudgetScope::TrailingMinute,
            unit: BudgetUnit::MicrosUsd,
            current: trailing_cost,
            limit: budget
                .spend_rate_alarm_micros_usd_per_minute
                .unwrap_or_default(),
        });
    }
    if budget
        .ai_credit_rate_alarm_micros_per_minute
        .is_some_and(|limit| trailing_credits >= limit)
    {
        events.push(PendingEvent::BudgetStatus {
            turn,
            level: BudgetLevel::SpendRateAlarm,
            scope: BudgetScope::TrailingMinute,
            unit: BudgetUnit::AiCreditMicros,
            current: trailing_credits,
            limit: budget
                .ai_credit_rate_alarm_micros_per_minute
                .unwrap_or_default(),
        });
    }
    Ok(BudgetCheck { events, hard_stop })
}

#[allow(clippy::too_many_lines)]
async fn build_cost_snapshot(
    state: &ActorState,
    config: &SessionActorConfig,
) -> Result<CostSnapshot, AgentLoopError> {
    let now = state.event_clock.unix_time_millis();
    let day_start = now.saturating_sub(now % 86_400_000);
    let ledger = config
        .event_sink
        .budget_totals(BudgetLedgerQuery {
            now_unix_ms: now,
            utc_day_start_unix_ms: day_start,
            trailing_minute_start_unix_ms: now.saturating_sub(60_000),
        })
        .await?;
    let mut usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
    };
    let mut local_cost = 0_u64;
    let mut local_credits = 0_u64;
    let mut local_subscription = 0_u64;
    let mut local_unavailable = 0_u64;
    let mut local_non_usd = 0_u64;
    for turn in &state.accounting {
        add_usage(&mut usage, &turn.usage);
        match &turn.cost {
            Cost::Monetary {
                amount_micros,
                currency,
            } if currency.eq_ignore_ascii_case("USD") => {
                local_cost = local_cost.saturating_add(*amount_micros);
            }
            Cost::AiCredits { credits_micros, .. } => {
                local_credits = local_credits.saturating_add(*credits_micros);
            }
            Cost::Monetary { .. } => local_non_usd = local_non_usd.saturating_add(1),
            Cost::SubscriptionQuota { .. } => {
                local_subscription = local_subscription.saturating_add(1);
            }
            Cost::Unavailable { .. } => {
                local_unavailable = local_unavailable.saturating_add(1);
            }
        }
    }
    let session_cost = ledger.session_cost_micros_usd.max(local_cost);
    let session_credits = ledger.session_ai_credit_micros.max(local_credits);
    // UTC-day/trailing windows are storage-authoritative. Session totals are
    // safely recoverable from this session's durable events; day membership is not.
    let daily_cost = ledger.daily_cost_micros_usd;
    let daily_credits = ledger.daily_ai_credit_micros;
    let session_subscription = ledger
        .session_subscription_quota_entries
        .max(local_subscription);
    let session_unavailable = ledger
        .session_cost_unavailable_entries
        .max(local_unavailable);
    let session_non_usd = ledger.session_non_usd_monetary_entries.max(local_non_usd);
    let budget = config.model.budget_config();
    let hard_cap_reached = budget
        .session_cost_cap_micros_usd
        .is_some_and(|limit| session_cost >= limit)
        || budget
            .daily_cost_cap_micros_usd
            .is_some_and(|limit| daily_cost >= limit)
        || budget
            .session_ai_credit_cap_micros
            .is_some_and(|limit| session_credits >= limit)
        || budget
            .daily_ai_credit_cap_micros
            .is_some_and(|limit| daily_credits >= limit);
    let input_total = usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens);
    let cache_hit_basis_points = if input_total == 0 {
        0
    } else {
        u16::try_from(
            u128::from(usage.cache_read_tokens).saturating_mul(10_000) / u128::from(input_total),
        )
        .unwrap_or(10_000)
    };
    let date = format_unix_rfc3339(now / 1_000, 0);
    Ok(CostSnapshot {
        utc_day: date.get(..10).unwrap_or("1970-01-01").to_owned(),
        turns: state.accounting.clone(),
        session_usage: usage,
        session_cost_micros_usd: session_cost,
        session_ai_credit_micros: session_credits,
        daily_cost_micros_usd: daily_cost,
        daily_ai_credit_micros: daily_credits,
        trailing_minute_cost_micros_usd: ledger.trailing_minute_cost_micros_usd,
        trailing_minute_ai_credit_micros: ledger.trailing_minute_ai_credit_micros,
        cache_hit_basis_points,
        session_cost_cap_micros_usd: budget.session_cost_cap_micros_usd,
        daily_cost_cap_micros_usd: budget.daily_cost_cap_micros_usd,
        session_ai_credit_cap_micros: budget.session_ai_credit_cap_micros,
        daily_ai_credit_cap_micros: budget.daily_ai_credit_cap_micros,
        spend_rate_alarm_micros_usd_per_minute: budget.spend_rate_alarm_micros_usd_per_minute,
        ai_credit_rate_alarm_micros_per_minute: budget.ai_credit_rate_alarm_micros_per_minute,
        hard_cap_reached,
        session_monetary_accounting_complete: session_subscription == 0
            && session_unavailable == 0
            && session_non_usd == 0,
        daily_monetary_accounting_complete: ledger.daily_subscription_quota_entries == 0
            && ledger.daily_cost_unavailable_entries == 0
            && ledger.daily_non_usd_monetary_entries == 0,
        session_subscription_quota_entries: session_subscription,
        session_cost_unavailable_entries: session_unavailable,
        session_non_usd_monetary_entries: session_non_usd,
        daily_subscription_quota_entries: ledger.daily_subscription_quota_entries,
        daily_cost_unavailable_entries: ledger.daily_cost_unavailable_entries,
        daily_non_usd_monetary_entries: ledger.daily_non_usd_monetary_entries,
    })
}

async fn apply_context_surgery(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    sink: &Arc<dyn SessionEventSink>,
    item_id: ContextItemId,
    pinned: bool,
) -> Result<(), AgentLoopError> {
    let effective_after_agent_turn = state.next_turn;
    let pending = if pinned {
        PendingEvent::ContextItemPinned {
            item_id: item_id.clone(),
            effective_after_agent_turn,
        }
    } else {
        PendingEvent::ContextItemEvicted {
            item_id: item_id.clone(),
            effective_after_agent_turn,
        }
    };
    emit(state, events, sink, pending).await?;
    state.context_surgery.push(ContextSurgeryAction {
        item_id,
        pinned,
        effective_after_agent_turn,
    });
    Ok(())
}

async fn apply_registered_context_surgery(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
    item_id: ContextItemId,
    pinned: bool,
) -> Result<(), AgentLoopError> {
    if !item_id.0.starts_with("conversation:") {
        return Err(AgentLoopError::InvalidConfiguration(
            "protected_context_item: only conversation-resident context items support pin or eviction"
                .to_owned(),
        ));
    }
    let known = assemble_session_context(
        config,
        &state.conversation,
        &state.queued,
        &state.context_surgery,
        &state.pruned_tool_outputs,
        false,
    )
    .is_ok_and(|assembled| assembled.items.iter().any(|item| item.id.0 == item_id.0));
    if !known {
        return Err(AgentLoopError::InvalidConfiguration(
            "unknown_context_item: context item is not present in the current inventory".to_owned(),
        ));
    }
    apply_context_surgery(state, events, &config.event_sink, item_id, pinned).await
}

fn requires_driver(command: &ClientCommand) -> bool {
    !matches!(
        command,
        ClientCommand::CreateSession { .. }
            | ClientCommand::AttachSession { .. }
            | ClientCommand::TakeDriver { .. }
            | ClientCommand::GetContext { .. }
            | ClientCommand::GetCost { .. }
            | ClientCommand::DumpPrompt { .. }
    )
}

fn unsupported_in_m2(command: &ClientCommand) -> bool {
    matches!(
        command,
        ClientCommand::CreateSession { .. }
            | ClientCommand::ResumeSession { .. }
            | ClientCommand::SwitchMode { .. }
            | ClientCommand::Fork { .. }
            | ClientCommand::ListSessions { .. }
            | ClientCommand::ListCommands { .. }
            | ClientCommand::ListModels { .. }
            | ClientCommand::SearchWorkspaceFiles { .. }
            | ClientCommand::PreviewWorkspaceFile { .. }
            | ClientCommand::GetWorkspaceStatus { .. }
            | ClientCommand::ShutdownHost { .. }
    )
}

fn start_manual_compaction(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    turn_signals: &mpsc::UnboundedSender<TurnSignal>,
    active_turn: &Arc<AtomicU64>,
    instructions: Option<String>,
    completion: Option<oneshot::Sender<Result<ProtocolCompletion, AgentLoopError>>>,
) {
    let summary_turn = state.next_turn;
    let cancellation = CancellationToken::default();
    state.running = Some(RunningTurn {
        id: summary_turn,
        cancellation: cancellation.clone(),
        caused_by: state.transient_cause.clone(),
    });
    active_turn.store(summary_turn, Ordering::Release);
    let mut conversation = state.conversation.clone();
    let mut context_surgery = state.context_surgery.clone();
    let local_session_accounting = session_accounting_fallback(&state.accounting);
    let config = Arc::new(config.with_model_alias(state.model_alias.clone()));
    let signals = turn_signals.clone();
    tokio::spawn(async move {
        let result = async {
            let pre_budget = evaluate_budget(
                summary_turn,
                config.event_clock.as_ref(),
                &config.event_sink,
                &config.model.budget_config(),
                local_session_accounting,
                0,
                0,
            )
            .await?;
            for event in pre_budget.events {
                persist_event(&signals, event).await?;
            }
            if pre_budget.hard_stop {
                return Err(AgentLoopError::InvalidConfiguration(
                    "budget hard cap prevents compaction model call".to_owned(),
                ));
            }
            compact_during_turn(
                summary_turn,
                &mut conversation,
                &mut context_surgery,
                CompactionReason::Manual,
                &config,
                &cancellation,
                &signals,
                local_session_accounting,
                0,
                0,
                instructions,
            )
            .await
            .map(|_| ())
        }
        .await;
        if let Err(error) = &result {
            let _ = persist_event(
                &signals,
                PendingEvent::Error {
                    message: error.to_string(),
                },
            )
            .await;
        }
        let _ = signals.send(TurnSignal::ManualCompactionComplete {
            turn: summary_turn,
            conversation,
            context_surgery,
            result,
            completion,
        });
    });
}

#[allow(clippy::too_many_lines)]
async fn handle_actor_command(
    command: ActorCommand,
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    tool_context: &ToolContext,
    turn_signals: &mpsc::UnboundedSender<TurnSignal>,
    events: &broadcast::Sender<RoutedEvent>,
    active_turn: &Arc<AtomicU64>,
) {
    match command {
        ActorCommand::Protocol {
            command,
            respond,
            mut completion,
        } => {
            let meta = client_command_meta(&command).clone();
            let session = client_command_session(&command).cloned();
            let rejection = if meta.protocol_version != PROTOCOL_VERSION {
                Some(protocol_rejection(
                    "unsupported_protocol_version",
                    format!(
                        "protocol version {} is unsupported; expected {PROTOCOL_VERSION}",
                        meta.protocol_version
                    ),
                ))
            } else if session.as_ref().is_some_and(|id| id != &config.session_id) {
                Some(protocol_rejection(
                    "session_mismatch",
                    "command session id does not match this actor",
                ))
            } else if unsupported_in_m2(&command) {
                Some(protocol_rejection(
                    "command_not_available",
                    "command is not available in milestone M2",
                ))
            } else if state.poisoned
                && !matches!(
                    (&command, &state.pending_rewind),
                    (
                        ClientCommand::Rewind {
                            target: RewindTarget::Turn { turn_id },
                            ..
                        },
                        Some((pending_turn, _))
                    ) if turn_id.0 == pending_turn.to_string()
                )
            {
                Some(protocol_rejection(
                    "session_requires_recovery",
                    "session is fail-closed until checkpoint journal recovery completes",
                ))
            } else if requires_driver(&command)
                && state.driver_client_id.as_ref() != Some(&meta.client_id)
            {
                Some(protocol_rejection(
                    "driver_required",
                    "mutating commands are accepted only from the current driver",
                ))
            } else {
                None
            };
            if let Some(outcome) = rejection {
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                return;
            }

            match &command {
                ClientCommand::AttachSession { role, .. } => {
                    if *role == ClientRole::Driver
                        && state
                            .driver_client_id
                            .as_ref()
                            .is_some_and(|driver| driver != &meta.client_id)
                    {
                        let outcome = protocol_rejection(
                            "driver_lease_held",
                            "another client holds the driver lease; attach as observer or take it explicitly",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
                ClientCommand::SendMessage { .. } if state.active_shell.is_some() => {
                    let outcome = protocol_rejection(
                        "user_shell_active",
                        "an agent turn cannot start while the foreground user shell is active",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SendMessage { attachments, .. }
                    if state.running.is_some() && !attachments.is_empty() =>
                {
                    let outcome = protocol_rejection(
                        "attachment_queue_unsupported",
                        "messages with attachments require an idle session so their provider-neutral blocks commit atomically",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SendMessage {
                    content,
                    attachments,
                    ..
                } if content.trim_start().starts_with('/') && !attachments.is_empty() => {
                    let outcome = protocol_rejection(
                        "command_attachments_unsupported",
                        "slash commands do not accept message attachments",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SendMessage {
                    content,
                    attachments,
                    ..
                } => {
                    if let Err(message) = prepare_user_message(
                        content,
                        attachments,
                        &state.model_alias,
                        config.model.as_ref(),
                    ) {
                        let outcome = protocol_rejection("invalid_attachment", message);
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
                ClientCommand::SwitchModel { .. }
                    if state.running.is_some() || state.active_shell.is_some() =>
                {
                    let outcome = protocol_rejection(
                        "session_not_idle",
                        "model switching requires an idle session with no active user shell",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::SwitchModel { model, .. }
                    if !config.model.has_model_alias(&model.0) =>
                {
                    let outcome = protocol_rejection(
                        "unknown_model_alias",
                        format!("model alias {:?} is not configured", model.0),
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::UserShellStarted { command, .. }
                    if command.trim().is_empty()
                        || state.running.is_some()
                        || state.active_shell.is_some() =>
                {
                    let outcome = protocol_rejection(
                        "shell_start_rejected",
                        "a non-empty foreground shell may start only while the session is idle",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::UserShellEnded {
                    shell_id,
                    captured_output,
                    ..
                } if state.active_shell.as_ref().map(|shell| &shell.shell_id) != Some(shell_id)
                    || captured_output
                        .as_ref()
                        .is_some_and(|output| output.len() > MAX_CAPTURED_SHELL_OUTPUT_BYTES) =>
                {
                    let outcome = protocol_rejection(
                        "shell_end_rejected",
                        "shell end must match the active shell id and its captured output must fit the durable limit",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::Rewind {
                    target: RewindTarget::Checkpoint { .. },
                    ..
                } => {
                    let outcome = protocol_rejection(
                        "checkpoint_target_not_available",
                        "rewind by checkpoint id is not available in milestone M2",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::Rewind {
                    target: RewindTarget::Turn { turn_id },
                    ..
                } if parse_turn_id(turn_id).is_err() => {
                    let outcome = protocol_rejection(
                        "invalid_turn_id",
                        "rewind turn id must be an unsigned decimal integer",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::Rewind {
                    target: RewindTarget::Turn { turn_id },
                    ..
                } if state.running.is_some()
                    || parse_turn_id(turn_id).is_ok_and(|to_turn| {
                        !state.turn_ends.contains_key(&to_turn)
                            && state.pending_rewind.as_ref().map(|pending| pending.0)
                                != Some(to_turn)
                    }) =>
                {
                    let outcome = protocol_rejection(
                        "invalid_rewind_target",
                        "rewind requires an idle session and a completed turn target",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::ApproveTool { tool_call_id, .. }
                    if !state.pending_approvals.contains_key(&tool_call_id.0) =>
                {
                    let outcome =
                        protocol_rejection("unknown_approval", "tool approval is not pending");
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::AnswerQuestion {
                    question_id,
                    answers,
                    ..
                } if !state.pending_questions.contains_key(&question_id.0)
                    || !answers.iter().any(|answer| {
                        answer.question_id == *question_id && !answer.values.is_empty()
                    }) =>
                {
                    let outcome = protocol_rejection(
                        "invalid_question_answer",
                        "question is not pending or its answer is empty",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::Compact { .. } if state.running.is_some() => {
                    let outcome = protocol_rejection(
                        "turn_running",
                        "manual compaction requires an idle session",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                ClientCommand::PinContext { item_id, .. }
                | ClientCommand::EvictContext { item_id, .. } => {
                    if state.running.is_some() {
                        let outcome = protocol_rejection(
                            "turn_running",
                            "context surgery requires an idle session",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                    if !item_id.0.starts_with("conversation:") {
                        let outcome = protocol_rejection(
                            "protected_context_item",
                            "only conversation-resident context items support pin or eviction",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                    let known = assemble_session_context(
                        config,
                        &state.conversation,
                        &state.queued,
                        &state.context_surgery,
                        &state.pruned_tool_outputs,
                        false,
                    )
                    .is_ok_and(|assembled| {
                        assembled.items.iter().any(|item| item.id.0 == item_id.0)
                    });
                    if !known {
                        let outcome = protocol_rejection(
                            "unknown_context_item",
                            "context item is not present in the current inventory",
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
                ClientCommand::DumpPrompt {
                    turn_id: Some(turn_id),
                    ..
                } if parse_turn_id(turn_id).is_err()
                    || (!state
                        .turn_ends
                        .contains_key(&turn_id.0.parse::<u64>().unwrap_or(u64::MAX))
                        && state.running.as_ref().map(|running| running.id)
                            != turn_id.0.parse::<u64>().ok()) =>
                {
                    let outcome = protocol_rejection(
                        "unknown_prompt_turn",
                        "prompt dump turn must identify a known completed or active turn",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                _ => {}
            }

            let attach_gap = if let ClientCommand::AttachSession {
                last_seen_sequence, ..
            } = &command
            {
                let tail = match config.event_sink.last_sequence().await {
                    Ok(tail) => tail,
                    Err(error) => {
                        let outcome = protocol_rejection(
                            "gap_replay_failed",
                            format!("could not read durable session tail: {error}"),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                };
                if last_seen_sequence
                    .is_some_and(|last_seen| tail.is_none_or(|tail| last_seen > tail))
                {
                    let outcome = protocol_rejection(
                        "sequence_ahead_of_log",
                        "last-seen sequence is ahead of the durable session tail",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                }
                match config.event_sink.read_after(*last_seen_sequence).await {
                    Ok(gap) => {
                        if let Err(error) =
                            validate_gap(*last_seen_sequence, &gap, &config.session_id)
                        {
                            let outcome = protocol_rejection(
                                "invalid_gap_replay",
                                format!("durable session gap is invalid: {error}"),
                            );
                            send_ack(state, events, &meta, session, outcome.clone());
                            let _ = respond.send(outcome);
                            return;
                        }
                        Some(gap)
                    }
                    Err(error) => {
                        let outcome = protocol_rejection(
                            "gap_replay_failed",
                            format!("could not read durable session gap: {error}"),
                        );
                        send_ack(state, events, &meta, session, outcome.clone());
                        let _ = respond.send(outcome);
                        return;
                    }
                }
            } else {
                None
            };

            state.transient_cause = Some(meta.request_id.clone());
            let lease_persist = match &command {
                ClientCommand::AttachSession { role, .. }
                    if *role == ClientRole::Driver && state.driver_client_id.is_none() =>
                {
                    let driver_event = if state.sequence.is_none() {
                        PendingEvent::SessionCreated {
                            driver_client_id: meta.client_id.clone(),
                        }
                    } else {
                        PendingEvent::DriverChanged {
                            driver_client_id: meta.client_id.clone(),
                        }
                    };
                    emit(state, events, &config.event_sink, driver_event).await
                }
                ClientCommand::TakeDriver { .. } => {
                    emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::DriverChanged {
                            driver_client_id: meta.client_id.clone(),
                        },
                    )
                    .await
                }
                _ => Ok(()),
            };
            if let Err(error) = lease_persist {
                state.transient_cause = None;
                let outcome = protocol_rejection(
                    "session_persistence_failure",
                    format!("could not persist the driver lease: {error}"),
                );
                send_ack(state, events, &meta, session, outcome.clone());
                let _ = respond.send(outcome);
                if let Some(complete) = completion.take() {
                    let _ = complete.send(Err(error));
                }
                return;
            }
            if let Some(gap) = &attach_gap {
                for event in gap {
                    let _ = events.send(RoutedEvent {
                        target: Some(meta.client_id.clone()),
                        event: event.clone(),
                    });
                }
            }
            let mut precommitted_answer = None;
            if let ClientCommand::AnswerQuestion {
                question_id,
                answers,
                ..
            } = &command
            {
                let Some(pending) = state.pending_questions.remove(&question_id.0) else {
                    state.transient_cause = None;
                    let outcome = protocol_rejection(
                        "invalid_question_answer",
                        "question stopped pending before its answer could be committed",
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    return;
                };
                let answer = answers
                    .iter()
                    .find(|answer| answer.question_id == *question_id)
                    .map(|answer| answer.values.join("\n"))
                    .unwrap_or_default();
                if let Err(error) = emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::QuestionAnswered {
                        turn: pending.turn,
                        question_id: question_id.clone(),
                        answers: answers.clone(),
                    },
                )
                .await
                {
                    if let Some(running) = &state.running {
                        running.cancellation.cancel();
                    }
                    drop(pending.respond);
                    state.poisoned = true;
                    state.transient_cause = None;
                    let outcome = protocol_rejection(
                        "session_persistence_failure",
                        format!("could not persist the question answer: {error}"),
                    );
                    send_ack(state, events, &meta, session, outcome.clone());
                    let _ = respond.send(outcome);
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(Err(error));
                    }
                    return;
                }
                precommitted_answer = Some((pending, answer));
            }
            let accepted = CommandOutcome::Accepted;
            send_ack(state, events, &meta, session, accepted.clone());
            let _ = respond.send(accepted);
            match command {
                ClientCommand::AttachSession { role, .. } => {
                    state
                        .client_roles
                        .insert(meta.client_id.0.clone(), role.clone());
                    if role == ClientRole::Driver && state.driver_client_id.is_none() {
                        state.driver_client_id = Some(meta.client_id.clone());
                    }
                }
                ClientCommand::TakeDriver { .. } => {
                    state.driver_client_id = Some(meta.client_id.clone());
                    state
                        .client_roles
                        .insert(meta.client_id.0.clone(), ClientRole::Driver);
                }
                ClientCommand::SwitchModel { model, .. } => {
                    let result = emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::ModelChanged {
                            model: model.clone(),
                        },
                    )
                    .await;
                    if result.is_ok() {
                        state.model_alias = model.0;
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::UserShellStarted { command, .. } => {
                    let shell_id = ShellId(format!(
                        "shell-{}",
                        state
                            .sequence
                            .map_or(0, |sequence| sequence.saturating_add(1))
                    ));
                    let shell = RecoveredUserShell {
                        shell_id: shell_id.clone(),
                        command: command.clone(),
                    };
                    let result = emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::UserShellStateChanged {
                            shell_id,
                            command,
                            active: true,
                            status: None,
                            captured_output: None,
                        },
                    )
                    .await;
                    if result.is_ok() {
                        state.active_shell = Some(shell);
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::UserShellEnded {
                    shell_id,
                    status,
                    captured_output,
                    ..
                } => {
                    let command = state
                        .active_shell
                        .as_ref()
                        .map(|shell| shell.command.clone())
                        .unwrap_or_default();
                    let result = emit(
                        state,
                        events,
                        &config.event_sink,
                        PendingEvent::UserShellStateChanged {
                            shell_id,
                            command,
                            active: false,
                            status: Some(status),
                            captured_output,
                        },
                    )
                    .await;
                    if result.is_ok() {
                        state.active_shell = None;
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::SendMessage {
                    content,
                    attachments,
                    ..
                } => {
                    let (internal_respond, internal_receive) = oneshot::channel();
                    Box::pin(handle_actor_command(
                        ActorCommand::SendMessage {
                            content,
                            attachments,
                            observed_turn: active_turn.load(Ordering::Acquire),
                            respond: internal_respond,
                        },
                        state,
                        config,
                        tool_context,
                        turn_signals,
                        events,
                        active_turn,
                    ))
                    .await;
                    match internal_receive.await {
                        Ok(Ok(disposition)) => {
                            if let Some(complete) = completion.take() {
                                let _ = complete.send(Ok(ProtocolCompletion::Message(disposition)));
                            }
                        }
                        Ok(Err(error)) => {
                            if let Some(complete) = completion.take() {
                                let _ = complete.send(Err(error.clone()));
                            }
                            state.poisoned = true;
                            let _ = emit(
                                state,
                                events,
                                &config.event_sink,
                                PendingEvent::Error {
                                    message: format!(
                                        "accepted message failed before turn execution: {error}"
                                    ),
                                },
                            )
                            .await;
                        }
                        Err(_) => {
                            if let Some(complete) = completion.take() {
                                let _ = complete.send(Err(AgentLoopError::Closed));
                            }
                            state.poisoned = true;
                        }
                    }
                }
                ClientCommand::Interrupt { .. } => {
                    if let Some(running) = &state.running {
                        running.cancellation.cancel();
                    }
                }
                ClientCommand::ApproveTool {
                    tool_call_id,
                    decision,
                    ..
                } => {
                    if let Some(sender) = state.pending_approvals.remove(&tool_call_id.0) {
                        let _ = sender.send(decision);
                    }
                }
                ClientCommand::AnswerQuestion { .. } => {
                    if let Some((pending, answer)) = precommitted_answer.take() {
                        let _ = pending.respond.send(answer);
                    }
                }
                ClientCommand::Rewind {
                    target: RewindTarget::Turn { turn_id },
                    ..
                } => {
                    let rewind = match parse_turn_id(&turn_id) {
                        Ok(to_turn) => rewind_state(state, config, events, to_turn).await,
                        Err(error) => Err(AgentLoopError::InvalidConfiguration(error.to_string())),
                    };
                    let result = match rewind {
                        Ok(unrestorable_paths) => {
                            let message = if unrestorable_paths.is_empty() {
                                format!("rewound to turn {}", turn_id.0)
                            } else {
                                format!(
                                    "rewound to turn {} with {} unrestorable path(s)",
                                    turn_id.0,
                                    unrestorable_paths.len()
                                )
                            };
                            emit(
                                state,
                                events,
                                &config.event_sink,
                                PendingEvent::CommandFinished {
                                    name: "rewind".to_owned(),
                                    message,
                                    unrestorable_paths: unrestorable_paths.clone(),
                                },
                            )
                            .await
                            .map(|()| unrestorable_paths)
                        }
                        Err(error) => Err(error),
                    };
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(ProtocolCompletion::Rewind));
                    }
                }
                ClientCommand::PinContext { item_id, .. } => {
                    let result =
                        apply_context_surgery(state, events, &config.event_sink, item_id, true)
                            .await;
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::EvictContext { item_id, .. } => {
                    let result =
                        apply_context_surgery(state, events, &config.event_sink, item_id, false)
                            .await;
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(|()| ProtocolCompletion::Unit));
                    }
                }
                ClientCommand::GetContext { .. } => {
                    let result = assemble_session_context(
                        config,
                        &state.conversation,
                        &state.queued,
                        &state.context_surgery,
                        &state.pruned_tool_outputs,
                        false,
                    )
                    .map(|assembled| {
                        context_snapshot(
                            &assembled,
                            &state.conversation,
                            &state.pruned_tool_outputs,
                            config.model.context_metadata(&config.model_alias),
                            &config.model.compaction_config(),
                            state
                                .running
                                .as_ref()
                                .map(|running| wire_turn_id(running.id)),
                        )
                    });
                    if let Ok(snapshot) = &result {
                        send_connection_event(
                            events,
                            &meta.client_id,
                            EngineEvent::ContextSnapshotReady {
                                meta: query_meta(state, &meta),
                                session_id: state.session_id.clone(),
                                snapshot: snapshot.clone(),
                            },
                        );
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(ProtocolCompletion::Context));
                    }
                }
                ClientCommand::GetCost { .. } => {
                    let result = build_cost_snapshot(state, config).await;
                    if let Ok(snapshot) = &result {
                        send_connection_event(
                            events,
                            &meta.client_id,
                            EngineEvent::CostSnapshotReady {
                                meta: query_meta(state, &meta),
                                session_id: state.session_id.clone(),
                                snapshot: snapshot.clone(),
                            },
                        );
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(ProtocolCompletion::Cost));
                    }
                }
                ClientCommand::DumpPrompt { turn_id, .. } => {
                    let historical = if let Some(requested) = &turn_id {
                        let events = config.event_sink.read_after(None).await;
                        events.and_then(|events| {
                            let boundary = events.iter().position(|event| {
                                matches!(
                                    event,
                                    EngineEvent::ContextUsageUpdated { turn_id, .. }
                                        if turn_id == requested
                                )
                            });
                            let boundary = boundary.ok_or_else(|| {
                                AgentLoopError::InvalidConfiguration(format!(
                                    "no assembled prompt was recorded for turn {}",
                                    requested.0
                                ))
                            })?;
                            project_session_events(&events[..=boundary])
                                .map_err(|error| AgentLoopError::Persistence(error.to_string()))
                        })
                    } else {
                        Ok(SessionRecoveredState {
                            conversation: state.conversation.clone(),
                            queued_messages: state.queued.iter().cloned().collect(),
                            context_surgery: state.context_surgery.clone(),
                            pruned_tool_outputs: state.pruned_tool_outputs.clone(),
                            accounting: state.accounting.clone(),
                            ..SessionRecoveredState::default()
                        })
                    };
                    let result = historical.and_then(|historical| {
                        assemble_session_context(
                            config,
                            &historical.conversation,
                            &historical.queued_messages.iter().cloned().collect(),
                            &historical.context_surgery,
                            &historical.pruned_tool_outputs,
                            true,
                        )
                        .map(|assembled| prompt_dump(&assembled, &config.model_alias, turn_id))
                    });
                    if let Ok(dump) = &result {
                        send_connection_event(
                            events,
                            &meta.client_id,
                            EngineEvent::PromptDumpReady {
                                meta: query_meta(state, &meta),
                                session_id: state.session_id.clone(),
                                dump: dump.clone(),
                            },
                        );
                    }
                    if let Some(complete) = completion.take() {
                        let _ = complete.send(result.map(ProtocolCompletion::Prompt));
                    }
                }
                ClientCommand::Compact { instructions, .. } => {
                    let completion = completion.take();
                    start_manual_compaction(
                        state,
                        config,
                        turn_signals,
                        active_turn,
                        instructions,
                        completion,
                    );
                }
                ClientCommand::CreateSession { .. }
                | ClientCommand::ResumeSession { .. }
                | ClientCommand::SwitchMode { .. }
                | ClientCommand::Fork { .. }
                | ClientCommand::ListSessions { .. }
                | ClientCommand::ListCommands { .. }
                | ClientCommand::ListModels { .. }
                | ClientCommand::SearchWorkspaceFiles { .. }
                | ClientCommand::PreviewWorkspaceFile { .. }
                | ClientCommand::GetWorkspaceStatus { .. }
                | ClientCommand::ShutdownHost { .. }
                | ClientCommand::Rewind {
                    target: RewindTarget::Checkpoint { .. },
                    ..
                } => {}
            }
            if let Some(complete) = completion.take() {
                let _ = complete.send(Err(AgentLoopError::InvalidConfiguration(
                    "command has no local completion result".to_owned(),
                )));
            }
            state.transient_cause = None;
        }
        ActorCommand::SendMessage {
            content,
            attachments,
            observed_turn,
            respond,
        } => {
            if content.trim_start().starts_with('/') {
                let mut context = SessionCommandContext {
                    running: state.running.is_some(),
                    queued_messages: state.queued.len(),
                };
                let result = config.commands.dispatch_line(&mut context, &content).await;
                let disposition = match result {
                    Ok(mut output) => {
                        let mut unrestorable_paths = Vec::new();
                        match output.action {
                            SessionCommandAction::Interrupt => {
                                if let Some(running) = &state.running
                                    && running.id == observed_turn
                                {
                                    running.cancellation.cancel();
                                }
                            }
                            SessionCommandAction::Rewind { to_turn } => {
                                match rewind_state(state, config, events, to_turn).await {
                                    Ok(report) => unrestorable_paths = report,
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::Context => {
                                let snapshot = assemble_session_context(
                                    config,
                                    &state.conversation,
                                    &state.queued,
                                    &state.context_surgery,
                                    &state.pruned_tool_outputs,
                                    false,
                                )
                                .map(|assembled| {
                                    context_snapshot(
                                        &assembled,
                                        &state.conversation,
                                        &state.pruned_tool_outputs,
                                        config.model.context_metadata(&config.model_alias),
                                        &config.model.compaction_config(),
                                        state
                                            .running
                                            .as_ref()
                                            .map(|running| wire_turn_id(running.id)),
                                    )
                                });
                                match snapshot.and_then(|snapshot| {
                                    serde_json::to_string_pretty(&snapshot).map_err(|error| {
                                        AgentLoopError::InvalidConfiguration(error.to_string())
                                    })
                                }) {
                                    Ok(message) => output.message = message,
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::PinContext { item_id } => {
                                if let Err(error) = apply_registered_context_surgery(
                                    state,
                                    config,
                                    events,
                                    item_id.clone(),
                                    true,
                                )
                                .await
                                {
                                    let _ = respond.send(Err(error));
                                    return;
                                }
                                output.message = format!("pinned {}", item_id.0);
                            }
                            SessionCommandAction::EvictContext { item_id } => {
                                if let Err(error) = apply_registered_context_surgery(
                                    state,
                                    config,
                                    events,
                                    item_id.clone(),
                                    false,
                                )
                                .await
                                {
                                    let _ = respond.send(Err(error));
                                    return;
                                }
                                output.message = format!("evicted {}", item_id.0);
                            }
                            SessionCommandAction::Cost => {
                                match build_cost_snapshot(state, config).await.and_then(
                                    |snapshot| {
                                        serde_json::to_string_pretty(&snapshot).map_err(|error| {
                                            AgentLoopError::InvalidConfiguration(error.to_string())
                                        })
                                    },
                                ) {
                                    Ok(message) => output.message = message,
                                    Err(error) => {
                                        let _ = respond.send(Err(error));
                                        return;
                                    }
                                }
                            }
                            SessionCommandAction::Compact { instructions } => {
                                start_manual_compaction(
                                    state,
                                    config,
                                    turn_signals,
                                    active_turn,
                                    instructions,
                                    None,
                                );
                            }
                            SessionCommandAction::None => {}
                        }
                        let name = content
                            .trim_start()
                            .trim_start_matches('/')
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_owned();
                        emit(
                            state,
                            events,
                            &config.event_sink,
                            PendingEvent::CommandFinished {
                                name,
                                message: output.message,
                                unrestorable_paths,
                            },
                        )
                        .await
                        .map(|()| MessageDisposition::Command)
                    }
                    Err(error) => {
                        let persisted = emit(
                            state,
                            events,
                            &config.event_sink,
                            PendingEvent::Error {
                                message: error.to_string(),
                            },
                        )
                        .await;
                        Err(persisted
                            .err()
                            .unwrap_or_else(|| AgentLoopError::Extension(error.to_string())))
                    }
                };
                let _ = respond.send(disposition);
            } else if state.running.is_some() {
                state.queued.push_back(content.clone());
                let persisted = emit(
                    state,
                    events,
                    &config.event_sink,
                    PendingEvent::MessageQueued {
                        position: state.queued.len(),
                        content,
                        attachments: Vec::new(),
                    },
                )
                .await;
                if let Err(error) = persisted {
                    state.queued.pop_back();
                    let _ = respond.send(Err(error));
                } else {
                    let _ = respond.send(Ok(MessageDisposition::Queued));
                }
            } else {
                let result = start_turn(
                    state,
                    config,
                    tool_context,
                    turn_signals,
                    events,
                    vec![(content, attachments)],
                    active_turn,
                )
                .await;
                let _ = respond.send(result.map(|()| MessageDisposition::Started));
            }
        }
        #[cfg(test)]
        ActorCommand::Interrupt {
            target_turn,
            respond,
        } => {
            let interrupted = state.running.as_ref().is_some_and(|running| {
                if running.id != target_turn {
                    return false;
                }
                running.cancellation.cancel();
                true
            });
            let _ = respond.send(interrupted);
        }
        ActorCommand::Snapshot { respond } => {
            let _ = respond.send(SessionSnapshot {
                conversation: state.conversation.clone(),
                queued_messages: state.queued.iter().cloned().collect(),
                running: state.running.is_some(),
                completed_turns: state.completed_turns,
                model_alias: state.model_alias.clone(),
                active_shell: state.active_shell.clone(),
            });
        }
    }
}

async fn rewind_state(
    state: &mut ActorState,
    config: &SessionActorConfig,
    events: &broadcast::Sender<RoutedEvent>,
    to_turn: u64,
) -> Result<Vec<UnrestorablePath>, AgentLoopError> {
    if state.running.is_some() {
        return Err(AgentLoopError::InvalidConfiguration(
            "cannot rewind while a turn is running".to_owned(),
        ));
    }
    if let Some((pending_turn, pending)) = state.pending_rewind.clone() {
        if pending_turn != to_turn {
            return Err(AgentLoopError::InvalidConfiguration(format!(
                "rewind to turn {pending_turn} is awaiting acknowledgement"
            )));
        }
        if let Err(error) = config.checkpoints.acknowledge_rewind(&pending).await {
            state.poisoned = true;
            return Err(error);
        }
        state.pending_rewind = None;
        state.poisoned = false;
        return Ok(pending.unrestorable_paths);
    }
    let Some(&conversation_len) = state.turn_ends.get(&to_turn) else {
        return Err(AgentLoopError::InvalidConfiguration(format!(
            "turn {to_turn} is not a completed rewind target"
        )));
    };
    let historical = config
        .event_sink
        .read_after(None)
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    let historical = historical
        .iter()
        .rposition(|event| {
            matches!(event, EngineEvent::TurnFinished { turn_id, .. } if parse_turn_id(turn_id) == Ok(to_turn))
        })
        .map(|boundary| project_session_events(&historical[..=boundary]))
        .transpose()
        .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
    let operation_id = format!(
        "rewind-{}-{to_turn}",
        state
            .sequence
            .map_or(0, |sequence| sequence.saturating_add(1))
    );
    let rewind = config
        .checkpoints
        .prepare_apply_rewind(&config.session_id, to_turn, &operation_id)
        .await?;
    if let Err(error) = emit(
        state,
        events,
        &config.event_sink,
        PendingEvent::ConversationRewound {
            to_turn,
            operation_id,
            unrestorable_paths: rewind.unrestorable_paths.clone(),
        },
    )
    .await
    {
        state.poisoned = true;
        return Err(error);
    }
    if let Some(historical) = historical {
        state.conversation = historical.conversation;
        state.context_surgery = historical.context_surgery;
        state.pruned_tool_outputs = historical.pruned_tool_outputs;
        state.budgeter = historical.budgeter;
    } else {
        state.conversation.truncate(conversation_len);
        state
            .context_surgery
            .retain(|action| action.effective_after_agent_turn <= to_turn);
    }
    state.turn_ends.retain(|turn, _| *turn <= to_turn);
    state.completed_turns = u64::try_from(state.turn_ends.len()).unwrap_or(u64::MAX);
    state.queued.clear();
    state.pending_rewind = Some((to_turn, rewind.clone()));
    if let Err(error) = config.checkpoints.acknowledge_rewind(&rewind).await {
        state.poisoned = true;
        return Err(error);
    }
    state.pending_rewind = None;
    Ok(rewind.unrestorable_paths)
}

#[allow(clippy::too_many_lines)]
async fn handle_turn_signal(
    signal: TurnSignal,
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    tool_context: &ToolContext,
    turn_signals: &mpsc::UnboundedSender<TurnSignal>,
    events: &broadcast::Sender<RoutedEvent>,
    active_turn: &Arc<AtomicU64>,
) -> Result<(), AgentLoopError> {
    match signal {
        TurnSignal::Event(event) | TurnSignal::ToolOutput { event, .. } => {
            emit(state, events, &config.event_sink, event).await?;
        }
        TurnSignal::DurableEvent { kind, respond } => {
            let compaction_accounting = match &kind {
                PendingEvent::CompactionAttemptFinished {
                    summary_turn,
                    usage,
                    cost,
                }
                | PendingEvent::CompactionFinished {
                    summary_turn,
                    usage: Some(usage),
                    cost: Some(cost),
                    ..
                } => Some(TurnAccounting {
                    turn_id: wire_turn_id(*summary_turn),
                    attribution: AccountingAttribution::Compaction,
                    usage: (*usage).into(),
                    cost: cost.clone(),
                }),
                _ => None,
            };
            let result = emit(state, events, &config.event_sink, kind).await;
            if result.is_ok()
                && let Some(accounting) = compaction_accounting
            {
                state.accounting.push(accounting);
            }
            let _ = respond.send(result.clone());
            result?;
        }
        TurnSignal::Approval { request, respond } => {
            let Some(turn) = state.running.as_ref().map(|running| running.id) else {
                let _ = respond.send(ApprovalDecision::Deny);
                return Ok(());
            };
            if let Some(previous) = state.pending_approvals.insert(request.id.clone(), respond) {
                let _ = previous.send(ApprovalDecision::Deny);
            }
            emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::PermissionRequested { turn, request },
            )
            .await?;
        }
        TurnSignal::Question { request, respond } => {
            let Some(turn) = state.running.as_ref().map(|running| running.id) else {
                let _ = respond.send(String::new());
                return Ok(());
            };
            let question_id = QuestionId(format!("question-{turn}-{}", state.next_question));
            state.next_question = state.next_question.saturating_add(1);
            if let Some(previous) = state
                .pending_questions
                .insert(question_id.0.clone(), PendingQuestion { turn, respond })
            {
                let _ = previous.respond.send(String::new());
            }
            let response_kind = if request.options.is_empty() {
                QuestionResponseKind::Text
            } else {
                QuestionResponseKind::SelectOne
            };
            let question = Question {
                id: question_id.clone(),
                prompt: request.question,
                response_kind,
                options: request
                    .options
                    .into_iter()
                    .map(|value| QuestionOption {
                        label: value.clone(),
                        value,
                        description: None,
                    })
                    .collect(),
            };
            emit(
                state,
                events,
                &config.event_sink,
                PendingEvent::QuestionAsked {
                    turn,
                    question_id,
                    questions: vec![question],
                },
            )
            .await?;
        }
        TurnSignal::Complete(outcome) => {
            if state.running.as_ref().map(|running| running.id) != Some(outcome.turn) {
                return Ok(());
            }
            state.running = None;
            active_turn.store(0, Ordering::Release);
            state.pending_approvals.clear();
            for (_, pending) in std::mem::take(&mut state.pending_questions) {
                let _ = pending.respond.send(String::new());
            }
            state.conversation = outcome.conversation;
            state.context_surgery = outcome.context_surgery;
            state.pruned_tool_outputs = outcome.pruned_tool_outputs;
            state.budgeter = outcome.budgeter;
            state.accounting.push(TurnAccounting {
                turn_id: wire_turn_id(outcome.turn),
                attribution: AccountingAttribution::Main,
                usage: outcome.usage.into(),
                cost: outcome.cost.clone(),
            });
            state.completed_turns = state.completed_turns.saturating_add(1);
            state
                .turn_ends
                .insert(outcome.turn, state.conversation.len());
            let mut terminal_events = Vec::with_capacity(3);
            if let Some(text) = outcome.deferred_terminal_delta {
                terminal_events.push(PendingEvent::TextDelta {
                    turn: outcome.turn,
                    text,
                });
            }
            if let Some(assistant_turn) = outcome.deferred_terminal_turn {
                terminal_events.push(PendingEvent::ConversationTurnCommitted {
                    agent_turn: outcome.turn,
                    turn: assistant_turn,
                });
            }
            terminal_events.push(PendingEvent::TurnFinished {
                turn: outcome.turn,
                status: outcome.status,
                usage: outcome.usage,
                cost: outcome.cost,
            });
            emit_batch(state, events, &config.event_sink, terminal_events).await?;
            if !state.queued.is_empty() {
                let messages = state
                    .queued
                    .drain(..)
                    .map(|content| (content, Vec::new()))
                    .collect();
                start_turn(
                    state,
                    config,
                    tool_context,
                    turn_signals,
                    events,
                    messages,
                    active_turn,
                )
                .await?;
            }
        }
        TurnSignal::ManualCompactionComplete {
            turn,
            conversation,
            context_surgery,
            result,
            completion,
        } => {
            if state.running.as_ref().map(|running| running.id) == Some(turn) {
                state.running = None;
                active_turn.store(0, Ordering::Release);
                if result.is_ok() {
                    state.conversation = conversation;
                    state.context_surgery = context_surgery;
                }
            }
            if let Some(completion) = completion {
                let _ = completion.send(result.map(|()| ProtocolCompletion::Unit));
            }
            if state.running.is_none() && !state.queued.is_empty() {
                let messages = state
                    .queued
                    .drain(..)
                    .map(|content| (content, Vec::new()))
                    .collect();
                start_turn(
                    state,
                    config,
                    tool_context,
                    turn_signals,
                    events,
                    messages,
                    active_turn,
                )
                .await?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn start_turn(
    state: &mut ActorState,
    config: &Arc<SessionActorConfig>,
    tool_context: &ToolContext,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    events: &broadcast::Sender<RoutedEvent>,
    messages: Vec<(String, Vec<Attachment>)>,
    active_turn: &Arc<AtomicU64>,
) -> Result<(), AgentLoopError> {
    let messages = messages
        .into_iter()
        .map(|(content, attachments)| {
            prepare_user_message(
                &content,
                &attachments,
                &state.model_alias,
                config.model.as_ref(),
            )
            .map_err(AgentLoopError::InvalidConfiguration)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let turn = state.next_turn;
    state.next_turn = state.next_turn.saturating_add(1);
    let cancellation = CancellationToken::default();
    state.running = Some(RunningTurn {
        id: turn,
        cancellation: cancellation.clone(),
        caused_by: state.transient_cause.clone(),
    });
    active_turn.store(turn, Ordering::Release);
    let prepare_users_synchronously = config
        .hooks
        .registrations(HookEvent::UserPromptSubmit)
        .len()
        == 0;
    let mut conversation = state.conversation.clone();
    let opening_capacity = if prepare_users_synchronously {
        messages.len().saturating_mul(2).saturating_add(1)
    } else {
        messages.len().saturating_add(1)
    };
    let mut opening_events = Vec::with_capacity(opening_capacity);
    opening_events.push(PendingEvent::TurnStarted { turn });
    opening_events.extend(
        messages
            .iter()
            .map(|message| PendingEvent::UserMessageAccepted {
                turn,
                content: message.content.clone(),
                attachments: message.stored_attachments.clone(),
            }),
    );
    if prepare_users_synchronously {
        for message in &messages {
            let user_turn = message.turn(message.content.clone());
            opening_events.push(PendingEvent::ConversationTurnCommitted {
                agent_turn: turn,
                turn: user_turn.clone(),
            });
            conversation.push(user_turn);
        }
    }
    if let Err(error) = emit_batch(state, events, &config.event_sink, opening_events).await {
        state.running = None;
        active_turn.store(0, Ordering::Release);
        return Err(error);
    }
    let panic_conversation = conversation.clone();
    let run_messages = if prepare_users_synchronously {
        Vec::new()
    } else {
        messages
    };
    let config = Arc::new(config.with_model_alias(state.model_alias.clone()));
    let protocol_asker: Arc<dyn QuestionAsker> = Arc::new(ActorQuestionAsker {
        signals: signals.clone(),
        cancellation: cancellation.clone(),
    });
    let tool_context = tool_context
        .clone()
        .with_cancellation(cancellation.clone())
        .with_question_asker(protocol_asker);
    let signals = signals.clone();
    let state_context_surgery = state.context_surgery.clone();
    let state_pruned_tool_outputs = state.pruned_tool_outputs.clone();
    let panic_context_surgery = state_context_surgery.clone();
    let panic_pruned_tool_outputs = state_pruned_tool_outputs.clone();
    let state_budgeter = state.budgeter;
    let local_session_accounting = session_accounting_fallback(&state.accounting);
    tokio::spawn(async move {
        let outcome = AssertUnwindSafe(run_turn(
            turn,
            run_messages,
            conversation,
            config,
            tool_context,
            cancellation,
            signals.clone(),
            state_context_surgery,
            state_pruned_tool_outputs,
            state_budgeter,
            local_session_accounting,
        ))
        .catch_unwind()
        .await
        .unwrap_or(TurnOutcome {
            turn,
            conversation: panic_conversation,
            status: AgentTurnStatus::Failed,
            usage: SessionUsage::default(),
            cost: unavailable_cost(),
            deferred_terminal_delta: None,
            deferred_terminal_turn: None,
            context_surgery: panic_context_surgery,
            pruned_tool_outputs: panic_pruned_tool_outputs,
            budgeter: state_budgeter,
        });
        let _ = signals.send(TurnSignal::Complete(outcome));
    });
    Ok(())
}

async fn emit(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    sink: &Arc<dyn SessionEventSink>,
    kind: PendingEvent,
) -> Result<(), AgentLoopError> {
    emit_batch(state, events, sink, vec![kind]).await
}

async fn emit_batch(
    state: &mut ActorState,
    events: &broadcast::Sender<RoutedEvent>,
    sink: &Arc<dyn SessionEventSink>,
    kinds: Vec<PendingEvent>,
) -> Result<(), AgentLoopError> {
    if kinds.is_empty() {
        return Ok(());
    }
    let first_expected = match state.sequence {
        Some(sequence) => sequence
            .checked_add(1)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?,
        None => 0,
    };
    let caused_by = state.caused_by();
    let requested = kinds
        .into_iter()
        .enumerate()
        .map(|(offset, kind)| {
            let offset = u64::try_from(offset)
                .map_err(|_| AgentLoopError::Persistence("event batch overflow".to_owned()))?;
            let sequence = first_expected
                .checked_add(offset)
                .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
            Ok(kind.stamp(EventMeta {
                protocol_version: PROTOCOL_VERSION,
                session_id: state.session_id.clone(),
                sequence_id: SequenceId(sequence),
                emitted_at: state.event_clock.emitted_at(),
                caused_by: caused_by.clone(),
            }))
        })
        .collect::<Result<Vec<_>, AgentLoopError>>()?;
    let persisted = sink.append_batch(requested.clone()).await?;
    if persisted.len() != requested.len() {
        return Err(AgentLoopError::Persistence(format!(
            "event sink returned {} events for a batch of {}",
            persisted.len(),
            requested.len()
        )));
    }
    for (offset, (event, requested_event)) in persisted.iter().zip(&requested).enumerate() {
        let offset = u64::try_from(offset)
            .map_err(|_| AgentLoopError::Persistence("event batch overflow".to_owned()))?;
        let expected = first_expected
            .checked_add(offset)
            .ok_or_else(|| AgentLoopError::Persistence("event sequence overflow".to_owned()))?;
        let meta = event_meta(event).ok_or_else(|| {
            AgentLoopError::Persistence(
                "event sink returned a connection-scoped acknowledgement".to_owned(),
            )
        })?;
        if meta.protocol_version != SESSION_EVENT_VERSION {
            return Err(AgentLoopError::Persistence(format!(
                "event sink returned unsupported version {}",
                meta.protocol_version
            )));
        }
        if meta.session_id != state.session_id {
            return Err(AgentLoopError::Persistence(
                "event sink substituted a different session id".to_owned(),
            ));
        }
        if meta.sequence_id.0 != expected {
            return Err(AgentLoopError::Persistence(format!(
                "event sink returned sequence {}, expected {expected}",
                meta.sequence_id.0
            )));
        }
        if event != requested_event {
            return Err(AgentLoopError::Persistence(
                "event sink substituted a different event payload".to_owned(),
            ));
        }
    }
    state.sequence = persisted
        .last()
        .and_then(event_meta)
        .map(|meta| meta.sequence_id.0);
    for event in persisted {
        let _ = events.send(RoutedEvent {
            target: None,
            event,
        });
    }
    Ok(())
}

struct ChannelApprover {
    signals: mpsc::UnboundedSender<TurnSignal>,
    cancellation: CancellationToken,
}

struct ActorQuestionAsker {
    signals: mpsc::UnboundedSender<TurnSignal>,
    cancellation: CancellationToken,
}

#[async_trait]
impl QuestionAsker for ActorQuestionAsker {
    async fn ask(
        &self,
        request: AskUserInput,
        _cancellation: CancellationToken,
    ) -> Result<String, ToolError> {
        let (respond, receive) = oneshot::channel();
        self.signals
            .send(TurnSignal::Question { request, respond })
            .map_err(|_| ToolError::Cancelled)?;
        tokio::select! {
            () = self.cancellation.cancelled() => Err(ToolError::Cancelled),
            response = receive => response.map_err(|_| ToolError::Cancelled),
        }
    }
}

#[async_trait]
impl PermissionApprover for ChannelApprover {
    async fn decide(&self, request: PermissionRequest) -> ApprovalDecision {
        let (respond, receive) = oneshot::channel();
        if self
            .signals
            .send(TurnSignal::Approval { request, respond })
            .is_err()
        {
            return ApprovalDecision::Deny;
        }
        tokio::select! {
            () = self.cancellation.cancelled() => ApprovalDecision::Deny,
            decision = receive => decision.unwrap_or(ApprovalDecision::Deny),
        }
    }
}

#[derive(Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: Option<Value>,
    index: usize,
}

struct ToolExecution {
    call: PendingToolCall,
    output: ToolOutput,
    is_error: bool,
}

enum PreparedToolCall {
    Execute {
        call: PendingToolCall,
        tool: Arc<dyn rw_tools::Tool>,
        arguments: Value,
        read_only: bool,
        mutation_scope: MutationScope,
    },
    Complete(ToolExecution),
}

impl PreparedToolCall {
    fn call(&self) -> &PendingToolCall {
        match self {
            Self::Execute { call, .. } | Self::Complete(ToolExecution { call, .. }) => call,
        }
    }
}

struct OrderedOutputState {
    current: usize,
    buffered: BTreeMap<usize, Vec<BoundedOutputChunk>>,
}

struct BoundedOutputChunk {
    id: String,
    chunk: ToolOutputChunk,
    permit: OwnedSemaphorePermit,
}

struct OrderedOutputCoordinator {
    turn: u64,
    signals: mpsc::UnboundedSender<TurnSignal>,
    state: Mutex<OrderedOutputState>,
    permits: Arc<Semaphore>,
    redactor: Arc<dyn SecretRedactor>,
}

impl OrderedOutputCoordinator {
    fn new(
        turn: u64,
        signals: mpsc::UnboundedSender<TurnSignal>,
        redactor: Arc<dyn SecretRedactor>,
    ) -> Self {
        Self {
            turn,
            signals,
            state: Mutex::new(OrderedOutputState {
                current: 0,
                buffered: BTreeMap::new(),
            }),
            permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS)),
            redactor,
        }
    }

    async fn emit(
        &self,
        index: usize,
        id: &str,
        mut chunk: ToolOutputChunk,
    ) -> Result<(), ToolError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| ToolError::Output("tool output channel is closed".to_owned()))?;
        chunk.content = self.redactor.redact(&chunk.content);
        let bounded = BoundedOutputChunk {
            id: id.to_owned(),
            chunk,
            permit,
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if index == state.current {
            drop(state);
            self.send_chunk(bounded);
        } else {
            state.buffered.entry(index).or_default().push(bounded);
        }
        Ok(())
    }

    fn advance(&self, next: usize) {
        let buffered = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.current = next;
            state.buffered.remove(&next).unwrap_or_default()
        };
        for chunk in buffered {
            self.send_chunk(chunk);
        }
    }

    fn send_chunk(&self, bounded: BoundedOutputChunk) {
        let _ = self.signals.send(TurnSignal::ToolOutput {
            event: PendingEvent::ToolOutput {
                turn: self.turn,
                id: bounded.id,
                stream: format!("{:?}", bounded.chunk.stream).to_ascii_lowercase(),
                chunk: bounded.chunk.content,
            },
            _permit: bounded.permit,
        });
    }
}

struct OrderedOutputSink {
    index: usize,
    id: String,
    coordinator: Arc<OrderedOutputCoordinator>,
    open: Arc<AtomicBool>,
    totals: Mutex<(usize, usize, bool)>,
}

#[async_trait]
impl ToolOutputSink for OrderedOutputSink {
    async fn emit(&self, chunk: ToolOutputChunk) -> Result<(), ToolError> {
        if !self.open.load(Ordering::Acquire) {
            return Err(ToolError::Output("tool output stream is closed".to_owned()));
        }
        let chunk = {
            let mut totals = self
                .totals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            totals.0 = totals.0.saturating_add(chunk.content.len());
            totals.1 = totals.1.saturating_add(1);
            if totals.0 > MAX_LIVE_TOOL_OUTPUT_BYTES || totals.1 > MAX_LIVE_TOOL_OUTPUT_CHUNKS {
                if totals.2 {
                    return Ok(());
                }
                totals.2 = true;
                ToolOutputChunk {
                    stream: chunk.stream,
                    content: "[live tool output truncated; command output continues to drain]"
                        .to_owned(),
                }
            } else {
                chunk
            }
        };
        self.coordinator.emit(self.index, &self.id, chunk).await
    }
}

struct DoomLoopGuard {
    threshold: usize,
    last_failure: Option<String>,
    identical_failures: usize,
}

impl DoomLoopGuard {
    fn new(threshold: usize) -> Self {
        Self {
            threshold,
            last_failure: None,
            identical_failures: 0,
        }
    }

    fn observe(&mut self, call: &PendingToolCall, result: &ToolExecution) -> bool {
        if !result.is_error {
            self.last_failure = None;
            self.identical_failures = 0;
            return false;
        }
        let signature = serde_json::to_string(&json!({
            "name": call.name,
            "arguments": call.arguments,
            "output": result.output,
        }))
        .unwrap_or_else(|_| "unserializable-tool-failure".to_owned());
        if self.last_failure.as_deref() == Some(&signature) {
            self.identical_failures = self.identical_failures.saturating_add(1);
        } else {
            self.last_failure = Some(signature);
            self.identical_failures = 1;
        }
        self.identical_failures >= self.threshold
    }
}

fn send_event(signals: &mpsc::UnboundedSender<TurnSignal>, kind: PendingEvent) {
    let _ = signals.send(TurnSignal::Event(kind));
}

fn flush_pending_text_delta(
    pending: &mut Option<String>,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
) {
    if let Some(text) = pending.take() {
        send_event(signals, PendingEvent::TextDelta { turn, text });
    }
}

async fn persist_event(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    kind: PendingEvent,
) -> Result<(), AgentLoopError> {
    let (respond, receive) = oneshot::channel();
    signals
        .send(TurnSignal::DurableEvent { kind, respond })
        .map_err(|_| AgentLoopError::Closed)?;
    receive.await.map_err(|_| AgentLoopError::Closed)?
}

async fn persist_conversation_turn(
    signals: &mpsc::UnboundedSender<TurnSignal>,
    agent_turn: u64,
    turn: &Turn,
) -> Result<(), AgentLoopError> {
    persist_event(
        signals,
        PendingEvent::ConversationTurnCommitted {
            agent_turn,
            turn: turn.clone(),
        },
    )
    .await
}

fn append_text(blocks: &mut Vec<Block>, delta: &str) {
    if let Some(Block::Text { text }) = blocks.last_mut() {
        text.push_str(delta);
    } else {
        blocks.push(Block::Text {
            text: delta.to_owned(),
        });
    }
}

fn tool_definition(descriptor: ToolDescriptor) -> ToolDefinition {
    ToolDefinition {
        name: descriptor.name,
        description: descriptor.description,
        input_schema: descriptor.input_schema,
    }
}

fn context_action_state(actions: &[ContextSurgeryAction], item_id: &ContextItemId) -> (bool, bool) {
    actions
        .iter()
        .rev()
        .find(|action| &action.item_id == item_id)
        .map_or((false, false), |action| {
            if action.pinned {
                (true, false)
            } else {
                (false, true)
            }
        })
}

fn prompt_tool_output(
    output: &ToolOutput,
    is_pruned: bool,
    toon: &mut ToonPromptEncoder,
) -> ToolOutput {
    if is_pruned {
        return ToolOutput::Text {
            text: PRUNED_TOOL_OUTPUT_REPLACEMENT.to_owned(),
        };
    }
    match output {
        ToolOutput::Text { .. } => output.clone(),
        ToolOutput::Structured { value } => toon.encode(value).map_or_else(
            |_| output.clone(),
            |encoded| ToolOutput::Text {
                text: encoded.prompt_text,
            },
        ),
        ToolOutput::Mixed { parts } => ToolOutput::Mixed {
            parts: parts
                .iter()
                .map(|part| match part {
                    ToolOutputPart::Structured { value } => toon.encode(value).map_or_else(
                        |_| part.clone(),
                        |encoded| ToolOutputPart::Text {
                            text: encoded.prompt_text,
                        },
                    ),
                    ToolOutputPart::Text { .. } | ToolOutputPart::Image { .. } => part.clone(),
                })
                .collect(),
        },
    }
}

fn prompt_turn(
    turn: &Turn,
    pruned_tool_outputs: &BTreeMap<String, u64>,
    toon: &mut ToonPromptEncoder,
) -> Turn {
    let mut prompt = turn.clone();
    prompt.blocks = prompt
        .blocks
        .into_iter()
        .map(|block| match block {
            Block::ToolResult {
                id,
                output,
                is_error,
            } => {
                let is_pruned = pruned_tool_outputs.contains_key(&id.0);
                Block::ToolResult {
                    id,
                    output: prompt_tool_output(&output, is_pruned, toon),
                    is_error,
                }
            }
            other => other,
        })
        .collect();
    prompt
}

fn assemble_session_context(
    config: &SessionActorConfig,
    conversation: &[Turn],
    queued: &VecDeque<String>,
    surgery: &[ContextSurgeryAction],
    pruned_tool_outputs: &BTreeMap<String, u64>,
    include_prompt_dump: bool,
) -> Result<AssembledContext, AgentLoopError> {
    let stable_prefix = config
        .initial_session_context
        .iter()
        .enumerate()
        .map(|(index, turn)| AssemblyContextItem {
            id: AssemblyContextItemId(format!("system:{index}")),
            kind: if index == 0 {
                AssemblyContextItemKind::System
            } else {
                AssemblyContextItemKind::ProjectInstructions
            },
            label: if index == 0 {
                "Base system instructions".to_owned()
            } else {
                format!("Project instructions {index}")
            },
            provenance: ContextProvenance::BuiltIn,
            turn: turn.clone(),
            pinned: false,
            evicted: false,
            summarized: false,
            pruned: false,
        })
        .collect();
    let mut toon = ToonPromptEncoder::default();
    let conversation = conversation
        .iter()
        .enumerate()
        .map(|(index, turn)| {
            let item_id = ContextItemId(format!("conversation:{index}"));
            let (pinned, evicted) = context_action_state(surgery, &item_id);
            let pruned = turn.blocks.iter().any(|block| {
                matches!(block, Block::ToolResult { id, .. } if pruned_tool_outputs.contains_key(&id.0))
            });
            AssemblyContextItem {
                id: AssemblyContextItemId(item_id.0),
                kind: if pinned {
                    AssemblyContextItemKind::Pin
                } else {
                    AssemblyContextItemKind::Conversation
                },
                label: format!("{:?} turn {}", turn.role, index.saturating_add(1)),
                provenance: if pinned {
                    ContextProvenance::UserPin
                } else {
                    ContextProvenance::Conversation {
                        sequence: u64::try_from(index).unwrap_or(u64::MAX),
                    }
                },
                turn: prompt_turn(turn, pruned_tool_outputs, &mut toon),
                pinned,
                evicted,
                summarized: turn.meta.summary,
                pruned,
            }
        })
        .collect();
    let queued = queued
        .iter()
        .enumerate()
        .map(|(index, content)| AssemblyContextItem {
            id: AssemblyContextItemId(format!("queued:{index}")),
            kind: AssemblyContextItemKind::Queued,
            label: format!("Queued message {}", index.saturating_add(1)),
            provenance: ContextProvenance::ClientQueue,
            turn: Turn {
                role: Role::User,
                blocks: vec![Block::Text {
                    text: content.clone(),
                }],
                meta: TurnMeta::default(),
            },
            pinned: false,
            evicted: false,
            summarized: false,
            pruned: false,
        })
        .collect();
    let metadata = config.model.context_metadata(&config.model_alias);
    ContextAssembler::assemble(AssemblyInput {
        stable_prefix,
        conversation,
        pins: Vec::new(),
        queued,
        tools: config
            .tools
            .descriptors()
            .into_iter()
            .map(tool_definition)
            .collect(),
        cache_support: metadata
            .cache_breakpoints
            .unwrap_or(CacheBreakpointSupport::None),
        include_prompt_dump,
    })
    .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))
}

fn protocol_context_kind(kind: AssemblyContextItemKind, role: Option<&Role>) -> ContextItemKind {
    match kind {
        AssemblyContextItemKind::System => ContextItemKind::System,
        AssemblyContextItemKind::ProjectInstructions => ContextItemKind::ProjectInstructions,
        AssemblyContextItemKind::SkillIndex => ContextItemKind::ToolDefinitions,
        AssemblyContextItemKind::Pin => ContextItemKind::Pinned,
        AssemblyContextItemKind::Queued => ContextItemKind::QueuedMessage,
        AssemblyContextItemKind::Conversation => {
            if role == Some(&Role::Tool) {
                ContextItemKind::ToolResult
            } else {
                ContextItemKind::Conversation
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn context_snapshot(
    assembled: &AssembledContext,
    durable_conversation: &[Turn],
    pruned_tool_outputs: &BTreeMap<String, u64>,
    metadata: ModelContextMetadata,
    compaction: &CompactionConfig,
    turn_id: Option<TurnId>,
) -> ContextSnapshot {
    let (usable_tokens, reserved_tokens) = metadata.max_context_tokens.map_or((0, 0), |window| {
        let policy = OverflowPolicy {
            context_window_tokens: window,
            max_output_tokens: metadata.max_output_tokens.unwrap_or(0),
            reserved_tokens_override: compaction.reserved_tokens,
            automatic_compaction: compaction.auto,
        };
        let reserved = policy.reserved_tokens();
        (window.saturating_sub(reserved), reserved)
    });
    let mut items = assembled
        .items
        .iter()
        .filter(|item| {
            let Some(index) = item
                .id
                .0
                .strip_prefix("conversation:")
                .and_then(|index| index.parse::<usize>().ok())
            else {
                return true;
            };
            durable_conversation
                .get(index)
                .is_none_or(|turn| turn.role != Role::Tool)
        })
        .map(|item| {
            let (source, machine_local_path) = match &item.provenance {
                ContextProvenance::BuiltIn => ("built_in".to_owned(), None),
                ContextProvenance::ProjectFile { path } => {
                    ("project_file".to_owned(), Some(path.clone()))
                }
                ContextProvenance::Extension { extension_id } => {
                    (format!("extension:{extension_id}"), None)
                }
                ContextProvenance::Conversation { sequence } => {
                    (format!("conversation:{sequence}"), None)
                }
                ContextProvenance::UserPin => ("user_pin".to_owned(), None),
                ContextProvenance::ClientQueue => ("client_queue".to_owned(), None),
            };
            let role = item
                .assembled_turn_index
                .and_then(|index| assembled.turns.get(index))
                .map(|turn| &turn.role);
            ContextItemSnapshot {
                item_id: ContextItemId(item.id.0.clone()),
                kind: protocol_context_kind(item.kind, role),
                label: item.label.clone(),
                source,
                machine_local_path,
                estimated_tokens: item.tokens,
                state: ContextItemState {
                    pinned: item.pinned,
                    evicted: item.evicted,
                    summarized: item.summarized,
                    pruned: item.pruned,
                },
            }
        })
        .collect::<Vec<_>>();
    items.extend(assembled.tools.iter().map(|tool| ContextItemSnapshot {
        item_id: ContextItemId(format!("tool:{}", tool.name)),
        kind: ContextItemKind::ToolDefinitions,
        label: tool.name.clone(),
        source: "tool_registry".to_owned(),
        machine_local_path: None,
        estimated_tokens: LocalTokenEstimator::tools(std::slice::from_ref(tool)),
        state: ContextItemState {
            pinned: true,
            evicted: false,
            summarized: false,
            pruned: false,
        },
    }));
    for (index, turn) in durable_conversation.iter().enumerate() {
        if turn.role != Role::Tool {
            continue;
        }
        let context_item_id = format!("conversation:{index}");
        let parent = assembled
            .items
            .iter()
            .find(|item| item.id.0 == context_item_id);
        let prompt_turn = parent
            .and_then(|item| item.assembled_turn_index)
            .and_then(|index| assembled.turns.get(index));
        for block in &turn.blocks {
            if let Block::ToolResult { id, .. } = block {
                let prompt_block = prompt_turn
                    .and_then(|turn| {
                        turn.blocks.iter().find(|block| {
                            matches!(block, Block::ToolResult { id: prompt_id, .. } if prompt_id == id)
                        })
                    })
                    .unwrap_or(block);
                items.push(ContextItemSnapshot {
                    item_id: ContextItemId(format!("tool_result:{}", id.0)),
                    kind: ContextItemKind::ToolResult,
                    label: format!("Tool result {}", id.0),
                    source: "conversation_tool_result".to_owned(),
                    machine_local_path: None,
                    estimated_tokens: LocalTokenEstimator::turn(&Turn {
                        role: Role::Tool,
                        blocks: vec![prompt_block.clone()],
                        meta: TurnMeta::default(),
                    }),
                    state: ContextItemState {
                        pinned: parent.is_some_and(|item| item.pinned),
                        evicted: parent.is_some_and(|item| item.evicted),
                        summarized: parent.is_some_and(|item| item.summarized),
                        pruned: pruned_tool_outputs.contains_key(&id.0),
                    },
                });
            }
        }
    }
    ContextSnapshot {
        turn_id,
        stable_prefix_hash: assembled.stable_prefix_hash.clone(),
        used_tokens: assembled.token_totals.total,
        usable_tokens,
        reserved_tokens,
        cache_breakpoints: assembled
            .cache_breakpoints
            .iter()
            .map(|breakpoint| CacheBreakpoint {
                after_item_id: breakpoint
                    .after_item_id
                    .as_ref()
                    .map(|id| ContextItemId(id.0.clone())),
            })
            .collect(),
        items,
    }
}

fn prompt_dump(
    assembled: &AssembledContext,
    model_alias: &str,
    turn_id: Option<TurnId>,
) -> PromptDump {
    PromptDump {
        turn_id,
        model_alias: ModelAlias(model_alias.to_owned()),
        turns: assembled.turns.clone(),
        tools: assembled
            .tools
            .iter()
            .map(|tool| PromptTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            })
            .collect(),
        stable_prefix_hash: assembled.stable_prefix_hash.clone(),
        cache_breakpoints: assembled
            .cache_breakpoints
            .iter()
            .map(|breakpoint| CacheBreakpoint {
                after_item_id: breakpoint
                    .after_item_id
                    .as_ref()
                    .map(|id| ContextItemId(id.0.clone())),
            })
            .collect(),
        estimated_tokens: assembled.token_totals.total,
    }
}

fn hook_event_name(event: HookEvent) -> &'static str {
    match event {
        HookEvent::SessionStart => "session_start",
        HookEvent::SessionEnd => "session_end",
        HookEvent::UserPromptSubmit => "user_prompt_submit",
        HookEvent::PreTool => "pre_tool",
        HookEvent::PostTool => "post_tool",
        HookEvent::PreCompact => "pre_compact",
        HookEvent::TurnEnd => "turn_end",
        HookEvent::PermissionCheck => "permission_check",
    }
}

fn report_hook_failures(
    event: HookEvent,
    failures: &[HookFailure],
    signals: &mpsc::UnboundedSender<TurnSignal>,
) {
    for failure in failures {
        send_event(
            signals,
            PendingEvent::HookFailure {
                event: hook_event_name(event).to_owned(),
                hook_id: failure.hook_id().to_owned(),
                fail_closed: failure.policy() == HookFailurePolicy::FailClosed,
                message: failure.error().to_string(),
            },
        );
    }
}

async fn dispatch_hook(
    dispatcher: &HookDispatcher,
    event: HookEvent,
    payload: Value,
    cancellation: &CancellationToken,
) -> Result<HookDispatchResult, AgentLoopError> {
    tokio::select! {
        () = cancellation.cancelled() => Err(AgentLoopError::Extension(
            format!("{} hook dispatch cancelled", hook_event_name(event)),
        )),
        result = dispatcher.dispatch(event, payload) => Ok(result),
    }
}

fn hook_rejection(status: &HookDispatchStatus) -> Option<String> {
    match status {
        HookDispatchStatus::Completed => None,
        HookDispatchStatus::Blocked { hook_id, message } => {
            Some(format!("hook `{hook_id}` blocked the operation: {message}"))
        }
        HookDispatchStatus::FailedClosed { hook_id } => {
            Some(format!("hook `{hook_id}` failed closed"))
        }
    }
}

fn permission_hook_override(
    status: &HookDispatchStatus,
    payload: &Value,
) -> Option<PermissionOutcome> {
    if !matches!(status, HookDispatchStatus::Completed) {
        return Some(PermissionOutcome::Denied);
    }
    match payload.get("decision").and_then(Value::as_str) {
        Some("allow") => Some(PermissionOutcome::Allowed),
        Some("deny") => Some(PermissionOutcome::Denied),
        _ => None,
    }
}

fn failed_execution(call: PendingToolCall, message: impl Into<String>) -> ToolExecution {
    ToolExecution {
        call,
        output: ToolOutput::Text {
            text: message.into(),
        },
        is_error: true,
    }
}

struct ResolvedToolSecurity {
    tool: Arc<dyn rw_tools::Tool>,
    capabilities: Vec<rw_types::ToolCapability>,
    mutation_scope: MutationScope,
    read_only: bool,
}

fn resolve_tool_security(
    config: &SessionActorConfig,
    name: &str,
    arguments: &Value,
) -> Option<ResolvedToolSecurity> {
    let tool = config.tools.resolve(name)?;
    let descriptor = tool.descriptor();
    let mutation_scope = config
        .tools
        .mutation_scope(name, arguments)
        .unwrap_or(MutationScope::OpaqueWorkspace);
    let mut capabilities = descriptor.capabilities.capabilities().to_vec();
    if !matches!(mutation_scope, MutationScope::None)
        && !capabilities.contains(&rw_types::ToolCapability::WriteFilesystem)
    {
        capabilities.push(rw_types::ToolCapability::WriteFilesystem);
    }
    let read_only = matches!(mutation_scope, MutationScope::None)
        && !capabilities.is_empty()
        && capabilities
            .iter()
            .all(|capability| matches!(capability, rw_types::ToolCapability::ReadFilesystem));
    Some(ResolvedToolSecurity {
        tool,
        capabilities,
        mutation_scope,
        read_only,
    })
}

async fn authorize_tool_call(
    call: &PendingToolCall,
    arguments: &Value,
    capabilities: Vec<rw_types::ToolCapability>,
    config: &SessionActorConfig,
    approver: &dyn PermissionApprover,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> Result<(), String> {
    let request = PermissionRequest {
        id: call.id.clone(),
        tool_name: call.name.clone(),
        arguments: arguments.clone(),
        capabilities,
    };
    let permission_hook = dispatch_hook(
        &config.hooks,
        HookEvent::PermissionCheck,
        json!({
            "id": request.id,
            "name": request.tool_name,
            "arguments": request.arguments,
            "capabilities": request.capabilities,
        }),
        cancellation,
    )
    .await
    .map_err(|error| error.to_string())?;
    report_hook_failures(
        HookEvent::PermissionCheck,
        permission_hook.failures(),
        signals,
    );
    let permission = config
        .permissions
        .authorize_with_override(
            request,
            approver,
            permission_hook_override(permission_hook.status(), permission_hook.payload()),
        )
        .await;
    if permission == PermissionOutcome::Denied {
        Err(format!("permission denied for tool `{}`", call.name))
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
async fn prepare_tool_call(
    turn: u64,
    mut call: PendingToolCall,
    config: &SessionActorConfig,
    approver: &dyn PermissionApprover,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> PreparedToolCall {
    let Some(arguments) = call.arguments.clone() else {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "provider did not finish tool-call arguments",
        ));
    };
    send_event(
        signals,
        PendingEvent::ToolCallStarted {
            turn,
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: arguments.clone(),
            index: call.index,
        },
    );
    let Some(initial_security) = resolve_tool_security(config, &call.name, &arguments) else {
        let name = call.name.clone();
        return PreparedToolCall::Complete(failed_execution(
            call,
            format!("unknown tool `{name}`"),
        ));
    };
    if let Err(message) = authorize_tool_call(
        &call,
        &arguments,
        initial_security.capabilities,
        config,
        approver,
        cancellation,
        signals,
    )
    .await
    {
        return PreparedToolCall::Complete(failed_execution(call, message));
    }
    let original_name = call.name.clone();
    let original_arguments = arguments.clone();
    let pre_tool = match dispatch_hook(
        &config.hooks,
        HookEvent::PreTool,
        json!({
            "id": call.id,
            "name": call.name,
            "arguments": arguments,
        }),
        cancellation,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return PreparedToolCall::Complete(failed_execution(call, error.to_string())),
    };
    report_hook_failures(HookEvent::PreTool, pre_tool.failures(), signals);
    if let Some(message) = hook_rejection(pre_tool.status()) {
        return PreparedToolCall::Complete(failed_execution(call, message));
    }
    let Some(name) = pre_tool.payload().get("name").and_then(Value::as_str) else {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "pre_tool hook returned an invalid tool name",
        ));
    };
    if name.trim().is_empty() {
        return PreparedToolCall::Complete(failed_execution(
            call,
            "pre_tool hook returned an empty tool name",
        ));
    }
    call.name = name.to_owned();
    let arguments = pre_tool
        .payload()
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Null);
    call.arguments = Some(arguments.clone());
    let Some(security) = resolve_tool_security(config, &call.name, &arguments) else {
        let name = call.name.clone();
        return PreparedToolCall::Complete(failed_execution(
            call,
            format!("unknown tool `{name}`"),
        ));
    };
    if (call.name != original_name || arguments != original_arguments)
        && let Err(message) = authorize_tool_call(
            &call,
            &arguments,
            security.capabilities,
            config,
            approver,
            cancellation,
            signals,
        )
        .await
    {
        return PreparedToolCall::Complete(failed_execution(call, message));
    }
    PreparedToolCall::Execute {
        call,
        tool: security.tool,
        arguments,
        read_only: security.read_only,
        mutation_scope: security.mutation_scope,
    }
}

fn tool_result_output(result: ToolResult) -> ToolOutput {
    if result.data.is_null() && !result.truncated {
        return ToolOutput::Text {
            text: result.content,
        };
    }
    let structured = ToolOutputPart::Structured {
        value: json!({
            "data": result.data,
            "truncated": result.truncated,
        }),
    };
    if result.content.is_empty() {
        ToolOutput::Mixed {
            parts: vec![structured],
        }
    } else {
        ToolOutput::Mixed {
            parts: vec![
                ToolOutputPart::Text {
                    text: result.content,
                },
                structured,
            ],
        }
    }
}

fn redact_json(value: &mut Value, redactor: &dyn SecretRedactor) {
    match value {
        Value::String(text) => *text = redactor.redact(text),
        Value::Array(values) => {
            for value in values {
                redact_json(value, redactor);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_json(value, redactor);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn redact_tool_output(output: &mut ToolOutput, redactor: &dyn SecretRedactor) {
    match output {
        ToolOutput::Text { text } => *text = redactor.redact(text),
        ToolOutput::Structured { value } => redact_json(value, redactor),
        ToolOutput::Mixed { parts } => {
            for part in parts {
                match part {
                    ToolOutputPart::Text { text } => *text = redactor.redact(text),
                    ToolOutputPart::Structured { value } => redact_json(value, redactor),
                    ToolOutputPart::Image { .. } => {}
                }
            }
        }
    }
}

fn validate_mutation_scope(scope: &MutationScope) -> Result<(), AgentLoopError> {
    let MutationScope::Paths(paths) = scope else {
        return Ok(());
    };
    if paths.is_empty() {
        return Err(AgentLoopError::ToolContext(
            "mutation scope contained no paths".to_owned(),
        ));
    }
    for path in paths {
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(AgentLoopError::ToolContext(
                "mutation scope contained an unsafe path".to_owned(),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn execute_prepared_tool(
    prepared: PreparedToolCall,
    context: ToolContext,
    cancellation: CancellationToken,
    coordinator: Arc<OrderedOutputCoordinator>,
    checkpoints: Arc<dyn MutationCheckpointCoordinator>,
    turn: u64,
) -> (ToolExecution, bool) {
    let (call, tool, arguments, mutation_scope) = match prepared {
        PreparedToolCall::Execute {
            call,
            tool,
            arguments,
            mutation_scope,
            ..
        } => (call, tool, arguments, mutation_scope),
        PreparedToolCall::Complete(execution) => return (execution, false),
    };
    let checkpoint = if matches!(mutation_scope, MutationScope::None) {
        None
    } else {
        if let Err(error) = validate_mutation_scope(&mutation_scope) {
            return (
                failed_execution(call, format!("checkpoint scope rejected: {error}")),
                false,
            );
        }
        let Some(session_id) = context.session_id() else {
            return (
                failed_execution(call, "tool context is missing a session id"),
                false,
            );
        };
        let begin = checkpoints
            .begin(session_id, turn, &call.id, &mutation_scope)
            .await;
        match begin {
            Ok(checkpoint) => Some(checkpoint),
            Err(error) => {
                return (
                    failed_execution(call, format!("checkpoint failed before tool: {error}")),
                    false,
                );
            }
        }
    };
    let output_open = Arc::new(AtomicBool::new(true));
    let sink = Arc::new(OrderedOutputSink {
        index: call.index,
        id: call.id.clone(),
        coordinator,
        open: output_open.clone(),
        totals: Mutex::new((0, 0, false)),
    });
    let invocation_context = context.with_output(sink);
    let result = if cancellation.is_cancelled() {
        Err(ToolError::Cancelled)
    } else {
        let execution =
            AssertUnwindSafe(tool.execute(&invocation_context, arguments)).catch_unwind();
        tokio::pin!(execution);
        let outcome = tokio::select! {
            outcome = &mut execution => Some(outcome),
            () = cancellation.cancelled() => {
                tokio::time::timeout(TOOL_CANCELLATION_GRACE, &mut execution)
                    .await
                    .ok()
            }
        };
        match outcome {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(ToolError::Command(
                "tool implementation panicked".to_owned(),
            )),
            None => Err(ToolError::Cancelled),
        }
    };
    output_open.store(false, Ordering::Release);
    let checkpoint_outcome = match &result {
        Ok(_) => MutationCheckpointOutcome::Completed,
        Err(ToolError::Cancelled) => MutationCheckpointOutcome::Cancelled,
        Err(_) => MutationCheckpointOutcome::Failed,
    };
    let (mut output, mut is_error) = match result {
        Ok(result) => (tool_result_output(result), false),
        Err(error) => (
            ToolOutput::Text {
                text: error.to_string(),
            },
            true,
        ),
    };
    if let Some(checkpoint) = &checkpoint {
        let finished = checkpoints.finish(checkpoint, checkpoint_outcome).await;
        if let Err(error) = finished {
            output = ToolOutput::Text {
                text: format!("checkpoint finalization failed: {error}"),
            };
            is_error = true;
        }
    }
    (
        ToolExecution {
            call,
            output,
            is_error,
        },
        true,
    )
}

async fn apply_post_tool_hook(
    mut execution: ToolExecution,
    config: &SessionActorConfig,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> ToolExecution {
    let post_tool = match dispatch_hook(
        &config.hooks,
        HookEvent::PostTool,
        json!({
            "id": execution.call.id,
            "name": execution.call.name,
            "arguments": execution.call.arguments,
            "output": execution.output,
            "is_error": execution.is_error,
        }),
        cancellation,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            execution.output = ToolOutput::Text {
                text: error.to_string(),
            };
            execution.is_error = true;
            return execution;
        }
    };
    report_hook_failures(HookEvent::PostTool, post_tool.failures(), signals);
    if let Some(message) = hook_rejection(post_tool.status()) {
        execution.output = ToolOutput::Text { text: message };
        execution.is_error = true;
        return execution;
    }
    if let Some(output) = post_tool.payload().get("output") {
        match serde_json::from_value(output.clone()) {
            Ok(output) => execution.output = output,
            Err(error) => {
                execution.output = ToolOutput::Text {
                    text: format!("post_tool hook returned invalid output: {error}"),
                };
                execution.is_error = true;
            }
        }
    }
    if let Some(is_error) = post_tool.payload().get("is_error").and_then(Value::as_bool) {
        execution.is_error = is_error;
    }
    execution
}

#[allow(clippy::too_many_lines)]
async fn execute_tool_calls(
    turn: u64,
    calls: Vec<PendingToolCall>,
    config: &SessionActorConfig,
    context: &ToolContext,
    cancellation: &CancellationToken,
    approver: &dyn PermissionApprover,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> Vec<ToolExecution> {
    let mut prepared = Vec::with_capacity(calls.len());
    for call in calls {
        prepared.push(prepare_tool_call(turn, call, config, approver, cancellation, signals).await);
    }
    let may_run_in_parallel = prepared.iter().all(|call| match call {
        PreparedToolCall::Execute { read_only, .. } => *read_only,
        PreparedToolCall::Complete(_) => true,
    });
    let coordinator = Arc::new(OrderedOutputCoordinator::new(
        turn,
        signals.clone(),
        Arc::clone(&config.secret_redactor),
    ));
    let total = prepared.len();
    let mut ordered = Vec::with_capacity(total);
    if !may_run_in_parallel {
        for call in prepared {
            let (mut execution, ran) = if cancellation.is_cancelled() {
                match call {
                    PreparedToolCall::Execute { call, .. } => (
                        failed_execution(call, "tool execution cancelled before start"),
                        false,
                    ),
                    PreparedToolCall::Complete(execution) => (execution, false),
                }
            } else {
                execute_prepared_tool(
                    call,
                    context.clone(),
                    cancellation.clone(),
                    Arc::clone(&coordinator),
                    Arc::clone(&config.checkpoints),
                    turn,
                )
                .await
            };
            if ran && !cancellation.is_cancelled() {
                execution = apply_post_tool_hook(execution, config, cancellation, signals).await;
            }
            redact_tool_output(&mut execution.output, config.secret_redactor.as_ref());
            send_event(
                signals,
                PendingEvent::ToolCallFinished {
                    turn,
                    id: execution.call.id.clone(),
                    output: execution.output.clone(),
                    is_error: execution.is_error,
                    index: execution.call.index,
                },
            );
            coordinator.advance(ordered.len().saturating_add(1));
            ordered.push(execution);
        }
        return ordered;
    }

    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel();
    let mut completed = (0..total).map(|_| None).collect::<Vec<_>>();
    for (index, call) in prepared.into_iter().enumerate() {
        match call {
            PreparedToolCall::Complete(execution) => {
                completed[index] = Some((execution, false));
            }
            call @ PreparedToolCall::Execute { .. } => {
                let fallback = call.call().clone();
                let context = context.clone();
                let cancellation = cancellation.clone();
                let coordinator = Arc::clone(&coordinator);
                let checkpoints = Arc::clone(&config.checkpoints);
                let completed_tx = completed_tx.clone();
                let _task = tokio::spawn(async move {
                    let result = AssertUnwindSafe(execute_prepared_tool(
                        call,
                        context,
                        cancellation,
                        coordinator,
                        checkpoints,
                        turn,
                    ))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        (
                            failed_execution(fallback, "tool implementation panicked"),
                            true,
                        )
                    });
                    let _ = completed_tx.send((index, result));
                });
            }
        }
    }
    drop(completed_tx);
    let mut next = 0;
    while next < total {
        if completed[next].is_none() {
            let Some((index, execution)) = completed_rx.recv().await else {
                let call = PendingToolCall {
                    id: format!("missing-{next}"),
                    name: "unknown".to_owned(),
                    arguments: None,
                    index: next,
                };
                completed[next] = Some((
                    failed_execution(call, "tool task ended without a result"),
                    true,
                ));
                continue;
            };
            completed[index] = Some(execution);
            continue;
        }
        let Some((mut execution, ran)) = completed[next].take() else {
            continue;
        };
        if ran && !cancellation.is_cancelled() {
            execution = apply_post_tool_hook(execution, config, cancellation, signals).await;
        }
        redact_tool_output(&mut execution.output, config.secret_redactor.as_ref());
        send_event(
            signals,
            PendingEvent::ToolCallFinished {
                turn,
                id: execution.call.id.clone(),
                output: execution.output.clone(),
                is_error: execution.is_error,
                index: execution.call.index,
            },
        );
        ordered.push(execution);
        next = next.saturating_add(1);
        coordinator.advance(next);
    }
    ordered
}

async fn prune_before_provider_request(
    conversation: &[Turn],
    context_surgery: &[ContextSurgeryAction],
    pruned_tool_outputs: &mut BTreeMap<String, u64>,
    signals: &mpsc::UnboundedSender<TurnSignal>,
) -> Result<(), AgentLoopError> {
    let mut tool_names = BTreeMap::<String, String>::new();
    for conversation_turn in conversation {
        for block in &conversation_turn.blocks {
            if let Block::ToolCall { id, name, .. } = block {
                tool_names.insert(id.0.clone(), name.clone());
            }
        }
    }
    let mut records = Vec::new();
    let mut toon = ToonPromptEncoder::default();
    let prompt_conversation = conversation
        .iter()
        .map(|turn| prompt_turn(turn, pruned_tool_outputs, &mut toon))
        .collect::<Vec<_>>();
    for (turn_index, (conversation_turn, prompt_conversation_turn)) in
        conversation.iter().zip(&prompt_conversation).enumerate()
    {
        let context_id = ContextItemId(format!("conversation:{turn_index}"));
        let (pinned, evicted) = context_action_state(context_surgery, &context_id);
        if evicted {
            records.push(PruneRecord {
                item_id: context_id.0,
                transcript_index: records.len(),
                kind: PruneRecordKind::PrunedMarker,
                tokens: 0,
                pinned: false,
            });
            continue;
        }
        if conversation_turn.meta.summary {
            records.push(PruneRecord {
                item_id: context_id.0.clone(),
                transcript_index: records.len(),
                kind: PruneRecordKind::SummaryMarker,
                tokens: LocalTokenEstimator::turn(prompt_conversation_turn),
                pinned,
            });
            continue;
        }
        if conversation_turn.role == Role::User {
            records.push(PruneRecord {
                item_id: context_id.0.clone(),
                transcript_index: records.len(),
                kind: PruneRecordKind::User,
                tokens: LocalTokenEstimator::turn(prompt_conversation_turn),
                pinned,
            });
        }
        for (block, prompt_block) in conversation_turn
            .blocks
            .iter()
            .zip(&prompt_conversation_turn.blocks)
        {
            let Block::ToolResult { id, .. } = block else {
                continue;
            };
            let tokens = LocalTokenEstimator::turn(&Turn {
                role: Role::Tool,
                blocks: vec![prompt_block.clone()],
                meta: TurnMeta::default(),
            });
            let already_pruned = pruned_tool_outputs.contains_key(&id.0);
            records.push(PruneRecord {
                item_id: format!("{}:tool:{}", context_id.0, id.0),
                transcript_index: records.len(),
                kind: if already_pruned {
                    PruneRecordKind::PrunedMarker
                } else {
                    PruneRecordKind::ToolOutput {
                        tool_call_id: id.0.clone(),
                        tool_name: tool_names
                            .get(&id.0)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_owned()),
                        completed: true,
                    }
                },
                tokens,
                pinned,
            });
        }
    }
    let plan = Pruner::plan(&records, &PruneConfig::default());
    for decision in plan.decisions {
        persist_event(
            signals,
            PendingEvent::ToolOutputPruned {
                tool_call_id: decision.tool_call_id.clone(),
                reclaimed_tokens: decision.original_tokens,
            },
        )
        .await?;
        pruned_tool_outputs.insert(decision.tool_call_id, decision.original_tokens);
    }
    Ok(())
}

struct CompactionExecution {
    conversation: Vec<Turn>,
    usage: SessionUsage,
    cost: Cost,
    reclaimed_tokens: u64,
    remapped_pins: Vec<ContextItemId>,
    hard_stop: bool,
    failed_attempt_cost_micros: u64,
    failed_attempt_credit_micros: u64,
}

fn context_compaction_reason(reason: &CompactionReason) -> ContextCompactionReason {
    match reason {
        CompactionReason::Automatic => ContextCompactionReason::AutomaticOverflow,
        CompactionReason::Manual => ContextCompactionReason::Manual,
        CompactionReason::ProviderOverflow => ContextCompactionReason::ProviderOverflow,
    }
}

async fn persist_failed_compaction_attempt(
    config: &SessionActorConfig,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    alias: &str,
    selected_route: Option<&str>,
    reported_model: Option<&str>,
    usage: SessionUsage,
) -> Result<Option<(Cost, bool)>, AgentLoopError> {
    if usage == SessionUsage::default() {
        return Ok(None);
    }
    let cost = config
        .model
        .cost_for_route(alias, selected_route, reported_model, usage.into());
    persist_event(
        signals,
        PendingEvent::CompactionAttemptFinished {
            summary_turn: turn,
            usage,
            cost: cost.clone(),
        },
    )
    .await?;
    let now = config.event_clock.unix_time_millis();
    let ledger = config
        .event_sink
        .budget_totals(BudgetLedgerQuery {
            now_unix_ms: now,
            utc_day_start_unix_ms: now.saturating_sub(now % 86_400_000),
            trailing_minute_start_unix_ms: now.saturating_sub(60_000),
        })
        .await?;
    Ok(Some((cost, ledger.authoritative)))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn execute_compaction(
    conversation: &[Turn],
    surgery: &[ContextSurgeryAction],
    reason: CompactionReason,
    instructions: Option<String>,
    config: &SessionActorConfig,
    local_session_accounting: SessionAccountingFallback,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    turn: u64,
    current_turn_cost_micros: u64,
    current_turn_credit_micros: u64,
    enforce_budget_via_signals: bool,
) -> Result<CompactionExecution, AgentLoopError> {
    let hook_result = dispatch_hook(
        &config.hooks,
        HookEvent::PreCompact,
        json!({
            "reason": format!("{reason:?}"),
            "conversation_turns": conversation.len(),
        }),
        cancellation,
    )
    .await?;
    report_hook_failures(HookEvent::PreCompact, hook_result.failures(), signals);
    if !hook_result.completed() {
        return Err(AgentLoopError::Extension(
            "pre_compact hook blocked compaction".to_owned(),
        ));
    }
    let hook = PreCompactHook {
        injected_context: hook_result
            .payload()
            .get("injected_context")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        replacement_prompt: hook_result
            .payload()
            .get("replacement_prompt")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    let automatic_continue = !hook_result
        .payload()
        .get("suppress_auto_continue")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut latest = BTreeMap::<String, &ContextSurgeryAction>::new();
    for action in surgery {
        latest.insert(action.item_id.0.clone(), action);
    }
    let pins = latest
        .values()
        .filter(|action| action.pinned)
        .filter_map(|action| {
            let index = action
                .item_id
                .0
                .strip_prefix("conversation:")?
                .parse::<usize>()
                .ok()?;
            let pinned_turn = conversation.get(index)?.clone();
            Some(ConversationPin {
                item_id: action.item_id.0.clone(),
                order: action.effective_after_agent_turn,
                turn: pinned_turn,
            })
        })
        .collect();
    let compaction_config = config.model.compaction_config();
    let plan = Compactor::plan(CompactionInput {
        conversation: conversation.to_vec(),
        pins,
        reason: context_compaction_reason(&reason),
        instructions,
        hook,
        session_model_alias: config.model_alias.clone(),
        compaction_model_alias: compaction_config.model_alias,
        automatic_continue,
    })
    .map_err(|error| AgentLoopError::InvalidConfiguration(error.to_string()))?;
    let mut summary_request_turns = plan.history.clone();
    summary_request_turns.push(Turn {
        role: Role::User,
        blocks: vec![Block::Text {
            text: plan.summary_prompt.clone(),
        }],
        meta: TurnMeta {
            synthetic: true,
            ..TurnMeta::default()
        },
    });
    let aliases = if plan.model_alias == config.model_alias {
        vec![plan.model_alias.clone()]
    } else {
        vec![plan.model_alias.clone(), config.model_alias.clone()]
    };
    let mut last_error = None;
    let mut completed = None;
    let mut failed_attempt_cost_micros = 0_u64;
    let mut failed_attempt_credit_micros = 0_u64;
    for alias in aliases {
        if enforce_budget_via_signals {
            let budget = evaluate_budget(
                turn,
                config.event_clock.as_ref(),
                &config.event_sink,
                &config.model.budget_config(),
                local_session_accounting,
                current_turn_cost_micros.saturating_add(failed_attempt_cost_micros),
                current_turn_credit_micros.saturating_add(failed_attempt_credit_micros),
            )
            .await?;
            for event in budget.events {
                persist_event(signals, event).await?;
            }
            if budget.hard_stop {
                return Err(AgentLoopError::InvalidConfiguration(
                    "budget hard cap prevents compaction model call".to_owned(),
                ));
            }
        }
        let request = ProviderRequest {
            model: alias.clone(),
            turns: summary_request_turns.clone(),
            tools: Vec::new(),
            tool_choice: ToolChoice::None,
            max_output_tokens: config.max_output_tokens,
            temperature: None,
            thinking: config.thinking,
            cache_hint: None,
        };
        let mut stream = match config.model.stream(&alias, request) {
            Ok(stream) => stream,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let mut summary = String::new();
        let mut usage = SessionUsage::default();
        let mut reported_model = None;
        let mut selected_route = None;
        let mut failed = None;
        let mut cancelled = false;
        loop {
            let event = tokio::select! {
                () = cancellation.cancelled() => {
                    cancelled = true;
                    failed = Some(AgentLoopError::Provider("compaction cancelled".to_owned()));
                    break;
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Ok(ProviderEvent::RouteSelected { route }) => selected_route = Some(route),
                Ok(ProviderEvent::MessageStart { model }) => reported_model = Some(model),
                Ok(ProviderEvent::TextDelta { text }) => summary.push_str(&text),
                Ok(ProviderEvent::Usage { usage: latest }) => usage.update(latest),
                Ok(
                    ProviderEvent::ToolCallStart { .. }
                    | ProviderEvent::ToolCallArgumentsDelta { .. }
                    | ProviderEvent::ToolCallEnd { .. },
                ) => {
                    failed = Some(AgentLoopError::Provider(
                        "compaction model attempted a tool call".to_owned(),
                    ));
                    break;
                }
                Ok(
                    ProviderEvent::ThinkingDelta { .. }
                    | ProviderEvent::Citation { .. }
                    | ProviderEvent::Finished { .. },
                ) => {}
                Err(error) => {
                    failed = Some(AgentLoopError::Provider(error.to_string()));
                    break;
                }
            }
        }
        if let Some(error) = failed {
            if let Some((cost, false)) = persist_failed_compaction_attempt(
                config,
                signals,
                turn,
                &alias,
                selected_route.as_deref(),
                reported_model.as_deref(),
                usage,
            )
            .await?
            {
                if persist_incomplete_budget_caps(
                    signals,
                    turn,
                    &config.model.budget_config(),
                    &cost,
                    current_turn_cost_micros,
                    current_turn_credit_micros,
                )
                .await?
                {
                    return Err(AgentLoopError::InvalidConfiguration(
                        "budget cap cannot price a failed compaction attempt".to_owned(),
                    ));
                }
                let (cost_micros, credit_micros) = cost_units(&cost);
                failed_attempt_cost_micros = failed_attempt_cost_micros.saturating_add(cost_micros);
                failed_attempt_credit_micros =
                    failed_attempt_credit_micros.saturating_add(credit_micros);
            }
            if cancelled {
                return Err(error);
            }
            last_error = Some(error);
            continue;
        }
        if summary.trim().is_empty() {
            if let Some((cost, false)) = persist_failed_compaction_attempt(
                config,
                signals,
                turn,
                &alias,
                selected_route.as_deref(),
                reported_model.as_deref(),
                usage,
            )
            .await?
            {
                if persist_incomplete_budget_caps(
                    signals,
                    turn,
                    &config.model.budget_config(),
                    &cost,
                    current_turn_cost_micros,
                    current_turn_credit_micros,
                )
                .await?
                {
                    return Err(AgentLoopError::InvalidConfiguration(
                        "budget cap cannot price a failed compaction attempt".to_owned(),
                    ));
                }
                let (cost_micros, credit_micros) = cost_units(&cost);
                failed_attempt_cost_micros = failed_attempt_cost_micros.saturating_add(cost_micros);
                failed_attempt_credit_micros =
                    failed_attempt_credit_micros.saturating_add(credit_micros);
            }
            last_error = Some(AgentLoopError::Provider(
                "compaction model returned an empty summary".to_owned(),
            ));
            continue;
        }
        let cost = config.model.cost_for_route(
            &alias,
            selected_route.as_deref(),
            reported_model.as_deref(),
            usage.into(),
        );
        let (compaction_cost, compaction_credits) = cost_units(&cost);
        let hard_stop = if enforce_budget_via_signals {
            let post_budget = evaluate_budget(
                turn,
                config.event_clock.as_ref(),
                &config.event_sink,
                &config.model.budget_config(),
                local_session_accounting,
                current_turn_cost_micros
                    .saturating_add(failed_attempt_cost_micros)
                    .saturating_add(compaction_cost),
                current_turn_credit_micros
                    .saturating_add(failed_attempt_credit_micros)
                    .saturating_add(compaction_credits),
            )
            .await?;
            for event in post_budget.events {
                persist_event(signals, event).await?;
            }
            let incomplete = persist_incomplete_budget_caps(
                signals,
                turn,
                &config.model.budget_config(),
                &cost,
                current_turn_cost_micros.saturating_add(failed_attempt_cost_micros),
                current_turn_credit_micros.saturating_add(failed_attempt_credit_micros),
            )
            .await?;
            post_budget.hard_stop || incomplete
        } else {
            false
        };
        completed = Some((summary, usage, cost, hard_stop));
        break;
    }
    let Some((summary, usage, cost, hard_stop)) = completed else {
        return Err(last_error.unwrap_or_else(|| {
            AgentLoopError::Provider("compaction model was unavailable".to_owned())
        }));
    };
    let old_tokens = conversation.iter().fold(0_u64, |total, turn| {
        total.saturating_add(LocalTokenEstimator::turn(turn))
    });
    let compacted = plan.post_summary_turns(summary);
    let new_tokens = compacted.iter().fold(0_u64, |total, turn| {
        total.saturating_add(LocalTokenEstimator::turn(turn))
    });
    let remapped_pins = (0..plan.ordered_pins.len())
        .map(|index| ContextItemId(format!("conversation:{}", index.saturating_add(1))))
        .collect();
    Ok(CompactionExecution {
        conversation: compacted,
        usage,
        cost,
        reclaimed_tokens: old_tokens.saturating_sub(new_tokens),
        remapped_pins,
        hard_stop,
        failed_attempt_cost_micros,
        failed_attempt_credit_micros,
    })
}

#[allow(clippy::too_many_arguments)]
async fn compact_during_turn(
    turn: u64,
    conversation: &mut Vec<Turn>,
    surgery: &mut Vec<ContextSurgeryAction>,
    reason: CompactionReason,
    config: &SessionActorConfig,
    cancellation: &CancellationToken,
    signals: &mpsc::UnboundedSender<TurnSignal>,
    local_session_accounting: SessionAccountingFallback,
    current_turn_cost_micros: u64,
    current_turn_credit_micros: u64,
    instructions: Option<String>,
) -> Result<(u64, u64, bool), AgentLoopError> {
    persist_event(
        signals,
        PendingEvent::CompactionStarted {
            reason: reason.clone(),
        },
    )
    .await?;
    let execution = execute_compaction(
        conversation,
        surgery,
        reason,
        instructions,
        config,
        local_session_accounting,
        cancellation,
        signals,
        turn,
        current_turn_cost_micros,
        current_turn_credit_micros,
        true,
    )
    .await?;
    for compacted_turn in &execution.conversation {
        persist_conversation_turn(signals, turn, compacted_turn).await?;
    }
    surgery.clear();
    for item_id in &execution.remapped_pins {
        persist_event(
            signals,
            PendingEvent::ContextItemPinned {
                item_id: item_id.clone(),
                effective_after_agent_turn: turn,
            },
        )
        .await?;
        surgery.push(ContextSurgeryAction {
            item_id: item_id.clone(),
            pinned: true,
            effective_after_agent_turn: turn,
        });
    }
    let (successful_cost_micros, successful_credit_micros) = cost_units(&execution.cost);
    let cost_micros = successful_cost_micros.saturating_add(execution.failed_attempt_cost_micros);
    let credit_micros =
        successful_credit_micros.saturating_add(execution.failed_attempt_credit_micros);
    persist_event(
        signals,
        PendingEvent::CompactionFinished {
            summary_turn: turn,
            reclaimed_tokens: execution.reclaimed_tokens,
            usage: Some(execution.usage),
            cost: Some(execution.cost),
        },
    )
    .await?;
    let now = config.event_clock.unix_time_millis();
    let ledger = config
        .event_sink
        .budget_totals(BudgetLedgerQuery {
            now_unix_ms: now,
            utc_day_start_unix_ms: now.saturating_sub(now % 86_400_000),
            trailing_minute_start_unix_ms: now.saturating_sub(60_000),
        })
        .await?;
    *conversation = execution.conversation;
    Ok((
        if ledger.authoritative { 0 } else { cost_micros },
        if ledger.authoritative {
            0
        } else {
            credit_micros
        },
        execution.hard_stop,
    ))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_turn(
    turn: u64,
    messages: Vec<PreparedUserMessage>,
    mut conversation: Vec<Turn>,
    config: Arc<SessionActorConfig>,
    tool_context: ToolContext,
    cancellation: CancellationToken,
    signals: mpsc::UnboundedSender<TurnSignal>,
    mut context_surgery: Vec<ContextSurgeryAction>,
    mut pruned_tool_outputs: BTreeMap<String, u64>,
    mut budgeter: Budgeter,
    local_session_accounting: SessionAccountingFallback,
) -> TurnOutcome {
    for message in messages {
        let Ok(hook) = dispatch_hook(
            &config.hooks,
            HookEvent::UserPromptSubmit,
            json!({ "content": message.content }),
            &cancellation,
        )
        .await
        else {
            return TurnOutcome {
                turn,
                conversation,
                status: AgentTurnStatus::Interrupted,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
                deferred_terminal_delta: None,
                deferred_terminal_turn: None,
                context_surgery,
                pruned_tool_outputs,
                budgeter,
            };
        };
        report_hook_failures(HookEvent::UserPromptSubmit, hook.failures(), &signals);
        if !hook.completed() {
            return TurnOutcome {
                turn,
                conversation,
                status: AgentTurnStatus::Failed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
                deferred_terminal_delta: None,
                deferred_terminal_turn: None,
                context_surgery,
                pruned_tool_outputs,
                budgeter,
            };
        }
        let content = hook
            .payload()
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let user_turn = message.turn(content);
        conversation.push(user_turn.clone());
        if persist_event(
            &signals,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: turn,
                turn: user_turn,
            },
        )
        .await
        .is_err()
        {
            return TurnOutcome {
                turn,
                conversation,
                status: AgentTurnStatus::Failed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
                deferred_terminal_delta: None,
                deferred_terminal_turn: None,
                context_surgery,
                pruned_tool_outputs,
                budgeter,
            };
        }
    }

    let approver = ChannelApprover {
        signals: signals.clone(),
        cancellation: cancellation.clone(),
    };
    let mut usage = SessionUsage::default();
    let mut doom = DoomLoopGuard::new(config.identical_tool_failure_limit);
    let mut status = AgentTurnStatus::MaxTurns;
    let mut deferred_terminal_delta = None;
    let mut deferred_terminal_turn = None;
    let mut current_turn_cost_micros = 0_u64;
    let mut current_turn_credit_micros = 0_u64;
    let budget_config = config.model.budget_config();
    let mut turn_cost = None;

    'iterations: for _ in 0..config.max_turns {
        if cancellation.is_cancelled() {
            status = AgentTurnStatus::Interrupted;
            break;
        }
        let budget = match evaluate_budget(
            turn,
            config.event_clock.as_ref(),
            &config.event_sink,
            &budget_config,
            local_session_accounting,
            current_turn_cost_micros,
            current_turn_credit_micros,
        )
        .await
        {
            Ok(check) => check,
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
        };
        for event in budget.events {
            if persist_event(&signals, event).await.is_err() {
                status = AgentTurnStatus::Failed;
                break 'iterations;
            }
        }
        if budget.hard_stop {
            status = AgentTurnStatus::BudgetExceeded;
            break;
        }
        if prune_before_provider_request(
            &conversation,
            &context_surgery,
            &mut pruned_tool_outputs,
            &signals,
        )
        .await
        .is_err()
        {
            status = AgentTurnStatus::Failed;
            break;
        }
        let mut assembled = match assemble_session_context(
            &config,
            &conversation,
            &VecDeque::new(),
            &context_surgery,
            &pruned_tool_outputs,
            false,
        ) {
            Ok(assembled) => assembled,
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
        };
        let metadata = config.model.context_metadata(&config.model_alias);
        let compaction = config.model.compaction_config();
        let mut input_estimate = budgeter.estimate(&assembled.turns, &assembled.tools);
        if let Some(context_window_tokens) = metadata.max_context_tokens {
            let overflow = OverflowPolicy {
                context_window_tokens,
                max_output_tokens: metadata.max_output_tokens.unwrap_or(0),
                reserved_tokens_override: compaction.reserved_tokens,
                automatic_compaction: compaction.auto,
            }
            .calculate(input_estimate.reconciled_tokens);
            if overflow.should_compact {
                match compact_during_turn(
                    turn,
                    &mut conversation,
                    &mut context_surgery,
                    CompactionReason::Automatic,
                    &config,
                    &cancellation,
                    &signals,
                    local_session_accounting,
                    current_turn_cost_micros,
                    current_turn_credit_micros,
                    None,
                )
                .await
                {
                    Ok((cost_micros, credit_micros, hard_stop)) => {
                        current_turn_cost_micros =
                            current_turn_cost_micros.saturating_add(cost_micros);
                        current_turn_credit_micros =
                            current_turn_credit_micros.saturating_add(credit_micros);
                        if hard_stop {
                            status = AgentTurnStatus::BudgetExceeded;
                            break;
                        }
                    }
                    Err(error) => {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: error.to_string(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        break;
                    }
                }
                assembled = match assemble_session_context(
                    &config,
                    &conversation,
                    &VecDeque::new(),
                    &context_surgery,
                    &pruned_tool_outputs,
                    false,
                ) {
                    Ok(assembled) => assembled,
                    Err(error) => {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: error.to_string(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        break;
                    }
                };
                input_estimate = budgeter.estimate(&assembled.turns, &assembled.tools);
            }
        }
        let mut snapshot = context_snapshot(
            &assembled,
            &conversation,
            &pruned_tool_outputs,
            metadata,
            &compaction,
            Some(wire_turn_id(turn)),
        );
        snapshot.used_tokens = input_estimate.reconciled_tokens;
        let context_metrics = (
            snapshot.used_tokens,
            snapshot.usable_tokens,
            snapshot.reserved_tokens,
            snapshot.stable_prefix_hash.clone(),
        );
        send_event(
            &signals,
            PendingEvent::ContextUsage {
                turn,
                used_tokens: snapshot.used_tokens,
                usable_tokens: snapshot.usable_tokens,
                reserved_tokens: snapshot.reserved_tokens,
                stable_prefix_hash: snapshot.stable_prefix_hash,
                cache_hit_basis_points: 0,
                estimated_input_tokens: input_estimate.local_tokens,
                provider_input_tokens: 0,
                correction_millionths: input_estimate.correction_millionths,
            },
        );
        let cache_hint = (assembled.stable_prefix_turn_count > 0 || !assembled.tools.is_empty())
            .then(|| CacheHint {
                stable_prefix_turns: u32::try_from(assembled.stable_prefix_turn_count)
                    .unwrap_or(u32::MAX),
                tools_in_prefix: !assembled.tools.is_empty(),
            });
        let request = ProviderRequest {
            model: config.model_alias.clone(),
            turns: assembled.turns,
            tools: assembled.tools,
            tool_choice: ToolChoice::Auto,
            max_output_tokens: config.max_output_tokens,
            temperature: None,
            thinking: config.thinking,
            cache_hint,
        };
        let mut stream = match config.model.stream(&config.model_alias, request) {
            Ok(stream) => stream,
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
        };
        let mut assistant = Turn {
            role: Role::Assistant,
            blocks: Vec::new(),
            meta: TurnMeta::default(),
        };
        let mut selected_route = None;
        let mut calls = Vec::<PendingToolCall>::new();
        let mut finish_reason = None;
        let mut iteration_usage = SessionUsage::default();
        let mut stream_failed = false;
        let mut provider_overflow_recovered = false;
        let mut pending_text_delta = None;
        loop {
            let next = if pending_text_delta.is_some() {
                tokio::select! {
                    () = cancellation.cancelled() => {
                        flush_pending_text_delta(&mut pending_text_delta, &signals, turn);
                        status = AgentTurnStatus::Interrupted;
                        break;
                    }
                    event = tokio::time::timeout(TEXT_DELTA_COALESCE_WINDOW, stream.next()) => {
                        if let Ok(event) = event {
                            event
                        } else {
                            flush_pending_text_delta(
                                &mut pending_text_delta,
                                &signals,
                                turn,
                            );
                            continue;
                        }
                    }
                }
            } else {
                tokio::select! {
                    () = cancellation.cancelled() => {
                        status = AgentTurnStatus::Interrupted;
                        break;
                    }
                    event = stream.next() => event,
                }
            };
            let Some(event) = next else {
                flush_pending_text_delta(&mut pending_text_delta, &signals, turn);
                break;
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    flush_pending_text_delta(&mut pending_text_delta, &signals, turn);
                    if error.kind == rw_providers::ProviderErrorKind::ContextOverflow
                        && assistant.blocks.is_empty()
                        && calls.is_empty()
                    {
                        match compact_during_turn(
                            turn,
                            &mut conversation,
                            &mut context_surgery,
                            CompactionReason::ProviderOverflow,
                            &config,
                            &cancellation,
                            &signals,
                            local_session_accounting,
                            current_turn_cost_micros,
                            current_turn_credit_micros,
                            None,
                        )
                        .await
                        {
                            Ok((cost_micros, credit_micros, hard_stop)) => {
                                current_turn_cost_micros =
                                    current_turn_cost_micros.saturating_add(cost_micros);
                                current_turn_credit_micros =
                                    current_turn_credit_micros.saturating_add(credit_micros);
                                if hard_stop {
                                    status = AgentTurnStatus::BudgetExceeded;
                                    stream_failed = true;
                                    break;
                                }
                                provider_overflow_recovered = true;
                                break;
                            }
                            Err(compaction_error) => {
                                send_event(
                                    &signals,
                                    PendingEvent::Error {
                                        message: compaction_error.to_string(),
                                    },
                                );
                                status = AgentTurnStatus::Failed;
                                stream_failed = true;
                                break;
                            }
                        }
                    }
                    send_event(
                        &signals,
                        PendingEvent::Error {
                            message: error.to_string(),
                        },
                    );
                    status = if error.kind == rw_providers::ProviderErrorKind::Cancelled {
                        AgentTurnStatus::Interrupted
                    } else {
                        AgentTurnStatus::Failed
                    };
                    stream_failed = true;
                    break;
                }
            };
            if !matches!(
                &event,
                ProviderEvent::TextDelta { .. } | ProviderEvent::Finished { .. }
            ) {
                flush_pending_text_delta(&mut pending_text_delta, &signals, turn);
            }
            match event {
                ProviderEvent::RouteSelected { route } => selected_route = Some(route),
                ProviderEvent::MessageStart { model } => assistant.meta.model = Some(model),
                ProviderEvent::TextDelta { text } => {
                    flush_pending_text_delta(&mut pending_text_delta, &signals, turn);
                    append_text(&mut assistant.blocks, &text);
                    pending_text_delta = Some(text);
                }
                ProviderEvent::ThinkingDelta { content, signature } => {
                    assistant.blocks.push(Block::Thinking {
                        content: content.clone(),
                        signature: signature.clone(),
                    });
                    send_event(
                        &signals,
                        PendingEvent::ThinkingDelta {
                            turn,
                            content,
                            signature,
                        },
                    );
                }
                ProviderEvent::ToolCallStart { id, name } => {
                    if calls.iter().any(|call| call.id == id) {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: format!("provider repeated tool call id `{id}`"),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        break;
                    }
                    calls.push(PendingToolCall {
                        id,
                        name,
                        arguments: None,
                        index: calls.len(),
                    });
                }
                ProviderEvent::ToolCallArgumentsDelta { .. } => {}
                ProviderEvent::ToolCallEnd { id, arguments } => {
                    if let Some(call) = calls.iter_mut().find(|call| call.id == id) {
                        if call.arguments.is_some() {
                            send_event(
                                &signals,
                                PendingEvent::Error {
                                    message: format!("provider ended tool call `{id}` twice"),
                                },
                            );
                            status = AgentTurnStatus::Failed;
                            stream_failed = true;
                            break;
                        }
                        call.arguments = Some(arguments.clone());
                        assistant.blocks.push(Block::ToolCall {
                            id: ToolCallId(id),
                            name: call.name.clone(),
                            args: arguments,
                        });
                    } else {
                        send_event(
                            &signals,
                            PendingEvent::Error {
                                message: "provider ended an unknown tool call".to_owned(),
                            },
                        );
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        break;
                    }
                }
                ProviderEvent::Citation { uri, title, .. } => {
                    assistant.blocks.push(Block::Citation {
                        uri: uri.clone(),
                        title: title.clone(),
                        excerpt: None,
                    });
                    send_event(&signals, PendingEvent::CitationDelta { turn, uri, title });
                }
                ProviderEvent::Usage { usage: latest } => iteration_usage.update(latest),
                ProviderEvent::Finished { reason } => {
                    if reason == FinishReason::ToolCalls || !calls.is_empty() {
                        flush_pending_text_delta(&mut pending_text_delta, &signals, turn);
                    }
                    finish_reason = Some(reason);
                    break;
                }
            }
        }
        let normalized_iteration_usage: TokenUsage = iteration_usage.into();
        let reconciliation =
            budgeter.reconcile(input_estimate.local_tokens, normalized_iteration_usage);
        let provider_input_tokens = normalized_iteration_usage
            .input_tokens
            .saturating_add(normalized_iteration_usage.cache_read_tokens)
            .saturating_add(normalized_iteration_usage.cache_write_tokens);
        let cache_hit_basis_points = if provider_input_tokens == 0 {
            0
        } else {
            u16::try_from(
                u128::from(normalized_iteration_usage.cache_read_tokens).saturating_mul(10_000)
                    / u128::from(provider_input_tokens),
            )
            .unwrap_or(10_000)
        };
        send_event(
            &signals,
            PendingEvent::ContextUsage {
                turn,
                used_tokens: context_metrics.0,
                usable_tokens: context_metrics.1,
                reserved_tokens: context_metrics.2,
                stable_prefix_hash: context_metrics.3.clone(),
                cache_hit_basis_points,
                estimated_input_tokens: input_estimate.local_tokens,
                provider_input_tokens,
                correction_millionths: reconciliation.correction_millionths,
            },
        );
        usage.add(iteration_usage);
        let iteration_cost = config.model.cost_for_route(
            &config.model_alias,
            selected_route.as_deref(),
            assistant.meta.model.as_deref(),
            normalized_iteration_usage,
        );
        turn_cost = Some(combine_cost(turn_cost.take(), iteration_cost.clone()));
        let (cost_micros, credit_micros) = cost_units(&iteration_cost);
        current_turn_cost_micros = current_turn_cost_micros.saturating_add(cost_micros);
        current_turn_credit_micros = current_turn_credit_micros.saturating_add(credit_micros);
        let mut budget_stop = false;
        match evaluate_budget(
            turn,
            config.event_clock.as_ref(),
            &config.event_sink,
            &budget_config,
            local_session_accounting,
            current_turn_cost_micros,
            current_turn_credit_micros,
        )
        .await
        {
            Ok(check) => {
                for event in check.events {
                    if persist_event(&signals, event).await.is_err() {
                        status = AgentTurnStatus::Failed;
                        stream_failed = true;
                        break;
                    }
                }
                budget_stop = check.hard_stop;
                if budget_stop {
                    status = AgentTurnStatus::BudgetExceeded;
                }
            }
            Err(error) => {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: error.to_string(),
                    },
                );
                status = AgentTurnStatus::Failed;
                stream_failed = true;
            }
        }
        match persist_incomplete_budget_caps(
            &signals,
            turn,
            &budget_config,
            &iteration_cost,
            current_turn_cost_micros,
            current_turn_credit_micros,
        )
        .await
        {
            Err(_) => {
                stream_failed = true;
                status = AgentTurnStatus::Failed;
            }
            Ok(true) => {
                budget_stop = true;
                status = AgentTurnStatus::BudgetExceeded;
            }
            Ok(false) => {}
        }
        if budget_stop {
            flush_pending_text_delta(&mut pending_text_delta, &signals, turn);
        }
        if provider_overflow_recovered {
            continue 'iterations;
        }
        let assistant_turn = if assistant.blocks.is_empty() {
            None
        } else {
            conversation.push(assistant.clone());
            Some(assistant)
        };
        if stream_failed || status == AgentTurnStatus::Interrupted || budget_stop {
            if let Some(assistant) = &assistant_turn
                && persist_conversation_turn(&signals, turn, assistant)
                    .await
                    .is_err()
            {
                status = AgentTurnStatus::Failed;
            }
            break;
        }
        let Some(reason) = finish_reason else {
            if let Some(assistant) = &assistant_turn
                && persist_conversation_turn(&signals, turn, assistant)
                    .await
                    .is_err()
            {
                status = AgentTurnStatus::Failed;
                break;
            }
            if status != AgentTurnStatus::Interrupted {
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: "provider stream ended without a finish reason".to_owned(),
                    },
                );
                status = AgentTurnStatus::Failed;
            }
            break;
        };
        if reason != FinishReason::ToolCalls {
            if !calls.is_empty() {
                if let Some(assistant) = &assistant_turn {
                    let _ = persist_conversation_turn(&signals, turn, assistant).await;
                }
                send_event(
                    &signals,
                    PendingEvent::Error {
                        message: "provider emitted tool calls with a non-tool finish reason"
                            .to_owned(),
                    },
                );
                status = AgentTurnStatus::Failed;
                break;
            }
            status = AgentTurnStatus::Completed;
            deferred_terminal_delta = pending_text_delta.take();
            deferred_terminal_turn = assistant_turn;
            break;
        }
        if let Some(assistant) = &assistant_turn
            && persist_conversation_turn(&signals, turn, assistant)
                .await
                .is_err()
        {
            status = AgentTurnStatus::Failed;
            break;
        }
        if calls.is_empty() || calls.iter().any(|call| call.arguments.is_none()) {
            send_event(
                &signals,
                PendingEvent::Error {
                    message: "provider reported incomplete tool calls".to_owned(),
                },
            );
            status = AgentTurnStatus::Failed;
            break;
        }
        let executions = execute_tool_calls(
            turn,
            calls,
            &config,
            &tool_context,
            &cancellation,
            &approver,
            &signals,
        )
        .await;
        let interrupted = cancellation.is_cancelled();
        let mut tool_blocks = Vec::new();
        let mut doom_triggered = false;
        for execution in executions {
            tool_blocks.push(Block::ToolResult {
                id: ToolCallId(execution.call.id.clone()),
                output: execution.output.clone(),
                is_error: execution.is_error,
            });
            doom_triggered |= !interrupted && doom.observe(&execution.call, &execution);
        }
        let tool_turn = Turn {
            role: Role::Tool,
            blocks: tool_blocks,
            meta: TurnMeta::default(),
        };
        conversation.push(tool_turn.clone());
        if persist_event(
            &signals,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: turn,
                turn: tool_turn,
            },
        )
        .await
        .is_err()
        {
            status = AgentTurnStatus::Failed;
            break;
        }
        if doom_triggered {
            send_event(
                &signals,
                PendingEvent::GuardTriggered {
                    turn,
                    guard: "identical_tool_failure".to_owned(),
                    message: "identical failing tool invocation repeated too many times".to_owned(),
                },
            );
            status = AgentTurnStatus::DoomLoop;
            break 'iterations;
        }
        if interrupted {
            status = AgentTurnStatus::Interrupted;
            break;
        }
    }

    if status == AgentTurnStatus::MaxTurns {
        send_event(
            &signals,
            PendingEvent::GuardTriggered {
                turn,
                guard: "max_turns".to_owned(),
                message: format!(
                    "maximum of {} provider iterations reached",
                    config.max_turns
                ),
            },
        );
    }

    let hook = dispatch_hook(
        &config.hooks,
        HookEvent::TurnEnd,
        json!({ "turn": turn, "status": format!("{status:?}") }),
        &cancellation,
    )
    .await;
    match hook {
        Ok(hook) => {
            report_hook_failures(HookEvent::TurnEnd, hook.failures(), &signals);
            if !hook.completed() && status == AgentTurnStatus::Completed {
                status = AgentTurnStatus::Failed;
            }
        }
        Err(_) if status == AgentTurnStatus::Completed => {
            status = AgentTurnStatus::Interrupted;
        }
        Err(_) => {}
    }
    if status != AgentTurnStatus::Completed
        && let Some(assistant) = deferred_terminal_turn.take()
    {
        if let Some(text) = deferred_terminal_delta.take() {
            let _ = persist_event(&signals, PendingEvent::TextDelta { turn, text }).await;
        }
        let _ = persist_conversation_turn(&signals, turn, &assistant).await;
    }
    let cost = turn_cost.unwrap_or_else(unavailable_cost);
    TurnOutcome {
        turn,
        conversation,
        status,
        usage,
        cost,
        deferred_terminal_delta,
        deferred_terminal_turn,
        context_surgery,
        pruned_tool_outputs,
        budgeter,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::{
        path::Path,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Duration,
    };

    use futures_util::stream;
    use rw_ext::{HookError, HookRegistration};
    use rw_providers::{
        Capabilities, FixtureRedactor, Provider, ProviderError, ProviderErrorKind, ProviderRouter,
        Recorder, ReplayProvider, RetryPolicy,
    };
    use rw_tools::{AskUserTool, CapabilityManifest, Tool, ToolLimits};
    use rw_types::{ToolCapability, ToolOutputStream, config::PermissionDecision};
    use tempfile::TempDir;
    use tokio::{sync::Notify, time::timeout};

    use super::*;

    type ProviderScript = Vec<Result<ProviderEvent, ProviderError>>;

    #[derive(Default)]
    struct ScriptedModel {
        scripts: Mutex<VecDeque<ProviderScript>>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    struct AliasVisionModel;

    impl ModelDriver for AliasVisionModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            Err(AgentLoopError::Provider(
                "alias fixture does not make provider calls".to_owned(),
            ))
        }

        fn has_model_alias(&self, alias: &str) -> bool {
            matches!(alias, "fast" | "slow")
        }

        fn supports_vision(&self, alias: &str) -> bool {
            alias == "slow"
        }
    }

    impl ScriptedModel {
        fn new(scripts: impl IntoIterator<Item = ProviderScript>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().expect("request lock").len()
        }
    }

    impl ModelDriver for ScriptedModel {
        fn stream(
            &self,
            _alias: &str,
            request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            self.requests.lock().expect("request lock").push(request);
            let events = self
                .scripts
                .lock()
                .expect("script lock")
                .pop_front()
                .ok_or_else(|| AgentLoopError::Provider("missing fixture script".to_owned()))?;
            Ok(Box::pin(stream::iter(events)))
        }
    }

    struct M3Model {
        scripts: Mutex<VecDeque<ProviderScript>>,
        requests: Mutex<Vec<ProviderRequest>>,
        metadata: ModelContextMetadata,
        compaction: CompactionConfig,
        budget: BudgetConfig,
        cost_override: Option<Cost>,
    }

    impl M3Model {
        fn new(scripts: impl IntoIterator<Item = ProviderScript>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
                metadata: ModelContextMetadata::default(),
                compaction: CompactionConfig::default(),
                budget: BudgetConfig::default(),
                cost_override: None,
            }
        }

        fn requests(&self) -> Vec<ProviderRequest> {
            self.requests.lock().expect("request lock").clone()
        }
    }

    impl ModelDriver for M3Model {
        fn stream(
            &self,
            _alias: &str,
            request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            self.requests.lock().expect("request lock").push(request);
            let script = self
                .scripts
                .lock()
                .expect("script lock")
                .pop_front()
                .ok_or_else(|| AgentLoopError::Provider("missing M3 script".to_owned()))?;
            Ok(Box::pin(stream::iter(script)))
        }

        fn context_metadata(&self, _alias: &str) -> ModelContextMetadata {
            self.metadata
        }

        fn compaction_config(&self) -> CompactionConfig {
            self.compaction.clone()
        }

        fn budget_config(&self) -> BudgetConfig {
            self.budget.clone()
        }

        fn cost(&self, _alias: &str, usage: TokenUsage) -> Cost {
            if let Some(cost) = &self.cost_override {
                return cost.clone();
            }
            Cost::Monetary {
                amount_micros: usage.output_tokens,
                currency: "USD".to_owned(),
            }
        }
    }

    struct ReplaySourceProvider {
        scripts: Mutex<VecDeque<ProviderScript>>,
    }

    #[async_trait]
    impl Provider for ReplaySourceProvider {
        fn name(&self) -> &'static str {
            "context-replay"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                tool_calling: false,
                vision: false,
                thinking: false,
                cache_breakpoints: CacheBreakpointSupport::Explicit,
                max_context_tokens: Some(2_000),
                max_output_tokens: Some(256),
                wire_mode: rw_providers::WireMode::NormalizedReplay,
            }
        }

        async fn stream(&self, _request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
            let script = self
                .scripts
                .lock()
                .expect("replay source scripts")
                .pop_front()
                .ok_or_else(|| {
                    ProviderError::new(ProviderErrorKind::ReplayMiss, "missing source script")
                })?;
            Ok(Box::pin(stream::iter(script)))
        }
    }

    struct ReplayHarnessModel {
        router: ProviderRouter,
    }

    impl ReplayHarnessModel {
        fn new(provider: Arc<dyn Provider>) -> Self {
            let router = ProviderRouter::new(
                BTreeMap::from([("fast".to_owned(), vec!["context-replay/model".to_owned()])]),
                [provider],
                RetryPolicy {
                    max_attempts: 1,
                    base_delay: Duration::ZERO,
                    max_delay: Duration::ZERO,
                    jitter_fraction: 0.0,
                },
            )
            .expect("replay router");
            Self { router }
        }
    }

    impl ModelDriver for ReplayHarnessModel {
        fn stream(
            &self,
            alias: &str,
            request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            self.router
                .stream_alias(alias, request)
                .map_err(|error| AgentLoopError::Provider(error.to_string()))
        }

        fn context_metadata(&self, _alias: &str) -> ModelContextMetadata {
            ModelContextMetadata {
                max_context_tokens: Some(2_000),
                max_output_tokens: Some(256),
                cache_breakpoints: Some(CacheBreakpointSupport::Explicit),
            }
        }

        fn budget_config(&self) -> BudgetConfig {
            BudgetConfig {
                session_cost_cap_micros_usd: Some(100),
                ..BudgetConfig::default()
            }
        }

        fn cost(&self, _alias: &str, usage: TokenUsage) -> Cost {
            Cost::Monetary {
                amount_micros: usage.output_tokens,
                currency: "USD".to_owned(),
            }
        }
    }

    struct RoutedCostModel {
        route: &'static str,
        requests: AtomicUsize,
        budget: BudgetConfig,
    }

    impl RoutedCostModel {
        fn new(route: &'static str) -> Self {
            Self {
                route,
                requests: AtomicUsize::new(0),
                budget: BudgetConfig {
                    session_cost_cap_micros_usd: Some(50),
                    ..BudgetConfig::default()
                },
            }
        }
    }

    impl ModelDriver for RoutedCostModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(stream::iter([
                Ok(ProviderEvent::RouteSelected {
                    route: self.route.to_owned(),
                }),
                Ok(ProviderEvent::MessageStart {
                    model: "shared-model-id".to_owned(),
                }),
                Ok(ProviderEvent::TextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Usage {
                    usage: TokenUsage {
                        output_tokens: 1,
                        ..TokenUsage::default()
                    },
                }),
                Ok(ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                }),
            ])))
        }

        fn budget_config(&self) -> BudgetConfig {
            self.budget.clone()
        }

        fn cost_for_route(
            &self,
            _alias: &str,
            route: Option<&str>,
            _reported_model: Option<&str>,
            _usage: TokenUsage,
        ) -> Cost {
            let amount_micros = match route {
                Some("__model_cheap") => 10,
                Some("__model_expensive") => 100,
                _ => {
                    return Cost::Unavailable {
                        reason: "unknown route".to_owned(),
                    };
                }
            };
            Cost::Monetary {
                amount_micros,
                currency: "USD".to_owned(),
            }
        }
    }

    struct DelayedSummaryModel;

    impl ModelDriver for DelayedSummaryModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            Ok(Box::pin(
                stream::iter([
                    Ok(ProviderEvent::MessageStart {
                        model: "fixture-model".to_owned(),
                    }),
                    Ok(ProviderEvent::Usage {
                        usage: TokenUsage {
                            input_tokens: 11,
                            output_tokens: 7,
                            ..TokenUsage::default()
                        },
                    }),
                ])
                .chain(stream::once(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(ProviderEvent::TextDelta {
                        text: "summary".to_owned(),
                    })
                })),
            ))
        }
    }

    struct PendingModel;

    impl ModelDriver for PendingModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            Ok(Box::pin(
                stream::iter([Ok(ProviderEvent::MessageStart {
                    model: "fixture-model".to_owned(),
                })])
                .chain(stream::pending::<Result<ProviderEvent, ProviderError>>()),
            ))
        }
    }

    struct GatedCompactionModel {
        calls: AtomicUsize,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl ModelDriver for GatedCompactionModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let started = Arc::clone(&self.started);
                let release = Arc::clone(&self.release);
                return Ok(Box::pin(
                    stream::once(async move {
                        started.notify_one();
                        release.notified().await;
                        Ok(ProviderEvent::TextDelta {
                            text: "## Goal\ncontinue\n\n## Instructions\n\n## Discoveries\n\n## Accomplished\n\n## Relevant files & directories\n".to_owned(),
                        })
                    })
                    .chain(stream::iter([Ok(ProviderEvent::Finished {
                        reason: FinishReason::Stop,
                    })])),
                ));
            }
            Ok(Box::pin(stream::iter(stop_script("queued answer", &[]))))
        }
    }

    struct DelayedFinishModel {
        delay: Duration,
    }

    impl ModelDriver for DelayedFinishModel {
        fn stream(
            &self,
            _alias: &str,
            _request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            let delay = self.delay;
            Ok(Box::pin(
                stream::iter([
                    Ok(ProviderEvent::MessageStart {
                        model: "fixture-model".to_owned(),
                    }),
                    Ok(ProviderEvent::TextDelta {
                        text: "visible promptly".to_owned(),
                    }),
                ])
                .chain(stream::once(async move {
                    tokio::time::sleep(delay).await;
                    Ok(ProviderEvent::Finished {
                        reason: FinishReason::Stop,
                    })
                })),
            ))
        }
    }

    #[derive(Default)]
    struct InstructionModel {
        observed: AtomicBool,
    }

    impl ModelDriver for InstructionModel {
        fn stream(
            &self,
            _alias: &str,
            request: ProviderRequest,
        ) -> Result<BoxEventStream, AgentLoopError> {
            let steered = request.turns.iter().any(|turn| {
                turn.role == Role::System
                    && turn.blocks.iter().any(|block| {
                        matches!(block, Block::Text { text } if text.contains("reply kennel"))
                    })
            });
            self.observed.store(steered, Ordering::SeqCst);
            if !steered {
                return Err(AgentLoopError::Provider(
                    "fixture root instruction was absent".to_owned(),
                ));
            }
            Ok(Box::pin(stream::iter(stop_script("kennel", &[]))))
        }
    }

    #[derive(Clone)]
    enum StubOutcome {
        Success(ToolResult),
        Failure(String),
    }

    struct StubTool {
        descriptor: ToolDescriptor,
        outcome: StubOutcome,
        calls: AtomicUsize,
        inputs: Mutex<Vec<Value>>,
    }

    impl StubTool {
        fn new(name: &str, capabilities: Vec<ToolCapability>, outcome: StubOutcome) -> Self {
            Self {
                descriptor: ToolDescriptor {
                    name: name.to_owned(),
                    description: format!("fixture {name}"),
                    input_schema: json!({"type": "object"}),
                    capabilities: CapabilityManifest::new(capabilities),
                },
                outcome,
                calls: AtomicUsize::new(0),
                inputs: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Tool for StubTool {
        fn descriptor(&self) -> ToolDescriptor {
            self.descriptor.clone()
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            input: Value,
        ) -> Result<ToolResult, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inputs.lock().expect("input lock").push(input);
            match &self.outcome {
                StubOutcome::Success(result) => Ok(result.clone()),
                StubOutcome::Failure(message) => Err(ToolError::InvalidInput(message.clone())),
            }
        }
    }

    struct ReverseCompletionTool {
        descriptor: ToolDescriptor,
        first: bool,
        release_first: Arc<Notify>,
        completion_order: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Tool for ReverseCompletionTool {
        fn descriptor(&self) -> ToolDescriptor {
            self.descriptor.clone()
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            if self.first {
                self.release_first.notified().await;
                self.completion_order
                    .lock()
                    .expect("completion lock")
                    .push(self.descriptor.name.clone());
            } else {
                self.completion_order
                    .lock()
                    .expect("completion lock")
                    .push(self.descriptor.name.clone());
                self.release_first.notify_one();
            }
            Ok(ToolResult::new(&self.descriptor.name, Value::Null))
        }
    }

    struct StreamingTool {
        descriptor: ToolDescriptor,
        release: Arc<Notify>,
        completed: Arc<AtomicBool>,
    }

    struct EmptySequentialTool {
        descriptor: ToolDescriptor,
        first: bool,
        first_started: Arc<Notify>,
        release_first: Arc<Notify>,
        second_started: Arc<AtomicBool>,
    }

    struct SessionCaptureTool {
        sessions: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Tool for SessionCaptureTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "session_capture".to_owned(),
                description: "captures the engine session id".to_owned(),
                input_schema: json!({"type": "object"}),
                capabilities: CapabilityManifest::default(),
            }
        }

        async fn execute(
            &self,
            context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            let session = context
                .session_id()
                .ok_or_else(|| ToolError::InvalidInput("missing session".to_owned()))?;
            self.sessions
                .lock()
                .expect("session capture")
                .push(session.0.clone());
            Ok(ToolResult::new("captured", Value::Null))
        }
    }

    #[async_trait]
    impl Tool for EmptySequentialTool {
        fn descriptor(&self) -> ToolDescriptor {
            self.descriptor.clone()
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            if self.first {
                self.first_started.notify_one();
                self.release_first.notified().await;
            } else {
                self.second_started.store(true, Ordering::SeqCst);
            }
            Ok(ToolResult::new("done", Value::Null))
        }
    }

    #[async_trait]
    impl Tool for StreamingTool {
        fn descriptor(&self) -> ToolDescriptor {
            self.descriptor.clone()
        }

        async fn execute(
            &self,
            context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            context
                .output
                .emit(ToolOutputChunk {
                    stream: ToolOutputStream::Stdout,
                    content: "live chunk".to_owned(),
                })
                .await?;
            tokio::select! {
                () = self.release.notified() => {
                    self.completed.store(true, Ordering::SeqCst);
                    Ok(ToolResult::new("done", Value::Null))
                }
                () = context.cancellation.cancelled() => Err(ToolError::Cancelled),
            }
        }
    }

    struct CleanupTool {
        cleanup_finished: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for CleanupTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "cleanup_tool".to_owned(),
                description: "cooperative cancellation fixture".to_owned(),
                input_schema: json!({"type": "object"}),
                capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
            }
        }

        async fn execute(
            &self,
            context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            context.cancellation.cancelled().await;
            context
                .output
                .emit(ToolOutputChunk {
                    stream: ToolOutputStream::Stderr,
                    content: "cleanup complete".to_owned(),
                })
                .await?;
            tokio::task::yield_now().await;
            self.cleanup_finished.store(true, Ordering::SeqCst);
            Err(ToolError::Cancelled)
        }
    }

    struct PanickingTool;

    #[async_trait]
    impl Tool for PanickingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "panic_tool".to_owned(),
                description: "panic fixture".to_owned(),
                input_schema: json!({"type": "object"}),
                capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
            }
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            panic!("fixture tool panic")
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<SessionEvent>>,
        batch_sizes: Mutex<Vec<usize>>,
        tail_floor: Mutex<Option<SequenceId>>,
    }

    #[async_trait]
    impl SessionEventSink for RecordingSink {
        async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            self.batch_sizes.lock().expect("batch sizes").push(1);
            self.events
                .lock()
                .expect("event sink lock")
                .push(observe_event(event.clone()).expect("durable fixture event"));
            Ok(event)
        }

        async fn append_batch(
            &self,
            events: Vec<EngineEvent>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            self.batch_sizes
                .lock()
                .expect("batch sizes")
                .push(events.len());
            self.events.lock().expect("event sink lock").extend(
                events
                    .iter()
                    .cloned()
                    .map(|event| observe_event(event).expect("durable fixture event")),
            );
            Ok(events)
        }

        async fn read_after(
            &self,
            last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            let first = last_seen.map_or(0, |sequence| sequence.0.saturating_add(1));
            Ok(self
                .events
                .lock()
                .expect("event sink lock")
                .iter()
                .filter(|event| event.sequence.0 >= first)
                .map(|event| event.wire.clone())
                .collect())
        }

        async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
            let floor = *self.tail_floor.lock().expect("tail floor");
            let actual = self
                .events
                .lock()
                .expect("event sink lock")
                .last()
                .map(|event| event.sequence);
            Ok(match (floor, actual) {
                (Some(floor), Some(actual)) => Some(floor.max(actual)),
                (floor, actual) => floor.or(actual),
            })
        }
    }

    #[derive(Default)]
    struct AccountingRecordingSink {
        inner: RecordingSink,
    }

    #[async_trait]
    impl SessionEventSink for AccountingRecordingSink {
        async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            self.inner.append(event).await
        }

        async fn append_batch(
            &self,
            events: Vec<EngineEvent>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            self.inner.append_batch(events).await
        }

        async fn read_after(
            &self,
            last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            self.inner.read_after(last_seen).await
        }

        async fn last_sequence(&self) -> Result<Option<SequenceId>, AgentLoopError> {
            self.inner.last_sequence().await
        }

        async fn budget_totals(
            &self,
            _query: BudgetLedgerQuery,
        ) -> Result<BudgetLedgerTotals, AgentLoopError> {
            let mut totals = BudgetLedgerTotals {
                authoritative: true,
                ..BudgetLedgerTotals::default()
            };
            for event in self.inner.events.lock().expect("event sink lock").iter() {
                let cost = match &event.kind {
                    PendingEvent::TurnFinished { cost, .. }
                    | PendingEvent::CompactionAttemptFinished { cost, .. }
                    | PendingEvent::CompactionFinished {
                        cost: Some(cost), ..
                    } => Some(cost),
                    _ => None,
                };
                match cost {
                    Some(Cost::Monetary {
                        amount_micros,
                        currency,
                    }) if currency.eq_ignore_ascii_case("USD") => {
                        totals.session_cost_micros_usd = totals
                            .session_cost_micros_usd
                            .saturating_add(*amount_micros);
                        totals.daily_cost_micros_usd =
                            totals.daily_cost_micros_usd.saturating_add(*amount_micros);
                    }
                    Some(Cost::Monetary { .. }) => {
                        totals.session_non_usd_monetary_entries =
                            totals.session_non_usd_monetary_entries.saturating_add(1);
                        totals.daily_non_usd_monetary_entries =
                            totals.daily_non_usd_monetary_entries.saturating_add(1);
                    }
                    Some(Cost::SubscriptionQuota { .. }) => {
                        totals.session_subscription_quota_entries =
                            totals.session_subscription_quota_entries.saturating_add(1);
                        totals.daily_subscription_quota_entries =
                            totals.daily_subscription_quota_entries.saturating_add(1);
                    }
                    Some(Cost::Unavailable { .. }) => {
                        totals.session_cost_unavailable_entries =
                            totals.session_cost_unavailable_entries.saturating_add(1);
                        totals.daily_cost_unavailable_entries =
                            totals.daily_cost_unavailable_entries.saturating_add(1);
                    }
                    Some(Cost::AiCredits { .. }) | None => {}
                }
            }
            Ok(totals)
        }
    }

    struct FailingSink;

    #[async_trait]
    impl SessionEventSink for FailingSink {
        async fn append(&self, _event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            Err(AgentLoopError::Persistence("fixture failure".to_owned()))
        }

        async fn read_after(
            &self,
            _last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            Err(AgentLoopError::Persistence("fixture failure".to_owned()))
        }
    }

    #[derive(Clone, Copy)]
    enum MalformedBatchMode {
        Payload,
        Sequence,
    }

    struct MalformedBatchSink {
        mode: MalformedBatchMode,
    }

    #[async_trait]
    impl SessionEventSink for MalformedBatchSink {
        async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            Ok(event)
        }

        async fn append_batch(
            &self,
            mut events: Vec<EngineEvent>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            match self.mode {
                MalformedBatchMode::Payload => {
                    if let Some(event) = events.get_mut(1) {
                        let meta = event.meta().expect("durable event").clone();
                        *event = EngineEvent::Error {
                            meta,
                            error: EngineError {
                                category: EngineErrorCategory::Internal,
                                code: "substituted".to_owned(),
                                message: "substituted".to_owned(),
                                retryable: false,
                                details: None,
                            },
                        };
                    }
                }
                MalformedBatchMode::Sequence => {
                    if let Some(event) = events.get_mut(1) {
                        event.meta_mut().expect("durable event").sequence_id = 9.into();
                    }
                }
            }
            Ok(events)
        }

        async fn read_after(
            &self,
            _last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            Ok(Vec::new())
        }
    }

    struct BlockingBatchSink {
        persisted: Mutex<Vec<EngineEvent>>,
        blocked_once: AtomicBool,
        entered: Notify,
        release: Notify,
    }

    impl BlockingBatchSink {
        fn persist(&self, events: Vec<EngineEvent>) -> Vec<EngineEvent> {
            self.persisted
                .lock()
                .expect("persisted events")
                .extend(events.iter().cloned());
            events
        }
    }

    #[async_trait]
    impl SessionEventSink for BlockingBatchSink {
        async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            self.persist(vec![event])
                .pop()
                .ok_or_else(|| AgentLoopError::Persistence("fixture append failed".to_owned()))
        }

        async fn append_batch(
            &self,
            events: Vec<EngineEvent>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            if events.len() > 1 && !self.blocked_once.swap(true, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(self.persist(events))
        }

        async fn read_after(
            &self,
            last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            let first = last_seen.map_or(0, |sequence| sequence.0.saturating_add(1));
            Ok(self
                .persisted
                .lock()
                .expect("persisted events")
                .iter()
                .filter(|event| event.meta().is_some_and(|meta| meta.sequence_id.0 >= first))
                .cloned()
                .collect())
        }
    }

    #[derive(Default)]
    struct RecordingCheckpoints {
        events: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl MutationCheckpointCoordinator for RecordingCheckpoints {
        async fn begin(
            &self,
            session_id: &SessionId,
            _agent_turn: u64,
            tool_call_id: &str,
            scope: &MutationScope,
        ) -> Result<MutationCheckpoint, AgentLoopError> {
            self.events
                .lock()
                .expect("checkpoint lock")
                .push(format!("begin:{}:{tool_call_id}:{scope:?}", session_id.0));
            Ok(MutationCheckpoint {
                id: Some(tool_call_id.to_owned()),
            })
        }

        async fn finish(
            &self,
            checkpoint: &MutationCheckpoint,
            outcome: MutationCheckpointOutcome,
        ) -> Result<(), AgentLoopError> {
            self.events
                .lock()
                .expect("checkpoint lock")
                .push(format!("finish:{:?}:{outcome:?}", checkpoint.id));
            Ok(())
        }

        async fn prepare_apply_rewind(
            &self,
            session_id: &SessionId,
            to_turn: u64,
            operation_id: &str,
        ) -> Result<RewindCheckpoint, AgentLoopError> {
            self.events
                .lock()
                .expect("checkpoint lock")
                .push(format!("rewind:{}:{to_turn}:{operation_id}", session_id.0));
            Ok(RewindCheckpoint {
                id: operation_id.to_owned(),
                unrestorable_paths: Vec::new(),
            })
        }

        async fn acknowledge_rewind(
            &self,
            checkpoint: &RewindCheckpoint,
        ) -> Result<(), AgentLoopError> {
            self.events
                .lock()
                .expect("checkpoint lock")
                .push(format!("ack:{}", checkpoint.id));
            Ok(())
        }
    }

    struct OrderedRewindSink {
        fail_rewind: AtomicBool,
        order: Arc<Mutex<Vec<String>>>,
        events: Mutex<Vec<EngineEvent>>,
    }

    #[async_trait]
    impl SessionEventSink for OrderedRewindSink {
        async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            if matches!(event, EngineEvent::ConversationRewound { .. }) {
                self.order
                    .lock()
                    .expect("rewind order")
                    .push("persist".to_owned());
                if self.fail_rewind.load(Ordering::SeqCst) {
                    return Err(AgentLoopError::Persistence(
                        "fixture rewind append failed".to_owned(),
                    ));
                }
            }
            self.events.lock().expect("events").push(event.clone());
            Ok(event)
        }

        async fn append_batch(
            &self,
            batch: Vec<EngineEvent>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            let mut events = Vec::with_capacity(batch.len());
            for event in batch {
                events.push(self.append(event).await?);
            }
            Ok(events)
        }

        async fn read_after(
            &self,
            last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            let first = last_seen.map_or(0, |sequence| sequence.0.saturating_add(1));
            Ok(self
                .events
                .lock()
                .expect("events")
                .iter()
                .filter(|event| event.meta().is_some_and(|meta| meta.sequence_id.0 >= first))
                .cloned()
                .collect())
        }
    }

    struct OrderedRewindCoordinator {
        order: Arc<Mutex<Vec<String>>>,
        fail_ack: Arc<AtomicBool>,
        unrestorable_paths: Vec<UnrestorablePath>,
    }

    #[async_trait]
    impl MutationCheckpointCoordinator for OrderedRewindCoordinator {
        async fn begin(
            &self,
            _session_id: &SessionId,
            _agent_turn: u64,
            _tool_call_id: &str,
            _scope: &MutationScope,
        ) -> Result<MutationCheckpoint, AgentLoopError> {
            Ok(MutationCheckpoint { id: None })
        }

        async fn finish(
            &self,
            _checkpoint: &MutationCheckpoint,
            _outcome: MutationCheckpointOutcome,
        ) -> Result<(), AgentLoopError> {
            Ok(())
        }

        async fn prepare_apply_rewind(
            &self,
            _session_id: &SessionId,
            _to_turn: u64,
            operation_id: &str,
        ) -> Result<RewindCheckpoint, AgentLoopError> {
            self.order
                .lock()
                .expect("rewind order")
                .push("apply".to_owned());
            Ok(RewindCheckpoint {
                id: operation_id.to_owned(),
                unrestorable_paths: self.unrestorable_paths.clone(),
            })
        }

        async fn acknowledge_rewind(
            &self,
            _checkpoint: &RewindCheckpoint,
        ) -> Result<(), AgentLoopError> {
            self.order
                .lock()
                .expect("rewind order")
                .push("ack".to_owned());
            if self.fail_ack.load(Ordering::SeqCst) {
                Err(AgentLoopError::Persistence(
                    "fixture acknowledgement failed".to_owned(),
                ))
            } else {
                Ok(())
            }
        }
    }

    struct FixedHook {
        label: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
        result: Result<HookDirective, HookError>,
    }

    #[async_trait]
    impl HookHandler for FixedHook {
        async fn invoke(
            &self,
            _invocation: HookInvocation<'_>,
        ) -> Result<HookDirective, HookError> {
            self.calls
                .lock()
                .expect("hook call lock")
                .push(self.label.to_owned());
            self.result.clone()
        }
    }

    struct RewriteArgumentsHook(Value);

    struct RewriteUserPromptHook(&'static str);

    struct NeverHook;

    #[async_trait]
    impl HookHandler for NeverHook {
        async fn invoke(
            &self,
            _invocation: HookInvocation<'_>,
        ) -> Result<HookDirective, HookError> {
            std::future::pending().await
        }
    }

    #[async_trait]
    impl HookHandler for RewriteArgumentsHook {
        async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
            let mut payload = invocation.payload().clone();
            payload["arguments"] = self.0.clone();
            Ok(HookDirective::Replace(payload))
        }
    }

    #[async_trait]
    impl HookHandler for RewriteUserPromptHook {
        async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
            let mut payload = invocation.payload().clone();
            payload["content"] = Value::String(self.0.to_owned());
            Ok(HookDirective::Replace(payload))
        }
    }

    struct EchoCommand;

    #[async_trait]
    impl CommandHandler<SessionCommandContext, SessionCommandOutput> for EchoCommand {
        async fn execute(
            &self,
            _context: &mut SessionCommandContext,
            invocation: CommandInvocation,
        ) -> Result<SessionCommandOutput, CommandExecutionError> {
            Ok(SessionCommandOutput {
                message: invocation.arguments().to_owned(),
                action: SessionCommandAction::None,
            })
        }
    }

    fn descriptor(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            name: name.to_owned(),
            description: format!("fixture {name}"),
            input_schema: json!({"type": "object"}),
            capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
        }
    }

    fn tool_script(calls: &[(&str, &str, Value)], usage: &[TokenUsage]) -> ProviderScript {
        let mut events = vec![Ok(ProviderEvent::MessageStart {
            model: "fixture-model".to_owned(),
        })];
        for (id, name, arguments) in calls {
            events.push(Ok(ProviderEvent::ToolCallStart {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
            }));
            events.push(Ok(ProviderEvent::ToolCallEnd {
                id: (*id).to_owned(),
                arguments: arguments.clone(),
            }));
        }
        events.extend(
            usage
                .iter()
                .copied()
                .map(|usage| Ok(ProviderEvent::Usage { usage })),
        );
        events.push(Ok(ProviderEvent::Finished {
            reason: FinishReason::ToolCalls,
        }));
        events
    }

    fn stop_script(text: &str, usage: &[TokenUsage]) -> ProviderScript {
        let mut events = vec![
            Ok(ProviderEvent::MessageStart {
                model: "fixture-model".to_owned(),
            }),
            Ok(ProviderEvent::TextDelta {
                text: text.to_owned(),
            }),
        ];
        events.extend(
            usage
                .iter()
                .copied()
                .map(|usage| Ok(ProviderEvent::Usage { usage })),
        );
        events.push(Ok(ProviderEvent::Finished {
            reason: FinishReason::Stop,
        }));
        events
    }

    fn config(
        root: &Path,
        model: Arc<dyn ModelDriver>,
        tools: Arc<ToolRegistry>,
        permissions: PermissionDecision,
        hooks: HookDispatcher,
    ) -> SessionActorConfig {
        SessionActorConfig {
            session_id: SessionId("fixture-session".to_owned()),
            workspace_root: root.to_path_buf(),
            initial_session_context: Vec::new(),
            model_alias: "fast".to_owned(),
            model,
            tools,
            permissions: Arc::new(PermissionGate::new(permissions)),
            hooks: Arc::new(hooks),
            commands: Arc::new(builtin_command_registry().expect("built-in commands")),
            event_sink: Arc::new(NoopSessionEventSink::default()),
            event_clock: Arc::new(FixedClock),
            secret_redactor: Arc::new(NoopSecretRedactor),
            checkpoints: Arc::new(NoopMutationCheckpointCoordinator),
            recovered: SessionRecoveredState::default(),
            max_turns: 10,
            identical_tool_failure_limit: 5,
            max_output_tokens: 256,
            thinking: ThinkingLevel::Off,
            event_capacity: 256,
        }
    }

    #[derive(Debug)]
    struct FixedClock;

    impl EventClock for FixedClock {
        fn emitted_at(&self) -> String {
            "2026-01-02T03:04:05.006Z".to_owned()
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct SessionEvent {
        version: u16,
        sequence: SequenceId,
        kind: PendingEvent,
        wire: EngineEvent,
    }

    fn observe_event(wire: EngineEvent) -> Option<SessionEvent> {
        let meta = wire.meta()?.clone();
        let kind = recovered_pending_event(&wire).ok()??;
        Some(SessionEvent {
            version: meta.protocol_version,
            sequence: meta.sequence_id,
            kind,
            wire,
        })
    }

    fn wire_event(sequence: u64, kind: PendingEvent) -> EngineEvent {
        kind.stamp(EventMeta {
            protocol_version: PROTOCOL_VERSION,
            session_id: SessionId("fixture-session".to_owned()),
            sequence_id: SequenceId(sequence),
            emitted_at: FixedClock.emitted_at(),
            caused_by: None,
        })
    }

    fn protocol_meta(client: &str, request: &str) -> CommandMeta {
        CommandMeta {
            protocol_version: PROTOCOL_VERSION,
            client_id: ClientId(client.to_owned()),
            request_id: RequestId(request.to_owned()),
        }
    }

    async fn next_matching(
        receiver: &mut SessionSubscription,
        mut matches: impl FnMut(&PendingEvent) -> bool,
    ) -> SessionEvent {
        loop {
            let wire = timeout(Duration::from_secs(3), receiver.recv())
                .await
                .expect("event timeout")
                .expect("event channel");
            let Some(event) = observe_event(wire) else {
                continue;
            };
            if matches(&event.kind) {
                return event;
            }
        }
    }

    async fn collect_turn(receiver: &mut SessionSubscription) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        loop {
            let wire = timeout(Duration::from_secs(3), receiver.recv())
                .await
                .expect("event timeout")
                .expect("event channel");
            let Some(event) = observe_event(wire) else {
                continue;
            };
            let done = matches!(event.kind, PendingEvent::TurnFinished { .. });
            events.push(event);
            if done {
                return events;
            }
        }
    }

    #[tokio::test]
    async fn commands_share_the_public_registry_and_events_round_trip() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::default());
        let mut commands = builtin_command_registry().expect("built-ins");
        commands
            .register(
                CommandDescriptor::new("echo", "fixture extension command"),
                EchoCommand,
            )
            .expect("extension command");
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.commands = Arc::new(commands);
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        assert_eq!(
            handle.send_message("/echo hello").await.expect("command"),
            MessageDisposition::Command
        );
        let event = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::CommandFinished { .. })
        })
        .await;
        assert!(matches!(
            &event.kind,
            PendingEvent::CommandFinished { name, message, .. }
                if name == "echo" && message == "hello"
        ));
        let encoded = serde_json::to_vec(&event.wire).expect("serialize event");
        assert!(String::from_utf8_lossy(&encoded).contains("\"sequence_id\":\""));
        let decoded: EngineEvent = serde_json::from_slice(&encoded).expect("deserialize event");
        assert_eq!(decoded, event.wire);
    }

    #[test]
    fn actor_rejects_session_ids_outside_the_storage_safe_alphabet() {
        let root = TempDir::new().expect("tempdir");
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::default()),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.session_id = SessionId("../escape".to_owned());
        assert!(matches!(
            SessionActor::spawn(actor_config),
            Err(AgentLoopError::InvalidConfiguration(_))
        ));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn rewind_applies_then_persists_then_acknowledges_and_never_acks_failed_append() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            stop_script("one", &[]),
            stop_script("two", &[]),
            stop_script("three", &[]),
        ]));
        let order = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(OrderedRewindSink {
            fail_rewind: AtomicBool::new(false),
            order: order.clone(),
            events: Mutex::new(Vec::new()),
        });
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let fail_ack = Arc::new(AtomicBool::new(false));
        actor_config.checkpoints = Arc::new(OrderedRewindCoordinator {
            order: order.clone(),
            fail_ack: fail_ack.clone(),
            unrestorable_paths: Vec::new(),
        });
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("first").await.expect("first");
        collect_turn(&mut events).await;
        handle.send_message("second").await.expect("second");
        collect_turn(&mut events).await;
        assert_eq!(
            handle
                .send_message("/rewind 1")
                .await
                .expect("rewind command"),
            MessageDisposition::Command
        );
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::ConversationRewound { to_turn: 1, .. })
        })
        .await;
        assert_eq!(
            order.lock().expect("rewind order").as_slice(),
            &["apply", "persist", "ack"]
        );
        assert_eq!(
            handle
                .snapshot()
                .await
                .expect("snapshot")
                .conversation
                .len(),
            2
        );

        handle.send_message("third").await.expect("third");
        collect_turn(&mut events).await;
        assert_eq!(
            handle
                .snapshot()
                .await
                .expect("snapshot")
                .conversation
                .len(),
            4
        );
        order.lock().expect("rewind order").clear();
        fail_ack.store(true, Ordering::SeqCst);
        assert!(matches!(
            handle.rewind(1).await,
            Err(AgentLoopError::Persistence(_))
        ));
        assert_eq!(
            handle
                .snapshot()
                .await
                .expect("snapshot")
                .conversation
                .len(),
            2
        );
        assert_eq!(
            order.lock().expect("rewind order").as_slice(),
            &["apply", "persist", "ack"]
        );
        fail_ack.store(false, Ordering::SeqCst);
        handle.rewind(1).await.expect("retry pending ack");
        assert_eq!(
            order.lock().expect("rewind order").as_slice(),
            &["apply", "persist", "ack", "ack"]
        );

        order.lock().expect("rewind order").clear();
        sink.fail_rewind.store(true, Ordering::SeqCst);
        assert!(matches!(
            handle.rewind(1).await,
            Err(AgentLoopError::Persistence(_))
        ));
        assert_eq!(
            order.lock().expect("rewind order").as_slice(),
            &["apply", "persist"]
        );
    }

    #[tokio::test]
    async fn slash_rewind_reports_unrestorable_paths_in_command_event() {
        let root = TempDir::new().expect("tempdir");
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::new([stop_script("one", &[])])),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.checkpoints = Arc::new(OrderedRewindCoordinator {
            order: Arc::new(Mutex::new(Vec::new())),
            fail_ack: Arc::new(AtomicBool::new(false)),
            unrestorable_paths: vec![UnrestorablePath {
                path: "missing.txt".to_owned(),
                reason: "deleted outside the checkpoint scope".to_owned(),
            }],
        });
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("first").await.expect("first");
        collect_turn(&mut events).await;
        handle
            .send_message("/rewind 1")
            .await
            .expect("rewind command");
        let command = next_matching(
            &mut events,
            |kind| matches!(kind, PendingEvent::CommandFinished { name, .. } if name == "rewind"),
        )
        .await;
        assert!(matches!(
            command.kind,
            PendingEvent::CommandFinished {
                unrestorable_paths,
                ..
            } if unrestorable_paths == vec![UnrestorablePath {
                path: "missing.txt".to_owned(),
                reason: "deleted outside the checkpoint scope".to_owned(),
            }]
        ));
    }

    #[tokio::test]
    async fn recovered_sequence_and_user_message_are_appended_before_broadcast() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
            batch_sizes: Mutex::new(Vec::new()),
            tail_floor: Mutex::new(Some(40.into())),
        });
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        actor_config.recovered = SessionRecoveredState {
            conversation: Vec::new(),
            queued_messages: Vec::new(),
            completed_turns: 6,
            next_turn: 7,
            last_sequence: Some(40.into()),
            interrupted_turn: None,
            turn_ends: BTreeMap::new(),
            ..SessionRecoveredState::default()
        };
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("persist me").await.expect("message");
        let started = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnStarted { .. })
        })
        .await;
        assert_eq!(started.sequence, 42.into());
        assert!(matches!(
            started.kind,
            PendingEvent::TurnStarted { turn: 7 }
        ));
        let accepted = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::UserMessageAccepted { .. })
        })
        .await;
        assert_eq!(accepted.sequence, 43.into());
        assert!(matches!(
            &accepted.kind,
            PendingEvent::UserMessageAccepted { turn: 7, content, .. }
                if content == "persist me"
        ));
        let persisted = sink.events.lock().expect("sink lock");
        assert_eq!(persisted.get(2), Some(&accepted));
    }

    #[tokio::test]
    async fn persistence_failure_is_returned_before_provider_work_or_broadcast() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([stop_script("unused", &[])]));
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = Arc::new(FailingSink);
        let handle = SessionActor::spawn(actor_config).expect("actor");
        assert!(matches!(
            handle.send_message("must persist").await,
            Err(AgentLoopError::Persistence(_))
        ));
        assert_eq!(model.request_count(), 0);
    }

    #[tokio::test]
    async fn opening_batch_is_fully_persisted_before_any_event_is_broadcast() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(BlockingBatchSink {
            persisted: Mutex::new(Vec::new()),
            blocked_once: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let mut actor_config = config(
            root.path(),
            Arc::new(PendingModel),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        handle.ensure_local_driver().await.expect("local driver");
        let mut events = handle.subscribe_client(ClientId("local".to_owned()), Some(0.into()));
        let sender = handle.clone();
        let send = tokio::spawn(async move { sender.send_message("persist together").await });

        timeout(Duration::from_secs(1), sink.entered.notified())
            .await
            .expect("opening batch reached sink");
        assert_eq!(sink.persisted.lock().expect("persisted events").len(), 1);
        assert!(matches!(
            events.recv().await.expect("command ack"),
            EngineEvent::CommandAcknowledged {
                outcome: CommandOutcome::Accepted,
                ..
            }
        ));
        assert!(
            timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err()
        );

        sink.release.notify_one();
        assert_eq!(
            send.await.expect("send task").expect("send message"),
            MessageDisposition::Started
        );
        let persisted = sink.persisted.lock().expect("persisted events").clone();
        assert!(persisted.len() >= 3);
        assert_eq!(
            persisted[1].meta().expect("event meta").sequence_id,
            1.into()
        );
        assert_eq!(
            persisted[2].meta().expect("event meta").sequence_id,
            2.into()
        );
        assert!(matches!(persisted[1], EngineEvent::TurnStarted { .. }));
        assert!(matches!(
            &persisted[2],
            EngineEvent::UserMessageAccepted { agent_turn: 1, content, .. }
                if content == "persist together"
        ));
        assert_eq!(events.recv().await.expect("started event"), persisted[1]);
        assert_eq!(events.recv().await.expect("accepted event"), persisted[2]);

        assert!(handle.interrupt().await.expect("cleanup interrupt"));
        collect_turn(&mut events).await;
    }

    #[tokio::test]
    async fn malformed_batch_payload_or_sequence_is_rejected_before_broadcast_or_model_work() {
        for mode in [MalformedBatchMode::Payload, MalformedBatchMode::Sequence] {
            let root = TempDir::new().expect("tempdir");
            let model = Arc::new(ScriptedModel::new([stop_script("unused", &[])]));
            let mut actor_config = config(
                root.path(),
                model.clone(),
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            );
            actor_config.event_sink = Arc::new(MalformedBatchSink { mode });
            let handle = SessionActor::spawn(actor_config).expect("actor");
            handle.ensure_local_driver().await.expect("local driver");
            let mut events = handle.subscribe_client(ClientId("local".to_owned()), Some(0.into()));

            assert!(matches!(
                handle.send_message("reject malformed batch").await,
                Err(AgentLoopError::Persistence(_))
            ));
            assert_eq!(model.request_count(), 0);
            assert!(matches!(
                events.recv().await.expect("command ack"),
                EngineEvent::CommandAcknowledged {
                    outcome: CommandOutcome::Accepted,
                    ..
                }
            ));
            let failure = events.recv().await.expect("caused-by failure event");
            assert!(matches!(
                failure,
                EngineEvent::Error {
                    meta: EventMeta {
                        caused_by: Some(RequestId(ref request)),
                        ..
                    },
                    ..
                } if request == "local-1"
            ));
            assert!(
                timeout(Duration::from_millis(20), events.recv())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn successful_single_delta_batches_delta_commit_and_finish() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(RecordingSink::default());
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::new([stop_script("terminal", &[])])),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();

        handle.send_message("run").await.expect("message");
        let observed = collect_turn(&mut events).await;
        let snapshot = handle.snapshot().await.expect("snapshot");
        assert_eq!(snapshot.conversation.len(), 2);
        assert!(matches!(
            &snapshot.conversation[0],
            Turn {
                role: Role::User,
                blocks,
                ..
            } if matches!(blocks.as_slice(), [Block::Text { text }] if text == "run")
        ));
        let deltas = observed
            .iter()
            .filter_map(|event| match &event.kind {
                PendingEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas, ["terminal"]);
        assert_eq!(
            sink.batch_sizes.lock().expect("batch sizes").as_slice(),
            &[1, 3, 1, 1, 3]
        );
        let persisted = sink.events.lock().expect("event sink lock");
        assert!(matches!(
            persisted[1].kind,
            PendingEvent::TurnStarted { turn: 1 }
        ));
        assert!(matches!(
            &persisted[2].kind,
            PendingEvent::UserMessageAccepted { turn: 1, content, .. } if content == "run"
        ));
        assert!(matches!(
            &persisted[3].kind,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: Turn {
                    role: Role::User,
                    blocks,
                    ..
                },
            } if matches!(blocks.as_slice(), [Block::Text { text }] if text == "run")
        ));
        let terminal = &persisted[persisted.len() - 3..];
        assert!(matches!(
            &terminal[0].kind,
            PendingEvent::TextDelta { turn: 1, text } if text == "terminal"
        ));
        assert!(matches!(
            terminal[1].kind,
            PendingEvent::ConversationTurnCommitted { agent_turn: 1, .. }
        ));
        assert!(matches!(
            terminal[2].kind,
            PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Completed,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn no_hook_multi_message_opening_batch_preserves_accept_and_commit_order() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(RecordingSink::default());
        let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        actor_config.recovered.queued_messages =
            vec!["first queued".to_owned(), "second queued".to_owned()];
        let handle = SessionActor::spawn(actor_config).expect("actor");

        timeout(Duration::from_secs(3), async {
            loop {
                let snapshot = handle.snapshot().await.expect("snapshot");
                if !snapshot.running && snapshot.completed_turns == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued turn completion");

        assert_eq!(
            sink.batch_sizes.lock().expect("batch sizes").as_slice(),
            &[5, 1, 1, 3]
        );
        let persisted = sink.events.lock().expect("event sink lock");
        assert!(matches!(
            persisted[0].kind,
            PendingEvent::TurnStarted { turn: 1 }
        ));
        for (event, expected) in persisted[1..3]
            .iter()
            .zip(["first queued", "second queued"])
        {
            assert!(matches!(
                &event.kind,
                PendingEvent::UserMessageAccepted { turn: 1, content, .. }
                    if content == expected
            ));
        }
        for (event, expected) in persisted[3..5]
            .iter()
            .zip(["first queued", "second queued"])
        {
            assert!(matches!(
                &event.kind,
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: 1,
                    turn: Turn {
                        role: Role::User,
                        blocks,
                        ..
                    },
                } if matches!(blocks.as_slice(), [Block::Text { text }] if text == expected)
            ));
        }
        drop(persisted);
        let requests = model.requests.lock().expect("request lock");
        let user_text = requests[0]
            .turns
            .iter()
            .filter(|turn| turn.role == Role::User)
            .filter_map(|turn| match turn.blocks.as_slice() {
                [Block::Text { text }] => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(user_text, ["first queued", "second queued"]);
    }

    #[tokio::test]
    async fn registered_user_prompt_hook_keeps_rewrite_on_the_separate_commit_path() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(RecordingSink::default());
        let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
        let mut hooks = builtin_hook_dispatcher().expect("hooks");
        hooks
            .register(
                HookRegistration::new("fixture.rewrite-user", HookEvent::UserPromptSubmit),
                RewriteUserPromptHook("rewritten by hook"),
            )
            .expect("user prompt hook");
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            hooks,
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();

        handle.send_message("raw input").await.expect("message");
        collect_turn(&mut events).await;

        assert_eq!(
            sink.batch_sizes.lock().expect("batch sizes").as_slice(),
            &[1, 2, 1, 1, 1, 3]
        );
        let persisted = sink.events.lock().expect("event sink lock");
        assert!(matches!(
            &persisted[2].kind,
            PendingEvent::UserMessageAccepted { content, .. } if content == "raw input"
        ));
        assert!(matches!(
            &persisted[3].kind,
            PendingEvent::ConversationTurnCommitted {
                turn: Turn { blocks, .. },
                ..
            } if matches!(blocks.as_slice(), [Block::Text { text }] if text == "rewritten by hook")
        ));
        drop(persisted);
        let requests = model.requests.lock().expect("request lock");
        assert!(requests[0].turns.iter().any(|turn| matches!(
            turn.blocks.as_slice(),
            [Block::Text { text }] if turn.role == Role::User && text == "rewritten by hook"
        )));
    }

    #[tokio::test]
    async fn multiple_immediate_deltas_keep_order_and_only_defer_the_trailing_delta() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(RecordingSink::default());
        let script = vec![
            Ok(ProviderEvent::MessageStart {
                model: "fixture-model".to_owned(),
            }),
            Ok(ProviderEvent::TextDelta {
                text: "first".to_owned(),
            }),
            Ok(ProviderEvent::TextDelta {
                text: "second".to_owned(),
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            }),
        ];
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::new([script])),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();

        handle.send_message("run").await.expect("message");
        let observed = collect_turn(&mut events).await;
        let deltas = observed
            .iter()
            .filter_map(|event| match &event.kind {
                PendingEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas, ["first", "second"]);
        assert_eq!(
            sink.batch_sizes.lock().expect("batch sizes").as_slice(),
            &[1, 3, 1, 1, 1, 3]
        );
    }

    #[tokio::test]
    async fn delayed_finish_never_holds_a_lone_delta_beyond_the_coalescing_window() {
        let root = TempDir::new().expect("tempdir");
        let handle = SessionActor::spawn(config(
            root.path(),
            Arc::new(DelayedFinishModel {
                delay: Duration::from_millis(50),
            }),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();

        handle.send_message("run").await.expect("message");
        let delta = timeout(
            Duration::from_millis(30),
            next_matching(&mut events, |kind| {
                matches!(kind, PendingEvent::TextDelta { .. })
            }),
        )
        .await
        .expect("delta must be visible promptly");
        assert!(matches!(
            delta.kind,
            PendingEvent::TextDelta { turn: 1, ref text }
                if text == "visible promptly"
        ));
        let finished = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { .. })
        })
        .await;
        assert!(matches!(
            finished.kind,
            PendingEvent::TurnFinished {
                status: AgentTurnStatus::Completed,
                ..
            }
        ));
    }

    #[test]
    fn projector_preserves_committed_partial_ir_and_marks_kill_tail_interrupted() {
        let user = Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "inspect".to_owned(),
            }],
            meta: TurnMeta::default(),
        };
        let partial = Turn {
            role: Role::Assistant,
            blocks: vec![
                Block::Thinking {
                    content: "opaque".to_owned(),
                    signature: Some("signed".to_owned()),
                },
                Block::Text {
                    text: "partial".to_owned(),
                },
                Block::Citation {
                    uri: "https://example.invalid/source".to_owned(),
                    title: Some("source".to_owned()),
                    excerpt: None,
                },
            ],
            meta: TurnMeta::default(),
        };
        let events = vec![
            wire_event(0, PendingEvent::TurnStarted { turn: 1 }),
            wire_event(
                1,
                PendingEvent::UserMessageAccepted {
                    turn: 1,
                    content: "inspect".to_owned(),
                    attachments: Vec::new(),
                },
            ),
            wire_event(
                2,
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: 1,
                    turn: user.clone(),
                },
            ),
            wire_event(
                3,
                PendingEvent::ThinkingDelta {
                    turn: 1,
                    content: "opaque".to_owned(),
                    signature: Some("signed".to_owned()),
                },
            ),
            wire_event(
                4,
                PendingEvent::TextDelta {
                    turn: 1,
                    text: "partial".to_owned(),
                },
            ),
            wire_event(
                5,
                PendingEvent::CitationDelta {
                    turn: 1,
                    uri: "https://example.invalid/source".to_owned(),
                    title: Some("source".to_owned()),
                },
            ),
        ];
        let recovered = project_session_events(&events).expect("project events");
        assert_eq!(recovered.conversation, vec![user, partial]);
        assert_eq!(recovered.interrupted_turn, Some(1));
        assert_eq!(recovered.next_turn, 2);
        assert_eq!(recovered.last_sequence, Some(5.into()));
    }

    #[test]
    fn projector_rewind_clears_future_queue_failed_uncommitted_and_partial_state() {
        let committed_user = Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "kept user".to_owned(),
            }],
            meta: TurnMeta::default(),
        };
        let committed_assistant = Turn {
            role: Role::Assistant,
            blocks: vec![Block::Text {
                text: "kept answer".to_owned(),
            }],
            meta: TurnMeta::default(),
        };
        let kinds = vec![
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::UserMessageAccepted {
                turn: 1,
                content: "kept user".to_owned(),
                attachments: Vec::new(),
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: committed_user.clone(),
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: committed_assistant.clone(),
            },
            PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Completed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
            },
            PendingEvent::MessageQueued {
                position: 1,
                content: "future duplicate".to_owned(),
                attachments: Vec::new(),
            },
            PendingEvent::TurnStarted { turn: 2 },
            PendingEvent::UserMessageAccepted {
                turn: 2,
                content: "future duplicate".to_owned(),
                attachments: Vec::new(),
            },
            PendingEvent::TextDelta {
                turn: 2,
                text: "future partial".to_owned(),
            },
            PendingEvent::TurnFinished {
                turn: 2,
                status: AgentTurnStatus::Failed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
            },
            PendingEvent::MessageQueued {
                position: 1,
                content: "queued after failure".to_owned(),
                attachments: Vec::new(),
            },
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind-fixture".to_owned(),
                unrestorable_paths: Vec::new(),
            },
        ];
        let events = kinds
            .into_iter()
            .enumerate()
            .map(|(sequence, kind)| {
                wire_event(u64::try_from(sequence).expect("fixture sequence"), kind)
            })
            .collect::<Vec<_>>();
        let recovered = project_session_events(&events).expect("project rewind");
        assert_eq!(
            recovered.conversation,
            vec![committed_user, committed_assistant]
        );
        assert!(recovered.queued_messages.is_empty());
        assert_eq!(recovered.interrupted_turn, None);
        assert_eq!(recovered.turn_ends, BTreeMap::from([(1, 2)]));
        assert_eq!(recovered.completed_turns, 1);
    }

    #[test]
    fn projector_kill_boundaries_never_duplicate_committed_tool_calls_or_results() {
        let user = Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "use tool".to_owned(),
            }],
            meta: TurnMeta::default(),
        };
        let assistant = Turn {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall {
                id: ToolCallId("call".to_owned()),
                name: "fixture".to_owned(),
                args: json!({}),
            }],
            meta: TurnMeta::default(),
        };
        let tool = Turn {
            role: Role::Tool,
            blocks: vec![Block::ToolResult {
                id: ToolCallId("call".to_owned()),
                output: ToolOutput::Text {
                    text: "done".to_owned(),
                },
                is_error: false,
            }],
            meta: TurnMeta::default(),
        };
        let kinds = vec![
            PendingEvent::TurnStarted { turn: 1 },
            PendingEvent::UserMessageAccepted {
                turn: 1,
                content: "use tool".to_owned(),
                attachments: Vec::new(),
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: user,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: assistant,
            },
            PendingEvent::ToolCallStarted {
                turn: 1,
                id: "call".to_owned(),
                name: "fixture".to_owned(),
                arguments: json!({}),
                index: 0,
            },
            PendingEvent::ToolCallFinished {
                turn: 1,
                id: "call".to_owned(),
                output: ToolOutput::Text {
                    text: "done".to_owned(),
                },
                is_error: false,
                index: 0,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: tool,
            },
        ];
        let events = kinds
            .into_iter()
            .enumerate()
            .map(|(sequence, kind)| {
                wire_event(u64::try_from(sequence).expect("fixture sequence"), kind)
            })
            .collect::<Vec<_>>();
        for (length, expected_results) in [(4, 1), (5, 1), (6, 1), (7, 1)] {
            let recovered = project_session_events(&events[..length]).expect("project prefix");
            let calls = recovered
                .conversation
                .iter()
                .flat_map(|turn| &turn.blocks)
                .filter(|block| matches!(block, Block::ToolCall { .. }))
                .count();
            let results = recovered
                .conversation
                .iter()
                .flat_map(|turn| &turn.blocks)
                .filter(|block| matches!(block, Block::ToolResult { .. }))
                .count();
            assert_eq!(calls, 1, "prefix {length}");
            assert_eq!(results, expected_results, "prefix {length}");
        }
    }

    #[tokio::test]
    async fn resume_durably_closes_projected_inflight_turn_before_new_commands() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
            batch_sizes: Mutex::new(Vec::new()),
            tail_floor: Mutex::new(Some(5.into())),
        });
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::default()),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        actor_config.recovered = SessionRecoveredState {
            conversation: vec![Turn {
                role: Role::Assistant,
                blocks: vec![Block::Text {
                    text: "partial".to_owned(),
                }],
                meta: TurnMeta::default(),
            }],
            queued_messages: Vec::new(),
            completed_turns: 0,
            next_turn: 2,
            last_sequence: Some(5.into()),
            interrupted_turn: Some(1),
            turn_ends: BTreeMap::new(),
            ..SessionRecoveredState::default()
        };
        let handle = SessionActor::spawn(actor_config).expect("actor");
        handle.send_message("/status").await.expect("status");
        let persisted = sink.events.lock().expect("sink events");
        assert!(matches!(
            persisted.first().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Interrupted,
                ..
            })
        ));
        assert_eq!(persisted[0].sequence, 6.into());
    }

    #[tokio::test]
    async fn resume_closes_interrupted_tail_then_auto_starts_recovered_queue() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([stop_script("queue resumed", &[])]));
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
            batch_sizes: Mutex::new(Vec::new()),
            tail_floor: Mutex::new(Some(9.into())),
        });
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        actor_config.recovered = SessionRecoveredState {
            conversation: vec![Turn {
                role: Role::Assistant,
                blocks: vec![Block::Text {
                    text: "partial prior answer".to_owned(),
                }],
                meta: TurnMeta::default(),
            }],
            queued_messages: vec!["queued during crash".to_owned()],
            completed_turns: 0,
            next_turn: 2,
            last_sequence: Some(9.into()),
            interrupted_turn: Some(1),
            turn_ends: BTreeMap::new(),
            ..SessionRecoveredState::default()
        };
        let handle = SessionActor::spawn(actor_config).expect("actor");
        timeout(Duration::from_secs(3), async {
            loop {
                if handle.snapshot().await.expect("snapshot").completed_turns == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recovered queue completion");
        assert_eq!(model.request_count(), 1);
        let persisted = sink.events.lock().expect("sink events");
        let kinds = persisted
            .iter()
            .map(|event| &event.kind)
            .collect::<Vec<_>>();
        assert!(matches!(
            kinds.first(),
            Some(PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Interrupted,
                ..
            })
        ));
        assert!(
            kinds
                .iter()
                .any(|kind| matches!(kind, PendingEvent::TurnStarted { turn: 2 }))
        );
        assert!(kinds.iter().any(|kind| matches!(
            kind,
            PendingEvent::UserMessageAccepted { turn: 2, content, .. }
                if content == "queued during crash"
        )));
    }

    #[tokio::test]
    async fn initial_project_instructions_steer_replay_without_entering_committed_history() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(InstructionModel::default());
        let sink = Arc::new(RecordingSink::default());
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.initial_session_context = vec![Turn {
            role: Role::System,
            blocks: vec![Block::Text {
                text: "Root AGENTS.md: reply kennel".to_owned(),
            }],
            meta: TurnMeta::default(),
        }];
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("what word?").await.expect("message");
        collect_turn(&mut events).await;
        assert!(model.observed.load(Ordering::SeqCst));
        let persisted = sink.events.lock().expect("event sink").clone();
        let wire = persisted
            .iter()
            .map(|event| event.wire.clone())
            .collect::<Vec<_>>();
        let recovered = project_session_events(&wire).expect("project persisted events");
        assert!(
            recovered
                .conversation
                .iter()
                .all(|turn| turn.role != Role::System)
        );
        assert_eq!(recovered.conversation.len(), 2);
    }

    #[tokio::test]
    async fn ask_permission_allows_or_denies_without_bypassing_the_gate() {
        for (decision, expected_calls, expected_error) in [
            (ApprovalDecision::AllowOnce, 1, false),
            (ApprovalDecision::Deny, 0, true),
        ] {
            let root = TempDir::new().expect("tempdir");
            let model = Arc::new(ScriptedModel::new([
                tool_script(&[("call", "fixture", json!({"path": "a"}))], &[]),
                stop_script("done", &[]),
            ]));
            let tool = Arc::new(StubTool::new(
                "fixture",
                vec![ToolCapability::WriteFilesystem],
                StubOutcome::Success(ToolResult::new("ok", Value::Null)),
            ));
            let mut tools = ToolRegistry::new();
            tools.register(tool.clone()).expect("register tool");
            let handle = SessionActor::spawn(config(
                root.path(),
                model,
                Arc::new(tools),
                PermissionDecision::Ask,
                builtin_hook_dispatcher().expect("hooks"),
            ))
            .expect("actor");
            let mut events = handle.subscribe();
            handle.send_message("run").await.expect("message");
            let request = next_matching(&mut events, |kind| {
                matches!(kind, PendingEvent::PermissionRequested { .. })
            })
            .await;
            let PendingEvent::PermissionRequested { request, .. } = request.kind else {
                unreachable!("matching event")
            };
            assert!(
                handle
                    .approve(request.id, decision)
                    .await
                    .expect("approval")
            );
            let finished = next_matching(&mut events, |kind| {
                matches!(kind, PendingEvent::ToolCallFinished { .. })
            })
            .await;
            assert!(matches!(
                finished.kind,
                PendingEvent::ToolCallFinished { is_error, .. }
                    if is_error == expected_error
            ));
            next_matching(&mut events, |kind| {
                matches!(kind, PendingEvent::TurnFinished { .. })
            })
            .await;
            assert_eq!(tool.calls.load(Ordering::SeqCst), expected_calls);
        }
    }

    #[tokio::test]
    async fn pre_tool_rewrite_is_the_exact_invocation_presented_for_approval() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(&[("call", "fixture", json!({"path": "original"}))], &[]),
            stop_script("done", &[]),
        ]));
        let tool = Arc::new(StubTool::new(
            "fixture",
            vec![ToolCapability::WriteFilesystem],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(tool.clone()).expect("register tool");
        let mut hooks = builtin_hook_dispatcher().expect("hooks");
        hooks
            .register(
                HookRegistration::new("fixture.rewrite", HookEvent::PreTool),
                RewriteArgumentsHook(json!({"path": "rewritten"})),
            )
            .expect("rewrite hook");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Ask,
            hooks,
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let event = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::PermissionRequested { .. })
        })
        .await;
        let PendingEvent::PermissionRequested { request, .. } = event.kind else {
            unreachable!("matching event")
        };
        assert_eq!(request.arguments, json!({"path": "original"}));
        handle
            .approve(request.id, ApprovalDecision::AllowOnce)
            .await
            .expect("initial approval");
        let event = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::PermissionRequested { .. })
        })
        .await;
        let PendingEvent::PermissionRequested { request, .. } = event.kind else {
            unreachable!("matching event")
        };
        assert_eq!(request.arguments, json!({"path": "rewritten"}));
        handle
            .approve(request.id, ApprovalDecision::AllowOnce)
            .await
            .expect("approval");
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { .. })
        })
        .await;
        assert_eq!(
            tool.inputs.lock().expect("input lock").as_slice(),
            &[json!({"path": "rewritten"})]
        );
    }

    #[tokio::test]
    async fn hook_order_and_fail_open_closed_are_enforced_by_the_turn_loop() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(&[("call", "fixture", json!({}))], &[]),
            stop_script("done", &[]),
        ]));
        let tool = Arc::new(StubTool::new(
            "fixture",
            vec![],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(tool.clone()).expect("register tool");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut hooks = builtin_hook_dispatcher().expect("hooks");
        hooks
            .register(
                HookRegistration::new("fixture.open", HookEvent::PreTool)
                    .with_priority(-10)
                    .with_failure_policy(HookFailurePolicy::FailOpen),
                FixedHook {
                    label: "open",
                    calls: calls.clone(),
                    result: Err(HookError::new("fixture", "open failure")),
                },
            )
            .expect("open hook");
        hooks
            .register(
                HookRegistration::new("fixture.middle", HookEvent::PreTool),
                FixedHook {
                    label: "middle",
                    calls: calls.clone(),
                    result: Ok(HookDirective::Continue),
                },
            )
            .expect("middle hook");
        hooks
            .register(
                HookRegistration::new("fixture.closed", HookEvent::PreTool)
                    .with_priority(10)
                    .with_failure_policy(HookFailurePolicy::FailClosed),
                FixedHook {
                    label: "closed",
                    calls: calls.clone(),
                    result: Err(HookError::new("fixture", "closed failure")),
                },
            )
            .expect("closed hook");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            hooks,
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let events = collect_turn(&mut events).await;
        assert_eq!(
            calls.lock().expect("hook calls").as_slice(),
            &["open", "middle", "closed"]
        );
        let failures = events
            .iter()
            .filter_map(|event| match &event.kind {
                PendingEvent::HookFailure {
                    hook_id,
                    fail_closed,
                    ..
                } => Some((hook_id.as_str(), *fail_closed)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            failures,
            vec![("fixture.open", false), ("fixture.closed", true)]
        );
        assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn session_lifecycle_hooks_run_on_start_and_actor_shutdown() {
        let root = TempDir::new().expect("tempdir");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut hooks = builtin_hook_dispatcher().expect("hooks");
        for (id, event, label) in [
            ("fixture.session-start", HookEvent::SessionStart, "start"),
            ("fixture.session-end", HookEvent::SessionEnd, "end"),
        ] {
            hooks
                .register(
                    HookRegistration::new(id, event)
                        .with_failure_policy(HookFailurePolicy::FailClosed),
                    FixedHook {
                        label,
                        calls: calls.clone(),
                        result: Ok(HookDirective::Continue),
                    },
                )
                .expect("lifecycle hook");
        }
        let handle = SessionActor::spawn(config(
            root.path(),
            Arc::new(ScriptedModel::default()),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            hooks,
        ))
        .expect("actor");
        handle.send_message("/status").await.expect("status");
        assert_eq!(
            calls.lock().expect("lifecycle calls").as_slice(),
            &["start"]
        );
        drop(handle);
        timeout(Duration::from_secs(3), async {
            loop {
                if calls.lock().expect("lifecycle calls").as_slice() == ["start", "end"] {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session end hook");
    }

    #[tokio::test]
    async fn interrupt_cancels_a_hung_session_hook_without_waiting_for_its_deadline() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(RecordingSink::default());
        let mut hooks = builtin_hook_dispatcher().expect("hooks");
        hooks
            .register(
                HookRegistration::new("fixture.never", HookEvent::UserPromptSubmit)
                    .with_failure_policy(HookFailurePolicy::FailClosed),
                NeverHook,
            )
            .expect("hung hook");
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::default()),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            hooks,
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("hang").await.expect("message");
        assert!(handle.interrupt().await.expect("interrupt"));
        let finished = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { .. })
        })
        .await;
        assert!(matches!(
            finished.kind,
            PendingEvent::TurnFinished {
                status: AgentTurnStatus::Interrupted,
                ..
            }
        ));
        assert_eq!(
            sink.batch_sizes.lock().expect("batch sizes").as_slice(),
            &[1, 2, 1]
        );
        assert!(
            !sink
                .events
                .lock()
                .expect("event sink lock")
                .iter()
                .any(|event| matches!(event.kind, PendingEvent::ConversationTurnCommitted { .. }))
        );
    }

    #[tokio::test]
    async fn parallel_tools_finish_reverse_but_emit_results_in_call_index_order() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(
                &[
                    ("first-id", "first", json!({})),
                    ("second-id", "second", json!({})),
                ],
                &[],
            ),
            stop_script("done", &[]),
        ]));
        let release_first = Arc::new(Notify::new());
        let completion_order = Arc::new(Mutex::new(Vec::new()));
        let mut tools = ToolRegistry::new();
        for (name, first) in [("first", true), ("second", false)] {
            tools
                .register(Arc::new(ReverseCompletionTool {
                    descriptor: descriptor(name),
                    first,
                    release_first: release_first.clone(),
                    completion_order: completion_order.clone(),
                }))
                .expect("register tool");
        }
        let sink = Arc::new(RecordingSink::default());
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let events = collect_turn(&mut events).await;
        assert_eq!(
            completion_order
                .lock()
                .expect("completion order")
                .as_slice(),
            &["second", "first"]
        );
        let indices = events
            .iter()
            .filter_map(|event| match event.kind {
                PendingEvent::ToolCallFinished { index, .. } => Some(index),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(indices, vec![0, 1]);
    }

    #[tokio::test]
    async fn empty_manifest_stateful_calls_never_run_in_parallel() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(
                &[
                    ("first-id", "stateful_first", json!({})),
                    ("second-id", "stateful_second", json!({})),
                ],
                &[],
            ),
            stop_script("done", &[]),
        ]));
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let second_started = Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        for (name, first) in [("stateful_first", true), ("stateful_second", false)] {
            tools
                .register(Arc::new(EmptySequentialTool {
                    descriptor: ToolDescriptor {
                        name: name.to_owned(),
                        description: "stateful fixture".to_owned(),
                        input_schema: json!({"type": "object"}),
                        capabilities: CapabilityManifest::default(),
                    },
                    first,
                    first_started: first_started.clone(),
                    release_first: release_first.clone(),
                    second_started: second_started.clone(),
                }))
                .expect("register tool");
        }
        let sink = Arc::new(RecordingSink::default());
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        timeout(Duration::from_secs(3), first_started.notified())
            .await
            .expect("first tool started");
        assert!(!second_started.load(Ordering::SeqCst));
        release_first.notify_one();
        collect_turn(&mut events).await;
        assert!(second_started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn tool_contexts_keep_stateful_data_isolated_by_session_id() {
        let root = TempDir::new().expect("tempdir");
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let tool = Arc::new(SessionCaptureTool {
            sessions: sessions.clone(),
        });
        for session_id in ["session-a", "session-b"] {
            let model = Arc::new(ScriptedModel::new([
                tool_script(&[("capture", "session_capture", json!({}))], &[]),
                stop_script("done", &[]),
            ]));
            let mut tools = ToolRegistry::new();
            tools.register(tool.clone()).expect("register tool");
            let mut actor_config = config(
                root.path(),
                model,
                Arc::new(tools),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            );
            actor_config.session_id = SessionId(session_id.to_owned());
            let handle = SessionActor::spawn(actor_config).expect("actor");
            let mut events = handle.subscribe();
            handle.send_message("capture").await.expect("message");
            collect_turn(&mut events).await;
        }
        assert_eq!(
            sessions.lock().expect("captured sessions").as_slice(),
            &["session-a", "session-b"]
        );
    }

    #[tokio::test]
    async fn earliest_running_tool_streams_before_it_completes() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(&[("stream-id", "stream", json!({}))], &[]),
            stop_script("done", &[]),
        ]));
        let release = Arc::new(Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(StreamingTool {
                descriptor: descriptor("stream"),
                release: release.clone(),
                completed: completed.clone(),
            }))
            .expect("register tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let chunk = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::ToolOutput { .. })
        })
        .await;
        assert!(matches!(
            chunk.kind,
            PendingEvent::ToolOutput { chunk, .. } if chunk == "live chunk"
        ));
        assert!(!completed.load(Ordering::SeqCst));
        release.notify_one();
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { .. })
        })
        .await;
        assert!(completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn mutating_calls_are_sequential_and_checkpointed_before_and_after_each() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(
                &[
                    ("write-1", "write_fixture", json!({"path": "a"})),
                    ("write-2", "write_fixture", json!({"path": "b"})),
                ],
                &[],
            ),
            stop_script("done", &[]),
        ]));
        let tool = Arc::new(StubTool::new(
            "write_fixture",
            vec![ToolCapability::WriteFilesystem],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(tool).expect("register tool");
        let checkpoints = Arc::new(RecordingCheckpoints::default());
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.checkpoints = checkpoints.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("write").await.expect("message");
        collect_turn(&mut events).await;
        assert_eq!(
            checkpoints
                .events
                .lock()
                .expect("checkpoint events")
                .as_slice(),
            &[
                "begin:fixture-session:write-1:OpaqueWorkspace",
                "finish:Some(\"write-1\"):Completed",
                "begin:fixture-session:write-2:OpaqueWorkspace",
                "finish:Some(\"write-2\"):Completed",
            ]
        );
    }

    #[tokio::test]
    async fn interrupt_during_mutating_tool_finishes_checkpoint_and_commits_cancelled_result() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([tool_script(
            &[("stream-id", "stream", json!({}))],
            &[],
        )]));
        let release = Arc::new(Notify::new());
        let completed = Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(StreamingTool {
                descriptor: ToolDescriptor {
                    capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
                    ..descriptor("stream")
                },
                release,
                completed: completed.clone(),
            }))
            .expect("register tool");
        let checkpoints = Arc::new(RecordingCheckpoints::default());
        let sink = Arc::new(RecordingSink::default());
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.checkpoints = checkpoints.clone();
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::ToolOutput { .. })
        })
        .await;
        assert!(handle.interrupt().await.expect("interrupt"));
        let finished = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { .. })
        })
        .await;
        assert!(matches!(
            finished.kind,
            PendingEvent::TurnFinished {
                status: AgentTurnStatus::Interrupted,
                ..
            }
        ));
        assert!(!completed.load(Ordering::SeqCst));
        assert!(
            checkpoints
                .events
                .lock()
                .expect("checkpoint events")
                .iter()
                .any(|event| event.ends_with(":Cancelled"))
        );
        let persisted = sink.events.lock().expect("sink events");
        assert!(persisted.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::ConversationTurnCommitted { turn, .. }
                if turn.role == Role::Tool
        )));
    }

    #[tokio::test]
    async fn interrupt_never_starts_later_tools_in_a_sequential_mutating_batch() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([tool_script(
            &[
                ("first", "first_write", json!({})),
                ("second", "second_write", json!({})),
            ],
            &[],
        )]));
        let second = Arc::new(StubTool::new(
            "second_write",
            vec![ToolCapability::WriteFilesystem],
            StubOutcome::Success(ToolResult::new("must not run", Value::Null)),
        ));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(StreamingTool {
                descriptor: ToolDescriptor {
                    capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
                    ..descriptor("first_write")
                },
                release: Arc::new(Notify::new()),
                completed: Arc::new(AtomicBool::new(false)),
            }))
            .expect("first tool");
        tools.register(second.clone()).expect("second tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::ToolOutput { .. })
        })
        .await;
        assert!(handle.interrupt().await.expect("interrupt"));
        let turn = collect_turn(&mut events).await;
        assert_eq!(second.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            turn.iter()
                .filter(|event| matches!(event.kind, PendingEvent::ToolCallFinished { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn cancellation_waits_for_tool_cleanup_before_result_checkpoint_and_terminal_events() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([tool_script(
            &[("cleanup-id", "cleanup_tool", json!({}))],
            &[],
        )]));
        let cleanup_finished = Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(CleanupTool {
                cleanup_finished: cleanup_finished.clone(),
            }))
            .expect("register cleanup tool");
        let checkpoints = Arc::new(RecordingCheckpoints::default());
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.checkpoints = checkpoints.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut receiver = handle.subscribe();
        handle.send_message("run").await.expect("message");
        next_matching(&mut receiver, |kind| {
            matches!(kind, PendingEvent::ToolCallStarted { .. })
        })
        .await;
        assert!(handle.interrupt().await.expect("interrupt"));
        let events = collect_turn(&mut receiver).await;
        assert!(cleanup_finished.load(Ordering::SeqCst));
        let cleanup_index = events
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    PendingEvent::ToolOutput { chunk, .. } if chunk == "cleanup complete"
                )
            })
            .expect("cleanup output");
        let result_index = events
            .iter()
            .position(|event| matches!(event.kind, PendingEvent::ToolCallFinished { .. }))
            .expect("tool result");
        let terminal_index = events
            .iter()
            .position(|event| matches!(event.kind, PendingEvent::TurnFinished { .. }))
            .expect("terminal event");
        assert!(cleanup_index < result_index && result_index < terminal_index);
        assert!(
            checkpoints
                .events
                .lock()
                .expect("checkpoint events")
                .last()
                .is_some_and(|event| event.ends_with(":Cancelled"))
        );
        assert!(
            timeout(Duration::from_millis(50), receiver.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn panicking_mutating_tool_is_failed_checkpointed_and_actor_remains_usable() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(&[("panic-id", "panic_tool", json!({}))], &[]),
            stop_script("recovered after panic", &[]),
            stop_script("next turn works", &[]),
        ]));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(PanickingTool))
            .expect("register panic tool");
        let checkpoints = Arc::new(RecordingCheckpoints::default());
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.checkpoints = checkpoints.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut receiver = handle.subscribe();
        handle
            .send_message("panic once")
            .await
            .expect("first message");
        let first = collect_turn(&mut receiver).await;
        assert!(first.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::ToolCallFinished {
                output: ToolOutput::Text { text },
                is_error: true,
                ..
            } if text.contains("panicked")
        )));
        assert!(
            checkpoints
                .events
                .lock()
                .expect("checkpoint events")
                .iter()
                .any(|event| event.ends_with(":Failed"))
        );
        assert_eq!(
            handle
                .send_message("still alive")
                .await
                .expect("second message"),
            MessageDisposition::Started
        );
        let second = collect_turn(&mut receiver).await;
        assert!(matches!(
            second.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::Completed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn queued_message_starts_after_a_well_formed_interrupted_turn() {
        let root = TempDir::new().expect("tempdir");
        let handle = SessionActor::spawn(config(
            root.path(),
            Arc::new(PendingModel),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        assert_eq!(
            handle.send_message("first").await.expect("first"),
            MessageDisposition::Started
        );
        assert_eq!(
            handle.send_message("second").await.expect("second"),
            MessageDisposition::Queued
        );
        assert!(handle.interrupt().await.expect("interrupt"));
        let first_finished = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { turn: 1, .. })
        })
        .await;
        assert!(matches!(
            first_finished.kind,
            PendingEvent::TurnFinished {
                status: AgentTurnStatus::Interrupted,
                ..
            }
        ));
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnStarted { turn: 2 })
        })
        .await;
        let (respond, receive) = oneshot::channel();
        handle
            .commands
            .send(ActorCommand::Interrupt {
                target_turn: 1,
                respond,
            })
            .await
            .expect("stale interrupt command");
        assert!(!receive.await.expect("stale interrupt response"));
        assert!(handle.snapshot().await.expect("snapshot").running);
        assert!(handle.interrupt().await.expect("cleanup interrupt"));
    }

    #[tokio::test]
    async fn provider_error_preserves_partial_output_and_emits_failed_terminal() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([vec![
            Ok(ProviderEvent::MessageStart {
                model: "fixture-model".to_owned(),
            }),
            Ok(ProviderEvent::TextDelta {
                text: "partial".to_owned(),
            }),
            Err(ProviderError::new(
                ProviderErrorKind::Network,
                "fixture stream failed",
            )),
        ]]));
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let events = collect_turn(&mut events).await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::TextDelta { text, .. } if text == "partial"
        )));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::Failed,
                ..
            })
        ));
        let snapshot = handle.snapshot().await.expect("snapshot");
        assert!(snapshot.conversation.iter().any(|turn| {
            turn.blocks
                .iter()
                .any(|block| matches!(block, Block::Text { text } if text == "partial"))
        }));
    }

    #[tokio::test]
    async fn usage_accumulates_latest_totals_once_per_provider_iteration() {
        let root = TempDir::new().expect("tempdir");
        let first_latest = TokenUsage {
            input_tokens: 7,
            output_tokens: 3,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
            reasoning_tokens: 4,
        };
        let second = TokenUsage {
            input_tokens: 11,
            output_tokens: 5,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
            reasoning_tokens: 6,
        };
        let model = Arc::new(ScriptedModel::new([
            tool_script(
                &[("call", "fixture", json!({}))],
                &[
                    TokenUsage {
                        input_tokens: 2,
                        ..TokenUsage::default()
                    },
                    first_latest,
                ],
            ),
            stop_script("done", &[second]),
        ]));
        let tool = Arc::new(StubTool::new(
            "fixture",
            vec![],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(tool).expect("register tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let finished = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { .. })
        })
        .await;
        assert!(matches!(
            finished.kind,
            PendingEvent::TurnFinished {
                usage: SessionUsage {
                    input_tokens: 18,
                    output_tokens: 8,
                    cache_read_tokens: 5,
                    cache_write_tokens: 3,
                    reasoning_tokens: 10,
                },
                ..
            }
        ));
    }

    #[test]
    fn usage_counters_round_trip_as_js_safe_decimal_strings() {
        let usage = SessionUsage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX - 1,
            cache_read_tokens: u64::MAX - 2,
            cache_write_tokens: u64::MAX - 3,
            reasoning_tokens: u64::MAX - 4,
        };
        let encoded = serde_json::to_string(&usage).expect("serialize usage");
        assert!(encoded.contains("\"input_tokens\":\"18446744073709551615\""));
        let decoded: SessionUsage = serde_json::from_str(&encoded).expect("deserialize usage");
        assert_eq!(decoded, usage);
    }

    #[derive(Default)]
    struct ToggleLeaseSink {
        events: Mutex<Vec<EngineEvent>>,
        fail_driver_change: AtomicBool,
        fail_question_answer: AtomicBool,
    }

    struct CorruptGapSink {
        event: EngineEvent,
    }

    #[async_trait]
    impl SessionEventSink for CorruptGapSink {
        async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            Ok(event)
        }

        async fn read_after(
            &self,
            _last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            Ok(vec![self.event.clone()])
        }
    }

    #[async_trait]
    impl SessionEventSink for ToggleLeaseSink {
        async fn append(&self, event: EngineEvent) -> Result<EngineEvent, AgentLoopError> {
            if self.fail_driver_change.load(Ordering::SeqCst)
                && matches!(event, EngineEvent::DriverChanged { .. })
            {
                return Err(AgentLoopError::Persistence(
                    "fixture driver-change failure".to_owned(),
                ));
            }
            if self.fail_question_answer.load(Ordering::SeqCst)
                && matches!(event, EngineEvent::QuestionAnswered { .. })
            {
                return Err(AgentLoopError::Persistence(
                    "fixture question-answer failure".to_owned(),
                ));
            }
            self.events.lock().expect("events").push(event.clone());
            Ok(event)
        }

        async fn read_after(
            &self,
            last_seen: Option<SequenceId>,
        ) -> Result<Vec<EngineEvent>, AgentLoopError> {
            let first = last_seen.map_or(0, |sequence| sequence.0.saturating_add(1));
            Ok(self
                .events
                .lock()
                .expect("events")
                .iter()
                .filter(|event| event.meta().is_some_and(|meta| meta.sequence_id.0 >= first))
                .cloned()
                .collect())
        }
    }

    struct PanicQuestionAsker;

    #[async_trait]
    impl QuestionAsker for PanicQuestionAsker {
        async fn ask(
            &self,
            _request: AskUserInput,
            _cancellation: CancellationToken,
        ) -> Result<String, ToolError> {
            panic!("engine protocol asker must override the tool fallback")
        }
    }

    struct FloodOutputTool;

    #[async_trait]
    impl Tool for FloodOutputTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "flood".to_owned(),
                description: "emit more live chunks than the bounded stream permits".to_owned(),
                input_schema: json!({"type": "object"}),
                capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
            }
        }

        async fn execute(
            &self,
            context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            for _ in 0..1_100 {
                context
                    .output
                    .emit(ToolOutputChunk {
                        stream: ToolOutputStream::Stdout,
                        content: "x".to_owned(),
                    })
                    .await?;
            }
            Ok(ToolResult::new("drained", Value::Null))
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn protocol_ack_lease_observer_and_takeover_are_one_durable_event_stream() {
        let root = TempDir::new().expect("tempdir");
        let handle = SessionActor::spawn(config(
            root.path(),
            Arc::new(PendingModel),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let session_id = SessionId("fixture-session".to_owned());
        let mut driver_events = handle.subscribe_client(ClientId("driver".to_owned()), None);
        assert_eq!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta("driver", "attach-driver"),
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("attach"),
            CommandOutcome::Accepted
        );
        let created = driver_events.recv().await.expect("session created");
        assert!(matches!(
            &created,
            EngineEvent::SessionCreated {
                meta: EventMeta {
                    caused_by: Some(RequestId(request)),
                    emitted_at,
                    ..
                },
                driver_client_id: ClientId(driver),
            } if request == "attach-driver"
                && emitted_at == "2026-01-02T03:04:05.006Z"
                && driver == "driver"
        ));
        assert!(matches!(
            driver_events.recv().await.expect("attach ack"),
            EngineEvent::CommandAcknowledged {
                meta: CommandAckMeta { emitted_at, .. },
                outcome: CommandOutcome::Accepted,
                ..
            } if emitted_at == "2026-01-02T03:04:05.006Z"
        ));

        let mut observer_events = handle.subscribe_client(ClientId("observer".to_owned()), None);
        assert_eq!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta("observer", "attach-observer"),
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Observer,
                })
                .await
                .expect("observer attach"),
            CommandOutcome::Accepted
        );
        assert!(matches!(
            observer_events.recv().await.expect("observer durable gap"),
            EngineEvent::SessionCreated { .. }
        ));
        assert!(matches!(
            observer_events.recv().await.expect("observer attach ack"),
            EngineEvent::CommandAcknowledged { .. }
        ));
        assert!(matches!(
            handle
                .dispatch(ClientCommand::SendMessage {
                    meta: protocol_meta("observer", "observer-mutation"),
                    session_id: session_id.clone(),
                    content: "must reject".to_owned(),
                    attachments: Vec::new(),
                })
                .await
                .expect("observer rejection"),
            CommandOutcome::Rejected { .. }
        ));
        assert_eq!(
            handle
                .dispatch(ClientCommand::TakeDriver {
                    meta: protocol_meta("observer", "take-driver"),
                    session_id: session_id.clone(),
                })
                .await
                .expect("take driver"),
            CommandOutcome::Accepted
        );
        let changed = loop {
            let event = observer_events.recv().await.expect("driver changed");
            if matches!(event, EngineEvent::DriverChanged { .. }) {
                break event;
            }
        };
        assert!(matches!(
            changed,
            EngineEvent::DriverChanged {
                meta: EventMeta {
                    caused_by: Some(RequestId(ref request)),
                    ..
                },
                driver_client_id: ClientId(ref driver),
            } if request == "take-driver" && driver == "observer"
        ));
        assert!(matches!(
            driver_events.recv().await.expect("old driver notification"),
            EngineEvent::DriverChanged { .. }
        ));
    }

    #[tokio::test]
    async fn lagged_subscription_replays_every_durable_sequence_and_continues_live() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(RecordingSink::default());
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::new([stop_script("many events", &[])])),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        actor_config.event_capacity = 1;
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let session_id = SessionId("fixture-session".to_owned());
        let mut events = handle.subscribe_client(ClientId("driver".to_owned()), None);
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach");
        assert!(matches!(
            events.recv().await.expect("created"),
            EngineEvent::SessionCreated { .. }
        ));
        assert!(matches!(
            events.recv().await.expect("attach ack"),
            EngineEvent::CommandAcknowledged { .. }
        ));
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "send"),
                session_id: session_id.clone(),
                content: "run".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        timeout(Duration::from_secs(1), async {
            loop {
                if handle.snapshot().await.expect("snapshot").completed_turns == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("turn completion");
        let durable_tail = sink
            .events
            .lock()
            .expect("events")
            .last()
            .expect("durable tail")
            .sequence;
        let mut replayed = Vec::new();
        while replayed.last().copied() != Some(durable_tail) {
            let event = events.recv().await.expect("gap event");
            if let Some(meta) = event.meta() {
                replayed.push(meta.sequence_id);
            }
        }
        assert_eq!(
            replayed,
            (1..=durable_tail.0).map(SequenceId).collect::<Vec<_>>()
        );
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "status"),
                session_id,
                content: "/status".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("status");
        loop {
            let event = events.recv().await.expect("continued live event");
            if let EngineEvent::CommandFinished { meta, name, .. } = event {
                assert_eq!(name, "status");
                assert_eq!(meta.sequence_id.0, durable_tail.0.saturating_add(1));
                break;
            }
        }
    }

    #[tokio::test]
    async fn attach_and_subscription_reject_wrong_session_or_protocol_gap_events() {
        for corrupt in ["session", "protocol"] {
            let root = TempDir::new().expect("tempdir");
            let mut event = wire_event(
                0,
                PendingEvent::SessionCreated {
                    driver_client_id: ClientId("prior".to_owned()),
                },
            );
            let meta = event.meta_mut().expect("meta");
            if corrupt == "session" {
                meta.session_id = SessionId("other-session".to_owned());
            } else {
                meta.protocol_version = PROTOCOL_VERSION.saturating_add(1);
            }
            let mut actor_config = config(
                root.path(),
                Arc::new(ScriptedModel::default()),
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            );
            actor_config.event_sink = Arc::new(CorruptGapSink { event });
            let handle = SessionActor::spawn(actor_config).expect("actor");
            let mut subscription = handle.subscribe_client(ClientId("driver".to_owned()), None);
            assert!(matches!(
                handle
                    .dispatch(ClientCommand::AttachSession {
                        meta: protocol_meta("driver", "attach"),
                        session_id: SessionId("fixture-session".to_owned()),
                        last_seen_sequence: None,
                        role: ClientRole::Driver,
                    })
                    .await
                    .expect("attach outcome"),
                CommandOutcome::Rejected { .. }
            ));
            assert!(matches!(
                subscription.recv().await,
                Err(AgentLoopError::Persistence(_))
            ));
        }
    }

    #[tokio::test]
    async fn failed_takeover_does_not_mutate_the_driver_lease() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(ToggleLeaseSink::default());
        let mut actor_config = config(
            root.path(),
            Arc::new(PendingModel),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let session_id = SessionId("fixture-session".to_owned());
        assert!(matches!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta("first", "first-attach"),
                    session_id: session_id.clone(),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("first attach"),
            CommandOutcome::Accepted
        ));
        assert!(matches!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta("second", "second-attach"),
                    session_id: session_id.clone(),
                    last_seen_sequence: Some(0.into()),
                    role: ClientRole::Observer,
                })
                .await
                .expect("second attach"),
            CommandOutcome::Accepted
        ));
        sink.fail_driver_change.store(true, Ordering::SeqCst);
        assert!(matches!(
            handle
                .dispatch(ClientCommand::TakeDriver {
                    meta: protocol_meta("second", "failed-takeover"),
                    session_id: session_id.clone(),
                })
                .await
                .expect("takeover outcome"),
            CommandOutcome::Rejected { .. }
        ));
        assert!(matches!(
            handle
                .dispatch(ClientCommand::SendMessage {
                    meta: protocol_meta("second", "still-observer"),
                    session_id,
                    content: "must reject".to_owned(),
                    attachments: Vec::new(),
                })
                .await
                .expect("observer outcome"),
            CommandOutcome::Rejected { .. }
        ));
    }

    #[tokio::test]
    async fn invalid_protocol_rewind_is_rejected_without_poisoning_the_session() {
        let root = TempDir::new().expect("tempdir");
        let handle = SessionActor::spawn(config(
            root.path(),
            Arc::new(ScriptedModel::new([stop_script("healthy", &[])])),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let session_id = SessionId("fixture-session".to_owned());
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach");
        assert!(matches!(
            handle
                .dispatch(ClientCommand::Rewind {
                    meta: protocol_meta("driver", "bad-rewind"),
                    session_id: session_id.clone(),
                    target: RewindTarget::Turn {
                        turn_id: TurnId("999".to_owned()),
                    },
                })
                .await
                .expect("rewind outcome"),
            CommandOutcome::Rejected { .. }
        ));
        assert_eq!(
            handle
                .dispatch(ClientCommand::SendMessage {
                    meta: protocol_meta("driver", "healthy-message"),
                    session_id,
                    content: "continue".to_owned(),
                    attachments: Vec::new(),
                })
                .await
                .expect("healthy command"),
            CommandOutcome::Accepted
        );
        timeout(Duration::from_secs(1), async {
            loop {
                if handle.snapshot().await.expect("snapshot").completed_turns == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("healthy turn completion");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn ask_user_is_persisted_and_answered_only_through_client_command() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(
                &[(
                    "question-call",
                    "ask_user",
                    json!({"question": "Continue?", "options": ["yes", "no"]}),
                )],
                &[],
            ),
            stop_script("done", &[]),
        ]));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(AskUserTool::new(
                Arc::new(PanicQuestionAsker),
                ToolLimits::default(),
            )))
            .expect("ask tool");
        let sink = Arc::new(RecordingSink::default());
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe_client(ClientId("driver".to_owned()), None);
        let session_id = SessionId("fixture-session".to_owned());
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach");
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "send-question"),
                session_id: session_id.clone(),
                content: "ask".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        let question_id = loop {
            if let EngineEvent::QuestionAsked {
                meta,
                question_id,
                questions,
                ..
            } = events.recv().await.expect("question event")
            {
                assert_eq!(meta.caused_by, Some(RequestId("send-question".to_owned())));
                assert_eq!(questions[0].prompt, "Continue?");
                break question_id;
            }
        };
        let asked_prefix = sink
            .events
            .lock()
            .expect("events")
            .iter()
            .map(|event| event.wire.clone())
            .collect::<Vec<_>>();
        let asked_projection =
            project_session_events(&asked_prefix).expect("project asked question");
        assert!(
            asked_projection
                .pending_questions
                .contains_key(&question_id.0)
        );
        assert_eq!(
            handle
                .dispatch(ClientCommand::AnswerQuestion {
                    meta: protocol_meta("driver", "answer"),
                    session_id,
                    question_id: question_id.clone(),
                    answers: vec![Answer {
                        question_id,
                        values: vec!["yes".to_owned()],
                    }],
                })
                .await
                .expect("answer"),
            CommandOutcome::Accepted
        );
        let mut durable_answer = false;
        loop {
            let event = events.recv().await.expect("terminal event");
            if let EngineEvent::QuestionAnswered { meta, answers, .. } = &event {
                assert_eq!(meta.caused_by, Some(RequestId("answer".to_owned())));
                assert_eq!(answers[0].values, ["yes"]);
                durable_answer = true;
            }
            if matches!(event, EngineEvent::TurnFinished { .. }) {
                break;
            }
        }
        assert!(durable_answer);
        let answered_log = sink
            .events
            .lock()
            .expect("events")
            .iter()
            .map(|event| event.wire.clone())
            .collect::<Vec<_>>();
        assert!(
            project_session_events(&answered_log)
                .expect("project answered question")
                .pending_questions
                .is_empty()
        );
        let snapshot = handle.snapshot().await.expect("snapshot");
        assert!(snapshot.conversation.iter().any(|turn| {
            turn.blocks.iter().any(|block| {
                matches!(
                    block,
                    Block::ToolResult {
                        output: ToolOutput::Mixed { parts },
                        ..
                    } if parts.iter().any(|part| matches!(
                        part,
                        ToolOutputPart::Text { text } if text == "yes"
                    ))
                )
            })
        }));
    }

    #[tokio::test]
    async fn question_answer_persistence_failure_rejects_ack_and_stops_tool_continuation() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([tool_script(
            &[(
                "question-call",
                "ask_user",
                json!({"question": "Continue?", "options": ["yes", "no"]}),
            )],
            &[],
        )]));
        let sink = Arc::new(ToggleLeaseSink::default());
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(AskUserTool::new(
                Arc::new(PanicQuestionAsker),
                ToolLimits::default(),
            )))
            .expect("ask tool");
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let session_id = SessionId("fixture-session".to_owned());
        let mut events = handle.subscribe_client(ClientId("driver".to_owned()), None);
        handle
            .dispatch(ClientCommand::AttachSession {
                meta: protocol_meta("driver", "attach"),
                session_id: session_id.clone(),
                last_seen_sequence: None,
                role: ClientRole::Driver,
            })
            .await
            .expect("attach");
        handle
            .dispatch(ClientCommand::SendMessage {
                meta: protocol_meta("driver", "send"),
                session_id: session_id.clone(),
                content: "ask".to_owned(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        let question_id = loop {
            if let EngineEvent::QuestionAsked { question_id, .. } =
                events.recv().await.expect("question")
            {
                break question_id;
            }
        };
        sink.fail_question_answer.store(true, Ordering::SeqCst);
        assert!(matches!(
            handle
                .dispatch(ClientCommand::AnswerQuestion {
                    meta: protocol_meta("driver", "failed-answer"),
                    session_id,
                    question_id: question_id.clone(),
                    answers: vec![Answer {
                        question_id,
                        values: vec!["yes".to_owned()],
                    }],
                })
                .await
                .expect("answer outcome"),
            CommandOutcome::Rejected { .. }
        ));
        assert!(
            sink.events
                .lock()
                .expect("events")
                .iter()
                .all(|event| !matches!(event, EngineEvent::QuestionAnswered { .. }))
        );
        timeout(Duration::from_secs(1), async {
            loop {
                if !handle.snapshot().await.expect("snapshot").running {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled question turn");
        assert_eq!(model.request_count(), 1);
    }

    #[tokio::test]
    async fn bounded_live_output_drains_excess_chunks_and_finishes() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(&[("flood-call", "flood", json!({}))], &[]),
            stop_script("done", &[]),
        ]));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(FloodOutputTool))
            .expect("flood tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("flood").await.expect("message");
        let turn = timeout(Duration::from_secs(3), collect_turn(&mut events))
            .await
            .expect("flood turn must not hang");
        let chunks = turn
            .iter()
            .filter_map(|event| match &event.kind {
                PendingEvent::ToolOutput { chunk, .. } => Some(chunk),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(chunks.len() <= MAX_LIVE_TOOL_OUTPUT_CHUNKS.saturating_add(1));
        assert!(chunks.iter().any(|chunk| chunk.contains("truncated")));
    }

    #[tokio::test]
    async fn resume_persists_tool_result_repairs_before_interrupted_closure() {
        let root = TempDir::new().expect("tempdir");
        let original = vec![
            wire_event(0, PendingEvent::TurnStarted { turn: 1 }),
            wire_event(
                1,
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: 1,
                    turn: Turn {
                        role: Role::Assistant,
                        blocks: vec![Block::ToolCall {
                            id: ToolCallId("lost-call".to_owned()),
                            name: "fixture".to_owned(),
                            args: json!({}),
                        }],
                        meta: TurnMeta::default(),
                    },
                },
            ),
        ];
        let recovered = project_session_events(&original).expect("project kill tail");
        assert_eq!(recovered.interrupted_tool_repairs.len(), 1);
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
            batch_sizes: Mutex::new(Vec::new()),
            tail_floor: Mutex::new(Some(1.into())),
        });
        let mut actor_config = config(
            root.path(),
            Arc::new(ScriptedModel::default()),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.recovered = recovered;
        actor_config.event_sink = sink.clone();
        let _handle = SessionActor::spawn(actor_config).expect("actor");
        timeout(Duration::from_secs(1), async {
            loop {
                if sink.events.lock().expect("events").len() >= 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("durable recovery closure");
        let repairs = sink.events.lock().expect("events").clone();
        assert!(matches!(
            repairs[0].kind,
            PendingEvent::ToolCallFinished {
                ref id,
                is_error: true,
                ..
            } if id == "lost-call"
        ));
        assert!(matches!(
            repairs[1].kind,
            PendingEvent::ConversationTurnCommitted {
                turn: Turn {
                    role: Role::Tool,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            repairs[2].kind,
            PendingEvent::TurnFinished {
                status: AgentTurnStatus::Interrupted,
                ..
            }
        ));
        let mut durable = original;
        durable.extend(repairs.into_iter().map(|event| event.wire));
        let projected = project_session_events(&durable).expect("project repaired log");
        assert_eq!(projected.interrupted_turn, None);
        assert_eq!(
            projected
                .conversation
                .iter()
                .flat_map(|turn| &turn.blocks)
                .filter(|block| matches!(block, Block::ToolResult { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn identical_failures_and_max_turns_stop_deterministically() {
        let root = TempDir::new().expect("tempdir");
        let repeated = (0..5)
            .map(|index| {
                tool_script(
                    &[(&format!("call-{index}"), "failing", json!({"same": true}))],
                    &[],
                )
            })
            .collect::<Vec<_>>();
        let doom_model = Arc::new(ScriptedModel::new(repeated));
        let failing = Arc::new(StubTool::new(
            "failing",
            vec![],
            StubOutcome::Failure("same failure".to_owned()),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(failing).expect("register tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            doom_model.clone(),
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let events = collect_turn(&mut events).await;
        assert_eq!(doom_model.request_count(), 5);
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::GuardTriggered { guard, .. }
                if guard == "identical_tool_failure"
        )));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::DoomLoop,
                ..
            })
        ));

        let root = TempDir::new().expect("tempdir");
        let max_model = Arc::new(ScriptedModel::new((0..2).map(|index| {
            tool_script(
                &[(&format!("call-{index}"), "ok", json!({"index": index}))],
                &[],
            )
        })));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(StubTool::new(
                "ok",
                vec![],
                StubOutcome::Success(ToolResult::new("ok", Value::Null)),
            )))
            .expect("register tool");
        let mut actor_config = config(
            root.path(),
            max_model.clone(),
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.max_turns = 2;
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let events = collect_turn(&mut events).await;
        assert_eq!(max_model.request_count(), 2);
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::GuardTriggered { guard, .. } if guard == "max_turns"
        )));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::MaxTurns,
                ..
            })
        ));
    }

    fn text_turn(role: Role, text: impl Into<String>) -> Turn {
        Turn {
            role,
            blocks: vec![Block::Text { text: text.into() }],
            meta: TurnMeta::default(),
        }
    }

    #[test]
    fn compaction_projection_is_atomic_across_crash_boundaries() {
        let old = text_turn(Role::User, "old history");
        let summary = rw_context::summary_turn("summary");
        let unfinished = vec![
            wire_event(
                0,
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: 1,
                    turn: old.clone(),
                },
            ),
            wire_event(
                1,
                PendingEvent::CompactionStarted {
                    reason: CompactionReason::Manual,
                },
            ),
            wire_event(
                2,
                PendingEvent::ConversationTurnCommitted {
                    agent_turn: 2,
                    turn: summary.clone(),
                },
            ),
        ];
        let recovered =
            project_session_events(&unfinished).expect("unfinished compaction projects");
        assert_eq!(recovered.conversation, vec![old.clone()]);
        assert!(recovered.interrupted_compaction);

        let mut finished = unfinished.clone();
        finished.push(wire_event(
            3,
            PendingEvent::CompactionFinished {
                summary_turn: 2,
                reclaimed_tokens: 100,
                usage: None,
                cost: None,
            },
        ));
        let recovered = project_session_events(&finished).expect("finished compaction projects");
        assert_eq!(recovered.conversation, vec![summary]);
        assert!(!recovered.interrupted_compaction);

        let later = text_turn(Role::User, "later after recovery");
        let mut aborted = unfinished;
        aborted.push(wire_event(
            3,
            PendingEvent::Error {
                message: "interrupted compaction was aborted during recovery".to_owned(),
            },
        ));
        aborted.push(wire_event(
            4,
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 3,
                turn: later.clone(),
            },
        ));
        aborted.push(wire_event(
            5,
            PendingEvent::TurnFinished {
                turn: 3,
                status: AgentTurnStatus::Completed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
            },
        ));
        let first_resume = project_session_events(&aborted).expect("first resume");
        let second_resume = project_session_events(&aborted).expect("second resume");
        assert_eq!(first_resume.conversation, vec![old.clone(), later.clone()]);
        assert_eq!(second_resume.conversation, vec![old, later]);
        assert!(!second_resume.interrupted_compaction);
    }

    #[test]
    fn projector_rewind_before_multiple_compactions_restores_original_history() {
        let original_user = text_turn(Role::User, "original request");
        let original_assistant = text_turn(Role::Assistant, "original answer");
        let first_summary = rw_context::summary_turn("first summary");
        let later_user = text_turn(Role::User, "later request");
        let second_summary = rw_context::summary_turn("second summary");
        let kinds = vec![
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: original_user.clone(),
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 1,
                turn: original_assistant.clone(),
            },
            PendingEvent::TurnFinished {
                turn: 1,
                status: AgentTurnStatus::Completed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
            },
            PendingEvent::CompactionStarted {
                reason: CompactionReason::Manual,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: first_summary,
            },
            PendingEvent::CompactionFinished {
                summary_turn: 2,
                reclaimed_tokens: 100,
                usage: None,
                cost: None,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 2,
                turn: later_user,
            },
            PendingEvent::TurnFinished {
                turn: 2,
                status: AgentTurnStatus::Completed,
                usage: SessionUsage::default(),
                cost: unavailable_cost(),
            },
            PendingEvent::CompactionStarted {
                reason: CompactionReason::Automatic,
            },
            PendingEvent::ConversationTurnCommitted {
                agent_turn: 3,
                turn: second_summary,
            },
            PendingEvent::CompactionFinished {
                summary_turn: 3,
                reclaimed_tokens: 100,
                usage: None,
                cost: None,
            },
            PendingEvent::ConversationRewound {
                to_turn: 1,
                operation_id: "rewind-before-first-compaction".to_owned(),
                unrestorable_paths: Vec::new(),
            },
        ];
        let events = kinds
            .into_iter()
            .enumerate()
            .map(|(sequence, kind)| {
                wire_event(u64::try_from(sequence).expect("fixture sequence"), kind)
            })
            .collect::<Vec<_>>();

        let recovered = project_session_events(&events).expect("project rewind after compactions");
        assert_eq!(
            recovered.conversation,
            vec![original_user, original_assistant]
        );
        assert_eq!(recovered.turn_ends, BTreeMap::from([(1, 2)]));
        assert_eq!(recovered.completed_turns, 1);
    }

    #[tokio::test]
    async fn actor_durably_aborts_interrupted_compaction_before_accepting_new_work() {
        for reason in [CompactionReason::Manual, CompactionReason::Automatic] {
            let root = TempDir::new().expect("tempdir");
            let old = text_turn(Role::User, format!("old history for {reason:?}"));
            let unfinished_summary = rw_context::summary_turn("must stay uncommitted");
            let durable_prefix = vec![
                wire_event(
                    0,
                    PendingEvent::ConversationTurnCommitted {
                        agent_turn: 1,
                        turn: old.clone(),
                    },
                ),
                wire_event(1, PendingEvent::CompactionStarted { reason }),
                wire_event(
                    2,
                    PendingEvent::ConversationTurnCommitted {
                        agent_turn: 2,
                        turn: unfinished_summary,
                    },
                ),
            ];
            let recovered = project_session_events(&durable_prefix).expect("recover prefix");
            assert!(recovered.interrupted_compaction);
            let sink = Arc::new(RecordingSink {
                events: Mutex::new(
                    durable_prefix
                        .into_iter()
                        .map(|event| observe_event(event).expect("durable prefix event"))
                        .collect(),
                ),
                batch_sizes: Mutex::new(Vec::new()),
                tail_floor: Mutex::new(None),
            });
            let model = Arc::new(ScriptedModel::new([stop_script("new answer", &[])]));
            let mut actor_config = config(
                root.path(),
                model,
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            );
            actor_config.event_sink = sink.clone();
            actor_config.recovered = recovered;

            let handle = SessionActor::spawn(actor_config).expect("actor");
            timeout(Duration::from_secs(3), async {
                loop {
                    let abort_persisted = sink.events.lock().expect("sink lock").iter().any(
                        |event| {
                            matches!(
                                &event.kind,
                                PendingEvent::Error { message }
                                    if message == "interrupted compaction was aborted during recovery"
                            )
                        },
                    );
                    if abort_persisted {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("recovery abort must be persisted before commands are accepted");

            let mut subscription = handle.subscribe();
            handle
                .send_message("later after recovery")
                .await
                .expect("later turn");
            collect_turn(&mut subscription).await;

            let durable_log = sink
                .events
                .lock()
                .expect("sink lock")
                .iter()
                .map(|event| event.wire.clone())
                .collect::<Vec<_>>();
            let first = project_session_events(&durable_log).expect("first reconstruction");
            let second = project_session_events(&durable_log).expect("second reconstruction");
            let mut assistant = text_turn(Role::Assistant, "new answer");
            assistant.meta.model = Some("fixture-model".to_owned());
            let expected = vec![
                old,
                text_turn(Role::User, "later after recovery"),
                assistant,
            ];
            assert_eq!(first.conversation, expected);
            assert_eq!(second.conversation, expected);
            assert!(!first.interrupted_compaction);
            assert!(!second.interrupted_compaction);
        }
    }

    #[tokio::test]
    async fn context_queries_and_surgery_are_offline_and_actor_consistent() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::default());
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.recovered.conversation = vec![text_turn(Role::User, "inspect me")];
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let snapshot = handle.context_snapshot().await.expect("context snapshot");
        assert_eq!(snapshot.items.len(), 1);
        let item_id = snapshot.items[0].item_id.clone();
        handle.pin_context(item_id.clone()).await.expect("pin");
        let pinned = handle.context_snapshot().await.expect("pinned snapshot");
        assert!(pinned.items[0].state.pinned);
        handle.evict_context(item_id).await.expect("evict");
        let evicted = handle.context_snapshot().await.expect("evicted snapshot");
        assert!(evicted.items[0].state.evicted);
        let dump = handle.dump_prompt(None).await.expect("offline prompt dump");
        assert!(dump.turns.is_empty());
        assert_eq!(model.request_count(), 0);
    }

    #[tokio::test]
    async fn context_inventory_exposes_tools_and_rejects_protected_item_surgery() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::default());
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(StubTool::new(
                "inspect",
                vec![],
                StubOutcome::Success(ToolResult::new("unused", Value::Null)),
            )))
            .expect("register tool");
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.initial_session_context = vec![text_turn(Role::System, "protected policy")];
        actor_config.recovered.conversation = vec![Turn {
            role: Role::Tool,
            blocks: vec![
                Block::ToolResult {
                    id: ToolCallId("call-inspect".to_owned()),
                    output: ToolOutput::Structured {
                        value: json!({"answer": 42}),
                    },
                    is_error: false,
                },
                Block::ToolResult {
                    id: ToolCallId("call-second".to_owned()),
                    output: ToolOutput::Text {
                        text: "second result".to_owned(),
                    },
                    is_error: false,
                },
            ],
            meta: TurnMeta::default(),
        }];
        actor_config.recovered.context_surgery = vec![ContextSurgeryAction {
            item_id: ContextItemId("conversation:0".to_owned()),
            pinned: true,
            effective_after_agent_turn: 0,
        }];
        actor_config
            .recovered
            .pruned_tool_outputs
            .insert("call-inspect".to_owned(), 100);
        let handle = SessionActor::spawn(actor_config).expect("actor");

        let snapshot = handle.context_snapshot().await.expect("context snapshot");
        let system = snapshot
            .items
            .iter()
            .find(|item| item.kind == ContextItemKind::System)
            .expect("system inventory item");
        assert!(snapshot.items.iter().any(|item| {
            item.item_id.0 == "tool:inspect" && item.kind == ContextItemKind::ToolDefinitions
        }));
        let tool_results = snapshot
            .items
            .iter()
            .filter(|item| item.kind == ContextItemKind::ToolResult)
            .collect::<Vec<_>>();
        assert_eq!(tool_results.len(), 2, "no aggregate tool-turn duplicate");
        let pruned = tool_results
            .iter()
            .find(|item| item.item_id.0 == "tool_result:call-inspect")
            .expect("first tool result");
        assert!(pruned.state.pinned);
        assert!(pruned.state.pruned);
        let second = tool_results
            .iter()
            .find(|item| item.item_id.0 == "tool_result:call-second")
            .expect("second tool result");
        assert!(second.state.pinned);
        assert!(!second.state.pruned);

        let error = handle
            .evict_context(system.item_id.clone())
            .await
            .expect_err("system policy must not be evictable");
        assert!(
            error
                .to_string()
                .contains("only conversation-resident context items")
        );
        let error = handle
            .pin_context(ContextItemId("tool:inspect".to_owned()))
            .await
            .expect_err("tool definitions must not be mutable through conversation surgery");
        assert!(
            error
                .to_string()
                .contains("only conversation-resident context items")
        );
        assert_eq!(model.request_count(), 0);
    }

    #[tokio::test]
    async fn zero_budget_cap_stops_before_any_provider_or_compaction_call() {
        let root = TempDir::new().expect("tempdir");
        let mut model = M3Model::new([stop_script("must not run", &[])]);
        model.budget.session_cost_cap_micros_usd = Some(0);
        let model = Arc::new(model);
        let handle = SessionActor::spawn(config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle
            .send_message("blocked")
            .await
            .expect("message accepted");
        let events = collect_turn(&mut events).await;
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::BudgetStatus {
                level: BudgetLevel::HardCap,
                ..
            }
        )));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::BudgetExceeded,
                ..
            })
        ));
        assert!(model.requests().is_empty());
    }

    #[tokio::test]
    async fn non_authoritative_sink_accumulates_session_cost_across_turns() {
        let root = TempDir::new().expect("tempdir");
        let billed_usage = TokenUsage {
            output_tokens: 600_000,
            ..TokenUsage::default()
        };
        let mut model = M3Model::new([
            stop_script("first billed response", &[billed_usage]),
            stop_script("second billed response", &[billed_usage]),
            stop_script("must remain unused", &[]),
        ]);
        model.budget.session_cost_cap_micros_usd = Some(1_000_000);
        let model = Arc::new(model);
        let handle = SessionActor::spawn(config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();

        handle.send_message("first").await.expect("first message");
        let first = collect_turn(&mut events).await;
        assert!(matches!(
            first.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::Completed,
                ..
            })
        ));
        assert_eq!(model.requests().len(), 1);

        handle.send_message("second").await.expect("second message");
        let second = collect_turn(&mut events).await;
        assert_eq!(
            model.requests().len(),
            2,
            "the first turn must not be counted twice before the second dispatch"
        );
        assert!(second.iter().any(|event| matches!(
            event.kind,
            PendingEvent::BudgetStatus {
                level: BudgetLevel::HardCap,
                scope: BudgetScope::Session,
                unit: BudgetUnit::MicrosUsd,
                current: 1_200_000,
                limit: 1_000_000,
                ..
            }
        )));
        assert!(matches!(
            second.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::BudgetExceeded,
                ..
            })
        ));

        handle.send_message("third").await.expect("third message");
        let third = collect_turn(&mut events).await;
        assert_eq!(
            model.requests().len(),
            2,
            "two completed $0.60 turns must block later dispatch under a $1.00 cap"
        );
        assert!(matches!(
            third.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::BudgetExceeded,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn authoritative_sink_does_not_double_count_local_session_cost() {
        let root = TempDir::new().expect("tempdir");
        let billed_usage = TokenUsage {
            output_tokens: 600_000,
            ..TokenUsage::default()
        };
        let mut model = M3Model::new([
            stop_script("first billed response", &[billed_usage]),
            stop_script("second billed response", &[billed_usage]),
        ]);
        model.budget.session_cost_cap_micros_usd = Some(1_000_000);
        let model = Arc::new(model);
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = Arc::new(AccountingRecordingSink::default());
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();

        handle.send_message("first").await.expect("first message");
        collect_turn(&mut events).await;
        handle.send_message("second").await.expect("second message");
        let second = collect_turn(&mut events).await;

        assert_eq!(
            model.requests().len(),
            2,
            "authoritative ledger totals must replace, not add to, local history"
        );
        assert!(matches!(
            second.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::BudgetExceeded,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn daily_cap_fails_closed_without_an_authoritative_ledger() {
        let root = TempDir::new().expect("tempdir");
        let mut model = M3Model::new([stop_script("must remain unused", &[])]);
        model.budget.daily_cost_cap_micros_usd = Some(1_000_000);
        let model = Arc::new(model);
        let handle = SessionActor::spawn(config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();

        handle.send_message("blocked").await.expect("message");
        let events = collect_turn(&mut events).await;

        assert!(model.requests().is_empty());
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::BudgetStatus {
                level: BudgetLevel::HardCap,
                scope: BudgetScope::Daily,
                unit: BudgetUnit::MicrosUsd,
                current: 0,
                limit: 1_000_000,
                ..
            }
        )));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::BudgetExceeded,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn incomplete_dollar_accounting_blocks_every_later_turn_before_provider_work() {
        for expected_scope in [BudgetScope::Session, BudgetScope::Daily] {
            let root = TempDir::new().expect("tempdir");
            let mut model = M3Model::new([
                stop_script("first billed response", &[]),
                stop_script("must remain unused", &[]),
            ]);
            model.cost_override = Some(Cost::Unavailable {
                reason: "fixture has no price".to_owned(),
            });
            match expected_scope {
                BudgetScope::Session => model.budget.session_cost_cap_micros_usd = Some(100),
                BudgetScope::Daily => model.budget.daily_cost_cap_micros_usd = Some(100),
                BudgetScope::TrailingMinute => unreachable!("fixture scope"),
            }
            let model = Arc::new(model);
            let sink = Arc::new(AccountingRecordingSink::default());
            let mut actor_config = config(
                root.path(),
                model.clone(),
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            );
            actor_config.event_sink = sink;
            let handle = SessionActor::spawn(actor_config).expect("actor");
            let mut events = handle.subscribe();

            handle.send_message("first").await.expect("first message");
            collect_turn(&mut events).await;
            assert_eq!(model.requests().len(), 1);

            handle.send_message("second").await.expect("second message");
            let second = collect_turn(&mut events).await;
            assert_eq!(
                model.requests().len(),
                1,
                "an active dollar cap must fail closed after unpriced accounting"
            );
            assert!(second.iter().any(|event| matches!(
                &event.kind,
                PendingEvent::BudgetStatus {
                    level: BudgetLevel::HardCap,
                    scope,
                    unit: BudgetUnit::MicrosUsd,
                    ..
                } if scope == &expected_scope
            )));
            assert!(matches!(
                second.last().map(|event| &event.kind),
                Some(PendingEvent::TurnFinished {
                    status: AgentTurnStatus::BudgetExceeded,
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn unavailable_credit_cost_preserves_response_and_blocks_later_dispatch() {
        for authoritative in [false, true] {
            let root = TempDir::new().expect("tempdir");
            let mut model = M3Model::new([
                stop_script("visible credit-billed response", &[]),
                stop_script("must remain unused", &[]),
            ]);
            model.cost_override = Some(Cost::Unavailable {
                reason: "credit burn unavailable".to_owned(),
            });
            model.budget.session_ai_credit_cap_micros = Some(100);
            let model = Arc::new(model);
            let mut actor_config = config(
                root.path(),
                model.clone(),
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            );
            if authoritative {
                actor_config.event_sink = Arc::new(AccountingRecordingSink::default());
            }
            let handle = SessionActor::spawn(actor_config).expect("actor");
            let mut events = handle.subscribe();

            handle.send_message("first").await.expect("first message");
            let first = collect_turn(&mut events).await;
            assert_eq!(model.requests().len(), 1);
            assert!(first.iter().any(|event| matches!(
                &event.kind,
                PendingEvent::TextDelta { text, .. }
                    if text == "visible credit-billed response"
            )));
            assert!(first.iter().any(|event| matches!(
                event.kind,
                PendingEvent::BudgetStatus {
                    level: BudgetLevel::HardCap,
                    scope: BudgetScope::Session,
                    unit: BudgetUnit::AiCreditMicros,
                    current: 0,
                    limit: 100,
                    ..
                }
            )));
            assert!(matches!(
                first.last().map(|event| &event.kind),
                Some(PendingEvent::TurnFinished {
                    status: AgentTurnStatus::BudgetExceeded,
                    ..
                })
            ));

            handle.send_message("second").await.expect("second message");
            let second = collect_turn(&mut events).await;
            assert_eq!(
                model.requests().len(),
                1,
                "unknown credit burn must block later provider dispatch"
            );
            assert!(matches!(
                second.last().map(|event| &event.kind),
                Some(PendingEvent::TurnFinished {
                    status: AgentTurnStatus::BudgetExceeded,
                    ..
                })
            ));
        }
    }

    #[tokio::test]
    async fn opaque_route_cost_controls_post_response_hard_cap_with_shared_model_ids() {
        async fn run(route: &'static str) -> (Vec<SessionEvent>, usize) {
            let root = TempDir::new().expect("tempdir");
            let model = Arc::new(RoutedCostModel::new(route));
            let handle = SessionActor::spawn(config(
                root.path(),
                model.clone(),
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            ))
            .expect("actor");
            let mut events = handle.subscribe();
            handle.send_message("route me").await.expect("message");
            let events = collect_turn(&mut events).await;
            (events, model.requests.load(Ordering::SeqCst))
        }

        let (cheap, cheap_requests) = run("__model_cheap").await;
        assert_eq!(cheap_requests, 1);
        assert!(matches!(
            cheap.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::Completed,
                cost: Cost::Monetary {
                    amount_micros: 10,
                    ..
                },
                ..
            })
        ));

        let (expensive, expensive_requests) = run("__model_expensive").await;
        assert_eq!(expensive_requests, 1);
        assert!(expensive.iter().any(|event| matches!(
            event.kind,
            PendingEvent::BudgetStatus {
                level: BudgetLevel::HardCap,
                current: 100,
                limit: 50,
                ..
            }
        )));
        assert!(matches!(
            expensive.last().map(|event| &event.kind),
            Some(PendingEvent::TurnFinished {
                status: AgentTurnStatus::BudgetExceeded,
                cost: Cost::Monetary {
                    amount_micros: 100,
                    ..
                },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn provider_usage_reconciles_next_meter_and_surfaces_cache_hits() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(M3Model::new([
            tool_script(
                &[("call-1", "ok", json!({}))],
                &[TokenUsage {
                    input_tokens: 500,
                    cache_read_tokens: 500,
                    ..TokenUsage::default()
                }],
            ),
            stop_script("done", &[]),
        ]));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(StubTool::new(
                "ok",
                vec![],
                StubOutcome::Success(ToolResult::new("ok", Value::Null)),
            )))
            .expect("register tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let events = collect_turn(&mut events).await;
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::ContextUsage {
                cache_hit_basis_points: 5_000,
                provider_input_tokens: 1_000,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::ContextUsage {
                provider_input_tokens: 0,
                correction_millionths: 4_000_000,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn one_hundred_fifty_turn_overflow_compacts_and_continues_through_actor() {
        let root = TempDir::new().expect("tempdir");
        let mut model = M3Model::new([
            stop_script(
                "## Goal\ncontinue\n\n## Instructions\nkeep intent\n\n## Discoveries\nsrc/lib.rs checksum amber-42\n\n## Accomplished\n150 turns\n\n## Relevant files & directories\nsrc/lib.rs\nPROJECT.md",
                &[TokenUsage {
                    input_tokens: 2_000,
                    output_tokens: 60,
                    ..TokenUsage::default()
                }],
            ),
            stop_script("amber-42", &[]),
        ]);
        model.metadata = ModelContextMetadata {
            max_context_tokens: Some(2_000),
            max_output_tokens: Some(256),
            cache_breakpoints: Some(CacheBreakpointSupport::Explicit),
        };
        model.budget.session_cost_cap_micros_usd = Some(100);
        let model = Arc::new(model);
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.recovered.conversation = (0..150)
            .map(|index| {
                text_turn(
                    if index % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    if index == 0 {
                        "src/lib.rs checksum amber-42".to_owned()
                    } else {
                        format!("turn {index}: {}", "context ".repeat(20))
                    },
                )
            })
            .collect();
        let sink = Arc::new(NoopSessionEventSink::default());
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle
            .send_message("What is the src/lib.rs checksum?")
            .await
            .expect("message");
        let events = collect_turn(&mut events).await;
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::CompactionStarted {
                reason: CompactionReason::Automatic
            }
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, PendingEvent::CompactionFinished { .. }))
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].tools.is_empty());
        assert!(requests[1].turns.iter().any(|turn| {
            turn.role == Role::User
                && matches!(turn.blocks.as_slice(), [Block::Text { text }] if text == rw_context::AUTO_CONTINUE_TEXT)
        }));
        let final_prompt = serde_json::to_string(&requests[1].turns).expect("serialize prompt");
        assert!(final_prompt.contains("amber-42"));
        assert!(!final_prompt.contains("turn 149:"));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::TextDelta { text, .. } if text == "amber-42"
        )));
        assert!(requests[1].cache_hint.is_none());
        let durable = sink.read_after(None).await.expect("durable events");
        let resumed = project_session_events(&durable).expect("resume projection");
        assert!(
            resumed
                .conversation
                .first()
                .is_some_and(|turn| turn.meta.summary)
        );
        assert!(resumed.conversation.len() < 10);
    }

    #[tokio::test]
    async fn one_hundred_fifty_turn_compaction_quality_replays_from_recorded_provider_fixtures() {
        async fn run(
            root: &Path,
            model: Arc<dyn ModelDriver>,
        ) -> (Vec<SessionEvent>, SessionHandle) {
            let mut actor_config = config(
                root,
                model,
                Arc::new(ToolRegistry::new()),
                PermissionDecision::Allow,
                builtin_hook_dispatcher().expect("hooks"),
            );
            actor_config.recovered.conversation = (0..150)
                .map(|index| {
                    text_turn(
                        if index % 2 == 0 {
                            Role::User
                        } else {
                            Role::Assistant
                        },
                        if index == 0 {
                            "src/lib.rs checksum amber-42".to_owned()
                        } else {
                            format!("turn {index}: {}", "context ".repeat(20))
                        },
                    )
                })
                .collect();
            let handle = SessionActor::spawn(actor_config).expect("actor");
            let mut events = handle.subscribe();
            handle
                .send_message("What is the src/lib.rs checksum?")
                .await
                .expect("message");
            (collect_turn(&mut events).await, handle)
        }

        let fixture_directory = TempDir::new().expect("fixture directory");
        let source = Arc::new(ReplaySourceProvider {
            scripts: Mutex::new(
                [
                    stop_script(
                        "## Goal\ncontinue\n\n## Instructions\nkeep intent\n\n## Discoveries\nsrc/lib.rs checksum amber-42\n\n## Accomplished\n150 turns\n\n## Relevant files & directories\nsrc/lib.rs\nPROJECT.md",
                        &[TokenUsage {
                            input_tokens: 2_000,
                            output_tokens: 60,
                            ..TokenUsage::default()
                        }],
                    ),
                    stop_script("amber-42", &[]),
                ]
                .into_iter()
                .collect(),
            ),
        });
        let recorder = Arc::new(Recorder::new(
            source,
            fixture_directory.path(),
            FixtureRedactor::default(),
        ));
        let recording_root = TempDir::new().expect("recording workspace");
        let (_recorded_events, recorded_handle) = run(
            recording_root.path(),
            Arc::new(ReplayHarnessModel::new(recorder.clone())),
        )
        .await;
        drop(recorded_handle);
        recorder.flush().await.expect("provider fixtures flush");

        let replay = Arc::new(
            ReplayProvider::load("context-replay", fixture_directory.path())
                .await
                .expect("replay fixtures load"),
        );
        let replay_root = TempDir::new().expect("replay workspace");
        let (events, handle) = run(
            replay_root.path(),
            Arc::new(ReplayHarnessModel::new(replay)),
        )
        .await;
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::CompactionStarted {
                reason: CompactionReason::Automatic
            }
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::TextDelta { text, .. } if text == "amber-42"
        )));
        let dump = handle
            .dump_prompt(None)
            .await
            .expect("post-replay prompt dump");
        let prompt = serde_json::to_string(&dump.turns).expect("serialize replay prompt");
        assert!(prompt.contains("amber-42"));
        assert!(!prompt.contains("turn 149:"));
    }

    #[tokio::test]
    async fn typed_provider_overflow_compacts_then_replays_last_real_user() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(M3Model::new([
            vec![Err(ProviderError::new(
                ProviderErrorKind::ContextOverflow,
                "sanitized overflow",
            ))],
            stop_script(
                "## Goal\nrecover\n\n## Instructions\n\n## Discoveries\n\n## Accomplished\n\n## Relevant files & directories\n",
                &[],
            ),
            stop_script("recovered answer", &[]),
        ]));
        let handle = SessionActor::spawn(config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle
            .send_message("keep me intact")
            .await
            .expect("message");
        let events = collect_turn(&mut events).await;
        assert!(events.iter().any(|event| matches!(
            event.kind,
            PendingEvent::CompactionStarted {
                reason: CompactionReason::ProviderOverflow
            }
        )));
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].turns.iter().any(|turn| {
            !turn.meta.synthetic
                && turn.role == Role::User
                && matches!(turn.blocks.as_slice(), [Block::Text { text }] if text == "keep me intact")
        }));
        assert!(!requests[2].turns.iter().any(|turn| {
            matches!(turn.blocks.as_slice(), [Block::Text { text }] if text == rw_context::AUTO_CONTINUE_TEXT)
        }));
    }

    #[tokio::test]
    async fn structured_tool_output_is_toon_only_at_provider_boundary() {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(M3Model::new([
            tool_script(&[("call-1", "structured", json!({}))], &[]),
            tool_script(&[("call-2", "structured", json!({}))], &[]),
            stop_script("done", &[]),
        ]));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(StubTool::new(
                "structured",
                vec![],
                StubOutcome::Success(ToolResult::new(
                    "plain prose",
                    json!({"rows": [{"id": 1}, {"id": 2}]}),
                )),
            )))
            .expect("register tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            model.clone(),
            Arc::new(tools),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run").await.expect("message");
        let events = collect_turn(&mut events).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    PendingEvent::ToolCallFinished {
                        output: ToolOutput::Mixed { parts },
                        ..
                    } if parts.iter().any(|part| matches!(part, ToolOutputPart::Structured { .. }))
                ))
                .count(),
            2
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            let prompt_json = serde_json::to_string(&request.turns).expect("prompt JSON");
            assert_eq!(prompt_json.matches(rw_context::TOON_FORMAT_NOTE).count(), 1);
            assert!(prompt_json.contains("plain prose"));
            assert!(!prompt_json.contains("\"Structured\""));
        }
    }

    #[tokio::test]
    async fn pruning_uses_provider_visible_toon_size_and_persists_that_reclamation() {
        let root = TempDir::new().expect("tempdir");
        let structured_value = json!({
            "rows": (0..30_000)
                .map(|index| json!({"id": index, "state": "candidate-sentinel"}))
                .collect::<Vec<_>>()
        });
        let candidate = Turn {
            role: Role::Tool,
            blocks: vec![Block::ToolResult {
                id: ToolCallId("candidate-call".to_owned()),
                output: ToolOutput::Structured {
                    value: structured_value,
                },
                is_error: false,
            }],
            meta: TurnMeta::default(),
        };
        let mut toon = ToonPromptEncoder::default();
        let provider_candidate = prompt_turn(&candidate, &BTreeMap::new(), &mut toon);
        let provider_visible_tokens = LocalTokenEstimator::turn(&provider_candidate);
        let durable_json_tokens = LocalTokenEstimator::turn(&candidate);
        assert!(provider_visible_tokens > 20_000);
        assert_ne!(provider_visible_tokens, durable_json_tokens);

        let assistant_call = |id: &str, name: &str| Turn {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall {
                id: ToolCallId(id.to_owned()),
                name: name.to_owned(),
                args: json!({}),
            }],
            meta: TurnMeta::default(),
        };
        let recent = Turn {
            role: Role::Tool,
            blocks: vec![Block::ToolResult {
                id: ToolCallId("recent-call".to_owned()),
                output: ToolOutput::Text {
                    text: "r".repeat(200_000),
                },
                is_error: false,
            }],
            meta: TurnMeta::default(),
        };
        let model = Arc::new(M3Model::new([stop_script("done", &[])]));
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.recovered.conversation = vec![
            assistant_call("candidate-call", "shell"),
            candidate,
            text_turn(Role::User, "older user boundary"),
            assistant_call("recent-call", "shell"),
            recent,
            text_turn(Role::User, "newer user boundary"),
        ];
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        handle.send_message("run pruning").await.expect("message");
        let events = collect_turn(&mut events).await;

        assert!(events.iter().any(|event| matches!(
            &event.kind,
            PendingEvent::ToolOutputPruned {
                tool_call_id,
                reclaimed_tokens,
            } if tool_call_id == "candidate-call" && *reclaimed_tokens == provider_visible_tokens
        )));
        let requests = model.requests();
        assert_eq!(requests.len(), 1);
        let prompt = serde_json::to_string(&requests[0].turns).expect("provider prompt");
        assert!(prompt.contains(PRUNED_TOOL_OUTPUT_REPLACEMENT));
        assert!(!prompt.contains("candidate-sentinel"));
    }

    #[tokio::test]
    async fn stable_prefix_hash_and_hint_remain_identical_across_twenty_turns() {
        let root = TempDir::new().expect("tempdir");
        let mut model = M3Model::new((0..20).map(|_| stop_script("ok", &[])));
        model.metadata.cache_breakpoints = Some(CacheBreakpointSupport::Automatic);
        let model = Arc::new(model);
        let sink = Arc::new(NoopSessionEventSink::default());
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.initial_session_context = vec![text_turn(Role::System, "stable policy")];
        actor_config.event_sink = sink.clone();
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut subscription = handle.subscribe();
        for index in 0..20 {
            handle
                .send_message(format!("message {index}"))
                .await
                .expect("message");
            collect_turn(&mut subscription).await;
        }
        let durable = sink.read_after(None).await.expect("durable events");
        let hashes = durable
            .iter()
            .filter_map(|event| match event {
                EngineEvent::ContextUsageUpdated {
                    stable_prefix_hash,
                    provider_input_tokens: 0,
                    ..
                } => Some(stable_prefix_hash.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(hashes.len(), 40);
        assert!(hashes.windows(2).all(|pair| pair[0] == pair[1]));
        let hints = model
            .requests()
            .into_iter()
            .map(|request| request.cache_hint)
            .collect::<Vec<_>>();
        assert!(hints.iter().all(|hint| *hint == hints[0]));
        assert_eq!(hints[0].map(|hint| hint.stable_prefix_turns), Some(1));
    }

    #[tokio::test]
    async fn running_turn_rejects_context_surgery_without_losing_durable_state() {
        let root = TempDir::new().expect("tempdir");
        let mut actor_config = config(
            root.path(),
            Arc::new(PendingModel),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.recovered.conversation = vec![text_turn(Role::User, "stable item")];
        let handle = SessionActor::spawn(actor_config).expect("actor");
        handle.ensure_local_driver().await.expect("driver");
        let mut subscription = handle.subscribe();
        handle.send_message("run").await.expect("message");
        next_matching(&mut subscription, |event| {
            matches!(event, PendingEvent::TurnStarted { .. })
        })
        .await;
        let error = handle
            .pin_context(ContextItemId("conversation:0".to_owned()))
            .await
            .expect_err("running surgery must reject");
        assert!(
            error
                .to_string()
                .contains("context surgery requires an idle session")
        );
        let snapshot = handle
            .context_snapshot()
            .await
            .expect("snapshot remains responsive");
        assert!(!snapshot.items[0].state.pinned);
        assert!(handle.interrupt().await.expect("interrupt"));
        collect_turn(&mut subscription).await;
    }

    #[tokio::test]
    async fn manual_compaction_keeps_queries_and_interrupt_responsive() {
        let root = TempDir::new().expect("tempdir");
        let sink = Arc::new(RecordingSink::default());
        let mut actor_config = config(
            root.path(),
            Arc::new(DelayedSummaryModel),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        actor_config.recovered.conversation = vec![text_turn(Role::User, "compact me")];
        let handle = SessionActor::spawn(actor_config).expect("actor");
        handle.ensure_local_driver().await.expect("driver");
        let compact_handle = handle.clone();
        let compact = tokio::spawn(async move { compact_handle.compact(None).await });
        timeout(Duration::from_millis(100), async {
            loop {
                if handle.active_turn.load(Ordering::Acquire) != 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("manual compaction must start");
        timeout(Duration::from_millis(100), handle.context_snapshot())
            .await
            .expect("query must remain responsive")
            .expect("context query");
        assert!(handle.interrupt().await.expect("interrupt"));
        let result = timeout(Duration::from_secs(1), compact)
            .await
            .expect("compaction cancellation timeout")
            .expect("compaction task join");
        assert!(result.is_err());
        let cost = handle
            .cost_snapshot()
            .await
            .expect("cancelled compaction cost");
        let cancelled = cost
            .turns
            .iter()
            .filter(|entry| entry.attribution == AccountingAttribution::Compaction)
            .collect::<Vec<_>>();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].usage.input_tokens, 11);
        assert_eq!(cancelled[0].usage.output_tokens, 7);
        let durable = sink
            .read_after(None)
            .await
            .expect("durable cancellation events");
        let first = project_session_events(&durable).expect("first cancellation resume");
        let second = project_session_events(&durable).expect("second cancellation resume");
        assert_eq!(first.accounting, second.accounting);
        assert_eq!(first.accounting.len(), 1);
        assert!(!first.interrupted_compaction);
    }

    #[tokio::test]
    async fn messages_queued_during_manual_compaction_resume_in_fifo_order() {
        let root = TempDir::new().expect("tempdir");
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let model = Arc::new(GatedCompactionModel {
            calls: AtomicUsize::new(0),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        });
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.recovered.conversation = vec![text_turn(Role::User, "compact me")];
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe();
        let compact_handle = handle.clone();
        let compact = tokio::spawn(async move { compact_handle.compact(None).await });
        timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("compaction provider started");

        assert_eq!(
            handle
                .send_message("queued first")
                .await
                .expect("first queue"),
            MessageDisposition::Queued
        );
        assert_eq!(
            handle
                .send_message("queued second")
                .await
                .expect("second queue"),
            MessageDisposition::Queued
        );
        release.notify_one();
        compact
            .await
            .expect("compaction join")
            .expect("compaction completion");
        collect_turn(&mut events).await;

        let snapshot = handle.snapshot().await.expect("conversation snapshot");
        let queued = snapshot
            .conversation
            .iter()
            .filter_map(|turn| {
                if turn.role != Role::User {
                    return None;
                }
                match turn.blocks.as_slice() {
                    [Block::Text { text }] => Some(text.as_str()),
                    _ => None,
                }
            })
            .filter(|text| text.starts_with("queued "))
            .collect::<Vec<_>>();
        assert_eq!(queued, ["queued first", "queued second"]);
    }

    #[tokio::test]
    async fn failed_compaction_alias_usage_is_accounted_before_successful_fallback() {
        let root = TempDir::new().expect("tempdir");
        let first_attempt = vec![
            Ok(ProviderEvent::MessageStart {
                model: "failed-compaction-model".to_owned(),
            }),
            Ok(ProviderEvent::Usage {
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 60,
                    ..TokenUsage::default()
                },
            }),
            Err(ProviderError::new(
                ProviderErrorKind::Network,
                "sanitized failed compaction alias",
            )),
        ];
        let mut model = M3Model::new([
            first_attempt,
            stop_script(
                "## Goal\ncontinue\n\n## Instructions\n\n## Discoveries\nfallback worked\n\n## Accomplished\n\n## Relevant files & directories\n",
                &[TokenUsage {
                    input_tokens: 80,
                    output_tokens: 20,
                    ..TokenUsage::default()
                }],
            ),
        ]);
        model.compaction.model_alias = Some("compact-first".to_owned());
        model.budget.session_cost_cap_micros_usd = Some(100);
        let model = Arc::new(model);
        let sink = Arc::new(AccountingRecordingSink::default());
        let mut actor_config = config(
            root.path(),
            model,
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Allow,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.event_sink = sink.clone();
        actor_config.recovered.conversation = vec![text_turn(Role::User, "retain this")];
        let handle = SessionActor::spawn(actor_config).expect("actor");
        handle.compact(None).await.expect("fallback compaction");

        let snapshot = handle
            .cost_snapshot()
            .await
            .expect("compaction cost snapshot");
        let compaction = snapshot
            .turns
            .iter()
            .filter(|entry| entry.attribution == AccountingAttribution::Compaction)
            .collect::<Vec<_>>();
        assert_eq!(compaction.len(), 2);
        assert_eq!(compaction[0].usage.output_tokens, 60);
        assert_eq!(compaction[1].usage.output_tokens, 20);
        assert_eq!(snapshot.session_cost_micros_usd, 80);

        let durable = sink
            .read_after(None)
            .await
            .expect("durable fallback events");
        assert_eq!(
            durable
                .iter()
                .filter(|event| matches!(event, EngineEvent::CompactionAttemptFinished { .. }))
                .count(),
            1
        );
        assert_eq!(
            durable
                .iter()
                .filter(|event| matches!(event, EngineEvent::CompactionFinished { .. }))
                .count(),
            1
        );
        let resumed = project_session_events(&durable).expect("fallback resume");
        assert_eq!(resumed.accounting.len(), 2);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn shell_gate_and_model_alias_are_durable_and_fail_closed() {
        let root = TempDir::new().expect("workspace");
        let actor_config = config(
            root.path(),
            Arc::new(AliasVisionModel),
            Arc::new(ToolRegistry::new()),
            PermissionDecision::Ask,
            HookDispatcher::new(),
        );
        let handle = SessionActor::spawn(actor_config).expect("actor");
        assert_eq!(
            handle
                .dispatch(ClientCommand::AttachSession {
                    meta: protocol_meta("driver", "attach"),
                    session_id: SessionId("fixture-session".to_owned()),
                    last_seen_sequence: None,
                    role: ClientRole::Driver,
                })
                .await
                .expect("attach"),
            CommandOutcome::Accepted
        );
        assert_eq!(
            handle
                .dispatch(ClientCommand::UserShellStarted {
                    meta: protocol_meta("driver", "shell-start"),
                    session_id: SessionId("fixture-session".to_owned()),
                    command: "python".to_owned(),
                })
                .await
                .expect("shell start"),
            CommandOutcome::Accepted
        );
        let active = handle
            .snapshot()
            .await
            .expect("active shell snapshot")
            .active_shell
            .expect("active shell");
        assert!(matches!(
            handle
                .dispatch(ClientCommand::SendMessage {
                    meta: protocol_meta("driver", "blocked-turn"),
                    session_id: SessionId("fixture-session".to_owned()),
                    content: "must wait".to_owned(),
                    attachments: Vec::new(),
                })
                .await
                .expect("blocked turn"),
            CommandOutcome::Rejected { error } if error.code == "user_shell_active"
        ));
        assert!(matches!(
            handle
                .dispatch(ClientCommand::UserShellEnded {
                    meta: protocol_meta("driver", "wrong-shell-end"),
                    session_id: SessionId("fixture-session".to_owned()),
                    shell_id: ShellId("wrong".to_owned()),
                    status: 0,
                    captured_output: None,
                })
                .await
                .expect("wrong shell end"),
            CommandOutcome::Rejected { error } if error.code == "shell_end_rejected"
        ));
        let durable = handle
            .event_sink
            .read_after(None)
            .await
            .expect("durable shell start");
        let recovered = project_session_events(&durable).expect("project shell gate");
        assert_eq!(recovered.active_shell.as_ref(), Some(&active));

        assert_eq!(
            handle
                .dispatch(ClientCommand::UserShellEnded {
                    meta: protocol_meta("driver", "shell-end"),
                    session_id: SessionId("fixture-session".to_owned()),
                    shell_id: active.shell_id,
                    status: 130,
                    captured_output: Some("full captured tail".to_owned()),
                })
                .await
                .expect("shell end"),
            CommandOutcome::Accepted
        );
        assert!(
            handle
                .snapshot()
                .await
                .expect("ended shell")
                .active_shell
                .is_none()
        );
        assert!(matches!(
            handle
                .dispatch(ClientCommand::SwitchModel {
                    meta: protocol_meta("driver", "unknown-model"),
                    session_id: SessionId("fixture-session".to_owned()),
                    model: ModelAlias("missing".to_owned()),
                })
                .await
                .expect("unknown model"),
            CommandOutcome::Rejected { error } if error.code == "unknown_model_alias"
        ));
        assert_eq!(
            handle
                .dispatch(ClientCommand::SwitchModel {
                    meta: protocol_meta("driver", "switch-model"),
                    session_id: SessionId("fixture-session".to_owned()),
                    model: ModelAlias("slow".to_owned()),
                })
                .await
                .expect("switch model"),
            CommandOutcome::Accepted
        );
        assert_eq!(
            handle.snapshot().await.expect("model snapshot").model_alias,
            "slow"
        );
        let durable = handle
            .event_sink
            .read_after(None)
            .await
            .expect("durable model switch");
        assert_eq!(
            project_session_events(&durable)
                .expect("project model")
                .model_alias
                .as_deref(),
            Some("slow")
        );
    }

    #[test]
    fn attachment_validation_is_bounded_provider_neutral_and_vision_gated() {
        let text = Attachment {
            name: "notes.txt".to_owned(),
            media_type: "text/plain".to_owned(),
            data: AttachmentData::Text {
                content: "bounded context".to_owned(),
            },
        };
        let prepared = prepare_user_message("inspect", &[text], "fast", &AliasVisionModel)
            .expect("text attachment");
        assert_eq!(prepared.stored_attachments.len(), 1);
        assert_eq!(prepared.stored_attachments[0].content_hash.len(), 64);
        assert!(matches!(prepared.attachment_blocks[0], Block::Text { .. }));

        let image = Attachment {
            name: "screen.png".to_owned(),
            media_type: "image/png".to_owned(),
            data: AttachmentData::InlineBase64 {
                data: "iVBORw0KGgo=".to_owned(),
            },
        };
        assert!(
            prepare_user_message(
                "inspect",
                std::slice::from_ref(&image),
                "fast",
                &AliasVisionModel
            )
            .expect_err("non-vision alias must reject before acceptance")
            .contains("does not support image")
        );
        let prepared = prepare_user_message("inspect", &[image], "slow", &AliasVisionModel)
            .expect("vision attachment");
        assert!(matches!(prepared.attachment_blocks[0], Block::Image { .. }));

        let unsafe_name = Attachment {
            name: "../secret.txt".to_owned(),
            media_type: "text/plain".to_owned(),
            data: AttachmentData::Text {
                content: "secret".to_owned(),
            },
        };
        assert!(
            prepare_user_message("inspect", &[unsafe_name], "fast", &AliasVisionModel).is_err()
        );
    }
}
