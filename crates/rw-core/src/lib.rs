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
mod provider_factory;
mod subscription_credentials;
mod update;

pub use rw_types::config::{
    BudgetConfig, CompactionConfig, Config, PermissionConfig, PermissionDecision, ProviderConfig,
    ThinkingLevel, UpdateChannel,
};
pub use rw_types::{
    AccountingAttribution, Attachment, AttachmentData, CommandDescriptor, CommandSource,
    ContextItemId, ContextSnapshot, CostSnapshot, McpApprovalReview, McpEnvironmentEntry,
    McpServerDescriptor, McpServerState, ModeId, ModelAlias, ModelAliasDescriptor,
    ModelCacheBehavior, ModelCapabilities, ModelCatalogSnapshot, ModelDescriptor, PermissionAction,
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
pub use engine::{
    AgentLoopError, AgentTurnStatus, BudgetLedgerQuery, BudgetLedgerTotals, CommandToolCall,
    CommandToolOutputKind, ContextSurgeryAction, EventClock, FolderTrustController,
    FolderTrustOperation, InterruptedToolRepair, MessageDisposition, ModelContextMetadata,
    ModelDriver, MutationCheckpoint, MutationCheckpointCoordinator, MutationCheckpointOutcome,
    NoopFolderTrustController, NoopMutationCheckpointCoordinator, NoopSecretRedactor,
    NoopSessionEventSink, NoopWorkspaceRootController, PluginSessionCapability, RecoveredQuestion,
    RewindCheckpoint, SESSION_EVENT_VERSION, SecretRedactor, SessionActor, SessionActorConfig,
    SessionCommandAction, SessionCommandContext, SessionCommandOutput, SessionEventSink,
    SessionHandle, SessionProjectionError, SessionRecoveredState, SessionSnapshot,
    SessionSubscription, SessionUsage, StartupNotification, SystemEventClock,
    TOOL_CANCELLATION_GRACE, WorkspaceRootController, WorkspaceRuntimeGeneration,
    builtin_command_registry, builtin_hook_dispatcher, project_session_events,
};
pub use host::{
    BoundClient, CompletedForkOperation, CreateSessionRequest, EngineHost, EngineHostConfig,
    ForkOperationKey, ForkOperationState, ForkSessionRequest, HostError, HostMcpService,
    HostQueryService, HostRuntimeService, HostSubagentService, HostedSession,
    PreparedForkOperation, ProviderApiKeySubmission, ProviderAuthAttempt, ProviderAuthCompletion,
    SessionFactory, SubagentReplay,
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
pub use model_catalog::{CachedModelCatalog, ModelCatalogError, ModelCatalogSource};
pub use orchestration::{
    ActorSubagentSessionFactory, DEFAULT_SUBAGENT_CONCURRENCY, DEFAULT_SUBAGENT_MAX_DEPTH,
    DEFAULT_SUBAGENT_MAX_DURATION, DEFAULT_SUBAGENT_MAX_TURNS, NoopSubagentMetadataStore,
    OrchestrationError, SpawnAgentTool, SubagentHandle, SubagentLaunch, SubagentLimits,
    SubagentMetadataStore, SubagentObserver, SubagentOrchestrator, SubagentPermissionMode,
    SubagentProgressObserver, SubagentRecoveryPhase, SubagentRecoveryPolicy,
    SubagentRecoveryRecord, SubagentRequest, SubagentSession, SubagentSessionFactory,
    SubagentTurnResult, WorktreeSubagentSessionFactory, diff_artifact_reference,
    incomplete_subagent_lifecycles, interrupted_subagent_recovery_result,
    subagent_result_tool_output,
};
pub use permission::{
    ClearedSessionPermissions, HeadlessPermissionMode, PermissionApprovalSnapshot,
    PermissionApprovalSummary, PermissionApprover, PermissionGate, PermissionGenerationUpdate,
    PermissionOutcome, PermissionRequest,
};
pub use provider_factory::{
    ProviderFactory, ProviderFactoryError, ProviderModelCatalogSource, ProviderNativeWebSearcher,
    ProviderRuntime, ResolvedModel, cost_from_model_metadata,
};
pub use rw_providers::{
    ProviderModelMetadata, TokenUsage as ModelTokenUsage, UsageAccounting as ModelAccounting,
};
pub use rw_types::PROTOCOL_VERSION;
pub use rw_types::{
    Answer, ClientCommand, ClientId, ClientRole, CommandAckMeta, CommandMeta, CommandOutcome, Cost,
    EngineError, EngineErrorCategory, EngineEvent, EventMeta, QuestionId, RequestId, SequenceId,
    SessionDescriptor, SessionId, ShellId, SubagentActivity, SubagentDescriptor, SubagentId,
    SubagentIsolation, SubagentResult, ToolOutputStream, TranscriptFormat, TurnId, TurnStatus,
    UnrestorablePath, Usage,
};
pub use update::{
    EMBEDDED_ROOT_KEY_ID, EMBEDDED_ROOT_KEYS_JSON, EMBEDDED_ROOT_PUBLIC_KEY,
    EMBEDDED_ROOT_THRESHOLD, EMBEDDED_ROOT_VERSION, TrustedRoot, UpdateHighWaterMark,
    UpdateVerificationError, UpdateVerificationPolicy, VerifiedUpdate, restore_trusted_root_chain,
    verify_update_metadata, verify_update_metadata_chain,
};

/// Stable construction and protocol surface for executable frontends.
///
/// Frontends own presentation and input handling while this facade exposes the
/// provider-neutral replay protocol, first-party tool boundaries, and shared IR
/// needed to assemble a headless runtime. Provider and tool implementations
/// remain in their lower architectural layers.
pub mod runtime_support {
    /// Extension and plugin composition surface for executable frontends.
    pub mod plugin {
        pub use rw_ext::{
            ApprovalRequirement, ApprovalStore, ApprovalStoreError, CapabilityEnforcer,
            CapabilityViolation, DenyPushHandler, ExecutableIdentity, HookDispatchStatus,
            HookDispatcher, HookEvent, HookHandler, HookRegistration, LaunchedPluginProcess,
            METHOD_SESSION_INJECT_MESSAGE, METHOD_SESSION_SET_STATUS, METHOD_TOOL_CALL,
            METHOD_UI_NOTIFY, PluginBoundaryRedactor, PluginCapabilities, PluginEventRouter,
            PluginHost, PluginLauncher, PluginManifest, PluginProcessConfig,
            PluginProcessConfigError, PluginProcessError, PluginRpcClient, PluginRpcError,
            PluginSandboxMode, PluginSandboxProfile, PluginStdin, PluginStdout,
            PluginToolCapability, PluginToolEffect, PushHandler, RpcCommandAdapter, RpcHookHandler,
            RpcProviderAdapter, RpcToolAdapter, SupervisedPluginProcess, approve_plugin_launch,
            plugin_launch_approval_requirement,
        };
    }

    /// MCP composition surface for executable frontends.
    pub mod mcp {
        pub use rw_mcp::{
            BridgeError, EngineMcpBridge, EngineTool, FilesystemSpool, McpClient,
            McpConnectionApprovalPolicy, McpConnector, McpError, McpLimits, McpManager,
            McpServerAuthority, McpServerConfig, McpStdioSandboxPolicy, McpToolCapabilityOverrides,
            McpTransportConfig, OverflowReference, OverflowSpool, RottweilerMcpServerFactory,
            SandboxedStdioConnector, ServerId, ServerState, ServerStatus, SessionSummary,
            serve_stdio,
        };
    }

    pub use rw_ext::{
        ActiveWasmExtensionLoadReport, AgentDefinition, AgentPermissionMode, AgentPromptSource,
        AgentRegistry, AgentRegistryError, ArtifactLocation, ArtifactOrigin, ArtifactScope,
        CLAUDE_FRONTMATTER_MIGRATION, CommandDescriptor, CommandExecutionError, CommandHandler,
        CommandInvocation, CommandRegistry, CommandRegistryError, CommandSource, CommandTemplate,
        DiscoveredAgent, DiscoveredCommand, DiscoveredShellHook, DiscoveredSkill,
        DiscoveredWorkflow, ExtensionCatalog, ExtensionDiscoveryConfig, ExtensionDiscoveryError,
        ExtensionRegistryCatalog, HookDirective, HookDispatchStatus, HookDispatcher, HookEffect,
        HookError, HookEvent, HookFailurePolicy, HookHandler, HookInvocation, HookRegistration,
        InstalledWasmExtension, InstalledWasmExtensionStatus, LoadedAgent, PluginManifest,
        RegistryError, RegistryRelease, TemplatePart, WasmActivationCatalog,
        WasmExtensionActivation, WasmHookHostError, WasmHookLimits, WasmProcessHook,
        WorkflowCondition, WorkflowOnFail, WorkflowRunError, WorkflowRunReport, WorkflowRunner,
        WorkflowStep, WorkflowStepArtifact, WorkflowStepExecutionError, WorkflowStepExecutor,
        WorkflowStepReport, WorkflowStepRequest, WorkflowStepTarget,
        activate_installed_wasm_extension, compose_agent_registry, deactivate_wasm_extension,
        inspect_installed_wasm_extension, install_verified_component,
        list_installed_wasm_extensions, load_active_wasm_extensions,
        load_active_wasm_extensions_report, load_installed_wasm_extension, read_activation_catalog,
    };
    pub use rw_providers::{
        BoxEventStream, CacheBreakpointSupport, CacheHint, Capabilities, FinishReason,
        FixtureRedactor, GuardedHttpFetchError, GuardedHttpFetchRequest, GuardedHttpFetchResponse,
        GuardedHttpMethod, GuardedHttpRequest, GuardedHttpStreamResponse,
        NativeWebSearchCapability, NativeWebSearchRequest, PricingTable, Provider, ProviderError,
        ProviderErrorKind, ProviderEvent, ProviderReachabilityRequest, ProviderRequest,
        ProxyAuthentication, ProxyEnvironment, ProxySettings, ProxySource, Recorder,
        ReplayProvider, Secret as ProviderSecret, ThinkingLevel, ToolChoice, ToolDefinition,
        WireMode, default_models_path, deny_outbound_network_for_process, guarded_http_fetch,
        guarded_http_request, provider_reachability_probe,
    };
    pub use rw_tools::{
        ApplyWorktreeDiffTool, AskUserInput, AskUserTool, BackgroundKillTool, BackgroundOutputTool,
        BackgroundProcessLimits, BackgroundProcessManager, BackgroundStatusTool, BashSandboxMode,
        BashTool, CancellationToken, CapabilityManifest, CodeIntelligence,
        CodeIntelligenceProvider, CommandExecutor, CommandFixtureRedactor,
        CommandOutcome as ToolCommandOutcome, CommandRequest, CommandSafetyClassifier,
        ConfiguredSearchApi, DefinitionTool, Diagnostic, DiagnosticSeverity, DiagnosticsTool,
        EditTool, EgressDecision, EgressPin, EgressPolicy, ExecutionLease, FetchRequest,
        FetchResponse, GlobTool, GrepTool, IntelligenceBackend, IntelligenceResult, Language,
        Location, LsTool, LspConfig, LspServerConfig, MultiEditTool, MutationScope,
        NetworkPolicy as SandboxNetworkPolicy, NoopOutputSink, Position, QuestionAsker, Range,
        ReadTool, RecordingCommandExecutor, ReferencesTool, RenameResult, RenameTool,
        ReplayCommandExecutor, SandboxPolicy, SandboxSupport, SandboxedLspSpawner,
        SandboxedProtocolLauncher, SubagentEventSink, SubagentLifecycleEvent,
        SubagentLifecycleMode, SubagentProgressEvent, SupervisedEgressProxy, SymbolIndex,
        SymbolsTool, TodoTool, TokioCommandExecutor, Tool, ToolContext, ToolDescriptor, ToolError,
        ToolLimits, ToolOutputChunk, ToolOutputSink, ToolRegistry, ToolResult, UpstreamProxy,
        WebFetchTool, WebFetcher, WebSearchRequest, WebSearchResponse, WebSearchResult,
        WebSearchSource, WebSearchTool, WebSearcher, WorkspaceBinding, WorkspaceSymbolIndex,
        WorkspaceUriMapper, WorktreeIsolation, WorktreeLeaseRecord, WorktreeLimits, WriteTool,
        discover_sandboxed_lsp_servers, maybe_run_sandbox_helper, normalize_egress_domain,
        probe_policy_egress, probe_sandbox, shell_launch_plan,
    };
    pub use rw_types::{
        Answer, ApprovalBinding, ApprovalDecision, Block, ClientCommand, ClientId, CommandOutcome,
        Cost, DiffArtifactRef, EngineError, EngineErrorCategory, EngineEvent, EventMeta,
        QuestionId, Role, SequenceId, SessionId, SessionMode, SubagentId, SubagentIsolation,
        SubagentResult, SubagentStatus, ToolCallId, ToolCapability, ToolOutput, ToolOutputPart,
        ToolOutputStream, Turn, TurnId, TurnMeta, TurnStatus, UnrestorablePath, Usage,
        config::{
            PermissionDecision, PermissionRule, ToolchainConfig, ToolchainRule, WebSearchConfig,
        },
    };
}

/// Identifies this workspace component in diagnostics.
pub const COMPONENT: &str = "core";
