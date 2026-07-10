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
    AccountingAttribution, Answer, ApprovalDecision, Attachment, AttachmentData, BudgetLevel,
    BudgetScope, BudgetUnit, CacheBreakpoint, ClientCommand, ClientId, ClientRole, CommandAckMeta,
    CommandMeta, CommandOutcome, CompactionReason, ContextItemId, ContextItemKind,
    ContextItemSnapshot, ContextItemState, ContextSnapshot, Cost, CostSnapshot, EngineError,
    EngineErrorCategory, EngineEvent, EventMeta, ModelAlias, PromptDump, PromptTool, Question,
    QuestionId, QuestionOption, QuestionResponseKind, RequestId, RewindTarget, SequenceId,
    SessionId, SubagentId, ToolCapability, ToolOutputStream, TurnAccounting, TurnId, TurnStatus,
    UnrestorablePath, Usage,
};

/// Version of the protocol emitted by these types.
pub const PROTOCOL_VERSION: u16 = 1;
