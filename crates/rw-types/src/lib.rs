//! Stable data types shared by Rottweiler's headless engine and its clients.
//!
//! This crate is the source of truth for the client protocol. Wire-facing
//! algebraic enums use internally tagged, named-field variants so Rust, JSON
//! Schema, and TypeScript all retain the same discriminated-union shape.

pub mod config;
mod error;
mod ir;
mod protocol;

pub use error::Error;

pub use ir::{Block, ImageRef, Role, ToolCallId, ToolOutput, ToolOutputPart, Turn, TurnMeta};
pub use protocol::{
    AccountingAttribution, Answer, ApprovalBinding, ApprovalDecision, Attachment, AttachmentData,
    BudgetLevel, BudgetScope, BudgetUnit, CacheBreakpoint, ClientCommand, ClientId, ClientRole,
    CommandAckMeta, CommandDescriptor, CommandMeta, CommandOutcome, CommandSource,
    CompactionReason, ContextItemId, ContextItemKind, ContextItemSnapshot, ContextItemState,
    ContextSnapshot, Cost, CostSnapshot, DiffArtifact, DiffArtifactRef, EngineError,
    EngineErrorCategory, EngineEvent, EventMeta, ModeId, ModelAlias, ModelAliasDescriptor,
    ModelCacheBehavior, ModelCapabilities, ModelCatalogSnapshot, ModelDescriptor, PlanArtifact,
    PlanDecision, PlanStep, PromptDump, PromptTool, ProviderAuthAttemptId, ProviderAuthChallenge,
    ProviderAuthKind, ProviderDescriptor, ProviderNextAction, Question, QuestionId, QuestionOption,
    QuestionResponseKind, RequestId, ReviewFileDecision, ReviewFileStatus, RewindTarget,
    SequenceId, SessionDescriptor, SessionId, SessionMode, SessionReview, SessionReviewFile,
    ShellId, StoredAttachment, SubagentId, SubagentIsolation, SubagentResult, SubagentStatus,
    ToolCapability, ToolOutputStream, TouchedFile, TouchedFileStatus, TurnAccounting, TurnId,
    TurnStatus, UnifiedDiff, UnrestorablePath, Usage, UserSettingDescriptor, WorkspaceDiff,
    WorkspaceFileMatch, WorkspaceFilePreview, WorkspaceRootDescriptor, WorkspaceStatus,
};

/// Version of the protocol emitted by these types.
pub const PROTOCOL_VERSION: u16 = 1;
