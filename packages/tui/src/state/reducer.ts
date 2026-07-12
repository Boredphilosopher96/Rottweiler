import type { EngineEvent, ToolOutput } from "../protocol"
import { durableSequenceId, isRecord, type WireEngineEvent } from "../transport"
import type { RottweilerAction } from "./actions"
import {
  createInitialState,
  type RottweilerState,
  type StreamingTail,
  type TodoProjection,
  type ToolProjection,
} from "./model"

const MAX_U64 = 18_446_744_073_709_551_615n
export const MAX_SUBAGENT_TASK_BYTES = 1_024
export const MAX_TERMINAL_SUBAGENT_HISTORY = 128
export const MAX_TODO_ITEMS = 128
export const MAX_TODO_ID_BYTES = 256
export const MAX_TODO_CONTENT_BYTES = 4_096
export const MAX_TODO_TOTAL_BYTES = 64 * 1_024
const KNOWN_EVENT_TYPES = new Set<EngineEvent["type"]>([
  "command_acknowledged",
  "context_snapshot_ready",
  "cost_snapshot_ready",
  "session_review_ready",
  "session_review_updated",
  "prompt_dump_ready",
  "session_replay_completed",
  "session_forked",
  "sessions_listed",
  "sessions_search_ready",
  "command_descriptors_listed",
  "models_listed",
  "settings_listed",
  "mcp_servers_listed",
  "mcp_server_approval_reviewed",
  "permissions_listed",
  "provider_auth_started",
  "provider_configured",
  "provider_auth_finished",
  "workspace_files_found",
  "workspace_file_preview_ready",
  "workspace_status_ready",
  "workspace_diff_ready",
  "host_shutdown",
  "session_created",
  "workspace_roots_changed",
  "driver_changed",
  "message_queued",
  "user_message_accepted",
  "plugin_message_injected",
  "plugin_status_changed",
  "ui_notification",
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
  "subagent_progress",
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
  "session_review_ready",
  "session_review_updated",
  "prompt_dump_ready",
  "session_replay_completed",
  "session_forked",
  "sessions_listed",
  "sessions_search_ready",
  "command_descriptors_listed",
  "models_listed",
  "settings_listed",
  "mcp_servers_listed",
  "mcp_server_approval_reviewed",
  "permissions_listed",
  "provider_auth_started",
  "provider_configured",
  "provider_auth_finished",
  "workspace_files_found",
  "workspace_file_preview_ready",
  "workspace_status_ready",
  "workspace_diff_ready",
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
        providerAuth: { ...state.providerAuth, pending: null },
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
        providerAuth: { ...state.providerAuth, pending: null },
        connection: { ...state.connection, phase: "closed", error: null },
      }
  }
}

export function reduceWireEvent(
  state: RottweilerState,
  event: WireEngineEvent,
): RottweilerState {
  // Child progress is connection-scoped: it updates the retained projection
  // without consuming or perturbing the parent's durable replay cursor.
  if (event.type === "subagent_progress") {
    return applyKnownEvent(state, event as EngineEvent, null)
  }
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
    case "session_review_ready":
    case "session_review_updated":
      return {
        ...state,
        review: projectSessionReview(event.review),
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
        replay: state.replay.active
          ? {
              ...state.replay,
              sessionId: event.session_id,
              completedThrough: event.through_sequence ?? state.lastSequence,
            }
          : state.replay,
        connection: {
          ...state.connection,
          phase: state.connection.gap === null ? "connected" : "replaying",
          error: null,
        },
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "session_forked":
      return {
        ...state,
        lastFork: {
          parentSessionId: event.parent_session_id,
          child: projectSession(event.child),
          atTurn: event.at_turn ?? null,
        },
        commandAcks: responseAck(
          state,
          event.meta.request_id,
          event.type,
          event.child.session_id,
        ),
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
        sessionSearch: null,
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "sessions_search_ready":
      return {
        ...state,
        sessions: event.sessions.map(projectSession),
        sessionSearch: { query: event.query, truncated: event.truncated },
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "command_descriptors_listed":
      return {
        ...state,
        commands: event.commands.map((command) => ({
          name: command.name,
          description: command.description,
          usage: command.usage,
          source: command.source ?? "builtin",
        })),
        commandsTruncated: event.truncated,
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "models_listed":
      const currentModels = event.models.filter(
        (model) => model.current === true && model.available !== false,
      )
      const currentModel = currentModels.length === 1 ? currentModels[0] : undefined
      return {
        ...state,
        ...(state.model !== null || currentModel === undefined
          ? {}
          : {
              model: currentModel.id ?? currentModel.alias,
              provider: currentModel.provider ?? currentModel.providers?.[0] ?? null,
            }),
        models: event.models.map((model) => ({
          alias: model.alias,
          ...(model.id === undefined ? {} : { id: model.id }),
          ...(model.display_name === undefined ? {} : { displayName: model.display_name }),
          ...(model.provider === undefined ? {} : { provider: model.provider }),
          // Older compatible hosts did not emit provider metadata.
          providers: model.providers ?? [],
          ...(model.aliases === undefined ? {} : { aliases: model.aliases }),
          ...(model.current === undefined ? {} : { current: model.current }),
          ...(model.available === undefined ? {} : { available: model.available }),
          ...(model.status === undefined ? {} : { status: model.status }),
          vision: model.capabilities.vision,
          thinking: model.capabilities.thinking,
          toolCalling: model.capabilities.tool_calling,
        })),
        modelAliases: (event.aliases ?? []).map((alias) => ({
          alias: alias.alias,
          candidates: alias.candidates,
          current: alias.current,
        })),
        providers: (event.providers ?? []).map((provider) => ({
          name: provider.name,
          authKind: provider.auth_kind,
          nextAction: provider.next_action,
          configured: provider.configured,
          authenticated: provider.authenticated,
          reachable: provider.reachable,
          modelCount: provider.model_count,
          status: provider.status ?? null,
        })),
        modelCatalogCached: event.cached ?? false,
        providerAuth:
          state.providerAuth.pending !== null &&
          event.providers.some(
            (provider) =>
              provider.name === state.providerAuth.pending?.provider && provider.authenticated,
          )
            ? {
                pending: null,
                last: {
                  provider: state.providerAuth.pending.provider,
                  success: true,
                  message: "provider authentication active",
                  warnings: [],
                },
              }
            : state.providerAuth,
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "settings_listed":
      return {
        ...state,
        settings: event.settings.map((setting) => ({
          key: setting.key,
          label: setting.label,
          value: setting.value,
          choices: setting.choices,
          provenance: setting.provenance,
          appliesImmediately: setting.applies_immediately,
        })),
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "mcp_servers_listed":
      return {
        ...state,
        mcpServers: event.servers.slice(0, 128),
        mcpApprovalReview:
          state.mcpApprovalReview !== null &&
          event.servers.some((server) => server.name === state.mcpApprovalReview?.server)
            ? state.mcpApprovalReview
            : null,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "mcp_server_approval_reviewed":
      return {
        ...state,
        mcpApprovalReview: event.review,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "permissions_listed":
      return {
        ...state,
        permissions: event.permissions,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "provider_auth_started":
      return {
        ...state,
        providerAuth: {
          pending: {
            attemptId: event.attempt_id,
            provider: event.provider,
            challenge: event.challenge,
            warnings: event.warnings,
          },
          last: null,
        },
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "provider_configured":
      return {
        ...state,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "provider_auth_finished":
      return {
        ...state,
        providerAuth: {
          pending:
            state.providerAuth.pending?.attemptId === event.attempt_id
              ? null
              : state.providerAuth.pending,
          last: {
            provider: event.provider,
            success: event.success,
            message: event.message,
            warnings: event.warnings,
          },
        },
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
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
    case "workspace_diff_ready":
      return {
        ...state,
        workspaceDiff: {
          path: event.diff.path,
          unifiedDiff: event.diff.unified_diff,
          truncated: event.diff.truncated,
          binary: event.diff.binary,
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
    case "plugin_message_injected":
      return state
    case "plugin_status_changed":
      return {
        ...state,
        pluginStatuses: { ...state.pluginStatuses, [event.plugin_id]: event.status },
      }
    case "ui_notification":
      return {
        ...state,
        pluginNotifications: [
          ...state.pluginNotifications.slice(-63),
          { pluginId: event.plugin_id, title: event.title, message: event.message },
        ],
      }
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
      const resolvedRoute =
        event.turn.role === "assistant" ? providerQualifiedRoute(event.turn.meta.model) : null
      return {
        ...state,
        transcript,
        streamingTail: clearsTail ? null : state.streamingTail,
        ...(resolvedRoute === null
          ? {}
          : { model: resolvedRoute.model, provider: resolvedRoute.provider }),
      }
    }
    case "conversation_rewound": {
      const target = parseU64(event.to_agent_turn)
      const retainedSubagentIds = state.subagentOrder.filter((subagentId) => {
        const turn = parseU64(state.subagents[subagentId]?.parentTurnId ?? null)
        return target === null || turn === null || turn <= target
      })
      const subagents = Object.fromEntries(
        retainedSubagentIds.flatMap((subagentId) => {
          const subagent = state.subagents[subagentId]
          return subagent === undefined ? [] : [[subagentId, subagent] as const]
        }),
      )
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
        todos: target === null ? [] : deriveTodosFromTools(state.tools, target),
        subagents,
        subagentOrder: retainedSubagentIds,
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
    case "subagent_spawned": {
      const existing = state.subagents[event.subagent_id]
      const parentTurnId = currentTurnId(state)
      let subagents = state.subagents
      let subagentOrder = state.subagentOrder
      if (existing !== undefined && existing.parentTurnId !== parentTurnId) {
        const archiveKey = nextSubagentArchiveKey(
          state.subagents,
          event.subagent_id,
          existing.parentTurnId,
        )
        subagents = {
          ...state.subagents,
          [archiveKey]: { ...existing, projectionId: archiveKey },
        }
        delete (subagents as Record<string, unknown>)[event.subagent_id]
        subagentOrder = state.subagentOrder.map((key) =>
          key === event.subagent_id ? archiveKey : key,
        )
      }
      const nextSubagents: RottweilerState["subagents"] = {
        ...subagents,
        [event.subagent_id]: {
          projectionId: event.subagent_id,
          subagentId: event.subagent_id,
          parentTurnId,
          task: boundedUtf8(event.task, MAX_SUBAGENT_TASK_BYTES),
          status: "running",
          childSessionId: event.child_session_id,
          lastChildSequence: existing?.lastChildSequence ?? null,
          activity: "starting",
          summary: null,
          touchedFileCount: 0,
          diffArtifactId: null,
        },
      }
      const nextOrder = subagentOrder.includes(event.subagent_id)
        ? subagentOrder
        : [...subagentOrder, event.subagent_id]
      return {
        ...state,
        ...boundedSubagentHistory(nextSubagents, nextOrder),
      }
    }
    case "subagent_progress": {
      const existing = state.subagents[event.subagent_id]
      if (existing === undefined || existing.childSessionId !== event.child_session_id) {
        return recordInvalid(state)
      }
      const childSequence = event.child_sequence ?? null
      const sequence = parseU64(childSequence)
      if (childSequence !== null && sequence === null) {
        return recordInvalid(state)
      }
      const lastSequence = parseU64(existing.lastChildSequence)
      if (sequence !== null && lastSequence !== null && sequence <= lastSequence) {
        return state
      }
      const activity = subagentActivity(event.event)
      return {
        ...state,
        subagents: {
          ...state.subagents,
          [event.subagent_id]: {
            projectionId: existing.projectionId,
            subagentId: event.subagent_id,
            parentTurnId: existing.parentTurnId,
            task: existing.task,
            status: existing.status,
            childSessionId: event.child_session_id,
            lastChildSequence: childSequence ?? existing.lastChildSequence,
            activity,
            summary: existing.summary,
            touchedFileCount: existing.touchedFileCount,
            diffArtifactId: existing.diffArtifactId,
          },
        },
      }
    }
    case "subagent_finished": {
      const existing = state.subagents[event.subagent_id]
      const terminal = subagentTerminalSummary(event.result)
      const nextSubagents: RottweilerState["subagents"] = {
        ...state.subagents,
        [event.subagent_id]: {
          projectionId: existing?.projectionId ?? event.subagent_id,
          subagentId: event.subagent_id,
          parentTurnId: existing?.parentTurnId ?? currentTurnId(state),
          task: boundedUtf8(existing?.task ?? event.subagent_id, MAX_SUBAGENT_TASK_BYTES),
          status: terminal.status,
          childSessionId: existing?.childSessionId ?? terminal.childSessionId,
          lastChildSequence: existing?.lastChildSequence ?? null,
          activity: existing?.activity ?? null,
          summary: terminal.summary,
          touchedFileCount: terminal.touchedFileCount,
          diffArtifactId: terminal.diffArtifactId,
        },
      }
      const nextOrder = state.subagentOrder.includes(event.subagent_id)
        ? state.subagentOrder
        : [...state.subagentOrder, event.subagent_id]
      return {
        ...state,
        ...boundedSubagentHistory(nextSubagents, nextOrder),
      }
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
      return {
        ...state,
        tools: { ...state.tools, [event.tool_call_id]: tool },
        streamingTail: attachToolToTail(state.streamingTail, event.turn_id, event.tool_call_id),
      }
    }
    case "tool_output_delta": {
      const existing: ToolProjection = state.tools[event.tool_call_id] ?? {
        toolCallId: event.tool_call_id,
        turnId: event.turn_id,
        name: "tool",
        args: null,
        status: "running",
        capabilities: [],
        rationale: null,
        diff: null,
        chunks: [],
        output: null,
        isError: null,
        callIndex: 0,
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
        streamingTail: attachToolToTail(state.streamingTail, event.turn_id, event.tool_call_id),
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
      const todos =
        tool.name === "todo" && !event.is_error ? projectTodoOutput(event.output) : null
      return {
        ...state,
        tools: { ...state.tools, [event.tool_call_id]: tool },
        streamingTail: attachToolToTail(state.streamingTail, event.turn_id, event.tool_call_id),
        ...(todos === null ? {} : { todos }),
      }
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
      return { ...state, model: event.model, provider: event.provider ?? null }
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
    case "command_finished": {
      const commandSequence = sequenceId ?? state.lastSequence ?? "0"
      const message = event.message.trim()
      return {
        ...state,
        transcript: [
          ...state.transcript,
          {
            sequenceId: commandSequence,
            agentTurn: `command:${event.name}:${commandSequence}`,
            turn: {
              role: "system",
              blocks: [
                {
                  type: "text",
                  text: `/${event.name}\n\n${message.length === 0 ? "Command completed." : message}`,
                },
              ],
              meta: { synthetic: true, summary: false },
            },
          },
        ],
      }
    }
    case "context_usage_updated":
    case "compaction_attempt_finished":
    case "tool_output_pruned":
    case "context_item_pinned":
    case "context_item_evicted":
    case "hook_failed":
    case "guard_triggered":
      return state
  }
}

function providerQualifiedRoute(
  value: string | null | undefined,
): { readonly provider: string; readonly model: string } | null {
  if (value === null || value === undefined) return null
  const separator = value.indexOf("/")
  if (separator <= 0 || separator === value.length - 1) return null
  return { provider: value.slice(0, separator), model: value }
}

function nextSubagentArchiveKey(
  subagents: RottweilerState["subagents"],
  subagentId: string,
  parentTurnId: string,
): string {
  const base = `${subagentId}@${parentTurnId}`
  if (subagents[base] === undefined) return base
  let ordinal = 2
  while (subagents[`${base}#${ordinal}`] !== undefined) ordinal += 1
  return `${base}#${ordinal}`
}

function boundedSubagentHistory(
  subagents: RottweilerState["subagents"],
  order: readonly string[],
): Pick<RottweilerState, "subagents" | "subagentOrder"> {
  const terminalIds = order.filter((id) => subagents[id]?.status !== "running")
  const retainedTerminalIds = new Set(terminalIds.slice(-MAX_TERMINAL_SUBAGENT_HISTORY))
  const subagentOrder = order.filter((id) => {
    const projection = subagents[id]
    return (
      projection !== undefined &&
      (projection.status === "running" || retainedTerminalIds.has(id))
    )
  })
  const retainedSubagents = Object.fromEntries(
    subagentOrder.map((id) => [id, subagents[id]!] as const),
  )
  return { subagents: retainedSubagents, subagentOrder }
}

function currentTurnId(state: RottweilerState): string {
  if (state.streamingTail !== null) return state.streamingTail.turnId
  const turns = Object.values(state.turns)
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    if (turns[index]?.status === "running") return turns[index]!.turnId
  }
  return "0"
}

const TERMINAL_SUBAGENT_STATUSES = new Set([
  "completed",
  "failed",
  "cancelled",
  "timed_out",
  "max_turns",
] as const)

function subagentTerminalSummary(
  result: Extract<EngineEvent, { type: "subagent_finished" }>["result"],
): {
  readonly status: "completed" | "failed" | "cancelled" | "timed_out" | "max_turns"
  readonly childSessionId: string | null
  readonly summary: string | null
  readonly touchedFileCount: number
  readonly diffArtifactId: string | null
} {
  return {
    status: TERMINAL_SUBAGENT_STATUSES.has(result.status)
      ? result.status
      : "failed",
    childSessionId: result.session_id,
    summary: boundedSummary(result.final_text),
    touchedFileCount: result.touched_files.length,
    diffArtifactId: result.diff_artifact?.id ?? null,
  }
}

function boundedSummary(value: string): string {
  return boundedUtf8(value, 512)
}

function projectSession(session: {
  readonly session_id: string
  readonly workspace_name: string
  readonly model: string
  readonly driver_client_id?: string | null
  readonly shell_active: boolean
}): RottweilerState["sessions"][number] {
  return {
    sessionId: session.session_id,
    workspaceName: session.workspace_name,
    model: session.model,
    driverClientId: session.driver_client_id ?? null,
    shellActive: session.shell_active,
  }
}

function projectSessionReview(review: {
  readonly session_id: string
  readonly files: readonly {
    readonly path: string
    readonly unified_diff: string
    readonly status: "pending" | "accepted" | "reverted"
    readonly truncated: boolean
    readonly unrestorable_reason?: string | null
    readonly original_hash: string
    readonly current_hash: string
  }[]
}): NonNullable<RottweilerState["review"]> {
  return {
    sessionId: review.session_id,
    files: review.files.map((file) => ({
      path: file.path,
      unifiedDiff: file.unified_diff,
      status: file.status,
      truncated: file.truncated,
      unrestorableReason: file.unrestorable_reason ?? null,
      originalHash: file.original_hash,
      currentHash: file.current_hash,
    })),
  }
}

function deriveTodosFromTools(
  tools: RottweilerState["tools"],
  throughTurn: bigint,
): readonly TodoProjection[] {
  const candidates = Object.values(tools)
    .flatMap((tool) => {
      const turn = parseU64(tool.turnId)
      return tool.name === "todo" && tool.status === "finished" && tool.isError === false && turn !== null && turn <= throughTurn
        ? [{ tool, turn }]
        : []
    })
    .sort((left, right) => {
      if (left.turn < right.turn) return -1
      if (left.turn > right.turn) return 1
      if (left.tool.callIndex !== right.tool.callIndex) {
        return left.tool.callIndex - right.tool.callIndex
      }
      return left.tool.toolCallId.localeCompare(right.tool.toolCallId)
    })

  let todos: readonly TodoProjection[] = []
  for (const { tool } of candidates) {
    if (tool.output === null) continue
    const projected = projectTodoOutput(tool.output)
    if (projected !== null) todos = projected
  }
  return todos
}

/** Accept only the exact bounded structured snapshot emitted by TodoTool. */
function projectTodoOutput(output: ToolOutput): readonly TodoProjection[] | null {
  const values =
    output.type === "structured"
      ? [output.value]
      : output.type === "mixed"
        ? output.parts.flatMap((part) => (part.type === "structured" ? [part.value] : []))
        : []
  for (let index = values.length - 1; index >= 0; index -= 1) {
    const projected = projectTodoValue(values[index])
    if (projected !== null) return projected
  }
  return null
}

function projectTodoValue(value: unknown): readonly TodoProjection[] | null {
  if (!isRecord(value)) return null
  let payload: Record<string, unknown> = value
  if ("data" in value || "truncated" in value) {
    if (value.truncated !== false || !isRecord(value.data)) return null
    payload = value.data
  }
  if (!Array.isArray(payload.items) || payload.items.length > MAX_TODO_ITEMS) return null
  if (
    typeof payload.count !== "number" ||
    !Number.isSafeInteger(payload.count) ||
    payload.count !== payload.items.length
  ) {
    return null
  }

  const encoder = new TextEncoder()
  const ids = new Set<string>()
  const projected: TodoProjection[] = []
  let totalBytes = 0
  for (const item of payload.items) {
    if (
      !isRecord(item) ||
      typeof item.id !== "string" ||
      typeof item.content !== "string" ||
      !isTodoStatus(item.status) ||
      item.id.length === 0 ||
      item.content.length === 0 ||
      ids.has(item.id)
    ) {
      return null
    }
    const idBytes = encoder.encode(item.id).byteLength
    const contentBytes = encoder.encode(item.content).byteLength
    totalBytes += idBytes + contentBytes
    if (
      idBytes > MAX_TODO_ID_BYTES ||
      contentBytes > MAX_TODO_CONTENT_BYTES ||
      totalBytes > MAX_TODO_TOTAL_BYTES
    ) {
      return null
    }
    ids.add(item.id)
    projected.push({ id: item.id, content: item.content, status: item.status })
  }
  return projected
}

function isTodoStatus(value: unknown): value is TodoProjection["status"] {
  return value === "pending" || value === "in_progress" || value === "completed" || value === "blocked"
}

function boundedUtf8(value: string, maxBytes: number): string {
  const encoder = new TextEncoder()
  const encoded = encoder.encode(value)
  if (encoded.byteLength <= maxBytes) return value
  const ellipsis = encoder.encode("…")
  if (maxBytes < ellipsis.byteLength) return ".".repeat(Math.max(0, maxBytes))
  const prefixLimit = maxBytes - ellipsis.byteLength
  let prefix = new TextDecoder().decode(encoded.subarray(0, prefixLimit))
  // A slice ending within a multibyte code point decodes to U+FFFD, which can
  // itself exceed the byte budget. Remove that replacement (or any partial
  // surrogate) until the encoded prefix is strictly within the limit.
  while (encoder.encode(prefix).byteLength > prefixLimit) {
    prefix = prefix.slice(0, -1)
  }
  return `${prefix}…`
}

function subagentActivity(event: unknown): string {
  if (!isRecord(event) || typeof event.type !== "string") {
    return "working"
  }
  switch (event.type) {
    case "turn_started":
      return "working"
    case "thinking_delta":
      return "thinking"
    case "text_delta":
      return "writing response"
    case "tool_call_started":
      return typeof event.name === "string" ? `using tool · ${event.name}` : "using tool"
    case "tool_approval_needed":
      return typeof event.name === "string"
        ? `awaiting approval · ${event.name}`
        : "awaiting approval"
    case "tool_output_delta":
      return "receiving tool output"
    case "tool_call_finished":
      return "tool finished"
    case "question_asked":
      return "awaiting answer"
    case "turn_finished":
      return "finalizing"
    case "error":
      return "error"
    default:
      return event.type.replaceAll("_", " ")
  }
}

function responseAck(
  state: RottweilerState,
  requestId: string,
  responseType:
    | "context_snapshot_ready"
    | "cost_snapshot_ready"
    | "session_review_ready"
    | "session_review_updated"
    | "prompt_dump_ready"
    | "session_replay_completed"
    | "session_forked"
    | "sessions_listed"
    | "sessions_search_ready"
    | "command_descriptors_listed"
    | "models_listed"
    | "settings_listed"
    | "mcp_servers_listed"
    | "mcp_server_approval_reviewed"
    | "permissions_listed"
    | "provider_auth_started"
    | "provider_configured"
    | "provider_auth_finished"
    | "workspace_files_found"
    | "workspace_file_preview_ready"
    | "workspace_status_ready"
    | "workspace_diff_ready"
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

function attachToolToTail(
  current: StreamingTail | null,
  turnId: string,
  toolCallId: string,
): StreamingTail {
  return updateTail(current, turnId, (tail) => ({
    ...tail,
    toolCallIds: tail.toolCallIds.includes(toolCallId)
      ? tail.toolCallIds
      : [...tail.toolCallIds, toolCallId],
  }))
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
  return "meta" in event && isRecord(event.meta) && typeof event.meta.sequence_id === "string"
}
