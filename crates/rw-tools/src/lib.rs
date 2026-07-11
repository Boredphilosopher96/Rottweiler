//! Public tool extension API, registry, and first-party tools.
//!
//! This crate deliberately does not make permission decisions. The core engine must inspect a
//! tool's [`CapabilityManifest`], pass the invocation through its permission chokepoint, and only
//! then call [`Tool::execute`].

mod bash;
mod builtins;
mod files;
mod intelligence;
mod interaction;
mod protocol;
mod registry;
mod search;
mod symbols;
mod web;
mod worktree;

pub use bash::{
    BashInput, BashSandboxMode, BashTool, CommandExecutor, CommandFixtureRedactor, CommandOutcome,
    CommandRequest, CommandSafety, CommandSafetyClassifier, ExecutionLease,
    IdentityCommandFixtureRedactor, RecordingCommandExecutor, ReplayCommandExecutor,
    TokioCommandExecutor, classify_safe_command,
};
pub use builtins::{BuiltinDependencies, BuiltinHandles, register_builtins};
pub use files::{
    EditInput, EditOperation, EditTool, MultiEditInput, MultiEditTool, ReadInput, ReadTool,
    WriteInput, WriteTool,
};
pub use intelligence::{
    CodeIntelligenceProvider, DefinitionTool, DiagnosticsInput, DiagnosticsTool, PositionInput,
    ReferencesTool, RenameInput, RenameTool, SandboxedLspSpawner, discover_sandboxed_lsp_servers,
};
pub use interaction::{
    AskUserInput, AskUserTool, QuestionAsker, SubmitPlanTool, TodoAction, TodoInput, TodoItem,
    TodoStatus, TodoTool,
};
pub use protocol::{
    ProtocolChildLauncher, ProtocolChildRequest, ProtocolProcessHandle, ProtocolSandboxPolicy,
    SandboxedProtocolLauncher, SpawnedProtocolChild,
};
pub use registry::{
    ApprovalPreview, CancellationToken, CapabilityManifest, McpToolPolicy, MutationScope,
    NoopOutputSink, SubagentEventSink, SubagentLifecycleEvent, SubagentLifecycleMode,
    SubagentProgressEvent, Tool, ToolContext, ToolDescriptor, ToolError, ToolLimits,
    ToolOutputChunk, ToolOutputSink, ToolRegistry, ToolResult, WorkspaceBinding,
    validate_mcp_virtual_tool,
};
pub use rw_intel::{
    CodeIntelligence, Diagnostic, DiagnosticSeverity, IntelligenceBackend, IntelligenceResult,
    Language, Location, LspConfig, LspProcessHandle, LspProcessSpawner, LspServerConfig, Position,
    Range, RenameResult, SpawnedLspProcess, SymbolIndex, WorkspaceUriMapper,
};
#[doc(hidden)]
pub use rw_sandbox::{
    EgressDecision, EgressPin, EgressPolicy, NetworkPolicy, SandboxError, SandboxPolicy,
    SandboxSupport, SupervisedEgressProxy, UpstreamProxy,
    maybe_run_helper as maybe_run_sandbox_helper, normalize_egress_domain, probe as probe_sandbox,
    probe_policy_egress, shell_launch_plan,
};
pub use rw_types::{DiffArtifact, TouchedFile, TouchedFileStatus};
pub use search::{GlobInput, GlobTool, GrepInput, GrepTool, LsInput, LsTool};
pub use symbols::{SymbolsInput, SymbolsTool, WorkspaceSymbolIndex};
pub use web::{
    ConfiguredSearchApi, FetchRequest, FetchResponse, WebFetchInput, WebFetchTool, WebFetcher,
    WebSearchInput, WebSearchRequest, WebSearchResponse, WebSearchResult, WebSearchSource,
    WebSearchTool, WebSearcher,
};
pub use worktree::{
    ApplyWorktreeDiffInput, ApplyWorktreeDiffTool, ChildReturnArtifact, DiffArtifactAuthority,
    SessionDiffArtifactAuthority, WorktreeIsolation, WorktreeLease, WorktreeLeaseRecord,
    WorktreeLimits,
};
