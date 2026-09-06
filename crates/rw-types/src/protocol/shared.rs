use rw_memory_derive::PrepareAllocation as Allocation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use thiserror::Error;
use ts_rs::TS;

use crate::{PermissionModeDescriptor, config::PermissionDecision};

pub(crate) mod decimal_u64 {
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
        #[derive(
            Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS, Allocation,
        )]
        pub struct $name(pub String);
    };
}

/// Maximum encoded length of a session identifier.
pub const MAX_SESSION_ID_BYTES: usize = 128;

/// Stable identifier of an engine session.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS, Allocation)]
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
/// Maximum retained request identity in host correlation ledgers.
pub const MAX_REQUEST_ID_BYTES: usize = 256;
impl RequestId {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty()
            && self.0.len() <= MAX_REQUEST_ID_BYTES
            && !self.0.chars().any(char::is_control)
    }
}
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
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    JsonSchema,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    TS,
    Allocation,
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct CommandMeta {
    pub protocol_version: u16,
    pub client_id: ClientId,
    pub request_id: RequestId,
}

/// Metadata common to persisted and streamed events.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub struct EventMeta {
    pub protocol_version: u16,
    pub session_id: SessionId,
    pub sequence_id: SequenceId,
    pub emitted_at: String,
    pub caused_by: Option<RequestId>,
}

/// Metadata for immediate command acknowledgements before session sequencing.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
pub enum ClientRole {
    Driver,
    Observer,
}

/// In-band attachment data; protocol messages never require shared files.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum AttachmentData {
    Text { content: String },
    InlineBase64 { data: String },
}

/// User-provided content attached to a message.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[derive(Allocation)]
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

/// Durable attachment content and its identity, independent of mutable local paths.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct StoredAttachment {
    pub data: AttachmentData,
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionDescriptor {
    pub session_id: SessionId,
    /// Human-facing session title.
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
#[derive(Allocation)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct CommandDescriptor {
    pub name: String,
    pub description: String,
    pub usage: String,
    pub source: CommandSource,
}

/// One bounded, credential-free interaction mode exposed to clients.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ModeDescriptor {
    pub id: ModeId,
    pub description: String,
    pub current: bool,
}

/// One engine-mediated user setting exposed to interactive clients.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum McpServerState {
    Disabled {},
    Connecting {},
    Ready {},
    ApprovalRequired {},
    Failed { message: String },
    Stopping {},
}

/// One server in the live session MCP inventory.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct McpApprovalReview {
    pub server: String,
    pub transport: String,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
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
#[serde(deny_unknown_fields)]
#[derive(Allocation)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
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
#[derive(Allocation)]
pub enum PermissionApprovalScope {
    Session,
    Project,
}

/// Stable, typed rule row rendered by permission-management clients.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct PermissionRuleDescriptor {
    /// Opaque stable id accepted by remove operations. Clients never rebuild it.
    pub id: String,
    pub pattern: String,
    pub action: PermissionDecision,
}

/// Opaque remembered approval metadata. Invocation arguments and fingerprints
/// are deliberately absent.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct PermissionApprovalDescriptor {
    pub id: String,
    pub scope: PermissionApprovalScope,
    pub tool_name: String,
    pub summary: String,
}

/// Bounded permission inventory for one live session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
pub enum ModelCacheBehavior {
    None,
    Explicit,
    ProviderManaged,
}

/// Provider-neutral capabilities used by model pickers and attachment checks.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    pub tool_calling: bool,
    pub vision: bool,
    pub thinking: bool,
    pub cache_behavior: ModelCacheBehavior,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub max_context_tokens: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub max_output_tokens: Option<u64>,
}

/// One concrete provider/model discovered from a live authenticated catalog.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    /// Concrete provider-qualified model id.
    pub id: String,
    pub display_name: String,
    /// Sanitized logical provider name. Adapter kind, endpoint, and auth
    /// material remain behind the provider boundary.
    pub provider: String,
    /// Configured role aliases which currently include this concrete model.
    pub aliases: Vec<ModelAlias>,
    pub current: bool,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub status: Option<String>,
    pub capabilities: ModelCapabilities,
}

/// Small provider-blind role mapping shown separately from the live catalog.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
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
#[derive(Allocation)]
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ModelCatalogSnapshot {
    pub aliases: Vec<ModelAliasDescriptor>,
    pub models: Vec<ModelDescriptor>,
    pub providers: Vec<ProviderDescriptor>,
    pub cached: bool,
    pub truncated: bool,
}

/// Relative workspace path returned by fuzzy file search.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFileMatch {
    pub path: String,
    pub is_directory: bool,
}

/// Remote-safe in-band file preview; paths are always workspace-relative.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceStatus {
    pub workspace_name: String,
    pub branch: Option<String>,
    pub changed_paths: Vec<String>,
    pub truncated: bool,
}

/// Bounded current-worktree diff for one exact workspace-relative path.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDiff {
    pub path: String,
    pub unified_diff: String,
    pub truncated: bool,
    pub binary: bool,
}

/// One stable session workspace root. Durable/wire events use only virtual
/// `@root/N` paths; canonical host paths stay in private local metadata.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRootDescriptor {
    pub index: u32,
    pub path: String,
    pub machine_local: bool,
}

/// Optional structured unified diff attached to a mutating-tool approval.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[derive(Allocation)]
pub struct ApprovalBinding {
    pub proposal_id: String,
    pub arguments_hash: String,
    pub base_hash: String,
    pub diff_hash: String,
}

/// Optional structured unified diff attached to a mutating-tool approval.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub description: String,
    pub files_touched: Vec<String>,
    pub verification: String,
}

/// Durable plan submitted before entering execute mode.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct PlanArtifact {
    pub title: String,
    pub summary_md: String,
    pub steps: Vec<PlanStep>,
    pub open_questions: Vec<String>,
}

/// Driver response to a submitted plan.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum PlanDecision {
    Approve,
    Reject,
}

/// Rewind destination selected by a client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
#[derive(Allocation)]
pub enum RewindTarget {
    Turn {
        turn_id: TurnId,
    },
    Source {
        expected_through: SequenceId,
        source: SequenceId,
        turn_id: TurnId,
        position: RewindSourcePosition,
    },
}

/// Completed boundary relative to an effective committed user source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum RewindSourcePosition {
    Before,
    Through,
}

/// Driver decision for one file in the cumulative session review.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum ReviewFileDecision {
    Accept,
    Revert,
}

/// Durable disposition of one file in the cumulative session review.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum ReviewFileStatus {
    Pending,
    Accepted,
    Reverted,
}

/// One deterministic workspace-relative entry in a cumulative session review.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SessionReview {
    pub session_id: SessionId,
    pub files: Vec<SessionReviewFile>,
}

/// Shape of a response accepted for an interactive question.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum QuestionResponseKind {
    Text,
    SelectOne,
}

/// Explicit handling of existing conversation context when changing models.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ModelSwitchQuestion {
    pub model: ModelAlias,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    pub provider: Option<String>,
}

/// A selectable response to an engine question.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub question_id: QuestionId,
    pub value: String,
}

/// Semantic class of one assembled prompt item.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ContextItemState {
    pub pinned: bool,
    pub evicted: bool,
    pub summarized: bool,
    pub pruned: bool,
}

/// One explicit cache boundary after an assembled item.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct CacheBreakpoint {
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(schema_with = "crate::schema::required_nullable::<ContextItemId>")]
    pub after_item_id: Option<ContextItemId>,
}

/// Exact engine-side context breakdown for one assembled request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct ContextSnapshot {
    /// Exact canonical source prefix used to assemble this read.
    #[serde(deserialize_with = "Option::deserialize")]
    pub through: Option<SequenceId>,
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
    pub context_window_known: bool,
    /// Provider-neutral explanation for an unknown context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub context_window_reason: Option<String>,
    pub cache_breakpoints: Vec<CacheBreakpoint>,
    pub items: Vec<ContextItemSnapshot>,
}

/// Usage and billing attributed to one completed agent turn.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
pub enum AccountingAttribution {
    Main,
    Compaction,
    Subagent,
    Title,
}

/// Session-level cost, token, cache, and burn-rate snapshot.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct CostSnapshot {
    pub utc_day: String,
    #[serde(deserialize_with = "Option::deserialize")]
    #[schemars(
        schema_with = "crate::schema::required_nullable::<crate::billing::SubscriptionQuotaSummary>"
    )]
    pub subscription_quota: Option<crate::billing::SubscriptionQuotaSummary>,
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
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_subscription_tokens: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_subscription_tokens: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub trailing_minute_subscription_tokens: u64,
    pub cache_hit_basis_points: u16,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub session_cost_cap_micros_usd: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub daily_cost_cap_micros_usd: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub session_ai_credit_cap_micros: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub daily_ai_credit_cap_micros: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub session_token_cap: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub daily_token_cap: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub spend_rate_alarm_micros_usd_per_minute: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub ai_credit_rate_alarm_micros_per_minute: Option<u64>,
    #[serde(with = "decimal_option_u64")]
    #[schemars(schema_with = "crate::schema::required_nullable::<String>")]
    #[ts(type = "string | null")]
    pub token_rate_alarm_per_minute: Option<u64>,
    pub hard_cap_reached: bool,
    pub session_monetary_accounting_complete: bool,
    pub daily_monetary_accounting_complete: bool,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_subscription_quota_entries: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_cost_unavailable_entries: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub session_non_usd_monetary_entries: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_subscription_quota_entries: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_cost_unavailable_entries: u64,
    #[serde(with = "decimal_u64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub daily_non_usd_monetary_entries: u64,
}

/// Provider-neutral tool definition included in a prompt dump.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct PromptTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Exact assembled model request exposed for prompt transparency.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct PromptDump {
    /// Exact canonical source prefix used to assemble this read.
    #[serde(deserialize_with = "Option::deserialize")]
    pub through: Option<SequenceId>,
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
#[derive(Allocation)]
pub enum TranscriptFormat {
    Markdown,
    Html,
    Json,
}

/// Capabilities used by the permission engine for a tool invocation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
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
#[derive(Allocation)]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
}

/// Terminal state of an agent turn.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
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
#[derive(Allocation)]
pub enum CompactionReason {
    Automatic,
    Manual,
    ProviderOverflow,
}

/// Billing unit evaluated by a budget guardrail.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum BudgetUnit {
    MicrosUsd,
    AiCreditMicros,
    Tokens,
}

/// Severity of a budget transition.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum BudgetLevel {
    Warning,
    SpendRateAlarm,
    HardCap,
}

/// Scope whose spend triggered a budget transition.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum BudgetScope {
    Session,
    Daily,
    TrailingMinute,
}

/// Provider-reported token accounting normalized by the router.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
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
#[derive(Allocation)]
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
#[derive(Allocation)]
pub enum SubagentActivity {
    Running,
    Idle,
}

/// Human-readable child metadata exposed only through its owning parent.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct SubagentDescriptor {
    pub subagent_id: SubagentId,
    pub child_session_id: SessionId,
    pub task: String,
    pub agent: String,
    pub model: String,
    pub isolation: SubagentIsolation,
    pub activity: SubagentActivity,
}

/// A path affected by an isolated child patch.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct TouchedFile {
    pub path: String,
    pub status: TouchedFileStatus,
}

/// Git change kind retained in a child diff manifest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
#[derive(Allocation)]
pub enum TouchedFileStatus {
    Added,
    Modified,
    Deleted,
    TypeChanged,
}

/// Complete durable patch returned by an isolated child.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct DiffArtifact {
    pub id: String,
    pub base_commit: String,
    pub touched_files: Vec<TouchedFile>,
    pub unified_diff: String,
}

/// Bounded model-facing reference to a full durable child patch retained by the host.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub struct SubagentResult {
    pub subagent_id: SubagentId,
    pub session_id: SessionId,
    pub status: SubagentStatus,
    pub final_text: String,
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS, Allocation)]
#[serde(deny_unknown_fields)]
pub struct UnrestorablePath {
    pub path: String,
    pub reason: String,
}

/// Provider-neutral billing/quota disposition for a completed turn. A missing
/// price is never represented as a zero-dollar API charge.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(tag = "kind", rename_all = "snake_case", optional_fields = nullable)]
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
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
#[derive(Allocation)]
#[serde(deny_unknown_fields)]
pub enum CommandOutcome {
    Accepted {},
    Rejected { error: EngineError },
}
