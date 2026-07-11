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

mod decimal_option_u64 {
    use serde::{Deserialize, Deserializer, Serialize as _, Serializer, de::Error as _};

    #[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.map(|value| value.to_string()).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)?
            .map(|value| value.parse().map_err(D::Error::custom))
            .transpose()
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
string_id!(
    ShellId,
    "Engine-generated identifier of one foreground user shell."
);
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
string_id!(
    ModeId,
    "Open identifier of a built-in or extension-provided mode."
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

/// Durable content-addressed attachment metadata persisted in the event log.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct StoredAttachment {
    pub name: String,
    pub media_type: String,
    pub content_hash: String,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub byte_len: u64,
}

/// One active or resumable session returned by the engine host.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct SessionDescriptor {
    pub session_id: SessionId,
    pub workspace_name: String,
    pub model: ModelAlias,
    pub driver_client_id: Option<ClientId>,
    pub shell_active: bool,
}

/// One slash command exposed to fuzzy pickers without UI-private metadata.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct CommandDescriptor {
    pub name: String,
    pub description: String,
    pub usage: String,
}

/// Prompt-cache behavior exposed without leaking a provider implementation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ModelCacheBehavior {
    None,
    Explicit,
    ProviderManaged,
}

/// Provider-neutral capabilities used by model pickers and attachment checks.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModelCapabilities {
    pub tool_calling: bool,
    pub vision: bool,
    pub thinking: bool,
    pub cache_behavior: ModelCacheBehavior,
}

/// One configured model alias and its offline-known capabilities.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModelDescriptor {
    pub alias: ModelAlias,
    pub capabilities: ModelCapabilities,
}

/// Relative workspace path returned by fuzzy file search.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct WorkspaceFileMatch {
    pub path: String,
    pub is_directory: bool,
}

/// Remote-safe in-band file preview; paths are always workspace-relative.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct WorkspaceFilePreview {
    pub path: String,
    pub media_type: String,
    pub data: AttachmentData,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub total_bytes: u64,
    pub truncated: bool,
}

/// Workspace status for the TUI status line and file picker.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct WorkspaceStatus {
    pub workspace_name: String,
    pub branch: Option<String>,
    pub changed_paths: Vec<String>,
    pub truncated: bool,
}

/// One stable session workspace root. Durable/wire events use only virtual
/// `@root/N` paths; canonical host paths stay in private local metadata.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct WorkspaceRootDescriptor {
    pub index: u32,
    pub path: String,
    pub machine_local: bool,
}

/// Optional structured unified diff attached to a mutating-tool approval.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ApprovalBinding {
    pub proposal_id: String,
    pub arguments_hash: String,
    pub base_hash: String,
    pub diff_hash: String,
}

/// Optional structured unified diff attached to a mutating-tool approval.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct UnifiedDiff {
    pub proposal_id: String,
    pub path: String,
    pub unified_diff: String,
    pub arguments_hash: String,
    pub base_hash: String,
    pub diff_hash: String,
    pub truncated: bool,
}

/// A driver's decision at the permission chokepoint.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    AllowProject,
    Deny,
}

/// Built-in interaction policy overlay active for one session.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SessionMode {
    Discuss,
    Plan,
    #[default]
    Execute,
}

/// One verifiable step in a model-submitted plan artifact.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PlanStep {
    pub description: String,
    #[serde(default)]
    pub files_touched: Vec<String>,
    pub verification: String,
}

/// Durable plan submitted before entering execute mode.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PlanArtifact {
    pub title: String,
    pub summary_md: String,
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

/// Driver response to a submitted plan.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum PlanDecision {
    Approve,
    Reject,
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

/// Semantic class of one assembled prompt item.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ContextItemKind {
    System,
    ToolDefinitions,
    ProjectInstructions,
    Conversation,
    ToolResult,
    Pinned,
    QueuedMessage,
}

/// Per-item token and surgery state shown by context inspectors.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ContextItemSnapshot {
    pub item_id: ContextItemId,
    pub kind: ContextItemKind,
    pub label: String,
    pub source: String,
    /// Machine-local provenance only; remote clients must not dereference it.
    pub machine_local_path: Option<String>,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub estimated_tokens: u64,
    pub state: ContextItemState,
}

/// Orthogonal surgery markers for one context item.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ContextItemState {
    pub pinned: bool,
    pub evicted: bool,
    pub summarized: bool,
    pub pruned: bool,
}

/// One explicit cache boundary after an assembled item.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct CacheBreakpoint {
    pub after_item_id: Option<ContextItemId>,
}

/// Exact engine-side context breakdown for one assembled request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ContextSnapshot {
    pub turn_id: Option<TurnId>,
    pub stable_prefix_hash: String,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub used_tokens: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub usable_tokens: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub reserved_tokens: u64,
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    pub items: Vec<ContextItemSnapshot>,
}

/// Usage and billing attributed to one completed agent turn.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct TurnAccounting {
    pub turn_id: TurnId,
    pub attribution: AccountingAttribution,
    pub usage: Usage,
    pub cost: Cost,
}

/// Runtime role to which usage and cost are attributed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum AccountingAttribution {
    Main,
    Compaction,
    Subagent,
}

/// Session-level cost, token, cache, and burn-rate snapshot.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct CostSnapshot {
    pub utc_day: String,
    pub turns: Vec<TurnAccounting>,
    pub session_usage: Usage,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_cost_micros_usd: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_ai_credit_micros: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_cost_micros_usd: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_ai_credit_micros: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub trailing_minute_cost_micros_usd: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub trailing_minute_ai_credit_micros: u64,
    pub cache_hit_basis_points: u16,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub session_cost_cap_micros_usd: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub daily_cost_cap_micros_usd: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub session_ai_credit_cap_micros: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub daily_ai_credit_cap_micros: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub spend_rate_alarm_micros_usd_per_minute: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub ai_credit_rate_alarm_micros_per_minute: Option<u64>,
    pub hard_cap_reached: bool,
    pub session_monetary_accounting_complete: bool,
    pub daily_monetary_accounting_complete: bool,
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_subscription_quota_entries: u64,
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_cost_unavailable_entries: u64,
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_non_usd_monetary_entries: u64,
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_subscription_quota_entries: u64,
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_cost_unavailable_entries: u64,
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_non_usd_monetary_entries: u64,
}

/// Provider-neutral tool definition included in a prompt dump.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PromptTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Exact assembled model request exposed for prompt transparency.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PromptDump {
    pub turn_id: Option<TurnId>,
    pub model_alias: ModelAlias,
    pub turns: Vec<crate::Turn>,
    pub tools: Vec<PromptTool>,
    pub stable_prefix_hash: String,
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub estimated_tokens: u64,
}

/// Commands accepted by the headless engine from any client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case", optional_fields = nullable)]
pub enum ClientCommand {
    CreateSession {
        meta: CommandMeta,
        cwd: String,
        model: Option<ModelAlias>,
    },
    ResumeSession {
        meta: CommandMeta,
        session_id: SessionId,
        last_seen_sequence: Option<SequenceId>,
        role: ClientRole,
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
        /// Required when the pending approval displayed a unified diff. The
        /// actor rejects missing or stale bindings without consuming the ask.
        binding: Option<ApprovalBinding>,
    },
    ApprovePlan {
        meta: CommandMeta,
        session_id: SessionId,
        decision: PlanDecision,
        revisions: Option<String>,
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
        mode: ModeId,
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
        shell_id: ShellId,
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
    GetContext {
        meta: CommandMeta,
        session_id: SessionId,
    },
    GetCost {
        meta: CommandMeta,
        session_id: SessionId,
    },
    DumpPrompt {
        meta: CommandMeta,
        session_id: SessionId,
        turn_id: Option<TurnId>,
    },
    ListSessions {
        meta: CommandMeta,
    },
    ListCommands {
        meta: CommandMeta,
    },
    ListModels {
        meta: CommandMeta,
    },
    SearchWorkspaceFiles {
        meta: CommandMeta,
        session_id: SessionId,
        query: String,
        limit: u32,
    },
    PreviewWorkspaceFile {
        meta: CommandMeta,
        session_id: SessionId,
        path: String,
        max_bytes: u32,
    },
    GetWorkspaceStatus {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ShutdownHost {
        meta: CommandMeta,
    },
}

impl ClientCommand {
    /// Returns caller-supplied command metadata. Transports must replace the
    /// client id with their authenticated, connection-bound identity before
    /// authorization or dispatch.
    #[must_use]
    pub fn meta(&self) -> &CommandMeta {
        match self {
            Self::CreateSession { meta, .. }
            | Self::ResumeSession { meta, .. }
            | Self::AttachSession { meta, .. }
            | Self::SendMessage { meta, .. }
            | Self::Interrupt { meta, .. }
            | Self::ApproveTool { meta, .. }
            | Self::ApprovePlan { meta, .. }
            | Self::AnswerQuestion { meta, .. }
            | Self::SwitchMode { meta, .. }
            | Self::SwitchModel { meta, .. }
            | Self::Compact { meta, .. }
            | Self::Fork { meta, .. }
            | Self::Rewind { meta, .. }
            | Self::TakeDriver { meta, .. }
            | Self::UserShellStarted { meta, .. }
            | Self::UserShellEnded { meta, .. }
            | Self::PinContext { meta, .. }
            | Self::EvictContext { meta, .. }
            | Self::GetContext { meta, .. }
            | Self::GetCost { meta, .. }
            | Self::DumpPrompt { meta, .. }
            | Self::ListSessions { meta, .. }
            | Self::ListCommands { meta, .. }
            | Self::ListModels { meta, .. }
            | Self::SearchWorkspaceFiles { meta, .. }
            | Self::PreviewWorkspaceFile { meta, .. }
            | Self::GetWorkspaceStatus { meta, .. }
            | Self::ShutdownHost { meta, .. } => meta,
        }
    }

    /// Mutable metadata used by a transport to bind authorization to its
    /// authenticated connection instead of trusting the wire `client_id`.
    #[must_use]
    pub fn meta_mut(&mut self) -> &mut CommandMeta {
        match self {
            Self::CreateSession { meta, .. }
            | Self::ResumeSession { meta, .. }
            | Self::AttachSession { meta, .. }
            | Self::SendMessage { meta, .. }
            | Self::Interrupt { meta, .. }
            | Self::ApproveTool { meta, .. }
            | Self::ApprovePlan { meta, .. }
            | Self::AnswerQuestion { meta, .. }
            | Self::SwitchMode { meta, .. }
            | Self::SwitchModel { meta, .. }
            | Self::Compact { meta, .. }
            | Self::Fork { meta, .. }
            | Self::Rewind { meta, .. }
            | Self::TakeDriver { meta, .. }
            | Self::UserShellStarted { meta, .. }
            | Self::UserShellEnded { meta, .. }
            | Self::PinContext { meta, .. }
            | Self::EvictContext { meta, .. }
            | Self::GetContext { meta, .. }
            | Self::GetCost { meta, .. }
            | Self::DumpPrompt { meta, .. }
            | Self::ListSessions { meta, .. }
            | Self::ListCommands { meta, .. }
            | Self::ListModels { meta, .. }
            | Self::SearchWorkspaceFiles { meta, .. }
            | Self::PreviewWorkspaceFile { meta, .. }
            | Self::GetWorkspaceStatus { meta, .. }
            | Self::ShutdownHost { meta, .. } => meta,
        }
    }
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
    BudgetExceeded,
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

/// Billing unit evaluated by a budget guardrail.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum BudgetUnit {
    MicrosUsd,
    AiCreditMicros,
}

/// Severity of a budget transition.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum BudgetLevel {
    Warning,
    SpendRateAlarm,
    HardCap,
}

/// Scope whose spend triggered a budget transition.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum BudgetScope {
    Session,
    Daily,
    TrailingMinute,
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
    ContextSnapshotReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        snapshot: ContextSnapshot,
    },
    CostSnapshotReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        snapshot: CostSnapshot,
    },
    PromptDumpReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        dump: PromptDump,
    },
    SessionReplayCompleted {
        meta: CommandAckMeta,
        session_id: SessionId,
        through_sequence: Option<SequenceId>,
    },
    SessionsListed {
        meta: CommandAckMeta,
        sessions: Vec<SessionDescriptor>,
    },
    CommandDescriptorsListed {
        meta: CommandAckMeta,
        commands: Vec<CommandDescriptor>,
    },
    ModelsListed {
        meta: CommandAckMeta,
        models: Vec<ModelDescriptor>,
    },
    WorkspaceFilesFound {
        meta: CommandAckMeta,
        session_id: SessionId,
        matches: Vec<WorkspaceFileMatch>,
        truncated: bool,
    },
    WorkspaceFilePreviewReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        preview: WorkspaceFilePreview,
    },
    WorkspaceStatusReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        status: WorkspaceStatus,
    },
    HostShutdown {
        meta: CommandAckMeta,
    },
    SessionCreated {
        meta: EventMeta,
        driver_client_id: ClientId,
    },
    WorkspaceRootsChanged {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        generation: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        effective_from_turn: u64,
        roots: Vec<WorkspaceRootDescriptor>,
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
        attachments: Vec<StoredAttachment>,
    },
    UserMessageAccepted {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        agent_turn: u64,
        content: String,
        attachments: Vec<StoredAttachment>,
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
        diff: Option<UnifiedDiff>,
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
    ContextUsageUpdated {
        meta: EventMeta,
        turn_id: TurnId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        used_tokens: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        usable_tokens: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        reserved_tokens: u64,
        stable_prefix_hash: String,
        cache_hit_basis_points: u16,
        #[serde(default, with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        estimated_input_tokens: u64,
        #[serde(default, with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        provider_input_tokens: u64,
        #[serde(default, with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        correction_millionths: u64,
    },
    BudgetStatusChanged {
        meta: EventMeta,
        turn_id: TurnId,
        level: BudgetLevel,
        scope: BudgetScope,
        unit: BudgetUnit,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        current: u64,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        limit: u64,
    },
    CompactionStarted {
        meta: EventMeta,
        reason: CompactionReason,
    },
    /// Accounting for one billed compaction provider attempt that did not
    /// produce the committed summary. This event is deliberately non-terminal:
    /// fallback attempts continue inside the same compaction transaction.
    CompactionAttemptFinished {
        meta: EventMeta,
        summary_turn_id: TurnId,
        usage: Usage,
        cost: Cost,
    },
    CompactionFinished {
        meta: EventMeta,
        summary_turn_id: TurnId,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        reclaimed_tokens: u64,
        #[serde(default)]
        usage: Option<Usage>,
        #[serde(default)]
        cost: Option<Cost>,
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
        mode: ModeId,
    },
    PlanSubmitted {
        meta: EventMeta,
        artifact: PlanArtifact,
    },
    PlanReviewed {
        meta: EventMeta,
        artifact: PlanArtifact,
        decision: PlanDecision,
        revisions: Option<String>,
    },
    ModelChanged {
        meta: EventMeta,
        model: ModelAlias,
    },
    ContextItemPinned {
        meta: EventMeta,
        item_id: ContextItemId,
        #[serde(default, with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        effective_after_agent_turn: u64,
    },
    ContextItemEvicted {
        meta: EventMeta,
        item_id: ContextItemId,
        #[serde(default, with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        effective_after_agent_turn: u64,
    },
    UserShellStateChanged {
        meta: EventMeta,
        shell_id: ShellId,
        command: Option<String>,
        active: bool,
        status: Option<i32>,
        captured_output: Option<String>,
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
            Self::CommandAcknowledged { .. }
            | Self::ContextSnapshotReady { .. }
            | Self::CostSnapshotReady { .. }
            | Self::PromptDumpReady { .. }
            | Self::SessionReplayCompleted { .. }
            | Self::SessionsListed { .. }
            | Self::CommandDescriptorsListed { .. }
            | Self::ModelsListed { .. }
            | Self::WorkspaceFilesFound { .. }
            | Self::WorkspaceFilePreviewReady { .. }
            | Self::WorkspaceStatusReady { .. }
            | Self::HostShutdown { .. } => None,
            Self::SessionCreated { meta, .. }
            | Self::WorkspaceRootsChanged { meta, .. }
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
            | Self::ContextUsageUpdated { meta, .. }
            | Self::BudgetStatusChanged { meta, .. }
            | Self::CompactionStarted { meta, .. }
            | Self::CompactionAttemptFinished { meta, .. }
            | Self::CompactionFinished { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentFinished { meta, .. }
            | Self::ToolOutputPruned { meta, .. }
            | Self::ModeChanged { meta, .. }
            | Self::PlanSubmitted { meta, .. }
            | Self::PlanReviewed { meta, .. }
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
            Self::CommandAcknowledged { .. }
            | Self::ContextSnapshotReady { .. }
            | Self::CostSnapshotReady { .. }
            | Self::PromptDumpReady { .. }
            | Self::SessionReplayCompleted { .. }
            | Self::SessionsListed { .. }
            | Self::CommandDescriptorsListed { .. }
            | Self::ModelsListed { .. }
            | Self::WorkspaceFilesFound { .. }
            | Self::WorkspaceFilePreviewReady { .. }
            | Self::WorkspaceStatusReady { .. }
            | Self::HostShutdown { .. } => None,
            Self::SessionCreated { meta, .. }
            | Self::WorkspaceRootsChanged { meta, .. }
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
            | Self::ContextUsageUpdated { meta, .. }
            | Self::BudgetStatusChanged { meta, .. }
            | Self::CompactionStarted { meta, .. }
            | Self::CompactionAttemptFinished { meta, .. }
            | Self::CompactionFinished { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentFinished { meta, .. }
            | Self::ToolOutputPruned { meta, .. }
            | Self::ModeChanged { meta, .. }
            | Self::PlanSubmitted { meta, .. }
            | Self::PlanReviewed { meta, .. }
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

#[cfg(test)]
mod tests {
    use super::{ClientCommand, ClientId, CommandMeta, RequestId};

    #[test]
    fn transport_can_replace_untrusted_wire_client_identity() {
        let mut command = ClientCommand::ShutdownHost {
            meta: CommandMeta {
                protocol_version: 1,
                client_id: ClientId("spoofed-on-wire".to_owned()),
                request_id: RequestId("request-1".to_owned()),
            },
        };
        command.meta_mut().client_id = ClientId("bound-connection".to_owned());
        assert_eq!(command.meta().client_id.0, "bound-connection");
    }
}
