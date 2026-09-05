import { emptyTodos, type TodoState } from "./todos"
import { MAX_TAIL_TEXT_BYTES, utf8Prefix, type ToolOutputBuffer } from "./display-buffer"
import type {
  Answer,
  AttachmentData,
  BudgetLevel,
  BudgetScope,
  BudgetUnit,
  CommandOutcome,
  CommandAckMeta,
  EngineEvent,
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

/** Replay lifecycle state; display history is read through the semantic transcript service. */
export interface ReplayProjection {
  readonly active: boolean
  readonly sessionId: string | null
  readonly completedThrough: string | null
}

export interface ShellActivityProjection {
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
  readonly displayBudget: {
    readonly text: { readonly bytes: number; readonly omittedBytes: number }
    readonly thinking: { readonly bytes: number; readonly omittedBytes: number }
  }
  readonly citations: readonly StreamingCitation[]
  readonly toolCallIds: readonly string[]
  readonly finished: {
    readonly status: TurnStatus
    readonly usage: Usage
    readonly cost: Cost
  } | null
}

export function createStreamingTail(value: Omit<StreamingTail, "displayBudget">): StreamingTail {
  const text = utf8Prefix(value.text, MAX_TAIL_TEXT_BYTES)
  const thinking = utf8Prefix(value.thinking, MAX_TAIL_TEXT_BYTES)
  const textBytes = Buffer.byteLength(text)
  const thinkingBytes = Buffer.byteLength(thinking)
  return { ...value, text, thinking, displayBudget: {
    text: { bytes: textBytes, omittedBytes: Buffer.byteLength(value.text) - textBytes },
    thinking: { bytes: thinkingBytes, omittedBytes: Buffer.byteLength(value.thinking) - thinkingBytes },
  } }
}

export type ActivityTimingProjection =
  | { readonly kind: "unknown" }
  | {
      readonly kind: "open"
      readonly startedAtMs: number
      readonly lastObservedAtMs: number
    }
  | {
      readonly kind: "closed"
      readonly startedAtMs: number | null
      readonly finishedAtMs: number
    }

export interface TurnProjection {
  readonly turnId: string
  readonly status: "running" | TurnStatus
  readonly usage: Usage | null
  readonly cost: Cost | null
  readonly timing: ActivityTimingProjection
}

export type ToolStatus = "running" | "awaiting_approval" | "finished"

export interface ToolProjection {
  readonly toolCallId: string
  readonly invocationId: string
  readonly turnId: string
  readonly name: string
  readonly args: unknown
  readonly status: ToolStatus
  readonly capabilities: readonly ToolCapability[]
  readonly rationale: string | null
  readonly diff: UnifiedDiff | null
  readonly chunks: ToolOutputBuffer
  readonly output: ToolOutput | null
  readonly isError: boolean | null
  readonly callIndex: number
  readonly timing: ActivityTimingProjection
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
  readonly responseType: Extract<EngineEvent, { readonly meta: CommandAckMeta }>["type"]
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
  readonly historyReady: { readonly sessionId: string; readonly through: string | null } | null
  readonly lastSequence: string | null
  readonly hasActivity: boolean
  readonly latestShell: ShellActivityProjection | null
  /** Kept separate so streaming deltas never replace or re-layout transcript history. */
  readonly streamingTail: StreamingTail | null
  readonly turns: Readonly<Record<string, TurnProjection>>
  readonly tools: Readonly<Record<string, ToolProjection>>
  readonly todos: TodoState
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
  readonly modelCatalogLoaded: boolean
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
    historyReady: null,
    lastSequence: null,
    hasActivity: false,
    latestShell: null,
    streamingTail: null,
    turns: {},
    tools: {},
    todos: emptyTodos(),
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
    modelCatalogLoaded: false,
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
