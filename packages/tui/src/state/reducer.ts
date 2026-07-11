import type { EngineEvent } from "../protocol"
import { durableSequenceId, isRecord, type WireEngineEvent } from "../transport"
import type { RottweilerAction } from "./actions"
import {
  createInitialState,
  type RottweilerState,
  type StreamingTail,
  type ToolProjection,
} from "./model"

const MAX_U64 = 18_446_744_073_709_551_615n
const KNOWN_EVENT_TYPES = new Set<EngineEvent["type"]>([
  "command_acknowledged",
  "context_snapshot_ready",
  "cost_snapshot_ready",
  "prompt_dump_ready",
  "session_replay_completed",
  "sessions_listed",
  "command_descriptors_listed",
  "models_listed",
  "workspace_files_found",
  "workspace_file_preview_ready",
  "workspace_status_ready",
  "host_shutdown",
  "session_created",
  "workspace_roots_changed",
  "driver_changed",
  "message_queued",
  "user_message_accepted",
  "conversation_turn_committed",
  "conversation_rewound",
  "turn_started",
  "text_delta",
  "thinking_delta",
  "citation_delta",
  "tool_call_started",
  "tool_approval_needed",
  "tool_output_delta",
  "tool_call_finished",
  "question_asked",
  "question_answered",
  "turn_finished",
  "context_usage_updated",
  "budget_status_changed",
  "compaction_started",
  "compaction_attempt_finished",
  "compaction_finished",
  "subagent_spawned",
  "subagent_finished",
  "tool_output_pruned",
  "mode_changed",
  "plan_submitted",
  "plan_reviewed",
  "model_changed",
  "context_item_pinned",
  "context_item_evicted",
  "user_shell_state_changed",
  "hook_failed",
  "command_finished",
  "guard_triggered",
  "error",
])
const ACK_EVENT_TYPES = new Set<EngineEvent["type"]>([
  "command_acknowledged",
  "context_snapshot_ready",
  "cost_snapshot_ready",
  "prompt_dump_ready",
  "session_replay_completed",
  "sessions_listed",
  "command_descriptors_listed",
  "models_listed",
  "workspace_files_found",
  "workspace_file_preview_ready",
  "workspace_status_ready",
  "host_shutdown",
])

export function reduceRottweilerState(
  state: RottweilerState = createInitialState(),
  action: RottweilerAction,
): RottweilerState {
  switch (action.type) {
    case "engine_event":
      return reduceWireEvent(state, action.event)
    case "transport_connecting":
      return {
        ...state,
        connection: {
          ...state.connection,
          phase: action.attempt === 0 ? "connecting" : "reconnecting",
          attempt: action.attempt,
          error: null,
        },
      }
    case "transport_reconnecting":
      return {
        ...state,
        connection: {
          ...state.connection,
          phase: "reconnecting",
          attempt: action.attempt,
          error: null,
        },
      }
    case "transport_connected":
      return {
        ...state,
        connection: {
          ...state.connection,
          phase: state.connection.gap === null ? "connected" : "replaying",
          attempt: action.attempt,
          error: null,
        },
      }
    case "transport_disconnected":
      return {
        ...state,
        connection: {
          ...state.connection,
          phase: state.connection.gap === null ? "disconnected" : "replaying",
          attempt: action.attempt,
          error: action.error ?? null,
        },
      }
    case "transport_closed":
      return {
        ...state,
        connection: { ...state.connection, phase: "closed", error: null },
      }
  }
}

export function reduceWireEvent(
  state: RottweilerState,
  event: WireEngineEvent,
): RottweilerState {
  if (ACK_EVENT_TYPES.has(event.type as EngineEvent["type"])) {
    return KNOWN_EVENT_TYPES.has(event.type as EngineEvent["type"])
      ? applyKnownEvent(state, event as EngineEvent, null)
      : recordUnknown(state, event.type)
  }

  const sequenceText = durableSequenceId(event)
  const sequence = parseU64(sequenceText)
  if (sequence === null || sequenceText === null) {
    return KNOWN_EVENT_TYPES.has(event.type as EngineEvent["type"])
      ? recordInvalid(state)
      : recordUnknown(state, event.type)
  }

  const last = parseU64(state.lastSequence)
  if (last !== null && sequence <= last) {
    return {
      ...state,
      protocol: {
        ...state.protocol,
        duplicateEvents: state.protocol.duplicateEvents + 1,
      },
    }
  }
  if (last !== null && sequence !== last + 1n) {
    return {
      ...state,
      connection: {
        ...state.connection,
        phase: "replaying",
        gap: {
          expected: (last + 1n).toString(),
          received: sequenceText,
        },
      },
    }
  }

  const withCursor: RottweilerState = {
    ...state,
    lastSequence: sequenceText,
  }
  const gap = state.connection.gap
  const caughtUp = gap !== null && sequence >= (parseU64(gap.received) ?? MAX_U64)
  const ready = caughtUp
    ? {
        ...withCursor,
        connection: {
          ...withCursor.connection,
          phase: "connected" as const,
          gap: null,
          error: null,
        },
      }
    : withCursor

  if (!KNOWN_EVENT_TYPES.has(event.type as EngineEvent["type"])) {
    return recordUnknown(ready, event.type)
  }
  return applyKnownEvent(ready, event as EngineEvent, sequenceText)
}

function applyKnownEvent(
  state: RottweilerState,
  event: EngineEvent,
  sequenceId: string | null,
): RottweilerState {
  switch (event.type) {
    case "command_acknowledged":
      return {
        ...state,
        commandAcks: {
          ...state.commandAcks,
          [event.meta.request_id]: {
            requestId: event.meta.request_id,
            responseType: event.type,
            outcome: event.outcome,
            sessionId: event.session_id ?? null,
          },
        },
      }
    case "context_snapshot_ready":
      return {
        ...state,
        context: event.snapshot,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "cost_snapshot_ready":
      return {
        ...state,
        cost: event.snapshot,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "prompt_dump_ready":
      return {
        ...state,
        promptDump: event.dump,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "session_replay_completed":
      return {
        ...state,
        connection: {
          ...state.connection,
          phase: state.connection.gap === null ? "connected" : "replaying",
          error: null,
        },
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "sessions_listed":
      return {
        ...state,
        sessions: event.sessions.map((session) => ({
          sessionId: session.session_id,
          workspaceName: session.workspace_name,
          model: session.model,
          driverClientId: session.driver_client_id ?? null,
          shellActive: session.shell_active,
        })),
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "command_descriptors_listed":
      return {
        ...state,
        commands: event.commands.map((command) => ({
          name: command.name,
          description: command.description,
          usage: command.usage,
        })),
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "models_listed":
      return {
        ...state,
        models: event.models.map((model) => ({
          alias: model.alias,
          vision: model.capabilities.vision,
          thinking: model.capabilities.thinking,
          toolCalling: model.capabilities.tool_calling,
        })),
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "host_shutdown":
      return {
        ...state,
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "workspace_files_found":
      return {
        ...state,
        workspaceFiles: event.matches.map((match) => ({
          path: match.path,
          isDirectory: match.is_directory,
        })),
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "workspace_file_preview_ready":
      return {
        ...state,
        workspacePreview: {
          path: event.preview.path,
          mediaType: event.preview.media_type,
          data: event.preview.data,
          totalBytes: event.preview.total_bytes,
          truncated: event.preview.truncated,
        },
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "workspace_status_ready":
      return {
        ...state,
        workspaceStatus: {
          workspaceName: event.status.workspace_name,
          branch: event.status.branch ?? null,
          changedPaths: event.status.changed_paths,
          truncated: event.status.truncated,
        },
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "session_created":
    case "driver_changed":
      return { ...state, driverClientId: event.driver_client_id }
    case "message_queued":
      return {
        ...state,
        queuedMessages: [
          ...state.queuedMessages,
          { position: event.position, content: event.content },
        ],
      }
    case "workspace_roots_changed":
      return {
        ...state,
        workspaceRoots: {
          generation: event.generation,
          effectiveFromTurn: event.effective_from_turn,
          roots: event.roots.map((root) => root.path),
        },
      }
    case "user_message_accepted":
      return state
    case "conversation_turn_committed": {
      const transcript = [
        ...state.transcript,
        {
          sequenceId: sequenceId ?? state.lastSequence ?? "0",
          agentTurn: event.agent_turn,
          turn: event.turn,
        },
      ]
      const clearsTail =
        event.turn.role === "assistant" && state.streamingTail?.turnId === event.agent_turn
      return {
        ...state,
        transcript,
        streamingTail: clearsTail ? null : state.streamingTail,
      }
    }
    case "conversation_rewound": {
      const target = parseU64(event.to_agent_turn)
      return {
        ...state,
        transcript:
          target === null
            ? state.transcript
            : state.transcript.filter((entry) => {
                const turn = parseU64(entry.agentTurn)
                return turn === null || turn <= target
              }),
        streamingTail: null,
        queuedMessages: [],
      }
    }
    case "turn_started":
      return {
        ...state,
        turns: {
          ...state.turns,
          [event.turn_id]: {
            turnId: event.turn_id,
            status: "running",
            usage: null,
            cost: null,
          },
        },
      }
    case "text_delta":
      return {
        ...state,
        streamingTail: updateTail(state.streamingTail, event.turn_id, (tail) => ({
          ...tail,
          text: tail.text + event.text,
        })),
      }
    case "thinking_delta":
      return {
        ...state,
        streamingTail: updateTail(state.streamingTail, event.turn_id, (tail) => ({
          ...tail,
          thinking: tail.thinking + event.text,
        })),
      }
    case "citation_delta":
      return {
        ...state,
        streamingTail: updateTail(state.streamingTail, event.turn_id, (tail) => ({
          ...tail,
          citations: [
            ...tail.citations,
            { uri: event.uri, title: event.title ?? null },
          ],
        })),
      }
    case "tool_call_started": {
      const tool: ToolProjection = {
        toolCallId: event.tool_call_id,
        turnId: event.turn_id,
        name: event.name,
        args: event.args,
        status: "running",
        capabilities: [],
        rationale: null,
        diff: null,
        chunks: [],
        output: null,
        isError: null,
        callIndex: event.call_index,
      }
      return {
        ...state,
        tools: { ...state.tools, [event.tool_call_id]: tool },
        streamingTail: updateTail(state.streamingTail, event.turn_id, (tail) => ({
          ...tail,
          toolCallIds: tail.toolCallIds.includes(event.tool_call_id)
            ? tail.toolCallIds
            : [...tail.toolCallIds, event.tool_call_id],
        })),
      }
    }
    case "tool_approval_needed": {
      const existing = state.tools[event.tool_call_id]
      const tool: ToolProjection = {
        toolCallId: event.tool_call_id,
        turnId: event.turn_id,
        name: event.name,
        args: event.args,
        status: "awaiting_approval",
        capabilities: event.capabilities,
        rationale: event.rationale,
        diff: event.diff ?? null,
        chunks: existing?.chunks ?? [],
        output: existing?.output ?? null,
        isError: existing?.isError ?? null,
        callIndex: existing?.callIndex ?? 0,
      }
      return { ...state, tools: { ...state.tools, [event.tool_call_id]: tool } }
    }
    case "tool_output_delta": {
      const existing = state.tools[event.tool_call_id]
      if (existing === undefined) {
        return state
      }
      return {
        ...state,
        tools: {
          ...state.tools,
          [event.tool_call_id]: {
            ...existing,
            chunks: [...existing.chunks, { stream: event.stream, chunk: event.chunk }],
          },
        },
      }
    }
    case "tool_call_finished": {
      const existing = state.tools[event.tool_call_id]
      const tool: ToolProjection = {
        toolCallId: event.tool_call_id,
        turnId: event.turn_id,
        name: existing?.name ?? "unknown",
        args: existing?.args ?? null,
        status: "finished",
        capabilities: existing?.capabilities ?? [],
        rationale: existing?.rationale ?? null,
        diff: existing?.diff ?? null,
        chunks: existing?.chunks ?? [],
        output: event.output,
        isError: event.is_error,
        callIndex: event.call_index,
      }
      return { ...state, tools: { ...state.tools, [event.tool_call_id]: tool } }
    }
    case "question_asked":
      return {
        ...state,
        questions: {
          ...state.questions,
          [event.question_id]: {
            questionId: event.question_id,
            turnId: event.turn_id,
            questions: event.questions,
            answers: null,
            answered: false,
          },
        },
      }
    case "question_answered": {
      const existing = state.questions[event.question_id]
      return {
        ...state,
        questions: {
          ...state.questions,
          [event.question_id]: {
            questionId: event.question_id,
            turnId: event.turn_id,
            questions: existing?.questions ?? [],
            answers: event.answers,
            answered: true,
          },
        },
      }
    }
    case "turn_finished": {
      const tail =
        state.streamingTail?.turnId === event.turn_id
          ? {
              ...state.streamingTail,
              finished: { status: event.status, usage: event.usage, cost: event.cost },
            }
          : state.streamingTail
      return {
        ...state,
        turns: {
          ...state.turns,
          [event.turn_id]: {
            turnId: event.turn_id,
            status: event.status,
            usage: event.usage,
            cost: event.cost,
          },
        },
        streamingTail: tail,
        queuedMessages: state.queuedMessages.slice(1),
      }
    }
    case "budget_status_changed":
      return {
        ...state,
        budgets: [
          ...state.budgets.slice(-63),
          {
            turnId: event.turn_id,
            level: event.level,
            scope: event.scope,
            unit: event.unit,
            current: event.current,
            limit: event.limit,
          },
        ],
      }
    case "compaction_started":
      return {
        ...state,
        compaction: {
          active: true,
          reason: event.reason,
          summaryTurnId: null,
          reclaimedTokens: null,
        },
      }
    case "compaction_finished":
      return {
        ...state,
        compaction: {
          active: false,
          reason: state.compaction.reason,
          summaryTurnId: event.summary_turn_id,
          reclaimedTokens: event.reclaimed_tokens,
        },
      }
    case "mode_changed":
      return event.mode === "plan"
        ? { ...state, mode: event.mode, pendingPlan: null, approvedPlan: null }
        : { ...state, mode: event.mode }
    case "plan_submitted":
      return { ...state, pendingPlan: event.artifact }
    case "plan_reviewed":
      return {
        ...state,
        pendingPlan: null,
        approvedPlan: event.decision === "approve" ? event.artifact : state.approvedPlan,
      }
    case "model_changed":
      return { ...state, model: event.model }
    case "user_shell_state_changed":
      return {
        ...state,
        shell: {
          shellId: event.shell_id,
          active: event.active,
          status: event.status ?? null,
          capturedOutput: event.captured_output ?? null,
        },
      }
    case "error":
      return { ...state, errors: [...state.errors.slice(-63), event.error] }
    case "context_usage_updated":
    case "compaction_attempt_finished":
    case "subagent_spawned":
    case "subagent_finished":
    case "tool_output_pruned":
    case "context_item_pinned":
    case "context_item_evicted":
    case "hook_failed":
    case "command_finished":
    case "guard_triggered":
      return state
  }
}

function responseAck(
  state: RottweilerState,
  requestId: string,
  responseType:
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
    | "host_shutdown",
  sessionId: string | null,
): RottweilerState["commandAcks"] {
  return {
    ...state.commandAcks,
    [requestId]: {
      requestId,
      responseType,
      outcome: null,
      sessionId,
    },
  }
}

function updateTail(
  current: StreamingTail | null,
  turnId: string,
  update: (tail: StreamingTail) => StreamingTail,
): StreamingTail {
  const tail =
    current?.turnId === turnId
      ? current
      : {
          turnId,
          text: "",
          thinking: "",
          citations: [],
          toolCallIds: [],
          finished: null,
        }
  return update(tail)
}

function parseU64(value: string | null): bigint | null {
  if (value === null || !/^(0|[1-9]\d*)$/.test(value)) {
    return null
  }
  const parsed = BigInt(value)
  return parsed <= MAX_U64 ? parsed : null
}

function recordInvalid(state: RottweilerState): RottweilerState {
  return {
    ...state,
    protocol: { ...state.protocol, invalidEvents: state.protocol.invalidEvents + 1 },
  }
}

function recordUnknown(state: RottweilerState, type: string): RottweilerState {
  return {
    ...state,
    protocol: {
      ...state.protocol,
      unknownEvents: state.protocol.unknownEvents + 1,
      lastUnknownType: type,
    },
  }
}

export function eventHasDurableSequence(event: WireEngineEvent): boolean {
  return isRecord(event.meta) && typeof event.meta.sequence_id === "string"
}
