import type {
  Answer,
  AttachmentData,
  BudgetLevel,
  BudgetScope,
  BudgetUnit,
  CommandOutcome,
  ContextSnapshot,
  Cost,
  CostSnapshot,
  EngineError,
  PromptDump,
  Question,
  ToolCapability,
  ToolOutput,
  ToolOutputStream,
  Turn,
  TurnStatus,
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

export interface TranscriptEntry {
  readonly sequenceId: string
  readonly agentTurn: string
  readonly turn: Turn
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
  readonly diff: unknown | null
  readonly chunks: readonly ToolOutputChunkProjection[]
  readonly output: ToolOutput | null
  readonly isError: boolean | null
  readonly callIndex: number
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
    | "prompt_dump_ready"
    | "session_replay_completed"
    | "sessions_listed"
    | "command_descriptors_listed"
    | "models_listed"
    | "workspace_files_found"
    | "workspace_file_preview_ready"
    | "workspace_status_ready"
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
  readonly workspaceName: string
  readonly model: string
  readonly driverClientId: string | null
  readonly shellActive: boolean
}

export interface CommandChoice {
  readonly name: string
  readonly description: string
  readonly usage: string
}

export interface ModelChoice {
  readonly alias: string
  readonly vision: boolean
  readonly thinking: boolean
  readonly toolCalling: boolean
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

export interface RottweilerState {
  readonly connection: ConnectionProjection
  readonly lastSequence: string | null
  readonly transcript: readonly TranscriptEntry[]
  /** Kept separate so streaming deltas never replace or re-layout transcript history. */
  readonly streamingTail: StreamingTail | null
  readonly turns: Readonly<Record<string, TurnProjection>>
  readonly tools: Readonly<Record<string, ToolProjection>>
  readonly questions: Readonly<Record<string, QuestionProjection>>
  readonly commandAcks: Readonly<Record<string, CommandAcknowledgement>>
  readonly queuedMessages: readonly QueuedMessageProjection[]
  readonly context: ContextSnapshot | null
  readonly cost: CostSnapshot | null
  readonly promptDump: PromptDump | null
  readonly mode: string | null
  readonly model: string | null
  readonly driverClientId: string | null
  readonly shell: ShellProjection
  readonly compaction: CompactionProjection
  readonly budgets: readonly BudgetProjection[]
  readonly errors: readonly EngineError[]
  readonly protocol: ProtocolProjection
  readonly sessions: readonly SessionChoice[]
  readonly commands: readonly CommandChoice[]
  readonly models: readonly ModelChoice[]
  readonly workspaceFiles: readonly WorkspaceFileChoice[]
  readonly workspacePreview: WorkspacePreviewProjection | null
  readonly workspaceStatus: WorkspaceStatusProjection | null
}

export function createInitialState(): RottweilerState {
  return {
    connection: {
      phase: "idle",
      attempt: 0,
      error: null,
      gap: null,
    },
    lastSequence: null,
    transcript: [],
    streamingTail: null,
    turns: {},
    tools: {},
    questions: {},
    commandAcks: {},
    queuedMessages: [],
    context: null,
    cost: null,
    promptDump: null,
    mode: null,
    model: null,
    driverClientId: null,
    shell: { shellId: null, active: false, status: null, capturedOutput: null },
    compaction: {
      active: false,
      reason: null,
      summaryTurnId: null,
      reclaimedTokens: null,
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
    commands: [],
    models: [],
    workspaceFiles: [],
    workspacePreview: null,
    workspaceStatus: null,
  }
}
