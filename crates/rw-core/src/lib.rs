//! Headless Rottweiler agent engine.

mod admin;
mod copilot_credentials;
mod engine;
mod host;
mod instructions;
mod permission;
mod provider_factory;
mod subscription_credentials;

pub use rw_types::config::{BudgetConfig, CompactionConfig, Config, PermissionConfig};
pub use rw_types::{
    AccountingAttribution, AttachmentData, CommandDescriptor, ContextItemId, ContextSnapshot,
    CostSnapshot, ModeId, ModelAlias, ModelCacheBehavior, ModelCapabilities, ModelDescriptor,
    PlanDecision, PromptDump, WorkspaceFileMatch, WorkspaceFilePreview, WorkspaceStatus,
};

pub use admin::{
    AdminError, DEFAULT_MODEL_CATALOG_URL, GitHubCopilotLogin, GitHubCopilotLoginResult,
    ModelCatalogRefresh, OAuthLogin, OAuthLoginResult, ProviderApiKey, ProviderLogin,
    ProviderLoginCancellation, ResolvedProviderApiKey, begin_oauth_login, begin_provider_login,
    default_provider_api_key_credential_id, refresh_model_catalog, resolve_provider_api_key,
    store_provider_api_key,
};
pub use engine::{
    AgentLoopError, AgentTurnStatus, BudgetLedgerQuery, BudgetLedgerTotals, ContextSurgeryAction,
    EventClock, FolderTrustController, FolderTrustOperation, InterruptedToolRepair,
    MessageDisposition, ModelContextMetadata, ModelDriver, MutationCheckpoint,
    MutationCheckpointCoordinator, MutationCheckpointOutcome, NoopFolderTrustController,
    NoopMutationCheckpointCoordinator, NoopSecretRedactor, NoopSessionEventSink,
    NoopWorkspaceRootController, RecoveredQuestion, RewindCheckpoint, SESSION_EVENT_VERSION,
    SecretRedactor, SessionActor, SessionActorConfig, SessionCommandAction, SessionCommandContext,
    SessionCommandOutput, SessionEventSink, SessionHandle, SessionProjectionError,
    SessionRecoveredState, SessionSnapshot, SessionSubscription, SessionUsage, SystemEventClock,
    TOOL_CANCELLATION_GRACE, WorkspaceRootController, WorkspaceRuntimeGeneration,
    builtin_command_registry, builtin_hook_dispatcher, project_session_events,
};
pub use host::{
    BoundClient, CreateSessionRequest, EngineHost, EngineHostConfig, HostError, HostQueryService,
    HostedSession, SessionFactory,
};
pub use instructions::{
    MAX_ROOT_INSTRUCTIONS_BYTES, ProjectInstructions, ProjectInstructionsError,
    base_agent_system_turn, initial_session_context, load_root_project_instructions,
};
pub use permission::{
    ClearedSessionPermissions, HeadlessPermissionMode, PermissionApprovalSnapshot,
    PermissionApprovalSummary, PermissionApprover, PermissionGate, PermissionGenerationUpdate,
    PermissionOutcome, PermissionRequest,
};
pub use provider_factory::{
    ProviderFactory, ProviderFactoryError, ProviderRuntime, ResolvedModel, cost_from_model_metadata,
};
pub use rw_providers::{
    ProviderModelMetadata, TokenUsage as ModelTokenUsage, UsageAccounting as ModelAccounting,
};
pub use rw_types::PROTOCOL_VERSION;
pub use rw_types::{
    Answer, ClientCommand, ClientId, CommandAckMeta, CommandMeta, CommandOutcome, Cost,
    EngineError, EngineErrorCategory, EngineEvent, EventMeta, QuestionId, RequestId, SequenceId,
    SessionDescriptor, SessionId, ShellId, ToolOutputStream, TurnId, TurnStatus, UnrestorablePath,
    Usage,
};

/// Stable construction and protocol surface for executable frontends.
///
/// Frontends own presentation and input handling while this facade exposes the
/// provider-neutral replay protocol, first-party tool boundaries, and shared IR
/// needed to assemble a headless runtime. Provider and tool implementations
/// remain in their lower architectural layers.
pub mod runtime_support {
    pub use rw_providers::{
        BoxEventStream, CacheBreakpointSupport, CacheHint, Capabilities, FinishReason,
        FixtureRedactor, GuardedHttpFetchError, GuardedHttpFetchRequest, GuardedHttpFetchResponse,
        PricingTable, Provider, ProviderError, ProviderErrorKind, ProviderEvent, ProviderRequest,
        ProxyAuthentication, ProxyEnvironment, ProxySettings, Recorder, ReplayProvider,
        Secret as ProviderSecret, ThinkingLevel, ToolChoice, ToolDefinition, WireMode,
        deny_outbound_network_for_process, guarded_http_fetch,
    };
    pub use rw_tools::{
        AskUserInput, AskUserTool, BashSandboxMode, BashTool, CancellationToken,
        CapabilityManifest, CommandExecutor, CommandFixtureRedactor, CommandSafetyClassifier,
        EditTool, EgressDecision, EgressPin, EgressPolicy, ExecutionLease, FetchRequest,
        FetchResponse, GlobTool, GrepTool, LsTool, MultiEditTool, MutationScope,
        NetworkPolicy as SandboxNetworkPolicy, QuestionAsker, ReadTool, RecordingCommandExecutor,
        ReplayCommandExecutor, SandboxPolicy, SandboxSupport, SupervisedEgressProxy, SymbolIndex,
        SymbolsTool, TodoTool, TokioCommandExecutor, Tool, ToolContext, ToolDescriptor, ToolError,
        ToolLimits, ToolRegistry, ToolResult, UpstreamProxy, WebFetchTool, WebFetcher,
        WorkspaceSymbolIndex, WriteTool, maybe_run_sandbox_helper, probe_policy_egress,
    };
    pub use rw_types::{
        Answer, ApprovalBinding, ApprovalDecision, Block, ClientCommand, ClientId, CommandOutcome,
        Cost, EngineError, EngineErrorCategory, EngineEvent, EventMeta, QuestionId, Role,
        SequenceId, SessionId, ToolCallId, ToolCapability, ToolOutput, ToolOutputStream, Turn,
        TurnId, TurnMeta, TurnStatus, UnrestorablePath, Usage, config::PermissionDecision,
    };
}

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "core";
