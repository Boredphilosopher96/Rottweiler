//! Headless Rottweiler agent engine.

mod admin;
mod copilot_credentials;
mod engine;
mod host;
mod init;
mod instructions;
mod mcp;
mod model_catalog;
mod orchestration;
mod permission;
pub mod provider_admission;
mod provider_factory;
mod subscription_credentials;
#[cfg(unix)]
pub mod todo_projection;
#[cfg(unix)]
pub mod transcript;
mod update;

pub use rw_types::config::{
    BudgetConfig, CompactionConfig, Config, PermissionConfig, PermissionDecision, ProviderConfig,
    ThinkingLevel, UpdateChannel,
};
pub use rw_types::{
    AccountingAttribution, Attachment, AttachmentData, CommandDescriptor, CommandSource,
    ContextItemId, ContextSnapshot, CostSnapshot, McpApprovalReview, McpEnvironmentEntry,
    McpServerDescriptor, McpServerState, ModeId, ModelAlias, ModelAliasDescriptor,
    ModelCacheBehavior, ModelCapabilities, ModelCatalogSnapshot, ModelDescriptor,
    PermissionApprovalDescriptor, PermissionApprovalScope, PermissionRuleDescriptor,
    PermissionStateDescriptor, PlanDecision, PromptDump, ProviderAuthChallenge, ProviderAuthKind,
    ProviderDescriptor, ProviderNextAction, ReviewFileDecision, ReviewFileStatus,
    RuntimeServiceDescriptor, RuntimeServiceKind, SessionReview, SessionReviewFile,
    UserSettingDescriptor, WorkspaceDiff, WorkspaceFileMatch, WorkspaceFilePreview,
    WorkspaceRootDescriptor, WorkspaceStatus,
};

pub use admin::{
    AdminError, DEFAULT_MODEL_CATALOG_URL, EMBEDDED_UPDATE_BASE_URL, GitHubCopilotLogin,
    GitHubCopilotLoginResult, ModelCatalogRefresh, OAuthLogin, OAuthLoginResult,
    PreparedGitHubCopilotCredential, PreparedOAuthCredential, ProviderApiKey, ProviderLogin,
    ProviderLoginCancellation, ResolvedProviderApiKey, UpdateNetworkClient, begin_oauth_login,
    begin_provider_login, default_provider_api_key_credential_id, prepare_update_network,
    refresh_model_catalog, resolve_provider_api_key, store_provider_api_key,
    validate_stored_provider_credential,
};
#[cfg(unix)]
pub use engine::recovery;

pub use engine::{
    AdmittedEventBatch, AgentLoopError, AgentTurnStatus, BudgetLedgerQuery, BudgetLedgerTotals,
    CommandToolCall, CommandToolOutputKind, ContextSurgeryAction, EventBatchPlan,
    EventBatchReservation, EventClock, ExtensionStateView, FolderTrustController,
    FolderTrustOperation, InterruptedToolRepair, MessageDisposition, ModelContextMetadata,
    ModelDriver, MutationCheckpoint, MutationCheckpointCoordinator, MutationCheckpointOutcome,
    NoopFolderTrustController, NoopMutationCheckpointCoordinator, NoopSecretRedactor,
    NoopSessionEventSink, NoopSessionExtensionController, NoopSessionResources,
    NoopWorkspaceRootController, PluginSessionCapability, RecoveredQuestion, RewindCheckpoint,
    SESSION_EVENT_VERSION, SecretRedactor, SessionActor, SessionActorConfig, SessionCommandAction,
    SessionCommandContext, SessionCommandOutput, SessionEventReadView, SessionEventSink,
    SessionExtensionController, SessionExtensionSnapshot, SessionHandle, SessionProjectionError,
    SessionProjector, SessionRecoveredState, SessionReplayLimits, SessionResources,
    SessionSnapshot, SessionSubscription, SessionUsage, StartupNotification, SystemEventClock,
    TOOL_CANCELLATION_GRACE, WorkspaceRootController, WorkspaceRuntimeGeneration,
    builtin_command_registry, builtin_hook_dispatcher, commit_session_events,
    project_session_events, project_session_events_with_modes, project_session_read_view,
};
pub use host::{
    BoundClient, CompletedForkOperation, CreateSessionRequest, EngineHost, EngineHostConfig,
    ForkOperationKey, ForkOperationState, ForkSessionRequest, HostError, HostMcpService,
    HostQueryService, HostReadChannel, HostReply, HostRuntimeService, HostSubagentService,
    HostedSession, PreparedForkOperation, ProviderApiKeySubmission, ProviderAuthAttempt,
    ProviderAuthCompletion, SessionFactory,
};
pub use init::{
    DEFAULT_INIT_FILE_BUDGET_BYTES, InitDepth, InitError, InitPlan, MAX_INIT_SCAN_ENTRIES,
    apply_init_plan, plan_init,
};
pub use instructions::{
    InstructionStack, MAX_INSTRUCTION_CONTEXT_BYTES, MAX_INSTRUCTION_FILES,
    MAX_ROOT_INSTRUCTIONS_BYTES, ProjectInstructions, ProjectInstructionsError,
    base_agent_system_turn, initial_session_context, load_instruction_stack,
    load_nested_instruction_stack, load_root_project_instructions,
};
pub use mcp::{
    LoopbackMcpAuthority, McpOAuthBinding, McpOAuthLogin, McpOAuthLoginConfig, McpOAuthLoginResult,
    McpOAuthRefreshBinding, McpPolicyProxy, ProductionMcpHttpClient, ProductionMcpHttpConnector,
    ProductionMcpHttpError, ToonMcpEncoder, VaultMcpTokenProvider, begin_mcp_oauth_login,
    encode_mcp_oauth_credential, register_mcp_tools,
};
pub use model_catalog::{
    CachedModelCatalog, ModelCatalogError, ModelCatalogSource, merge_model_catalog_provider,
    retain_model_catalog_provider,
};
pub use orchestration::{
    ActorSubagentSessionFactory, DEFAULT_SUBAGENT_CONCURRENCY, DEFAULT_SUBAGENT_MAX_DEPTH,
    DEFAULT_SUBAGENT_MAX_DURATION, DEFAULT_SUBAGENT_MAX_TURNS, NoopSubagentMetadataStore,
    OrchestrationError, SpawnAgentTool, SubagentHandle, SubagentLaunch, SubagentLimits,
    SubagentMetadataStore, SubagentObserver, SubagentOrchestrator, SubagentProgressObserver,
    SubagentRecoveryPhase, SubagentRecoveryPolicy, SubagentRecoveryRecord, SubagentRequest,
    SubagentSession, SubagentSessionFactory, SubagentTurnResult, WorktreeSubagentSessionFactory,
    diff_artifact_reference, incomplete_subagent_lifecycles, interrupted_subagent_recovery_result,
    subagent_result_tool_output,
};
pub use permission::{
    ClearedSessionPermissions, PermissionApprovalSnapshot, PermissionApprovalSummary,
    PermissionApprover, PermissionGate, PermissionGenerationUpdate, PermissionOutcome,
    PermissionRequest,
};
pub use provider_factory::{
    AdapterKind, BUILTIN_PROVIDER_PROFILES, BuiltinProviderId, BuiltinProviderProfile,
    ModelPricingSource, ProviderFactory, ProviderFactoryError, ProviderModelCatalogSource,
    ProviderNativeWebSearchFactory, ProviderRuntime, ResolvedModel, cost_from_model_metadata,
};
pub use rw_providers::{
    ProviderModelMetadata, TokenUsage as ModelTokenUsage, UsageAccounting as ModelAccounting,
};
pub use rw_types::PROTOCOL_VERSION;
pub use rw_types::{
    Answer, ClientCommand, ClientId, ClientRole, CommandAckMeta, CommandMeta, CommandOutcome,
    CommandReply, Cost, EngineError, EngineErrorCategory, EngineEvent, EventMeta, QuestionId,
    RequestId, SequenceId, SessionDescriptor, SessionId, ShellId, SubagentActivity,
    SubagentDescriptor, SubagentId, SubagentIsolation, SubagentResult, ToolOutputStream,
    TranscriptFormat, TurnId, TurnStatus, UnrestorablePath, Usage,
};
pub use update::{
    EMBEDDED_ROOT_KEYS_JSON, EMBEDDED_ROOT_THRESHOLD, EMBEDDED_ROOT_VERSION, TrustedRoot,
    UpdateHighWaterMark, UpdateVerificationError, UpdateVerificationPolicy, VerifiedUpdate,
    restore_trusted_root_chain, verify_update_metadata, verify_update_metadata_chain,
};

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "core";
