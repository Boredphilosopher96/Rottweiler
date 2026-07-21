import type { EngineEvent, ToolOutput } from "../protocol"
import { durableSequenceId, isRecord, type WireEngineEvent } from "../transport"
import type { RottweilerAction } from "./actions"
import {
  createInitialState,
  type RottweilerState,
  type TranscriptEntry,
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
export const MAX_COMMAND_ACKS = 256
export const MAX_COMPACTION_STREAM_BYTES = 256 * 1_024
export const MAX_SHELL_COMMAND_BYTES = 8 * 1_024
export const MAX_SHELL_OUTPUT_BYTES = 64 * 1_024
export const MAX_SHELL_OUTPUT_LINES = 32
const KNOWN_EVENT_TYPES = new Set<EngineEvent["type"]>([
  "command_acknowledged",
  "context_snapshot_ready",
  "cost_snapshot_ready",
  "session_review_ready",
  "session_review_updated",
  "prompt_dump_ready",
  "session_replay_completed",
  "session_forked",
  "session_exported",
  "sessions_listed",
  "subagents_listed",
  "subagent_replay_batch",
  "subagent_replay_completed",
  "sessions_search_ready",
  "command_descriptors_listed",
  "models_listed",
  "settings_listed",
  "mcp_servers_listed",
  "runtime_services_listed",
  "mcp_server_approval_reviewed",
  "permissions_listed",
  "provider_auth_started",
  "provider_configured",
  "provider_auth_finished",
  "provider_activation_finished",
  "workspace_files_found",
  "workspace_file_preview_ready",
  "workspace_status_ready",
  "workspace_diff_ready",
  "host_shutdown",
  "session_created",
  "workspace_roots_changed",
  "driver_changed",
  "message_queued",
  "queued_message_removed",
  "queued_messages_cleared",
  "user_message_accepted",
  "session_title_updated",
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
  "tool_diff_ready",
  "tool_output_delta",
  "tool_call_finished",
  "question_asked",
  "question_answered",
  "turn_finished",
  "context_usage_updated",
  "budget_status_changed",
  "compaction_started",
  "compaction_attempt_started",
  "compaction_text_delta",
  "compaction_thinking_delta",
  "compaction_attempt_finished",
  "compaction_finished",
  "compaction_failed",
  "subagent_spawned",
  "subagent_finished",
  "subagent_progress",
  "tool_output_pruned",
  "mode_changed",
  "permission_mode_changed",
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
  "session_exported",
  "sessions_listed",
  "subagents_listed",
  "subagent_replay_batch",
  "subagent_replay_completed",
  "sessions_search_ready",
  "command_descriptors_listed",
  "models_listed",
  "settings_listed",
  "mcp_servers_listed",
  "runtime_services_listed",
  "mcp_server_approval_reviewed",
  "permissions_listed",
  "provider_auth_started",
  "provider_configured",
  "provider_auth_finished",
  "provider_activation_finished",
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
  if (
    event.type === "subagent_progress" ||
    event.type === "compaction_attempt_started" ||
    event.type === "compaction_text_delta" ||
    event.type === "compaction_thinking_delta"
  ) {
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

export function projectSessionTitleUpdate(
  state: RottweilerState,
  event: Extract<EngineEvent, { type: "session_title_updated" }>,
): RottweilerState {
  return {
    ...state,
    sessions: state.sessions.map((session) =>
      session.sessionId === event.meta.session_id
        ? { ...session, title: event.title }
        : session,
    ),
  }
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
        commandAcks: boundedCommandAcks(state.commandAcks, event.meta.request_id, {
            requestId: event.meta.request_id,
            responseType: event.type,
            outcome: event.outcome,
            sessionId: event.session_id ?? null,
          }),
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
    case "session_exported":
      return {
        ...state,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    case "sessions_listed":
      const activeSession =
        state.driverClientId === null
          ? undefined
          : event.sessions.find((session) => session.driver_client_id === state.driverClientId)
      return {
        ...state,
        ...(state.model !== null || activeSession === undefined
          ? {}
          : { model: activeSession.model }),
        sessions: event.sessions.map((session) => ({
          sessionId: session.session_id,
          ...(session.title ? { title: session.title } : {}),
          workspaceName: session.workspace_name,
          model: session.model,
          driverClientId: session.driver_client_id ?? null,
          shellActive: session.shell_active,
        })),
        sessionSearch: null,
        commandAcks: responseAck(state, event.meta.request_id, event.type, null),
      }
    case "subagents_listed":
    case "subagent_replay_batch":
    case "subagent_replay_completed":
      return {
        ...state,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
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
    case "runtime_services_listed":
      return {
        ...state,
        runtimeServices: event.services.slice(0, 64),
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
    case "provider_activation_finished":
      return {
        ...state,
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
    case "queued_message_removed":
      return {
        ...state,
        queuedMessages: state.queuedMessages.filter(
          (message) => message.position !== event.position,
        ),
      }
    case "queued_messages_cleared":
      return { ...state, queuedMessages: [] }
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
      return { ...state, errors: [] }
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
        (event.turn.role === "assistant" || event.turn.role === "tool") &&
        state.streamingTail?.turnId === event.agent_turn
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
        errors: [],
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
      const parsedSpawnedAtMs = Date.parse(event.meta.emitted_at)
      const spawnedAtMs = Number.isFinite(parsedSpawnedAtMs) ? parsedSpawnedAtMs : null
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
          spawnedAtMs,
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
            spawnedAtMs: existing.spawnedAtMs,
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
          spawnedAtMs: existing?.spawnedAtMs ?? null,
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
        errors: [],
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
    case "tool_diff_ready": {
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
          [event.tool_call_id]: { ...existing, diff: event.diff },
        },
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
        errors: [],
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
          attempt: null,
          text: "",
          thinking: "",
        },
      }
    case "compaction_attempt_started":
      return {
        ...state,
        compaction: {
          ...state.compaction,
          active: true,
          summaryTurnId: event.summary_turn_id,
          attempt: event.attempt,
          text: "",
          thinking: "",
        },
      }
    case "compaction_text_delta": {
      if (
        !state.compaction.active ||
        state.compaction.summaryTurnId !== event.summary_turn_id ||
        state.compaction.attempt !== event.attempt
      ) return state
      return {
        ...state,
        compaction: {
          ...state.compaction,
          text: boundedUtf8(
            `${state.compaction.text}${event.text}`,
            MAX_COMPACTION_STREAM_BYTES,
          ),
        },
      }
    }
    case "compaction_thinking_delta": {
      if (
        !state.compaction.active ||
        state.compaction.summaryTurnId !== event.summary_turn_id ||
        state.compaction.attempt !== event.attempt
      ) return state
      return {
        ...state,
        compaction: {
          ...state.compaction,
          thinking: boundedUtf8(
            `${state.compaction.thinking}${event.text}`,
            MAX_COMPACTION_STREAM_BYTES,
          ),
        },
      }
    }
    case "compaction_finished":
      if (
        state.compaction.summaryTurnId !== null &&
        state.compaction.summaryTurnId !== event.summary_turn_id
      ) return state
      return {
        ...state,
        compaction: {
          active: false,
          reason: state.compaction.reason,
          summaryTurnId: event.summary_turn_id,
          reclaimedTokens: event.reclaimed_tokens,
          attempt: null,
          text: "",
          thinking: "",
        },
      }
    case "compaction_failed":
      if (
        state.compaction.summaryTurnId !== null &&
        state.compaction.summaryTurnId !== event.summary_turn_id
      ) return state
      return {
        ...state,
        compaction: {
          ...state.compaction,
          active: false,
          summaryTurnId: event.summary_turn_id,
          attempt: null,
          text: "",
          thinking: "",
        },
      }
    case "mode_changed":
      return event.mode === "plan"
        ? { ...state, mode: event.mode, pendingPlan: null, approvedPlan: null }
        : { ...state, mode: event.mode }
    case "permission_mode_changed":
      // The durable override is reflected by the typed permissions projection;
      // replay only needs to advance the cursor here.
      return state
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
      return projectShellEvent({
        ...state,
        shell: {
          shellId: event.shell_id,
          active: event.active,
          status: event.status ?? null,
          capturedOutput: event.captured_output ?? null,
        },
      }, event, sequenceId ?? state.lastSequence ?? "0")
    case "error":
      return { ...state, errors: [...state.errors.slice(-63), event.error] }
    case "command_finished": {
      const commandSequence = sequenceId ?? state.lastSequence ?? "0"
      const message = formatCommandMessage(event.name, event.message, state)
      return {
        ...state,
        errors: [],
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
                  text: message.length === 0 ? "Command completed." : message,
                },
              ],
              meta: { synthetic: true, summary: false },
            },
            presentation: "command_result",
            title: `/${event.name}`,
          },
        ],
      }
    }
    case "context_usage_updated":
      return {
        ...state,
        context: {
          turn_id: event.turn_id,
          stable_prefix_hash: event.stable_prefix_hash,
          used_tokens: event.used_tokens,
          usable_tokens: event.usable_tokens,
          reserved_tokens: event.reserved_tokens,
          context_window_known: event.context_window_known,
          ...(event.context_window_reason === undefined
            ? {}
            : { context_window_reason: event.context_window_reason }),
          cache_breakpoints: state.context?.cache_breakpoints ?? [],
          items: state.context?.items ?? [],
        },
      }
    case "session_title_updated":
      return projectSessionTitleUpdate(state, event)
    case "compaction_attempt_finished":
    case "tool_output_pruned":
    case "model_context_cleared":
    case "context_item_pinned":
    case "context_item_evicted":
    case "hook_failed":
    case "guard_triggered":
      return state
  }
}

const HIDDEN_COMMAND_RESULT_FIELDS = new Set([
  "protocol_version",
  "request_id",
  "session_id",
  "turn_id",
  "item_id",
  "stable_prefix_hash",
  "machine_local_path",
  "original_hash",
  "current_hash",
  "base_hash",
  "diff_hash",
  "truncated",
])

/** Keep extension command payloads structured on the wire without exposing wire JSON in the UI. */
function formatCommandMessage(name: string, source: string, state: RottweilerState): string {
  if (name === "context" && state.context !== null) return formatContextCommand(state.context)
  if (name === "cost" && state.cost !== null) return formatCostCommand(state.cost)

  const trimmed = source.trim()
  if (name === "help") return formatHelpCommand(trimmed)
  if (name === "status") return formatStatusCommand(trimmed)
  if (name === "mode") return formatModeCommand(trimmed)
  if (name === "permissions") return formatPermissionCommand(trimmed)
  if (name === "plan") return formatPlanCommand(trimmed)
  if (name === "review") return formatReviewCommand(trimmed)
  if (name === "trust") return formatTrustCommand(trimmed)
  if (name === "mcp") return formatMcpCommand(trimmed)
  const completion = commandCompletionTitle(name)
  if (completion !== null) return trimmed.length === 0
    ? `**${completion}**`
    : `**${completion}** · ${sentenceCase(singleLineCommand(trimmed, 180))}`
  if (trimmed.length === 0 || (!trimmed.startsWith("{") && !trimmed.startsWith("["))) {
    return boundedCommandText(trimmed)
  }
  try {
    const parsed: unknown = JSON.parse(trimmed)
    const lines = humanResultLines(parsed, 0)
    return lines.length === 0 ? "Command completed." : boundedCommandRows(lines)
  } catch {
    // A structured-looking result that cannot be decoded is not safe UI text.
    // It may be a truncated wire payload, so fail closed instead of dumping it.
    return "_Command returned structured details that could not be displayed safely._"
  }
}

function formatContextCommand(snapshot: NonNullable<RottweilerState["context"]>): string {
  const used = unsigned(snapshot.used_tokens)
  const usable = unsigned(snapshot.usable_tokens)
  const reserved = unsigned(snapshot.reserved_tokens)
  const percent = snapshot.context_window_known && usable > 0n
    ? Number((used * 100n) / usable)
    : null
  const filled = percent === null ? 0 : Math.min(20, Math.round(percent / 5))
  const meter = percent === null ? "" : `\`${"█".repeat(filled)}${"░".repeat(20 - filled)}\` ${percent}%`
  const groups = new Map<string, { count: number; tokens: bigint }>()
  for (const item of snapshot.items) {
    const current = groups.get(item.kind) ?? { count: 0, tokens: 0n }
    current.count += 1
    current.tokens += unsigned(item.estimated_tokens)
    groups.set(item.kind, current)
  }
  const rows = [...groups.entries()]
    .sort((left, right) => left[1].tokens === right[1].tokens
      ? left[0].localeCompare(right[0])
      : left[1].tokens > right[1].tokens ? -1 : 1)
    .map(([kind, group]) => `| ${contextKindLabel(kind)} | ${group.count} | ${compactNumber(group.tokens)} |`)

  const capacity = snapshot.context_window_known
    ? `**${compactNumber(used)} / ${compactNumber(usable)} tokens** · ${compactNumber(reserved)} reserved`
    : `**${compactNumber(used)} tokens used** · context limit unavailable`
  return [
    capacity,
    ...(meter === "" ? [] : [meter]),
    `**${snapshot.items.length} items** in the active context`,
    ...(rows.length === 0
      ? ["\n_No context items yet._"]
      : ["\n| Source | Items | Tokens |", "| --- | ---: | ---: |", ...rows]),
  ].join("\n")
}

function formatCostCommand(snapshot: NonNullable<RottweilerState["cost"]>): string {
  const usage = snapshot.session_usage
  const cachePercent = (snapshot.cache_hit_basis_points / 100).toFixed(
    snapshot.cache_hit_basis_points % 100 === 0 ? 0 : 2,
  )
  const subscription = unsigned(snapshot.session_subscription_quota_entries) > 0n
  const unavailable = unsigned(snapshot.session_cost_unavailable_entries) > 0n
  const billing = subscription
    ? "Covered by subscription quota"
    : unavailable || !snapshot.session_monetary_accounting_complete
      ? "Cost unavailable for part of this session"
      : formatMicrosUsd(snapshot.session_cost_micros_usd)
  return [
    `**${billing}**`,
    `| Input | Output | Reasoning | Cache read | Cache hit |`,
    `| ---: | ---: | ---: | ---: | ---: |`,
    `| ${compactNumber(unsigned(usage.input_tokens))} | ${compactNumber(unsigned(usage.output_tokens))} | ${compactNumber(unsigned(usage.reasoning_tokens))} | ${compactNumber(unsigned(usage.cache_read_tokens))} | ${cachePercent}% |`,
    `\n${snapshot.turns.length} accounted turn${snapshot.turns.length === 1 ? "" : "s"} · ${snapshot.utc_day} UTC`,
  ].join("\n")
}

function formatHelpCommand(source: string): string {
  const rows = source.split("\n").map((line) => line.trim()).filter(Boolean).flatMap((line) => {
    const [usage, description] = line.split(/\s+—\s+/, 2)
    return usage === undefined || description === undefined ? [] : [`| \`${usage}\` | ${description} |`]
  })
  return rows.length === 0
    ? (source.length === 0 ? "No commands are available." : source)
    : [
        "| Command | What it does |",
        "| --- | --- |",
        ...rows.slice(0, 30),
        ...(rows.length > 30 ? [`| … | ${rows.length - 30} more commands |`] : []),
      ].join("\n")
}

function formatStatusCommand(source: string): string {
  const values = new Map(source.split("\n").flatMap((line) => {
    const separator = line.indexOf(":")
    return separator < 0 ? [] : [[line.slice(0, separator).trim().toLowerCase(), line.slice(separator + 1).trim()]]
  }))
  const agent = values.get("agent")
  const mode = values.get("mode")
  const queued = values.get("queued messages")
  if (agent === undefined || mode === undefined || queued === undefined) return source
  return `**${sentenceCase(agent)}** · ${sentenceCase(mode)} mode · ${queued} queued message${queued === "1" ? "" : "s"}`
}

function formatPermissionCommand(source: string): string {
  if (source.length === 0) return "**Permissions updated**"
  const lines = source.split("\n").map((line) => line.trim()).filter(Boolean)
  if (lines.length <= 1) return `**Permissions** · ${sentenceCase(lines[0] ?? "updated")}`

  const mode = lines.find((line) => /^permission mode:/i.test(line))?.split(":", 2)[1]?.trim()
  const fallback = lines.find((line) => /^default permission:/i.test(line))?.split(":", 2)[1]?.trim()
  const approvals = lines.find((line) => /^remembered approvals:/i.test(line))
  const rules: string[] = []
  let scope = "Project"
  for (const line of lines) {
    if (/^configured rules:/i.test(line)) {
      scope = "Project"
      continue
    }
    if (/^session rules:/i.test(line)) {
      scope = "Session"
      continue
    }
    if (/^this session:/i.test(line)) {
      scope = "Session"
      continue
    }
    if (/^this project:/i.test(line)) {
      scope = "Project"
      continue
    }
    if (!line.startsWith("- ")) continue
    const values = line.slice(2).split(" · ")
    // Approval inventory rows contain an opaque revocation id. The dedicated
    // permission picker owns revocation; the transcript should show intent,
    // not internal credential/rule identifiers.
    if (values.length >= 3 && values.at(-1)?.startsWith("revoke with ")) {
      rules.push(`| ${scope} | Remembered | ${markdownCell(humanLabel(values[0] ?? "tool"))} |`)
      continue
    }
    rules.push(`| ${scope} | ${sentenceCase(values[0] ?? "ask")} | \`${markdownCell(values.slice(1).join(" · ") || "all tools")}\` |`)
  }
  const heading = mode === undefined
    ? "**Permission settings**"
    : `**${sentenceCase(mode)} permissions**${fallback === undefined ? "" : ` · ${fallback} by default`}`
  return [
    heading,
    ...(approvals === undefined ? [] : [approvals.replace(/^remembered approvals:/i, "Remembered:")]),
    ...(rules.length === 0
      ? []
      : ["\n| Scope | Decision | Applies to |", "| --- | --- | --- |", ...rules.slice(0, 16)]),
    ...(rules.length > 16 ? [`\n… ${rules.length - 16} more rules · open \`/permissions\` to manage`] : []),
  ].join("\n")
}

function formatModeCommand(source: string): string {
  const match = /^(?:active mode:|mode changed to)\s*(\S+)/i.exec(source)
  if (match === null) return source.length === 0 ? "**Mode unchanged**" : boundedCommandText(source)
  const mode = sentenceCase(match[1] ?? "execute")
  return source.toLocaleLowerCase().startsWith("active")
    ? `**${mode} mode** · currently active`
    : `**${mode} mode enabled**`
}

function formatPlanCommand(source: string): string {
  if (source.length === 0 || /^no plan/i.test(source)) return "_No plan has been submitted._"
  const lines = source.split("\n")
  const title = lines.shift()?.trim() ?? "Plan"
  const body = boundedCommandRows(lines, 32)
  return [`## ${title.replace(/^#+\s*/, "")}`, body].filter(Boolean).join("\n\n")
}

function formatReviewCommand(source: string): string {
  if (source.length === 0 || /no changed files/i.test(source)) return "**No changed files**"
  const lines = source.split("\n").map((line) => line.trim()).filter(Boolean)
  const summary = lines.shift() ?? "Session review"
  const files = lines.filter((line) => line.startsWith("- ")).map((line) => {
    const [path, status, note] = line.slice(2).split(" · ")
    return `| \`${markdownCell(path ?? "file")}\` | ${sentenceCase(status ?? "changed")} | ${markdownCell(note ?? "")} |`
  })
  return [
    `**${sentenceCase(summary)}**`,
    ...(files.length === 0 ? [] : ["\n| File | Status | Note |", "| --- | --- | --- |", ...files.slice(0, 20)]),
    ...(files.length > 20 ? [`\n… ${files.length - 20} more files · open \`/review\` for the full diff`] : []),
  ].join("\n")
}

function formatTrustCommand(source: string): string {
  if (source.length === 0) return "**Folder trust updated**"
  const safe = singleLineCommand(source, 200)
  const trusted = /(?:^|\b)(?:trusted|granted)(?:\b|$)/i.test(safe) && !/untrusted|not trusted/i.test(safe)
  const revoked = /revoked|untrusted|not trusted/i.test(safe)
  return `**${trusted ? "Folder trusted" : revoked ? "Folder not trusted" : "Folder trust"}** · ${sentenceCase(safe)}`
}

function formatMcpCommand(source: string): string {
  if (source.length === 0) return "**MCP settings updated**"
  const lines = source.split("\n").map((line) => line.trim()).filter(Boolean)
  const rows = lines.flatMap((line) => {
    const values = line.replace(/^-\s*/, "").split(" · ")
    return values.length < 2 ? [] : [`| ${markdownCell(values[0] ?? "Server")} | ${markdownCell(values.slice(1).join(" · "))} |`]
  })
  return rows.length === 0
    ? boundedCommandText(source)
    : ["| Server | Status |", "| --- | --- |", ...rows.slice(0, 20), ...(rows.length > 20 ? [`| … | ${rows.length - 20} more servers |`] : [])].join("\n")
}

function commandCompletionTitle(name: string): string | null {
  return ({
    compact: "Compaction started",
    interrupt: "Interrupt requested",
    rewind: "Session rewound",
    fork: "Session forked",
    "add-dir": "Workspace updated",
    init: "Workspace initialized",
    "deep-init": "Workspace initialized",
  } as Record<string, string>)[name] ?? null
}

function boundedCommandText(source: string): string {
  if (source === "") return "Command completed."
  return boundedCommandRows(source.split("\n"), 32)
}

function boundedCommandRows(lines: readonly string[], maximum = 24): string {
  if (lines.length <= maximum) return lines.join("\n")
  return [...lines.slice(0, maximum), `\n… ${lines.length - maximum} more lines`].join("\n")
}

function singleLineCommand(source: string, maximum: number): string {
  const safe = source.replace(/[\u0000-\u001f\u007f-\u009f]/g, " ").replace(/\s+/g, " ").trim()
  return safe.length <= maximum ? safe : `${safe.slice(0, maximum - 1)}…`
}

function markdownCell(value: string): string {
  return value.replaceAll("|", "\\|").replaceAll("`", "'")
}

function contextKindLabel(kind: string): string {
  return ({
    system: "System",
    tool_definitions: "Tools",
    project_instructions: "Project instructions",
    conversation: "Conversation",
    tool_result: "Tool results",
    pinned: "Pinned",
    queued_message: "Queued messages",
  } as Record<string, string>)[kind] ?? humanLabel(kind)
}

function compactNumber(value: bigint): string {
  const units = [[1_000_000_000n, "B"], [1_000_000n, "M"], [1_000n, "k"]] as const
  for (const [divisor, suffix] of units) {
    if (value < divisor) continue
    const whole = value / divisor
    const tenth = (value % divisor) * 10n / divisor
    return `${whole}${tenth === 0n ? "" : `.${tenth}`}${suffix}`
  }
  return value.toString()
}

function formatMicrosUsd(value: string): string {
  const micros = unsigned(value)
  const dollars = micros / 1_000_000n
  const cents = (micros % 1_000_000n) / 10_000n
  return `$${dollars}.${cents.toString().padStart(2, "0")}`
}

function unsigned(value: string): bigint {
  try {
    const parsed = BigInt(value)
    return parsed < 0n ? 0n : parsed
  } catch {
    return 0n
  }
}

function sentenceCase(value: string): string {
  if (value.length === 0) return value
  return `${value.slice(0, 1).toUpperCase()}${value.slice(1).replace(/[.!]+$/, "")}`
}

function humanResultLines(value: unknown, depth: number, label?: string): string[] {
  if (depth > 5) return label === undefined ? [] : [`${label}: details omitted`]
  if (value === null || value === undefined) {
    return label === undefined ? [] : [`${label}: none`]
  }
  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
    const rendered = typeof value === "string" ? humanEnum(value) : String(value)
    return label === undefined ? [rendered] : [`${label}: ${rendered}`]
  }
  if (Array.isArray(value)) {
    if (value.length === 0) return label === undefined ? ["None"] : [`${label}: none`]
    const heading = label === undefined ? [] : [`${label}:`]
    return [
      ...heading,
      ...value.flatMap((item) =>
        humanResultLines(item, depth + 1).map((line, index) =>
          `${index === 0 ? "- " : "  "}${line}`,
        ),
      ),
    ]
  }
  if (typeof value !== "object") return []
  const record = value as Record<string, unknown>
  const entries = Object.entries(record).filter(
    ([key, item]) =>
      !HIDDEN_COMMAND_RESULT_FIELDS.has(key) &&
      !(key === "data" && item !== null && typeof item === "object"),
  )
  const unwrapped = record.data
  const lines =
    unwrapped !== null && typeof unwrapped === "object"
      ? humanResultLines(unwrapped, depth + 1)
      : entries.flatMap(([key, item]) => {
          const label = humanLabel(key)
          return sensitiveCommandResultField(key)
            ? [`${label}: [redacted]`]
            : humanResultLines(item, depth + 1, label)
        })
  return label === undefined || lines.length === 0 ? lines : [`${label}:`, ...lines.map((line) => `  ${line}`)]
}

function sensitiveCommandResultField(key: string): boolean {
  return /token|secret|password|authorization|api[_-]?key|credential/i.test(key)
}

function humanLabel(value: string): string {
  const words = value.replaceAll("_", " ").replaceAll("-", " ")
  return `${words.slice(0, 1).toUpperCase()}${words.slice(1)}`
}

function humanEnum(value: string): string {
  return value.includes("_") && !value.includes(" ") ? value.replaceAll("_", " ") : value
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
  readonly title?: string
  readonly workspace_name: string
  readonly model: string
  readonly driver_client_id?: string | null
  readonly shell_active: boolean
}): RottweilerState["sessions"][number] {
  return {
    sessionId: session.session_id,
    ...(session.title ? { title: session.title } : {}),
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

type UserShellStateChangedEvent = Extract<EngineEvent, { type: "user_shell_state_changed" }>

function projectShellEvent(
  state: RottweilerState,
  event: UserShellStateChangedEvent,
  sequenceId: string,
): RottweilerState {
  const agentTurn = `shell:${event.shell_id}`
  const existingIndex = state.transcript.findIndex((entry) => entry.agentTurn === agentTurn)
  const existing = existingIndex < 0 ? undefined : state.transcript[existingIndex]
  const commandSource = typeof event.command === "string"
    ? event.command
    : existing?.shell?.command ?? "Shell command"
  const command = boundedUtf8(sanitizeShellText(commandSource).trim(), MAX_SHELL_COMMAND_BYTES)
  const rawOutput = sanitizeShellText(event.captured_output ?? existing?.shell?.capturedOutput ?? "")
  const lineBound = boundedShellLines(rawOutput, MAX_SHELL_OUTPUT_LINES)
  const capturedOutput = boundedUtf8(lineBound, MAX_SHELL_OUTPUT_BYTES)
  const outputTruncated =
    rawOutput.split("\n").length > MAX_SHELL_OUTPUT_LINES ||
    new TextEncoder().encode(lineBound).byteLength > MAX_SHELL_OUTPUT_BYTES
  const shell = {
    shellId: event.shell_id,
    command: command === "" ? "Shell command" : command,
    active: event.active,
    status: event.status ?? existing?.shell?.status ?? null,
    capturedOutput,
    outputTruncated,
  } as const
  const entry: TranscriptEntry = {
    sequenceId: existing?.sequenceId ?? sequenceId,
    agentTurn,
    turn: {
      role: "system",
      blocks: [],
      meta: { synthetic: true, summary: false },
    },
    presentation: "shell_result",
    shell,
  }
  const projectedState = {
    ...state,
    shell: { ...state.shell, capturedOutput },
  }
  if (existingIndex < 0) {
    return { ...projectedState, transcript: [...state.transcript, entry] }
  }
  const transcript = [...state.transcript]
  transcript[existingIndex] = entry
  return { ...projectedState, transcript }
}

function sanitizeShellText(value: string): string {
  return value
    .replace(/\r\n/g, "\n")
    .replace(/\r/g, "\n")
    // OSC and CSI escape sequences are terminal instructions, not retained
    // transcript content. Removing them prevents output from changing the UI.
    .replace(/\u001b\][^\u0007]*(?:\u0007|\u001b\\)/g, "")
    .replace(/\u001b(?:\[[0-?]*[ -/]*[@-~]|.)/g, "")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001a\u001c-\u001f\u007f]/g, "")
}

function boundedShellLines(value: string, maximum: number): string {
  const lines = value.split("\n")
  if (lines.length <= maximum) return value
  return [
    ...lines.slice(0, Math.max(0, maximum - 1)),
    `… ${lines.length - maximum + 1} more lines`,
  ].join("\n")
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
    case "tool_call_started": {
      if (typeof event.name !== "string") return "using tool"
      const toolName = compactActivityValue(event.name, 24) ?? "tool"
      const detail = safeSubagentToolDetail(event.name, event.args)
      return boundedActivity(`using tool · ${toolName}${detail === null ? "" : ` · ${detail}`}`)
    }
    case "tool_approval_needed":
      return typeof event.name === "string"
        ? `awaiting approval · ${event.name}`
        : "awaiting approval"
    case "tool_diff_ready":
      return "prepared diff"
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

function safeSubagentToolDetail(name: string, args: unknown): string | null {
  try {
    return subagentToolDetail(name, args)
  } catch {
    return null
  }
}

function subagentToolDetail(name: string, args: unknown): string | null {
  if (!isRecord(args)) return null
  const normalized = name.toLowerCase()
  if (normalized === "bash" || normalized === "shell") {
    return compactActivityValue(firstString(args, ["command", "cmd"]), 48)
  }
  if (normalized === "read" || normalized === "write" || normalized === "edit") {
    const path = firstString(args, ["path", "file_path", "filePath"])
    return path === null ? null : compactSubagentPath(path, 48)
  }
  if (normalized === "grep" || normalized === "glob") {
    return compactActivityValue(firstString(args, ["pattern", "query", "regex"]), 48)
  }
  return null
}

function firstString(
  record: Readonly<Record<string, unknown>>,
  keys: readonly string[],
): string | null {
  for (const key of keys) {
    const value = record[key]
    if (typeof value === "string") return value
  }
  return null
}

function compactSubagentPath(value: string, limit: number): string | null {
  const compact = value.replaceAll("\\", "/").replace(/\s+/g, " ").trim()
  if (compact === "") return null
  const parts = compact.split("/").filter(Boolean)
  const tail = parts.length <= 2 ? parts.join("/") : parts.slice(-2).join("/")
  if (tail.length <= limit) return tail
  return `…${tail.slice(-(limit - 1))}`
}

function compactActivityValue(value: string | null, limit: number): string | null {
  if (value === null) return null
  const compact = value.replace(/\s+/g, " ").trim()
  if (compact === "") return null
  return compact.length <= limit ? compact : `${compact.slice(0, limit - 1)}…`
}

function boundedActivity(value: string): string {
  const compact = value.replace(/\s+/g, " ").trim()
  return compact.length <= 72 ? compact : `${compact.slice(0, 71)}…`
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
    | "session_exported"
    | "sessions_listed"
    | "subagents_listed"
    | "subagent_replay_batch"
    | "subagent_replay_completed"
    | "sessions_search_ready"
    | "command_descriptors_listed"
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
    | "host_shutdown",
  sessionId: string | null,
): RottweilerState["commandAcks"] {
  return boundedCommandAcks(state.commandAcks, requestId, {
      requestId,
      responseType,
      outcome: null,
      sessionId,
    })
}

function boundedCommandAcks(
  current: RottweilerState["commandAcks"],
  requestId: string,
  acknowledgement: RottweilerState["commandAcks"][string],
): RottweilerState["commandAcks"] {
  const next = { ...current }
  delete (next as Record<string, unknown>)[requestId]
  ;(next as Record<string, unknown>)[requestId] = acknowledgement
  const overflow = Object.keys(next).length - MAX_COMMAND_ACKS
  for (const key of Object.keys(next).slice(0, Math.max(0, overflow))) {
    delete (next as Record<string, unknown>)[key]
  }
  return next
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
