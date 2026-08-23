use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use thiserror::Error;
use ts_rs::TS;

use crate::{PermissionModeDescriptor, ToolCallId, ToolOutput, config::PermissionDecision};

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

/// Maximum encoded length of a session identifier.
pub const MAX_SESSION_ID_BYTES: usize = 128;

/// Stable identifier of an engine session.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionId(pub String);

/// A session identifier failed the canonical path-component grammar.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("session id is empty, too long, a dot component, or contains an unsafe character")]
pub struct SessionIdError;

impl SessionId {
    /// Validates the canonical session identifier grammar without allocating.
    ///
    /// # Errors
    ///
    /// Returns [`SessionIdError`] unless `value` is a non-dot ASCII path
    /// component using at most [`MAX_SESSION_ID_BYTES`] bytes.
    pub fn validate(value: &str) -> Result<(), SessionIdError> {
        if value.is_empty()
            || value.len() > MAX_SESSION_ID_BYTES
            || matches!(value, "." | "..")
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(SessionIdError);
        }
        Ok(())
    }

    /// Parses an untrusted string into a validated session identifier.
    ///
    /// # Errors
    ///
    /// Returns [`SessionIdError`] when the value does not satisfy the canonical
    /// session identifier grammar.
    pub fn parse(value: impl Into<String>) -> Result<Self, SessionIdError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }
}
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
string_id!(
    ProviderAuthAttemptId,
    "Connection-scoped identifier of one provider authentication attempt."
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
    /// Optional normalized workspace-relative source path. Local absolute paths
    /// never cross the client protocol; their in-band content may still be
    /// attached under a safe basename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub source_path: Option<String>,
    pub media_type: String,
    pub data: AttachmentData,
}

/// Durable content-addressed attachment metadata persisted in the event log.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct StoredAttachment {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub source_path: Option<String>,
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
    /// Human-facing session title. Empty only when reading an older peer.
    #[serde(default)]
    pub title: String,
    pub workspace_name: String,
    pub model: ModelAlias,
    pub driver_client_id: Option<ClientId>,
    pub shell_active: bool,
}

/// One slash command exposed to fuzzy pickers without UI-private metadata.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum CommandSource {
    #[default]
    Builtin,
    Project,
    User,
    Plugin,
    Skill,
    Workflow,
    Mcp,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct CommandDescriptor {
    pub name: String,
    pub description: String,
    pub usage: String,
    #[serde(default)]
    pub source: CommandSource,
}

/// One bounded, credential-free interaction mode exposed to clients.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModeDescriptor {
    pub id: ModeId,
    pub description: String,
    pub current: bool,
}

/// One engine-mediated user setting exposed to interactive clients.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct UserSettingDescriptor {
    pub key: String,
    pub label: String,
    pub value: String,
    pub choices: Vec<String>,
    pub provenance: String,
    pub applies_immediately: bool,
}

/// Bounded live state of one MCP server. Transport credentials and process
/// environment are intentionally never exposed on the interactive protocol.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
pub enum McpServerState {
    Disabled,
    Connecting,
    Ready,
    ApprovalRequired,
    Failed { message: String },
    Stopping,
}

/// One server in the live session MCP inventory.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct McpServerDescriptor {
    pub name: String,
    pub enabled: bool,
    pub approved: bool,
    pub state: McpServerState,
    pub tool_count: u32,
    pub resource_count: u32,
    pub prompt_count: u32,
}

/// Exact, redacted configuration identity presented before MCP approval.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct McpApprovalReview {
    pub server: String,
    pub transport: String,
    pub endpoint: Option<String>,
    pub origin: String,
    pub defer_tools: bool,
    pub fingerprint: String,
    pub previously_approved: bool,
}

/// One explicit environment entry for a stdio MCP server.
///
/// The value is carried on the authenticated command wire but never exposed by
/// debug formatting.
#[derive(Clone, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct McpEnvironmentEntry {
    pub key: String,
    pub value: String,
}

impl fmt::Debug for McpEnvironmentEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpEnvironmentEntry")
            .field("key", &self.key)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

/// One credential-free process identity currently serving the active session.
///
/// Configured-but-idle integrations are deliberately absent. Names are short
/// executable identities, never command lines, arguments, paths, or output.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RuntimeServiceDescriptor {
    pub kind: RuntimeServiceKind,
    pub name: String,
}

/// Live service categories projected to clients.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum RuntimeServiceKind {
    Lsp,
    Linter,
    Formatter,
    Test,
}

/// Scope of a remembered exact approval.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum PermissionApprovalScope {
    Session,
    Project,
}

/// Stable, typed rule row rendered by permission-management clients.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PermissionRuleDescriptor {
    /// Opaque stable id accepted by remove operations. Clients never rebuild it.
    pub id: String,
    pub pattern: String,
    pub action: PermissionDecision,
}

/// Opaque remembered approval metadata. Invocation arguments and fingerprints
/// are deliberately absent.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PermissionApprovalDescriptor {
    pub id: String,
    pub scope: PermissionApprovalScope,
    pub tool_name: String,
    pub summary: String,
}

/// Bounded permission inventory for one live session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PermissionStateDescriptor {
    pub default: PermissionDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub runtime_mode: Option<PermissionModeDescriptor>,
    /// Effective immutable rules assembled from trusted user configuration.
    pub effective_rules: Vec<PermissionRuleDescriptor>,
    /// Project rule authority is intentionally empty while project permission
    /// config remains forbidden; the typed field makes that policy explicit.
    pub project_rules: Vec<PermissionRuleDescriptor>,
    /// Ephemeral rules added by the current session's driver.
    pub session_rules: Vec<PermissionRuleDescriptor>,
    pub approvals: Vec<PermissionApprovalDescriptor>,
    pub truncated: bool,
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
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub max_context_tokens: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub max_output_tokens: Option<u64>,
}

/// One concrete provider/model discovered from a live authenticated catalog.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModelDescriptor {
    /// Concrete provider-qualified model id.
    pub id: String,
    pub display_name: String,
    /// Sanitized logical provider name. Adapter kind, endpoint, and auth
    /// material remain behind the provider boundary.
    pub provider: String,
    /// Configured role aliases which currently include this concrete model.
    #[serde(default)]
    pub aliases: Vec<ModelAlias>,
    pub current: bool,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub status: Option<String>,
    pub capabilities: ModelCapabilities,
}

/// Small provider-blind role mapping shown separately from the live catalog.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModelAliasDescriptor {
    pub alias: ModelAlias,
    pub candidates: Vec<String>,
    pub current: bool,
}

/// Sanitized provider inventory row. Credentials, endpoints, and adapter
/// implementation details never cross this boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ProviderAuthKind {
    ApiKey,
    Oauth,
    DeviceFlow,
    None,
}

/// Exact safe action offered for one provider inventory row.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ProviderNextAction {
    Configure,
    Authenticate,
    SelectModels,
    ApiKeyCli,
    None,
}

/// Sanitized, connection-scoped authentication prompt. This is never a
/// durable session event and contains no token or credential value.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(tag = "kind", rename_all = "snake_case", optional_fields = nullable)]
pub enum ProviderAuthChallenge {
    Oauth {
        authorization_url: String,
        redirect_uri: String,
    },
    DeviceFlow {
        verification_uri: String,
        user_code: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderDescriptor {
    pub name: String,
    pub auth_kind: ProviderAuthKind,
    pub next_action: ProviderNextAction,
    pub configured: bool,
    pub authenticated: bool,
    pub reachable: bool,
    pub model_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub status: Option<String>,
}

/// One bounded live-catalog projection shared by the host and CLI.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModelCatalogSnapshot {
    pub aliases: Vec<ModelAliasDescriptor>,
    pub models: Vec<ModelDescriptor>,
    pub providers: Vec<ProviderDescriptor>,
    pub cached: bool,
    pub truncated: bool,
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

/// Bounded current-worktree diff for one exact workspace-relative path.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct WorkspaceDiff {
    pub path: String,
    pub unified_diff: String,
    pub truncated: bool,
    pub binary: bool,
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

impl SessionMode {
    /// Stable declarative and protocol spelling for this interaction policy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discuss => "discuss",
            Self::Plan => "plan",
            Self::Execute => "execute",
        }
    }
}

impl std::str::FromStr for SessionMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "discuss" => Ok(Self::Discuss),
            "plan" => Ok(Self::Plan),
            "execute" => Ok(Self::Execute),
            _ => Err(format!("unknown session mode `{value}`")),
        }
    }
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

/// Driver decision for one file in the cumulative session review.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ReviewFileDecision {
    Accept,
    Revert,
}

/// Durable disposition of one file in the cumulative session review.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ReviewFileStatus {
    Pending,
    Accepted,
    Reverted,
}

/// One deterministic workspace-relative entry in a cumulative session review.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct SessionReviewFile {
    pub path: String,
    pub unified_diff: String,
    pub status: ReviewFileStatus,
    pub truncated: bool,
    pub unrestorable_reason: Option<String>,
    pub original_hash: String,
    pub current_hash: String,
}

/// Complete replacement snapshot for the cumulative session review reducer.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionReview {
    pub session_id: SessionId,
    pub files: Vec<SessionReviewFile>,
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

/// Explicit handling of existing conversation context when changing models.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ModelContextTransfer {
    /// Compact the current conversation, then give the summary to the new model.
    PassSummary,
    /// Keep the complete current conversation for the new model.
    PassFullContext,
    /// Retain only system/project instructions and start a fresh conversation.
    StartWithoutContext,
}

/// Target retained in a durable model-switch interaction until the user chooses
/// how existing context should cross the model boundary.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModelSwitchQuestion {
    pub model: ModelAlias,
    #[serde(default)]
    pub provider: Option<String>,
}

/// A selectable response to an engine question.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct QuestionOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    /// Present only for the three typed model-context transfer choices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub model_context_transfer: Option<ModelContextTransfer>,
}

/// A typed question sent to an interactive client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct Question {
    pub id: QuestionId,
    pub prompt: String,
    pub response_kind: QuestionResponseKind,
    pub options: Vec<QuestionOption>,
    /// Present only when this question gates a model switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub model_switch: Option<ModelSwitchQuestion>,
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
    /// False when zero capacity means unknown rather than exhausted.
    #[serde(default)]
    pub context_window_known: bool,
    /// Provider-neutral explanation for an unknown context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub context_window_reason: Option<String>,
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
    Title,
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
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_subscription_tokens: u64,
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_subscription_tokens: u64,
    #[serde(default, with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub trailing_minute_subscription_tokens: u64,
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
    pub session_token_cap: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub daily_token_cap: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub spend_rate_alarm_micros_usd_per_minute: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub ai_credit_rate_alarm_micros_per_minute: Option<u64>,
    #[serde(default, with = "decimal_option_u64")]
    #[schemars(with = "Option<String>")]
    #[ts(type = "string | null")]
    pub token_rate_alarm_per_minute: Option<u64>,
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

/// Stable transcript formats shared by CLI and authenticated clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum TranscriptFormat {
    Markdown,
    Html,
    Json,
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
        /// When set, route only through this configured provider for the
        /// selected alias instead of using the alias's automatic fallback chain.
        #[serde(default)]
        provider: Option<String>,
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
        /// Stable client-generated identity retained until the correlated fork completes.
        operation_id: String,
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
    AttachDevelopmentPlugin {
        meta: CommandMeta,
        session_id: SessionId,
        source: String,
    },
    DetachDevelopmentPlugin {
        meta: CommandMeta,
        session_id: SessionId,
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
    GetSessionReview {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ReviewFile {
        meta: CommandMeta,
        session_id: SessionId,
        path: String,
        decision: ReviewFileDecision,
        current_hash: String,
    },
    DumpPrompt {
        meta: CommandMeta,
        session_id: SessionId,
        turn_id: Option<TurnId>,
    },
    ListSessions {
        meta: CommandMeta,
    },
    SearchSessions {
        meta: CommandMeta,
        query: String,
        limit: u32,
    },
    ListCommands {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ListModes {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ListModels {
        meta: CommandMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session_id: Option<SessionId>,
        #[serde(default)]
        refresh: bool,
    },
    ListSettings {
        meta: CommandMeta,
        session_id: SessionId,
    },
    SetSetting {
        meta: CommandMeta,
        session_id: SessionId,
        key: String,
        value: String,
    },
    ListMcpServers {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ListRuntimeServices {
        meta: CommandMeta,
        session_id: SessionId,
    },
    AddMcpHttpServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
        endpoint: String,
    },
    AddMcpStdioServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
        executable: String,
        args: Vec<String>,
        environment: Vec<McpEnvironmentEntry>,
    },
    RemoveMcpServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
    },
    ReviewMcpServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
    },
    ApproveMcpServer {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
        fingerprint: String,
    },
    SetMcpServerEnabled {
        meta: CommandMeta,
        session_id: SessionId,
        name: String,
        enabled: bool,
    },
    ListPermissions {
        meta: CommandMeta,
        session_id: SessionId,
    },
    AddSessionPermissionRule {
        meta: CommandMeta,
        session_id: SessionId,
        pattern: String,
        action: PermissionDecision,
    },
    RemoveSessionPermissionRule {
        meta: CommandMeta,
        session_id: SessionId,
        rule_id: String,
    },
    RemoveQueuedMessage {
        meta: CommandMeta,
        session_id: SessionId,
        position: String,
    },
    ClearQueuedMessages {
        meta: CommandMeta,
        session_id: SessionId,
    },
    RenameSession {
        meta: CommandMeta,
        session_id: SessionId,
        title: String,
    },
    ExportSession {
        meta: CommandMeta,
        session_id: SessionId,
        format: TranscriptFormat,
        output_path: String,
        force: bool,
    },
    RevokePermissionApproval {
        meta: CommandMeta,
        session_id: SessionId,
        approval_id: String,
        scope: PermissionApprovalScope,
    },
    BeginProviderAuth {
        meta: CommandMeta,
        session_id: SessionId,
        provider: String,
    },
    ConfigureBuiltinProvider {
        meta: CommandMeta,
        session_id: SessionId,
        provider: String,
    },
    CompleteProviderAuth {
        meta: CommandMeta,
        session_id: SessionId,
        provider: String,
        attempt_id: ProviderAuthAttemptId,
    },
    CancelProviderAuth {
        meta: CommandMeta,
        session_id: SessionId,
        provider: String,
        attempt_id: ProviderAuthAttemptId,
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
    GetWorkspaceDiff {
        meta: CommandMeta,
        session_id: SessionId,
        path: String,
        max_bytes: u32,
    },
    ListSubagents {
        meta: CommandMeta,
        session_id: SessionId,
    },
    ReplaySubagent {
        meta: CommandMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
        after_sequence: Option<SequenceId>,
    },
    ContinueSubagent {
        meta: CommandMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
        content: String,
    },
    InterruptSubagent {
        meta: CommandMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
    },
    CloseSubagent {
        meta: CommandMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
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
            | Self::AttachDevelopmentPlugin { meta, .. }
            | Self::DetachDevelopmentPlugin { meta, .. }
            | Self::PinContext { meta, .. }
            | Self::EvictContext { meta, .. }
            | Self::GetContext { meta, .. }
            | Self::GetCost { meta, .. }
            | Self::GetSessionReview { meta, .. }
            | Self::ReviewFile { meta, .. }
            | Self::DumpPrompt { meta, .. }
            | Self::ListSessions { meta, .. }
            | Self::SearchSessions { meta, .. }
            | Self::ListCommands { meta, .. }
            | Self::ListModes { meta, .. }
            | Self::ListModels { meta, .. }
            | Self::ListSettings { meta, .. }
            | Self::SetSetting { meta, .. }
            | Self::ListMcpServers { meta, .. }
            | Self::ListRuntimeServices { meta, .. }
            | Self::AddMcpHttpServer { meta, .. }
            | Self::AddMcpStdioServer { meta, .. }
            | Self::RemoveMcpServer { meta, .. }
            | Self::ReviewMcpServer { meta, .. }
            | Self::ApproveMcpServer { meta, .. }
            | Self::SetMcpServerEnabled { meta, .. }
            | Self::ListPermissions { meta, .. }
            | Self::AddSessionPermissionRule { meta, .. }
            | Self::RemoveSessionPermissionRule { meta, .. }
            | Self::RemoveQueuedMessage { meta, .. }
            | Self::ClearQueuedMessages { meta, .. }
            | Self::RenameSession { meta, .. }
            | Self::ExportSession { meta, .. }
            | Self::RevokePermissionApproval { meta, .. }
            | Self::BeginProviderAuth { meta, .. }
            | Self::ConfigureBuiltinProvider { meta, .. }
            | Self::CompleteProviderAuth { meta, .. }
            | Self::CancelProviderAuth { meta, .. }
            | Self::SearchWorkspaceFiles { meta, .. }
            | Self::PreviewWorkspaceFile { meta, .. }
            | Self::GetWorkspaceStatus { meta, .. }
            | Self::GetWorkspaceDiff { meta, .. }
            | Self::ListSubagents { meta, .. }
            | Self::ReplaySubagent { meta, .. }
            | Self::ContinueSubagent { meta, .. }
            | Self::InterruptSubagent { meta, .. }
            | Self::CloseSubagent { meta, .. }
            | Self::ShutdownHost { meta, .. } => meta,
        }
    }

    /// Returns the target session for session-scoped commands.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::CreateSession { .. }
            | Self::ListSessions { .. }
            | Self::SearchSessions { .. }
            | Self::ListModels { .. }
            | Self::ShutdownHost { .. } => None,
            Self::ResumeSession { session_id, .. }
            | Self::AttachSession { session_id, .. }
            | Self::SendMessage { session_id, .. }
            | Self::Interrupt { session_id, .. }
            | Self::ApproveTool { session_id, .. }
            | Self::ApprovePlan { session_id, .. }
            | Self::AnswerQuestion { session_id, .. }
            | Self::SwitchMode { session_id, .. }
            | Self::SwitchModel { session_id, .. }
            | Self::Compact { session_id, .. }
            | Self::Fork { session_id, .. }
            | Self::Rewind { session_id, .. }
            | Self::TakeDriver { session_id, .. }
            | Self::UserShellStarted { session_id, .. }
            | Self::UserShellEnded { session_id, .. }
            | Self::AttachDevelopmentPlugin { session_id, .. }
            | Self::DetachDevelopmentPlugin { session_id, .. }
            | Self::PinContext { session_id, .. }
            | Self::EvictContext { session_id, .. }
            | Self::GetContext { session_id, .. }
            | Self::GetCost { session_id, .. }
            | Self::DumpPrompt { session_id, .. }
            | Self::GetSessionReview { session_id, .. }
            | Self::ReviewFile { session_id, .. }
            | Self::SearchWorkspaceFiles { session_id, .. }
            | Self::PreviewWorkspaceFile { session_id, .. }
            | Self::GetWorkspaceStatus { session_id, .. }
            | Self::GetWorkspaceDiff { session_id, .. }
            | Self::ListCommands { session_id, .. }
            | Self::ListModes { session_id, .. }
            | Self::ListSettings { session_id, .. }
            | Self::SetSetting { session_id, .. }
            | Self::ListMcpServers { session_id, .. }
            | Self::ListRuntimeServices { session_id, .. }
            | Self::AddMcpHttpServer { session_id, .. }
            | Self::AddMcpStdioServer { session_id, .. }
            | Self::RemoveMcpServer { session_id, .. }
            | Self::ReviewMcpServer { session_id, .. }
            | Self::ApproveMcpServer { session_id, .. }
            | Self::SetMcpServerEnabled { session_id, .. }
            | Self::ListPermissions { session_id, .. }
            | Self::AddSessionPermissionRule { session_id, .. }
            | Self::RemoveSessionPermissionRule { session_id, .. }
            | Self::RemoveQueuedMessage { session_id, .. }
            | Self::ClearQueuedMessages { session_id, .. }
            | Self::RenameSession { session_id, .. }
            | Self::ExportSession { session_id, .. }
            | Self::RevokePermissionApproval { session_id, .. }
            | Self::BeginProviderAuth { session_id, .. }
            | Self::ConfigureBuiltinProvider { session_id, .. }
            | Self::CompleteProviderAuth { session_id, .. }
            | Self::CancelProviderAuth { session_id, .. }
            | Self::ListSubagents { session_id, .. }
            | Self::ReplaySubagent { session_id, .. }
            | Self::ContinueSubagent { session_id, .. }
            | Self::InterruptSubagent { session_id, .. }
            | Self::CloseSubagent { session_id, .. } => Some(session_id),
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
            | Self::AttachDevelopmentPlugin { meta, .. }
            | Self::DetachDevelopmentPlugin { meta, .. }
            | Self::PinContext { meta, .. }
            | Self::EvictContext { meta, .. }
            | Self::GetContext { meta, .. }
            | Self::GetCost { meta, .. }
            | Self::GetSessionReview { meta, .. }
            | Self::ReviewFile { meta, .. }
            | Self::DumpPrompt { meta, .. }
            | Self::ListSessions { meta, .. }
            | Self::SearchSessions { meta, .. }
            | Self::ListCommands { meta, .. }
            | Self::ListModes { meta, .. }
            | Self::ListModels { meta, .. }
            | Self::ListSettings { meta, .. }
            | Self::SetSetting { meta, .. }
            | Self::ListMcpServers { meta, .. }
            | Self::ListRuntimeServices { meta, .. }
            | Self::AddMcpHttpServer { meta, .. }
            | Self::AddMcpStdioServer { meta, .. }
            | Self::RemoveMcpServer { meta, .. }
            | Self::ReviewMcpServer { meta, .. }
            | Self::ApproveMcpServer { meta, .. }
            | Self::SetMcpServerEnabled { meta, .. }
            | Self::ListPermissions { meta, .. }
            | Self::AddSessionPermissionRule { meta, .. }
            | Self::RemoveSessionPermissionRule { meta, .. }
            | Self::RemoveQueuedMessage { meta, .. }
            | Self::ClearQueuedMessages { meta, .. }
            | Self::RenameSession { meta, .. }
            | Self::ExportSession { meta, .. }
            | Self::RevokePermissionApproval { meta, .. }
            | Self::BeginProviderAuth { meta, .. }
            | Self::ConfigureBuiltinProvider { meta, .. }
            | Self::CompleteProviderAuth { meta, .. }
            | Self::CancelProviderAuth { meta, .. }
            | Self::SearchWorkspaceFiles { meta, .. }
            | Self::PreviewWorkspaceFile { meta, .. }
            | Self::GetWorkspaceStatus { meta, .. }
            | Self::GetWorkspaceDiff { meta, .. }
            | Self::ListSubagents { meta, .. }
            | Self::ReplaySubagent { meta, .. }
            | Self::ContinueSubagent { meta, .. }
            | Self::InterruptSubagent { meta, .. }
            | Self::CloseSubagent { meta, .. }
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
    Tokens,
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

/// Terminal disposition of one child-agent invocation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubagentStatus {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    MaxTurns,
}

/// Filesystem isolation selected for one child agent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubagentIsolation {
    /// Run in a private detached Git worktree and return a diff artifact.
    #[default]
    Worktree,
    /// Share the parent workspace. This requires the parent's ordinary write approvals.
    Shared,
}

/// Current parent-visible lifecycle state of a retained child agent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubagentActivity {
    Running,
    Idle,
}

/// Human-readable child metadata exposed only through its owning parent.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SubagentDescriptor {
    pub subagent_id: SubagentId,
    pub child_session_id: SessionId,
    pub task: String,
    pub agent: String,
    pub model: String,
    pub isolation: SubagentIsolation,
    pub activity: SubagentActivity,
}

/// One ordered durable child event carried inside a bounded replay batch.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SubagentReplayItem {
    pub child_sequence: SequenceId,
    pub event: Value,
}

/// A path affected by an isolated child patch.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct TouchedFile {
    pub path: String,
    pub status: TouchedFileStatus,
}

/// Git change kind retained in a child diff manifest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum TouchedFileStatus {
    Added,
    Modified,
    Deleted,
    TypeChanged,
}

/// Complete durable patch returned by an isolated child.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct DiffArtifact {
    pub id: String,
    pub base_commit: String,
    pub touched_files: Vec<TouchedFile>,
    pub unified_diff: String,
}

/// Bounded model-facing reference to a full durable child patch retained by the host.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct DiffArtifactRef {
    pub artifact_id: String,
    pub base_commit: String,
    pub touched_files: Vec<TouchedFile>,
    pub manifest_truncated: bool,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub patch_bytes: u64,
    pub patch_hash: String,
    pub preview: String,
    pub preview_truncated: bool,
}

/// Predictable result returned from `spawn_agent` and workflow agent steps.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct SubagentResult {
    pub subagent_id: SubagentId,
    pub session_id: SessionId,
    pub status: SubagentStatus,
    pub final_text: String,
    #[serde(default)]
    pub touched_files: Vec<String>,
    pub diff_artifact: Option<DiffArtifact>,
    pub usage: Usage,
    pub cost: Cost,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub turns: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub duration_millis: u64,
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

/// Token accounting derived from one provider billing disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionTokenAccounting {
    /// The disposition is not subscription quota.
    NotApplicable,
    /// The provider reported a valid token count.
    Metered(u64),
    /// Subscription quota was reported without a usable token count.
    Unavailable,
}

impl Cost {
    /// Returns the canonical token-accounting interpretation for subscription quota.
    #[must_use]
    pub fn subscription_token_accounting(&self) -> SubscriptionTokenAccounting {
        let Self::SubscriptionQuota { used, unit } = self else {
            return SubscriptionTokenAccounting::NotApplicable;
        };
        if !unit
            .as_deref()
            .is_some_and(|unit| unit.eq_ignore_ascii_case("tokens"))
        {
            return SubscriptionTokenAccounting::Unavailable;
        }
        used.as_deref()
            .and_then(|used| used.parse::<u64>().ok())
            .map_or(
                SubscriptionTokenAccounting::Unavailable,
                SubscriptionTokenAccounting::Metered,
            )
    }
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

/// Delivery lifetime owned by an engine event variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineEventDelivery {
    /// Stored in the ordered session log and replayed after reconnect.
    Durable,
    /// Returned only to the requesting connection.
    Connection,
    /// Broadcast as live progress without advancing the durable cursor.
    Transient,
}

/// Non-durable event tags that still belong to a live session stream.
pub const TRANSIENT_ENGINE_EVENT_TYPES: &[&str] = &[
    "subagent_progress",
    "compaction_attempt_started",
    "compaction_text_delta",
    "compaction_thinking_delta",
];

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
    SessionReviewReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        review: SessionReview,
    },
    SessionReviewUpdated {
        meta: CommandAckMeta,
        session_id: SessionId,
        path: String,
        decision: ReviewFileDecision,
        review: SessionReview,
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
    SessionForked {
        meta: CommandAckMeta,
        parent_session_id: SessionId,
        child: SessionDescriptor,
        at_turn: TurnId,
    },
    SessionExported {
        meta: CommandAckMeta,
        session_id: SessionId,
        output_path: String,
    },
    SessionsListed {
        meta: CommandAckMeta,
        sessions: Vec<SessionDescriptor>,
    },
    SubagentsListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        subagents: Vec<SubagentDescriptor>,
    },
    SubagentReplayBatch {
        meta: CommandAckMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
        child_session_id: SessionId,
        events: Vec<SubagentReplayItem>,
    },
    SubagentReplayCompleted {
        meta: CommandAckMeta,
        session_id: SessionId,
        subagent_id: SubagentId,
        /// Last child sequence included in this page, if the page is non-empty.
        through_sequence: Option<SequenceId>,
        /// Cursor to pass as `after_sequence` for the next forward page.
        next_cursor: Option<SequenceId>,
        /// Durable tail observed by the descriptor-stable page scan.
        tail_sequence: Option<SequenceId>,
        /// Whether another forward page remains after `next_cursor`.
        has_more: bool,
        /// Number of durable child events preceding the first event in this page.
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        events_before_page: u64,
        /// Whether the requested view omitted events before or after this page.
        truncated: bool,
    },
    SessionsSearchReady {
        meta: CommandAckMeta,
        query: String,
        sessions: Vec<SessionDescriptor>,
        truncated: bool,
    },
    CommandDescriptorsListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        commands: Vec<CommandDescriptor>,
        truncated: bool,
    },
    ModesListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        modes: Vec<ModeDescriptor>,
        truncated: bool,
    },
    ModelsListed {
        meta: CommandAckMeta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session_id: Option<SessionId>,
        models: Vec<ModelDescriptor>,
        #[serde(default)]
        aliases: Vec<ModelAliasDescriptor>,
        #[serde(default)]
        providers: Vec<ProviderDescriptor>,
        #[serde(default)]
        cached: bool,
        #[serde(default)]
        truncated: bool,
    },
    SettingsListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        settings: Vec<UserSettingDescriptor>,
    },
    McpServersListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        servers: Vec<McpServerDescriptor>,
    },
    RuntimeServicesListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        services: Vec<RuntimeServiceDescriptor>,
    },
    McpServerApprovalReviewed {
        meta: CommandAckMeta,
        session_id: SessionId,
        review: McpApprovalReview,
    },
    PermissionsListed {
        meta: CommandAckMeta,
        session_id: SessionId,
        permissions: PermissionStateDescriptor,
    },
    ProviderAuthStarted {
        meta: CommandAckMeta,
        session_id: SessionId,
        attempt_id: ProviderAuthAttemptId,
        provider: String,
        challenge: ProviderAuthChallenge,
        warnings: Vec<String>,
    },
    ProviderConfigured {
        meta: CommandAckMeta,
        session_id: SessionId,
        provider: String,
        auth_kind: ProviderAuthKind,
    },
    ProviderAuthFinished {
        meta: CommandAckMeta,
        session_id: SessionId,
        attempt_id: ProviderAuthAttemptId,
        provider: String,
        success: bool,
        message: String,
        warnings: Vec<String>,
    },
    ProviderActivationFinished {
        meta: CommandAckMeta,
        session_id: SessionId,
        provider: String,
        success: bool,
        message: String,
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
    WorkspaceDiffReady {
        meta: CommandAckMeta,
        session_id: SessionId,
        diff: WorkspaceDiff,
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
    QueuedMessageRemoved {
        meta: EventMeta,
        #[serde(with = "decimal_u64")]
        #[schemars(with = "String")]
        #[ts(type = "string")]
        position: u64,
    },
    QueuedMessagesCleared {
        meta: EventMeta,
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
    /// Human-facing title selected asynchronously after the first successful
    /// assistant turn. This is durable so replay and session lists agree.
    SessionTitleUpdated {
        meta: EventMeta,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        cost: Option<Cost>,
    },
    /// A plugin-originated user message was admitted through the bounded
    /// machine boundary. The ordinary message/turn events remain authoritative
    /// for conversation reconstruction.
    PluginMessageInjected {
        meta: EventMeta,
        plugin_id: String,
        content: String,
        queued: bool,
    },
    /// Session-local status text published by an approved plugin.
    PluginStatusChanged {
        meta: EventMeta,
        plugin_id: String,
        status: String,
    },
    /// Session-local UI notification published by an approved plugin.
    UiNotification {
        meta: EventMeta,
        plugin_id: String,
        title: String,
        message: String,
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
    /// Redacted mutation preview retained independently of whether the active
    /// permission mode needed to ask the user. This keeps inline review
    /// available under remembered approvals and YOLO without coupling display
    /// state to a permission dialog.
    ToolDiffReady {
        meta: EventMeta,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        diff: UnifiedDiff,
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
        /// False when zero capacity means unknown rather than exhausted.
        #[serde(default)]
        context_window_known: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        context_window_reason: Option<String>,
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
    /// Connection-scoped boundary for one compaction provider attempt.
    /// This never enters the durable session sequence.
    CompactionAttemptStarted {
        session_id: SessionId,
        summary_turn_id: TurnId,
        attempt: u32,
    },
    /// Connection-scoped provider text produced while a compaction summary is
    /// generated. This never enters the durable session sequence.
    CompactionTextDelta {
        session_id: SessionId,
        summary_turn_id: TurnId,
        attempt: u32,
        text: String,
    },
    /// Connection-scoped provider reasoning produced during compaction.
    CompactionThinkingDelta {
        session_id: SessionId,
        summary_turn_id: TurnId,
        attempt: u32,
        text: String,
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
    CompactionFailed {
        meta: EventMeta,
        summary_turn_id: TurnId,
    },
    SubagentSpawned {
        meta: EventMeta,
        subagent_id: SubagentId,
        child_session_id: SessionId,
        task: String,
    },
    SubagentFinished {
        meta: EventMeta,
        subagent_id: SubagentId,
        result: SubagentResult,
    },
    /// Connection-scoped child progress. This is never appended to the parent log.
    SubagentProgress {
        parent_session_id: SessionId,
        subagent_id: SubagentId,
        child_session_id: SessionId,
        child_sequence: Option<SequenceId>,
        event: Value,
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
        /// BLAKE3 hash of the canonical mode semantics.
        definition_fingerprint: String,
    },
    PermissionModeChanged {
        meta: EventMeta,
        /// Session-local override. `None` restores the configured policy.
        mode: Option<String>,
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
        /// Exact provider route selected for this session, or `None` for the
        /// alias's automatic fallback chain.
        #[serde(default)]
        provider: Option<String>,
        /// Durable per-session effort applied to this selection, including
        /// concrete provider/model routes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        thinking: Option<crate::config::ThinkingLevel>,
    },
    /// The user explicitly chose to start the selected model without prior
    /// conversation. System and project instructions remain available.
    ModelContextCleared {
        meta: EventMeta,
        strategy: ModelContextTransfer,
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
    /// Returns the authoritative delivery lifetime for this event variant.
    #[must_use]
    pub fn delivery(&self) -> EngineEventDelivery {
        match self {
            Self::SubagentProgress { .. }
            | Self::CompactionAttemptStarted { .. }
            | Self::CompactionTextDelta { .. }
            | Self::CompactionThinkingDelta { .. } => EngineEventDelivery::Transient,
            _ if self.meta().is_some() => EngineEventDelivery::Durable,
            _ => EngineEventDelivery::Connection,
        }
    }

    /// Returns durable session metadata. Non-durable events return `None`.
    #[must_use]
    pub fn meta(&self) -> Option<&EventMeta> {
        match self {
            Self::CommandAcknowledged { .. }
            | Self::ContextSnapshotReady { .. }
            | Self::CostSnapshotReady { .. }
            | Self::SessionReviewReady { .. }
            | Self::SessionReviewUpdated { .. }
            | Self::PromptDumpReady { .. }
            | Self::SessionReplayCompleted { .. }
            | Self::SessionForked { .. }
            | Self::SessionExported { .. }
            | Self::SessionsListed { .. }
            | Self::SubagentsListed { .. }
            | Self::SubagentReplayBatch { .. }
            | Self::SubagentReplayCompleted { .. }
            | Self::SessionsSearchReady { .. }
            | Self::CommandDescriptorsListed { .. }
            | Self::ModesListed { .. }
            | Self::ModelsListed { .. }
            | Self::SettingsListed { .. }
            | Self::McpServersListed { .. }
            | Self::RuntimeServicesListed { .. }
            | Self::McpServerApprovalReviewed { .. }
            | Self::PermissionsListed { .. }
            | Self::ProviderAuthStarted { .. }
            | Self::ProviderConfigured { .. }
            | Self::ProviderAuthFinished { .. }
            | Self::ProviderActivationFinished { .. }
            | Self::WorkspaceFilesFound { .. }
            | Self::WorkspaceFilePreviewReady { .. }
            | Self::WorkspaceStatusReady { .. }
            | Self::WorkspaceDiffReady { .. }
            | Self::SubagentProgress { .. }
            | Self::CompactionAttemptStarted { .. }
            | Self::CompactionTextDelta { .. }
            | Self::CompactionThinkingDelta { .. }
            | Self::HostShutdown { .. } => None,
            Self::SessionCreated { meta, .. }
            | Self::WorkspaceRootsChanged { meta, .. }
            | Self::DriverChanged { meta, .. }
            | Self::MessageQueued { meta, .. }
            | Self::QueuedMessageRemoved { meta, .. }
            | Self::QueuedMessagesCleared { meta, .. }
            | Self::UserMessageAccepted { meta, .. }
            | Self::SessionTitleUpdated { meta, .. }
            | Self::PluginMessageInjected { meta, .. }
            | Self::PluginStatusChanged { meta, .. }
            | Self::UiNotification { meta, .. }
            | Self::ConversationTurnCommitted { meta, .. }
            | Self::ConversationRewound { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::TextDelta { meta, .. }
            | Self::ThinkingDelta { meta, .. }
            | Self::CitationDelta { meta, .. }
            | Self::ToolCallStarted { meta, .. }
            | Self::ToolApprovalNeeded { meta, .. }
            | Self::ToolDiffReady { meta, .. }
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
            | Self::CompactionFailed { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentFinished { meta, .. }
            | Self::ToolOutputPruned { meta, .. }
            | Self::ModeChanged { meta, .. }
            | Self::PermissionModeChanged { meta, .. }
            | Self::PlanSubmitted { meta, .. }
            | Self::PlanReviewed { meta, .. }
            | Self::ModelChanged { meta, .. }
            | Self::ModelContextCleared { meta, .. }
            | Self::ContextItemPinned { meta, .. }
            | Self::ContextItemEvicted { meta, .. }
            | Self::UserShellStateChanged { meta, .. }
            | Self::HookFailed { meta, .. }
            | Self::CommandFinished { meta, .. }
            | Self::GuardTriggered { meta, .. }
            | Self::Error { meta, .. } => Some(meta),
        }
    }

    /// Mutable durable session metadata for storage adapters and validators.
    /// Non-durable events return `None`.
    #[must_use]
    pub fn meta_mut(&mut self) -> Option<&mut EventMeta> {
        match self {
            Self::CommandAcknowledged { .. }
            | Self::ContextSnapshotReady { .. }
            | Self::CostSnapshotReady { .. }
            | Self::SessionReviewReady { .. }
            | Self::SessionReviewUpdated { .. }
            | Self::PromptDumpReady { .. }
            | Self::SessionReplayCompleted { .. }
            | Self::SessionForked { .. }
            | Self::SessionExported { .. }
            | Self::SessionsListed { .. }
            | Self::SubagentsListed { .. }
            | Self::SubagentReplayBatch { .. }
            | Self::SubagentReplayCompleted { .. }
            | Self::SessionsSearchReady { .. }
            | Self::CommandDescriptorsListed { .. }
            | Self::ModesListed { .. }
            | Self::ModelsListed { .. }
            | Self::SettingsListed { .. }
            | Self::McpServersListed { .. }
            | Self::RuntimeServicesListed { .. }
            | Self::McpServerApprovalReviewed { .. }
            | Self::PermissionsListed { .. }
            | Self::ProviderAuthStarted { .. }
            | Self::ProviderConfigured { .. }
            | Self::ProviderAuthFinished { .. }
            | Self::ProviderActivationFinished { .. }
            | Self::WorkspaceFilesFound { .. }
            | Self::WorkspaceFilePreviewReady { .. }
            | Self::WorkspaceStatusReady { .. }
            | Self::WorkspaceDiffReady { .. }
            | Self::SubagentProgress { .. }
            | Self::CompactionAttemptStarted { .. }
            | Self::CompactionTextDelta { .. }
            | Self::CompactionThinkingDelta { .. }
            | Self::HostShutdown { .. } => None,
            Self::SessionCreated { meta, .. }
            | Self::WorkspaceRootsChanged { meta, .. }
            | Self::DriverChanged { meta, .. }
            | Self::MessageQueued { meta, .. }
            | Self::QueuedMessageRemoved { meta, .. }
            | Self::QueuedMessagesCleared { meta, .. }
            | Self::UserMessageAccepted { meta, .. }
            | Self::SessionTitleUpdated { meta, .. }
            | Self::PluginMessageInjected { meta, .. }
            | Self::PluginStatusChanged { meta, .. }
            | Self::UiNotification { meta, .. }
            | Self::ConversationTurnCommitted { meta, .. }
            | Self::ConversationRewound { meta, .. }
            | Self::TurnStarted { meta, .. }
            | Self::TextDelta { meta, .. }
            | Self::ThinkingDelta { meta, .. }
            | Self::CitationDelta { meta, .. }
            | Self::ToolCallStarted { meta, .. }
            | Self::ToolApprovalNeeded { meta, .. }
            | Self::ToolDiffReady { meta, .. }
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
            | Self::CompactionFailed { meta, .. }
            | Self::SubagentSpawned { meta, .. }
            | Self::SubagentFinished { meta, .. }
            | Self::ToolOutputPruned { meta, .. }
            | Self::ModeChanged { meta, .. }
            | Self::PermissionModeChanged { meta, .. }
            | Self::PlanSubmitted { meta, .. }
            | Self::PlanReviewed { meta, .. }
            | Self::ModelChanged { meta, .. }
            | Self::ModelContextCleared { meta, .. }
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
    use super::{
        ClientCommand, ClientId, CommandAckMeta, CommandMeta, Cost, EngineEvent,
        EngineEventDelivery, EventMeta, McpEnvironmentEntry, ModeId, RequestId, SequenceId,
        SessionId, SubagentId, SubscriptionTokenAccounting, TranscriptFormat,
    };

    #[test]
    fn cost_owns_subscription_token_interpretation() {
        let metered = Cost::SubscriptionQuota {
            used: Some("736".to_owned()),
            unit: Some("TOKENS".to_owned()),
        };
        let missing = Cost::SubscriptionQuota {
            used: None,
            unit: Some("tokens".to_owned()),
        };
        let other = Cost::AiCredits {
            credits_micros: 1,
            nominal_amount_micros: None,
            currency: None,
        };

        assert_eq!(
            metered.subscription_token_accounting(),
            SubscriptionTokenAccounting::Metered(736)
        );
        assert_eq!(
            missing.subscription_token_accounting(),
            SubscriptionTokenAccounting::Unavailable
        );
        assert_eq!(
            other.subscription_token_accounting(),
            SubscriptionTokenAccounting::NotApplicable
        );
    }

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

    #[test]
    fn session_id_parser_owns_the_path_component_grammar() {
        for value in ["session", "session-1", "session_1", "session.1"] {
            assert_eq!(SessionId::parse(value), Ok(SessionId(value.to_owned())));
        }
        for value in ["", ".", "..", "../escape", "has/slash", "has space"] {
            assert!(SessionId::parse(value).is_err(), "accepted {value:?}");
        }
        assert!(SessionId::parse("a".repeat(super::MAX_SESSION_ID_BYTES)).is_ok());
        assert!(SessionId::parse("a".repeat(super::MAX_SESSION_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn command_session_accessor_distinguishes_host_and_session_commands() {
        let meta = CommandMeta {
            protocol_version: 1,
            client_id: ClientId("client".to_owned()),
            request_id: RequestId("request".to_owned()),
        };
        let session = SessionId("session".to_owned());
        let scoped = ClientCommand::AttachSession {
            meta: meta.clone(),
            session_id: session.clone(),
            last_seen_sequence: None,
            role: super::ClientRole::Driver,
        };
        let host = ClientCommand::ListSessions { meta };

        assert_eq!(scoped.session_id(), Some(&session));
        assert_eq!(host.session_id(), None);
    }

    #[test]
    fn session_export_command_and_result_have_stable_wire_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let command = ClientCommand::ExportSession {
            meta: CommandMeta {
                protocol_version: 1,
                client_id: ClientId("driver".to_owned()),
                request_id: RequestId("export".to_owned()),
            },
            session_id: SessionId("session".to_owned()),
            format: TranscriptFormat::Markdown,
            output_path: "/tmp/transcript.md".to_owned(),
            force: true,
        };
        let command = serde_json::to_value(command)?;
        assert_eq!(command["type"], "export_session");
        assert_eq!(command["format"], "markdown");
        assert_eq!(command["output_path"], "/tmp/transcript.md");
        assert_eq!(command["force"], true);

        let event = EngineEvent::SessionExported {
            meta: CommandAckMeta {
                protocol_version: 1,
                client_id: ClientId("driver".to_owned()),
                request_id: RequestId("export".to_owned()),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            },
            session_id: SessionId("session".to_owned()),
            output_path: "/private/tmp/transcript.md".to_owned(),
        };
        let event = serde_json::to_value(event)?;
        assert_eq!(event["type"], "session_exported");
        assert_eq!(event["output_path"], "/private/tmp/transcript.md");
        Ok(())
    }

    #[test]
    fn session_rename_command_has_a_stable_wire_shape() -> Result<(), Box<dyn std::error::Error>> {
        let command = ClientCommand::RenameSession {
            meta: CommandMeta {
                protocol_version: 1,
                client_id: ClientId("picker".to_owned()),
                request_id: RequestId("rename".to_owned()),
            },
            session_id: SessionId("session".to_owned()),
            title: "Auth refactor".to_owned(),
        };
        let command = serde_json::to_value(command)?;
        assert_eq!(command["type"], "rename_session");
        assert_eq!(command["session_id"], "session");
        assert_eq!(command["title"], "Auth refactor");
        Ok(())
    }

    #[test]
    fn mcp_stdio_management_commands_have_stable_redacted_wire_shapes()
    -> Result<(), Box<dyn std::error::Error>> {
        let meta = CommandMeta {
            protocol_version: 1,
            client_id: ClientId("picker".to_owned()),
            request_id: RequestId("mcp-stdio".to_owned()),
        };
        let secret = "wire-secret-canary";
        let command = ClientCommand::AddMcpStdioServer {
            meta: meta.clone(),
            session_id: SessionId("session".to_owned()),
            name: "docs".to_owned(),
            executable: "/usr/local/bin/docs-mcp".to_owned(),
            args: vec!["--stdio".to_owned()],
            environment: vec![McpEnvironmentEntry {
                key: "DOCS_TOKEN".to_owned(),
                value: secret.to_owned(),
            }],
        };
        let debug = format!("{command:?}");
        assert!(debug.contains("DOCS_TOKEN"));
        assert!(!debug.contains(secret));
        let wire = serde_json::to_value(command)?;
        assert_eq!(wire["type"], "add_mcp_stdio_server");
        assert_eq!(wire["session_id"], "session");
        assert_eq!(wire["name"], "docs");
        assert_eq!(wire["executable"], "/usr/local/bin/docs-mcp");
        assert_eq!(wire["args"], serde_json::json!(["--stdio"]));
        assert_eq!(
            wire["environment"],
            serde_json::json!([{"key":"DOCS_TOKEN","value":secret}])
        );

        let remove = serde_json::to_value(ClientCommand::RemoveMcpServer {
            meta,
            session_id: SessionId("session".to_owned()),
            name: "docs".to_owned(),
        })?;
        assert_eq!(remove["type"], "remove_mcp_server");
        assert_eq!(remove["session_id"], "session");
        assert_eq!(remove["name"], "docs");
        Ok(())
    }

    #[test]
    fn subagent_replay_completion_exposes_page_and_tail_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let event = EngineEvent::SubagentReplayCompleted {
            meta: CommandAckMeta {
                protocol_version: 1,
                client_id: ClientId("driver".to_owned()),
                request_id: RequestId("replay".to_owned()),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            },
            session_id: SessionId("parent".to_owned()),
            subagent_id: SubagentId("child".to_owned()),
            through_sequence: Some(SequenceId(15)),
            next_cursor: Some(SequenceId(15)),
            tail_sequence: Some(SequenceId(30)),
            has_more: true,
            events_before_page: 8,
            truncated: true,
        };
        let wire = serde_json::to_value(event)?;
        assert_eq!(wire["through_sequence"], "15");
        assert_eq!(wire["next_cursor"], "15");
        assert_eq!(wire["tail_sequence"], "30");
        assert_eq!(wire["has_more"], true);
        assert_eq!(wire["events_before_page"], "8");
        assert_eq!(wire["truncated"], true);
        Ok(())
    }

    #[test]
    fn event_delivery_is_owned_by_the_protocol_variant() {
        let connection = EngineEvent::SessionExported {
            meta: CommandAckMeta {
                protocol_version: 1,
                client_id: ClientId("driver".to_owned()),
                request_id: RequestId("export".to_owned()),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
            },
            session_id: SessionId("session".to_owned()),
            output_path: "/tmp/export.md".to_owned(),
        };
        let transient = EngineEvent::SubagentProgress {
            parent_session_id: SessionId("session".to_owned()),
            subagent_id: SubagentId("child".to_owned()),
            child_session_id: SessionId("child-session".to_owned()),
            child_sequence: None,
            event: serde_json::json!({"type": "progress"}),
        };
        let durable = EngineEvent::ModeChanged {
            meta: EventMeta {
                protocol_version: 1,
                session_id: SessionId("session".to_owned()),
                sequence_id: SequenceId(1),
                emitted_at: "2026-01-01T00:00:00Z".to_owned(),
                caused_by: None,
            },
            mode: ModeId("execute".to_owned()),
            definition_fingerprint: "fixture".to_owned(),
        };

        assert_eq!(connection.delivery(), EngineEventDelivery::Connection);
        assert_eq!(transient.delivery(), EngineEventDelivery::Transient);
        assert_eq!(durable.delivery(), EngineEventDelivery::Durable);
    }
}
