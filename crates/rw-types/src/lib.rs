//! Stable data types shared by Rottweiler's headless engine and its clients.
//!
//! This crate is the source of truth for the client protocol. Wire-facing
//! algebraic enums use internally tagged, named-field variants so Rust, JSON
//! Schema, and TypeScript all retain the same discriminated-union shape.

pub mod allocation;
pub mod command_receipt;

mod accounting;
pub use accounting::{ProviderCallActuals, ProviderCallIdentity};

pub mod attachment_contract;
pub mod config;
mod error;
pub mod extension_contract;
pub mod extension_events;
pub mod extension_ui;
pub mod hook_contract;
mod ir;
pub mod mcp;
mod permission_mode;
mod protocol;
pub mod schema;
pub mod todo;
pub mod transcript;
pub mod release_contract {
    include!("generated/release_contract.rs");
}
pub mod update_contract;
pub mod workflow;

pub use error::Error;

pub use config::PermissionDecision;
pub use ir::{
    Block, ImageRef, Role, ToolCallId, ToolInvocationId, ToolOutput, ToolOutputPart, Turn, TurnMeta,
};
pub use mcp::{MAX_MCP_SERVER_ID_BYTES, MCP_SERVER_ID_PATTERN, McpServerId, McpServerIdError};
pub use permission_mode::PermissionModeDescriptor;
pub use protocol::{
    AccountingAttribution, Answer, ApprovalBinding, ApprovalDecision, Attachment, AttachmentData,
    BudgetLevel, BudgetScope, BudgetUnit, CacheBreakpoint, ClientCommand, ClientId, ClientRole,
    CommandAckMeta, CommandDescriptor, CommandExecution, CommandMeta, CommandOutcome, CommandReply,
    CommandSource, CompactionReason, ContextItemId, ContextItemKind, ContextItemSnapshot,
    ContextItemState, ContextSnapshot, Cost, CostSnapshot, DiffArtifact, DiffArtifactRef,
    EngineError, EngineErrorCategory, EngineEvent, EngineEventDelivery, EventMeta,
    MAX_CLIENT_READS, MAX_COMMAND_REPLY_BYTES, MAX_SESSION_ID_BYTES, McpApprovalReview,
    McpEnvironmentEntry, McpServerDescriptor, McpServerState, ModeDescriptor, ModeId, ModelAlias,
    ModelAliasDescriptor, ModelCacheBehavior, ModelCapabilities, ModelCatalogSnapshot,
    ModelContextTransfer, ModelDescriptor, ModelSwitchQuestion, PermissionApprovalDescriptor,
    PermissionApprovalScope, PermissionRuleDescriptor, PermissionStateDescriptor, PlanArtifact,
    PlanDecision, PlanStep, PromptDump, PromptTool, ProviderAuthAttemptId, ProviderAuthChallenge,
    ProviderAuthKind, ProviderDescriptor, ProviderNextAction, Question, QuestionId, QuestionOption,
    QuestionResponseKind, RequestId, ReviewFileDecision, ReviewFileStatus, RewindSourcePosition,
    RewindTarget, RuntimeServiceDescriptor, RuntimeServiceKind, SequenceId, SessionDescriptor,
    SessionId, SessionIdError, SessionMode, SessionReview, SessionReviewFile, ShellId,
    StoredAttachment, SubagentActivity, SubagentDescriptor, SubagentId, SubagentIsolation,
    SubagentResult, SubagentStatus, SubscriptionTokenAccounting, TRANSIENT_ENGINE_EVENT_TYPES,
    ToolCapability, ToolOutputStream, TouchedFile, TouchedFileStatus, TranscriptFormat,
    TurnAccounting, TurnId, TurnStatus, UnifiedDiff, UnrestorablePath, Usage,
    UserSettingDescriptor, WorkspaceDiff, WorkspaceFileMatch, WorkspaceFilePreview,
    WorkspaceRootDescriptor, WorkspaceStatus,
};

/// Version of the protocol emitted by these types.
pub const PROTOCOL_VERSION: u16 = 1;

pub use rw_operation_contract::{OperationLifetime, ProgressAmount, ToolProgress};
