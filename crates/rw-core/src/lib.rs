//! Headless Rottweiler agent engine.

mod admin;
mod copilot_credentials;
mod engine;
mod instructions;
mod permission;
mod provider_factory;
mod subscription_credentials;

pub use admin::{
    AdminError, DEFAULT_MODEL_CATALOG_URL, GitHubCopilotLogin, GitHubCopilotLoginResult,
    ModelCatalogRefresh, OAuthLogin, OAuthLoginResult, ProviderApiKey, ProviderLogin,
    ProviderLoginCancellation, ResolvedProviderApiKey, begin_oauth_login, begin_provider_login,
    default_provider_api_key_credential_id, refresh_model_catalog, resolve_provider_api_key,
    store_provider_api_key,
};
pub use engine::{
    AgentLoopError, AgentTurnStatus, EventClock, InterruptedToolRepair, MessageDisposition,
    ModelDriver, MutationCheckpoint, MutationCheckpointCoordinator, MutationCheckpointOutcome,
    NoopMutationCheckpointCoordinator, NoopSecretRedactor, NoopSessionEventSink, RecoveredQuestion,
    RewindCheckpoint, SESSION_EVENT_VERSION, SecretRedactor, SessionActor, SessionActorConfig,
    SessionCommandAction, SessionCommandContext, SessionCommandOutput, SessionEventSink,
    SessionHandle, SessionProjectionError, SessionRecoveredState, SessionSnapshot,
    SessionSubscription, SessionUsage, SystemEventClock, TOOL_CANCELLATION_GRACE,
    builtin_command_registry, builtin_hook_dispatcher, project_session_events,
};
pub use instructions::{
    MAX_ROOT_INSTRUCTIONS_BYTES, ProjectInstructions, ProjectInstructionsError,
    base_agent_system_turn, initial_session_context, load_root_project_instructions,
};
pub use permission::{
    HeadlessPermissionMode, PermissionApprover, PermissionGate, PermissionOutcome,
    PermissionRequest,
};
pub use provider_factory::{ProviderFactory, ProviderFactoryError, ProviderRuntime, ResolvedModel};
pub use rw_providers::{ProviderModelMetadata, UsageAccounting as ModelAccounting};
pub use rw_types::{
    Answer, ClientCommand, ClientId, CommandOutcome, Cost, EngineError, EngineErrorCategory,
    EngineEvent, EventMeta, QuestionId, SequenceId, ToolOutputStream, TurnId, TurnStatus,
    UnrestorablePath, Usage,
};

/// Stable construction and protocol surface for executable frontends.
///
/// Frontends own presentation and input handling while this facade exposes the
/// provider-neutral replay protocol, first-party tool boundaries, and shared IR
/// needed to assemble a headless runtime. Provider and tool implementations
/// remain in their lower architectural layers.
pub mod runtime_support {
    pub use rw_providers::{
        BoxEventStream, CacheBreakpointSupport, Capabilities, FinishReason, FixtureRedactor,
        GuardedHttpFetchError, GuardedHttpFetchRequest, GuardedHttpFetchResponse, PricingTable,
        Provider, ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest,
        ProxyEnvironment, ProxySettings, Recorder, ReplayProvider, ThinkingLevel, WireMode,
        deny_outbound_network_for_process, guarded_http_fetch,
    };
    pub use rw_tools::{
        AskUserInput, AskUserTool, BashTool, CancellationToken, CommandExecutor,
        CommandFixtureRedactor, EditTool, ExecutionLease, FetchRequest, FetchResponse, GlobTool,
        GrepTool, LsTool, MultiEditTool, MutationScope, QuestionAsker, ReadTool,
        RecordingCommandExecutor, ReplayCommandExecutor, SymbolIndex, SymbolsTool, TodoTool,
        TokioCommandExecutor, Tool, ToolContext, ToolDescriptor, ToolError, ToolLimits,
        ToolRegistry, ToolResult, WebFetchTool, WebFetcher, WriteTool,
    };
    pub use rw_types::{
        Answer, ApprovalDecision, Block, ClientCommand, ClientId, CommandOutcome, Cost,
        EngineError, EngineErrorCategory, EngineEvent, EventMeta, QuestionId, Role, SequenceId,
        SessionId, ToolCallId, ToolCapability, ToolOutput, ToolOutputStream, Turn, TurnId,
        TurnMeta, TurnStatus, UnrestorablePath, Usage, config::PermissionDecision,
    };
}

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "core";
