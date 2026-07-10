use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::{ToolCallId, ToolOutput};

mod decimal_u64 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
        pub struct $name(pub String);
    };
}

string_id!(SessionId, "Stable identifier of an engine session.");
string_id!(ClientId, "Stable identifier of a connected client.");
string_id!(
    RequestId,
    "Client-generated identifier used to correlate a command."
);
string_id!(TurnId, "Stable identifier of a conversation turn.");
string_id!(QuestionId, "Stable identifier of an interactive question.");
string_id!(SubagentId, "Stable identifier of a child agent session.");
string_id!(
    ContextItemId,
    "Stable identifier of one assembled context item."
);
string_id!(
    ModelAlias,
    "Provider-blind model alias resolved by the engine router."
);

/// Monotonic per-session sequence encoded as a decimal string on the wire.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
pub struct SequenceId(
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub u64,
);

impl From<u64> for SequenceId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Metadata common to commands from all client transports.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct CommandMeta {
    pub protocol_version: u16,
    pub client_id: ClientId,
    pub request_id: RequestId,
}

/// Metadata common to persisted and streamed events.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct EventMeta {
    pub protocol_version: u16,
    pub session_id: SessionId,
    pub sequence_id: SequenceId,
    pub emitted_at: String,
    pub caused_by: Option<RequestId>,
}

/// Metadata for immediate command acknowledgements before session sequencing.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct CommandAckMeta {
    pub protocol_version: u16,
    pub client_id: ClientId,
    pub request_id: RequestId,
    pub emitted_at: String,
}

/// Whether a client mutates the session or only observes it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ClientRole {
    Driver,
    Observer,
}

/// In-band attachment data; protocol messages never require shared files.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum AttachmentData {
    Text { content: String },
    InlineBase64 { data: String },
}

/// User-provided content attached to a message.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct Attachment {
    pub name: String,
    pub media_type: String,
    pub data: AttachmentData,
}

/// A driver's decision at the permission chokepoint.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

/// Rewind destination selected by a client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum RewindTarget {
    Turn { turn_id: TurnId },
    Checkpoint { checkpoint_id: String },
}

/// Shape of a response accepted for an interactive question.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum QuestionResponseKind {
    Text,
    SelectOne,
    SelectMany,
}

/// A selectable response to an engine question.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct QuestionOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// A typed question sent to an interactive client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct Question {
    pub id: QuestionId,
    pub prompt: String,
    pub response_kind: QuestionResponseKind,
    pub options: Vec<QuestionOption>,
}

/// One answer returned for a question entry.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct Answer {
    pub question_id: QuestionId,
    pub values: Vec<String>,
}

/// Commands accepted by the headless engine from any client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case", optional_fields = nullable)]
pub enum ClientCommand {
    CreateSession {
        meta: CommandMeta,
        cwd: String,
    },
    AttachSession {
        meta: CommandMeta,
        session_id: SessionId,
        last_seen_sequence: Option<SequenceId>,
        role: ClientRole,
    },
    SendMessage {
        meta: CommandMeta,
        session_id: SessionId,
        content: String,
        attachments: Vec<Attachment>,
    },
    Interrupt {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ApproveTool {
        meta: CommandMeta,
        session_id: SessionId,
        tool_call_id: ToolCallId,
        decision: ApprovalDecision,
    },
    AnswerQuestion {
        meta: CommandMeta,
        session_id: SessionId,
        question_id: QuestionId,
        answers: Vec<Answer>,
    },
    SwitchMode {
        meta: CommandMeta,
        session_id: SessionId,
        mode: String,
    },
    SwitchModel {
        meta: CommandMeta,
        session_id: SessionId,
        model: ModelAlias,
    },
    Compact {
        meta: CommandMeta,
        session_id: SessionId,
        instructions: Option<String>,
    },
    Fork {
        meta: CommandMeta,
        session_id: SessionId,
        at_turn: Option<TurnId>,
    },
    Rewind {
        meta: CommandMeta,
        session_id: SessionId,
        target: RewindTarget,
    },
    TakeDriver {
        meta: CommandMeta,
        session_id: SessionId,
    },
    UserShellStarted {
        meta: CommandMeta,
        session_id: SessionId,
        command: String,
    },
    UserShellEnded {
        meta: CommandMeta,
        session_id: SessionId,
        status: i32,
        captured_output: Option<String>,
    },
    PinContext {
        meta: CommandMeta,
        session_id: SessionId,
        item_id: ContextItemId,
    },
    EvictContext {
        meta: CommandMeta,
        session_id: SessionId,
        item_id: ContextItemId,
    },
}

/// Capabilities used by the permission engine for a tool invocation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ToolCapability {
    ReadFilesystem,
    WriteFilesystem,
    Network,
    Execute,
}

/// A stream associated with partial output from a running tool.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
}

/// Terminal state of an agent turn.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    MaxTurns,
    DoomLoop,
}

/// Why a context compaction began.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum CompactionReason {
    Automatic,
    Manual,
    ProviderOverflow,
}

/// Provider-reported token accounting normalized by the router.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct Usage {
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub input_tokens: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub output_tokens: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub cache_read_tokens: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub cache_write_tokens: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub reasoning_tokens: u64,
}

/// A workspace path that a rewind could not restore exactly.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct UnrestorablePath {
    pub path: String,
    pub reason: String,
}

/// Provider-neutral billing/quota disposition for a completed turn. A missing
/// price is never represented as a zero-dollar API charge.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(tag = "kind", rename_all = "snake_case", optional_fields = nullable)]
pub enum Cost {
    Monetary {
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        amount_micros: u64,
        currency: String,
    },
    AiCredits {
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        credits_micros: u64,
        nominal_amount_micros: Option<String>,
        currency: Option<String>,
    },
    SubscriptionQuota {
        used: Option<String>,
        unit: Option<String>,
    },
    Unavailable {
        reason: String,
    },
}

/// Stable error categories used across engine boundaries.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum EngineErrorCategory {
    Provider,
    Tool,
    Sandbox,
    Config,
    Extension,
    Protocol,
    Internal,
}

/// Actionable client-safe error payload.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct EngineError {
    pub category: EngineErrorCategory,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Option<Value>,
}

/// Immediate disposition of a client command, delivered on the event channel.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum CommandOutcome {
    Accepted,
    Rejected { error: EngineError },
}

/// Events streamed to clients and persisted in the session event log.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case", optional_fields = nullable)]
pub enum EngineEvent {
    CommandAcknowledged {
        meta: CommandAckMeta,
        session_id: Option<SessionId>,
        outcome: CommandOutcome,
    },
    SessionCreated {
        meta: EventMeta,
        driver_client_id: ClientId,
    },
    DriverChanged {
        meta: EventMeta,
        driver_client_id: ClientId,
    },
    MessageQueued {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        position: u64,
        content: String,
    },
    UserMessageAccepted {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        agent_turn: u64,
        content: String,
        attachments: Vec<Attachment>,
    },
    ConversationTurnCommitted {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        agent_turn: u64,
        turn: crate::Turn,
    },
    ConversationRewound {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        to_agent_turn: u64,
        operation_id: String,
        unrestorable_paths: Vec<UnrestorablePath>,
    },
    TurnStarted {
        meta: EventMeta,
        turn_id: TurnId,
    },
    TextDelta {
        meta: EventMeta,
        turn_id: TurnId,
        text: String,
    },
    ThinkingDelta {
        meta: EventMeta,
        turn_id: TurnId,
        text: String,
        signature: Option<String>,
    },
    CitationDelta {
        meta: EventMeta,
        turn_id: TurnId,
        uri: String,
        title: Option<String>,
    },
    ToolCallStarted {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        name: String,
        args: Value,
        call_index: u32,
    },
    ToolApprovalNeeded {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        name: String,
        args: Value,
        capabilities: Vec<ToolCapability>,
        rationale: String,
    },
    ToolOutputDelta {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        stream: ToolOutputStream,
        chunk: String,
    },
    ToolCallFinished {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        output: ToolOutput,
        is_error: bool,
        call_index: u32,
    },
    QuestionAsked {
        meta: EventMeta,
        turn_id: TurnId,
        question_id: QuestionId,
        questions: Vec<Question>,
    },
    QuestionAnswered {
        meta: EventMeta,
        turn_id: TurnId,
        question_id: QuestionId,
        answers: Vec<Answer>,
    },
    TurnFinished {
        meta: EventMeta,
        turn_id: TurnId,
        status: TurnStatus,
        usage: Usage,
        cost: Cost,
    },
    CompactionStarted {
        meta: EventMeta,
        reason: CompactionReason,
    },
    CompactionFinished {
        meta: EventMeta,
        summary_turn_id: TurnId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        reclaimed_tokens: u64,
    },
    SubagentSpawned {
        meta: EventMeta,
        subagent_id: SubagentId,
        task: String,
    },
    SubagentFinished {
        meta: EventMeta,
        subagent_id: SubagentId,
        output: ToolOutput,
        is_error: bool,
    },
    ToolOutputPruned {
        meta: EventMeta,
        tool_call_id: ToolCallId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        reclaimed_tokens: u64,
    },
    ModeChanged {
        meta: EventMeta,
        mode: String,
    },
    ModelChanged {
        meta: EventMeta,
        model: ModelAlias,
    },
    ContextItemPinned {
        meta: EventMeta,
        item_id: ContextItemId,
    },
    ContextItemEvicted {
        meta: EventMeta,
        item_id: ContextItemId,
    },
    UserShellStateChanged {
        meta: EventMeta,
        active: bool,
        status: Option<i32>,
    },
    HookFailed {
        meta: EventMeta,
        event: String,
        hook_id: String,
        fail_closed: bool,
        message: String,
    },
    CommandFinished {
        meta: EventMeta,
        name: String,
        message: String,
        unrestorable_paths: Vec<UnrestorablePath>,
    },
    GuardTriggered {
        meta: EventMeta,
        turn_id: TurnId,
        guard: String,
        message: String,
    },
    Error {
        meta: EventMeta,
        error: EngineError,
    },
}

impl EngineEvent {
    /// Returns durable session metadata, or `None` for the connection-scoped
    /// command acknowledgement that is never written to a session log.
    #[must_use]
    pub fn meta(&self) -> Option<&EventMeta> {
        match self {
            Self::CommandAcknowledged { .. } => None,
            Self::SessionCreated { meta, .. }
            | Self::DriverChanged { meta, .. }
            | Self::MessageQueued { meta, .. }
            | Self::UserMessageAccepted { meta, .. }
            | Self::ConversationTurnCommitted { meta, .. }
            | Self::ConversationRewound { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::TextDelta { meta, .. }
            | Self::ThinkingDelta { meta, .. }
            | Self::CitationDelta { meta, .. }
            | Self::ToolCallStarted { meta, .. }
            | Self::ToolApprovalNeeded { meta, .. }
            | Self::ToolOutputDelta { meta, .. }
            | Self::ToolCallFinished { meta, .. }
            | Self::QuestionAsked { meta, .. }
            | Self::QuestionAnswered { meta, .. }
            | Self::TurnFinished { meta, .. }
            | Self::CompactionStarted { meta, .. }
            | Self::CompactionFinished { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentFinished { meta, .. }
            | Self::ToolOutputPruned { meta, .. }
            | Self::ModeChanged { meta, .. }
            | Self::ModelChanged { meta, .. }
            | Self::ContextItemPinned { meta, .. }
            | Self::ContextItemEvicted { meta, .. }
            | Self::UserShellStateChanged { meta, .. }
            | Self::HookFailed { meta, .. }
            | Self::CommandFinished { meta, .. }
            | Self::GuardTriggered { meta, .. }
            | Self::Error { meta, .. } => Some(meta),
        }
    }

    /// Mutable durable session metadata for storage adapters and protocol
    /// validators. Connection-scoped acknowledgements return `None`.
    #[must_use]
    pub fn meta_mut(&mut self) -> Option<&mut EventMeta> {
        match self {
            Self::CommandAcknowledged { .. } => None,
            Self::SessionCreated { meta, .. }
            | Self::DriverChanged { meta, .. }
            | Self::MessageQueued { meta, .. }
            | Self::UserMessageAccepted { meta, .. }
            | Self::ConversationTurnCommitted { meta, .. }
            | Self::ConversationRewound { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::TextDelta { meta, .. }
            | Self::ThinkingDelta { meta, .. }
            | Self::CitationDelta { meta, .. }
            | Self::ToolCallStarted { meta, .. }
            | Self::ToolApprovalNeeded { meta, .. }
            | Self::ToolOutputDelta { meta, .. }
            | Self::ToolCallFinished { meta, .. }
            | Self::QuestionAsked { meta, .. }
            | Self::QuestionAnswered { meta, .. }
            | Self::TurnFinished { meta, .. }
            | Self::CompactionStarted { meta, .. }
            | Self::CompactionFinished { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentFinished { meta, .. }
            | Self::ToolOutputPruned { meta, .. }
            | Self::ModeChanged { meta, .. }
            | Self::ModelChanged { meta, .. }
            | Self::ContextItemPinned { meta, .. }
            | Self::ContextItemEvicted { meta, .. }
            | Self::UserShellStateChanged { meta, .. }
            | Self::HookFailed { meta, .. }
            | Self::CommandFinished { meta, .. }
            | Self::GuardTriggered { meta, .. }
            | Self::Error { meta, .. } => Some(meta),
        }
    }
}
