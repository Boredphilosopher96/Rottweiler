//! Public tool extension API, registry, and first-party tools.
//!
//! This crate deliberately does not make permission decisions. The core engine must inspect a
//! tool's [`CapabilityManifest`], pass the invocation through its permission chokepoint, and only
//! then call [`Tool::execute`].

mod bash;
mod builtins;
mod files;
mod interaction;
mod registry;
mod search;
mod symbols;
mod web;

pub use bash::{
    BashInput, BashTool, CommandExecutor, CommandFixtureRedactor, CommandOutcome, CommandRequest,
    CommandSafety, ExecutionLease, IdentityCommandFixtureRedactor, RecordingCommandExecutor,
    ReplayCommandExecutor, TokioCommandExecutor, classify_safe_command,
};
pub use builtins::{BuiltinDependencies, BuiltinHandles, register_builtins};
pub use files::{
    EditInput, EditOperation, EditTool, MultiEditInput, MultiEditTool, ReadInput, ReadTool,
    WriteInput, WriteTool,
};
pub use interaction::{
    AskUserInput, AskUserTool, QuestionAsker, SubmitPlanTool, TodoAction, TodoInput, TodoItem,
    TodoStatus, TodoTool,
};
pub use registry::{
    ApprovalPreview, CancellationToken, CapabilityManifest, MutationScope, NoopOutputSink, Tool,
    ToolContext, ToolDescriptor, ToolError, ToolLimits, ToolOutputChunk, ToolOutputSink,
    ToolRegistry, ToolResult,
};
pub use rw_intel::SymbolIndex;
pub use search::{GlobInput, GlobTool, GrepInput, GrepTool, LsInput, LsTool};
pub use symbols::{SymbolsInput, SymbolsTool};
pub use web::{FetchRequest, FetchResponse, WebFetchInput, WebFetchTool, WebFetcher};
