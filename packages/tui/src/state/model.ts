import type {
  Answer,
  AttachmentData,
  BudgetLevel,
  BudgetScope,
  BudgetUnit,
  CommandOutcome,
  CommandSource,
  ContextSnapshot,
  Cost,
  CostSnapshot,
  EngineError,
  McpApprovalReview,
  McpServerDescriptor,
  PlanArtifact,
  PermissionStateDescriptor,
  PromptDump,
  ProviderAuthKind,
  ProviderAuthChallenge,
  ProviderNextAction,
  Question,
  RuntimeServiceDescriptor,
  SubagentStatus as SubagentTerminalStatus,
  ToolCapability,
  ToolOutput,
  ToolOutputStream,
  Turn,
  TurnStatus,
  UnifiedDiff,
  Usage,
} from "../protocol"

export type ConnectionPhase =
  | "idle"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "replaying"
  | "disconnected"
  | "closed"

export interface SequenceGap {
  readonly expected: string
  readonly received: string
}

export interface ConnectionProjection {
  readonly phase: ConnectionPhase
  readonly attempt: number
  readonly error: string | null
  readonly gap: SequenceGap | null
}

/** Presentation-only replay state; durable transcript data still comes from EngineEvent. */
export interface ReplayProjection {
  readonly active: boolean
  readonly sessionId: string | null
  readonly completedThrough: string | null
}

export interface BoundedCommandTextProjection {
  readonly lines: readonly string[]
  readonly omittedLineCount: number
}

export interface StructuredCommandResultRow {
  readonly prefixes: readonly ("bullet" | "indent")[]
  readonly label: string | null
  readonly value: StructuredCommandResultValue
}

export type StructuredCommandResultValue =
  | { readonly kind: "heading" }
  | { readonly kind: "string"; readonly value: string }
  | { readonly kind: "number"; readonly value: number }
  | { readonly kind: "boolean"; readonly value: boolean }
  | { readonly kind: "none" }
  | { readonly kind: "empty_list" }
  | { readonly kind: "redacted" }
  | { readonly kind: "details_omitted" }

/** Bounded semantic content retained for a completed slash command. */
export type CommandResultProjection =
  | {
      readonly kind: "context"
      readonly usedTokens: string
      readonly usableTokens: string
      readonly reservedTokens: string
      readonly contextWindowKnown: boolean
      readonly itemCount: number
      readonly groups: readonly {
        readonly kind: string
        readonly itemCount: number
        readonly estimatedTokens: string
      }[]
    }
  | {
      readonly kind: "cost"
      readonly inputTokens: string
      readonly outputTokens: string
      readonly reasoningTokens: string
      readonly cacheReadTokens: string
      readonly cacheHitBasisPoints: number
      readonly subscriptionQuotaEntries: string
      readonly costUnavailableEntries: string
      readonly monetaryAccountingComplete: boolean
      readonly costMicrosUsd: string
      readonly accountedTurnCount: number
      readonly utcDay: string
    }
  | {
      readonly kind: "help"
      readonly commands: readonly {
        readonly usage: string
        readonly description: string
      }[]
      readonly omittedCommandCount: number
      readonly fallback: BoundedCommandTextProjection | null
    }
  | {
      readonly kind: "status"
      readonly agent: string
      readonly mode: string
      readonly queuedMessages: string
    }
  | {
      readonly kind: "permissions"
      readonly summary: string | null
      readonly mode: string | null
      readonly defaultPermission: string | null
      readonly rememberedApprovals: string | null
      readonly rules: readonly {
        readonly scope: "Project" | "Session"
        readonly decision: string
        readonly target: string
        readonly remembered: boolean
      }[]
      readonly omittedRuleCount: number
    }
  | {
      readonly kind: "mode"
      readonly mode: string | null
      readonly active: boolean
    }
  | {
      readonly kind: "plan"
      readonly title: string | null
      readonly body: BoundedCommandTextProjection | null
    }
  | {
      readonly kind: "review"
      readonly summary: string | null
      readonly files: readonly {
        readonly path: string
        readonly status: string
        readonly note: string
      }[]
      readonly omittedFileCount: number
    }
  | {
      readonly kind: "trust"
      readonly trust: "updated" | "trusted" | "untrusted" | "unknown"
      readonly message: string | null
    }
  | {
      readonly kind: "mcp"
      readonly updated: boolean
      readonly servers: readonly {
        readonly name: string
        readonly status: string
      }[]
      readonly omittedServerCount: number
      readonly fallback: BoundedCommandTextProjection | null
    }
  | {
      readonly kind: "completion"
      readonly title: string
      readonly detail: string | null
    }
  | {
      readonly kind: "message"
      readonly content: BoundedCommandTextProjection
    }
  | {
      readonly kind: "structured"
      readonly rows: readonly StructuredCommandResultRow[]
      readonly omittedRowCount: number
    }
  | { readonly kind: "unsafe_structured" }

export interface TranscriptEntry {
  readonly sequenceId: string
  readonly agentTurn: string
  readonly turn: Turn
  /** Presentation kind for host results that are not conversation turns. */
  readonly presentation?: "conversation" | "command_result" | "shell_result"
  readonly title?: string
  /** Structured semantic content for a completed slash command. */
  readonly commandResult?: CommandResultProjection
  /** Structured foreground-shell content. Kept out of Markdown so commands and
   * terminal output cannot be interpreted as user-authored formatting. */
  readonly shell?: ShellTranscriptPresentation
}

export interface ShellTranscriptPresentation {
  readonly shellId: string
  readonly command: string
  readonly active: boolean
  readonly status: number | null
  readonly capturedOutput: string
  readonly outputTruncated: boolean
}

export interface StreamingCitation {
  readonly uri: string
  readonly title: string | null
}

export interface StreamingTail {
  readonly turnId: string
  readonly text: string
  readonly thinking: string
  readonly citations: readonly StreamingCitation[]
  readonly toolCallIds: readonly string[]
  readonly finished: {
    readonly status: TurnStatus
    readonly usage: Usage
    readonly cost: Cost
  } | null
}

export interface TurnProjection {
  readonly turnId: string
  readonly status: "running" | TurnStatus
  readonly usage: Usage | null
  readonly cost: Cost | null
}

export type ToolStatus = "running" | "awaiting_approval" | "finished"

export interface ToolOutputChunkProjection {
  readonly stream: ToolOutputStream
  readonly chunk: string
}

export interface ToolProjection {
  readonly toolCallId: string
  readonly turnId: string
  readonly name: string
  readonly args: unknown
  readonly status: ToolStatus
  readonly capabilities: readonly ToolCapability[]
  readonly rationale: string | null
  readonly diff: UnifiedDiff | null
  readonly chunks: readonly ToolOutputChunkProjection[]
  readonly output: ToolOutput | null
  readonly isError: boolean | null
  readonly callIndex: number
}

export type TodoStatusProjection = "pending" | "in_progress" | "completed" | "blocked"

/** Bounded, display-safe projection of the session todo tool's latest successful snapshot. */
export interface TodoProjection {
  readonly id: string
  readonly content: string
  readonly status: TodoStatusProjection
}

export type SubagentStatus = "running" | SubagentTerminalStatus

export interface SubagentProjection {
  readonly projectionId: string
  readonly subagentId: string
  readonly parentTurnId: string
  readonly task: string
  readonly spawnedAtMs: number | null
  readonly status: SubagentStatus
  readonly childSessionId: string | null
  readonly lastChildSequence: string | null
  readonly activity: string | null
  readonly summary: string | null
  readonly touchedFileCount: number
  readonly diffArtifactId: string | null
}

export interface QuestionProjection {
  readonly questionId: string
  readonly turnId: string
  readonly questions: readonly Question[]
  readonly answers: readonly Answer[] | null
  readonly answered: boolean
}

export interface CommandAcknowledgement {
  readonly requestId: string
  readonly responseType:
    | "command_acknowledged"
    | "context_snapshot_ready"
    | "cost_snapshot_ready"
    | "session_review_ready"
    | "session_review_updated"
    | "prompt_dump_ready"
    | "session_replay_completed"
    | "session_forked"
    | "session_exported"
    | "sessions_listed"
    | "subagents_listed"
    | "subagent_replay_batch"
    | "subagent_replay_completed"
    | "sessions_search_ready"
    | "command_descriptors_listed"
    | "modes_listed"
    | "models_listed"
    | "settings_listed"
    | "mcp_servers_listed"
    | "runtime_services_listed"
    | "mcp_server_approval_reviewed"
    | "permissions_listed"
    | "provider_auth_started"
    | "provider_configured"
    | "provider_auth_finished"
    | "provider_activation_finished"
    | "workspace_files_found"
    | "workspace_file_preview_ready"
    | "workspace_status_ready"
    | "workspace_diff_ready"
    | "host_shutdown"
  readonly outcome: CommandOutcome | null
  readonly sessionId: string | null
}

export interface QueuedMessageProjection {
  readonly position: string
  readonly content: string
}

export interface BudgetProjection {
  readonly turnId: string
  readonly level: BudgetLevel
  readonly scope: BudgetScope
  readonly unit: BudgetUnit
  readonly current: string
  readonly limit: string
}

export interface CompactionProjection {
  readonly active: boolean
  readonly reason: string | null
  readonly summaryTurnId: string | null
  readonly reclaimedTokens: string | null
  readonly attempt: number | null
  readonly text: string
  readonly thinking: string
}

export interface ShellProjection {
  readonly shellId: string | null
  readonly active: boolean
  readonly status: number | null
  readonly capturedOutput: string | null
}

export interface ProtocolProjection {
  readonly duplicateEvents: number
  readonly invalidEvents: number
  readonly unknownEvents: number
  readonly lastUnknownType: string | null
}

export interface SessionChoice {
  readonly sessionId: string
  readonly title?: string
  readonly workspaceName: string
  readonly model: string
  readonly driverClientId: string | null
  readonly shellActive: boolean
}

export type ReviewFileStatus = "pending" | "accepted" | "reverted"

export interface SessionReviewFileProjection {
  readonly path: string
  readonly unifiedDiff: string
  readonly status: ReviewFileStatus
  readonly truncated: boolean
  readonly unrestorableReason: string | null
  readonly originalHash: string
  readonly currentHash: string
}

export interface SessionReviewProjection {
  readonly sessionId: string
  readonly files: readonly SessionReviewFileProjection[]
}

export interface SessionSearchProjection {
  readonly query: string
  readonly truncated: boolean
}

export interface SessionForkProjection {
  readonly parentSessionId: string
  readonly child: SessionChoice
  readonly atTurn: string | null
}

export interface CommandChoice {
  readonly name: string
  readonly description: string
  readonly usage: string
  readonly source?: CommandSource
}

export interface ModeChoice {
  readonly id: string
  readonly description: string
  readonly current: boolean
}

export interface ModelChoice {
  readonly id: string
  readonly displayName: string
  readonly provider: string
  readonly aliases: readonly string[]
  readonly current: boolean
  readonly available: boolean
  readonly status: string | null
  readonly vision: boolean
  readonly thinking: boolean
  readonly toolCalling: boolean
}

export interface ModelAliasChoice {
  readonly alias: string
  readonly candidates: readonly string[]
  readonly current: boolean
}

export interface ProviderChoice {
  readonly name: string
  readonly authKind: ProviderAuthKind
  readonly nextAction: ProviderNextAction
  readonly configured: boolean
  readonly authenticated: boolean
  readonly reachable: boolean
  readonly modelCount: number
  readonly status: string | null
}

export interface ProviderAuthProjection {
  readonly pending: {
    readonly attemptId: string
    readonly provider: string
    readonly challenge: ProviderAuthChallenge
    readonly warnings: readonly string[]
  } | null
  readonly last: {
    readonly provider: string
    readonly success: boolean
    readonly message: string
    readonly warnings: readonly string[]
  } | null
}

export interface UserSettingChoice {
  readonly key: string
  readonly label: string
  readonly value: string
  readonly choices: readonly string[]
  readonly provenance: string
  readonly appliesImmediately: boolean
}

export interface WorkspaceFileChoice {
  readonly path: string
  readonly isDirectory: boolean
}

export interface WorkspacePreviewProjection {
  readonly path: string
  readonly mediaType: string
  readonly data: AttachmentData
  readonly totalBytes: string
  readonly truncated: boolean
}

export interface WorkspaceStatusProjection {
  readonly workspaceName: string
  readonly branch: string | null
  readonly changedPaths: readonly string[]
  readonly truncated: boolean
}

export interface WorkspaceDiffProjection {
  readonly path: string
  readonly unifiedDiff: string
  readonly truncated: boolean
  readonly binary: boolean
}

export interface WorkspaceRootsProjection {
  readonly generation: string
  readonly effectiveFromTurn: string
  readonly roots: readonly string[]
}

export interface PluginNotificationProjection {
  readonly pluginId: string
  readonly title: string
  readonly message: string
}

export interface RottweilerState {
  readonly connection: ConnectionProjection
  readonly replay: ReplayProjection
  readonly lastSequence: string | null
  readonly transcript: readonly TranscriptEntry[]
  /** Kept separate so streaming deltas never replace or re-layout transcript history. */
  readonly streamingTail: StreamingTail | null
  readonly turns: Readonly<Record<string, TurnProjection>>
  readonly tools: Readonly<Record<string, ToolProjection>>
  readonly todos: readonly TodoProjection[]
  /** Current parent-turn children, kept separate from durable transcript history. */
  readonly subagents: Readonly<Record<string, SubagentProjection>>
  readonly subagentOrder: readonly string[]
  readonly questions: Readonly<Record<string, QuestionProjection>>
  readonly commandAcks: Readonly<Record<string, CommandAcknowledgement>>
  readonly queuedMessages: readonly QueuedMessageProjection[]
  readonly pluginStatuses: Readonly<Record<string, string>>
  readonly pluginNotifications: readonly PluginNotificationProjection[]
  readonly context: ContextSnapshot | null
  readonly cost: CostSnapshot | null
  readonly promptDump: PromptDump | null
  readonly mode: string | null
  readonly pendingPlan: PlanArtifact | null
  readonly approvedPlan: PlanArtifact | null
  readonly model: string | null
  readonly provider: string | null
  readonly driverClientId: string | null
  readonly shell: ShellProjection
  readonly compaction: CompactionProjection
  readonly budgets: readonly BudgetProjection[]
  readonly errors: readonly EngineError[]
  readonly protocol: ProtocolProjection
  readonly sessions: readonly SessionChoice[]
  readonly sessionSearch: SessionSearchProjection | null
  readonly review: SessionReviewProjection | null
  readonly lastFork: SessionForkProjection | null
  readonly commands: readonly CommandChoice[]
  readonly commandsTruncated: boolean
  readonly modes: readonly ModeChoice[]
  readonly modesTruncated: boolean
  readonly models: readonly ModelChoice[]
  readonly modelAliases: readonly ModelAliasChoice[]
  readonly providers: readonly ProviderChoice[]
  readonly providerAuth: ProviderAuthProjection
  readonly modelCatalogCached: boolean
  readonly settings: readonly UserSettingChoice[]
  readonly mcpServers: readonly McpServerDescriptor[]
  readonly runtimeServices: readonly RuntimeServiceDescriptor[]
  readonly mcpApprovalReview: McpApprovalReview | null
  readonly permissions: PermissionStateDescriptor | null
  readonly workspaceFiles: readonly WorkspaceFileChoice[]
  readonly workspacePreview: WorkspacePreviewProjection | null
  readonly workspaceStatus: WorkspaceStatusProjection | null
  readonly workspaceDiff: WorkspaceDiffProjection | null
  readonly workspaceRoots: WorkspaceRootsProjection | null
}

export function createInitialState(): RottweilerState {
  return {
    connection: {
      phase: "idle",
      attempt: 0,
      error: null,
      gap: null,
    },
    replay: { active: false, sessionId: null, completedThrough: null },
    lastSequence: null,
    transcript: [],
    streamingTail: null,
    turns: {},
    tools: {},
    todos: [],
    subagents: {},
    subagentOrder: [],
    questions: {},
    commandAcks: {},
    queuedMessages: [],
    pluginStatuses: {},
    pluginNotifications: [],
    context: null,
    cost: null,
    promptDump: null,
    mode: "execute",
    pendingPlan: null,
    approvedPlan: null,
    model: null,
    provider: null,
    driverClientId: null,
    shell: { shellId: null, active: false, status: null, capturedOutput: null },
    compaction: {
      active: false,
      reason: null,
      summaryTurnId: null,
      reclaimedTokens: null,
      attempt: null,
      text: "",
      thinking: "",
    },
    budgets: [],
    errors: [],
    protocol: {
      duplicateEvents: 0,
      invalidEvents: 0,
      unknownEvents: 0,
      lastUnknownType: null,
    },
    sessions: [],
    sessionSearch: null,
    review: null,
    lastFork: null,
    commands: [],
    commandsTruncated: false,
    modes: [],
    modesTruncated: false,
    models: [],
    modelAliases: [],
    providers: [],
    providerAuth: { pending: null, last: null },
    modelCatalogCached: false,
    settings: [],
    mcpServers: [],
    runtimeServices: [],
    mcpApprovalReview: null,
    permissions: null,
    workspaceFiles: [],
    workspacePreview: null,
    workspaceStatus: null,
    workspaceDiff: null,
    workspaceRoots: null,
  }
}

/** Enter immutable historical presentation without changing reducer semantics. */
export function enterReplayMode(
  state: RottweilerState,
  sessionId: string,
): RottweilerState {
  return {
    ...state,
    replay: { active: true, sessionId, completedThrough: null },
    streamingTail: null,
  }
}
