import {
  ENGINE_EVENT_DELIVERY,
  type EngineEvent,
} from "../protocol"
import { MAX_U64, durableSequenceId, parseU64 } from "../transport"
import type { RottweilerAction } from "./actions"
import { boundedCommandAcks, responseAck } from "./command-acks"
import { projectCommandResult } from "./command-results"
import { EMPTY_TOOL_OUTPUT, boundedUtf8 } from "./display-buffer"
import {
  createInitialState,
  type RottweilerState,
  type ToolProjection
} from "./model"
import { projectSession, projectSessionReview, providerQualifiedRoute } from "./session-projections"
import { projectShellEvent } from "./shell-state"
import { MAX_SUBAGENT_TASK_BYTES, boundedSubagentHistory, nextSubagentArchiveKey, subagentActivity, subagentTerminalSummary } from "./subagents"
import { UNKNOWN_ACTIVITY_TIMING, closeActivityTiming, deriveTodosFromTools, observeActivityTiming, openActivityTiming, projectTodoOutput, retainRecentTools, updateTool } from "./tool-state"
import { MAX_COMPACTION_STREAM_BYTES, appendTailText, attachToolToTail, currentTurnId, retainRecentTurns, retainTranscriptEntry, updateTail } from "./turn-state"

export function reduceRottweilerState(
  state: RottweilerState = createInitialState(),
  action: RottweilerAction,
  activeSessionId: string | null = null,
): RottweilerState {
  switch (action.type) {
    case "engine_event":
      return reduceWireEvent(state, action.event, activeSessionId)
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
  event: EngineEvent,
  activeSessionId: string | null = null,
): RottweilerState {
  const scope = ENGINE_EVENT_DELIVERY[event.type]
  // Transient progress updates retained projections without consuming the durable replay cursor.
  if (scope === "transient") {
    return applyKnownEvent(state, event, null, activeSessionId)
  }
  if (scope === "connection") {
    return applyKnownEvent(state, event, null, activeSessionId)
  }

  const sequenceText = durableSequenceId(event)
  const sequence = parseU64(sequenceText)
  if (sequence === null || sequenceText === null) {
    return recordInvalid(state)
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

  return applyKnownEvent(ready, event, sequenceText, activeSessionId)
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
  activeSessionId: string | null,
): RottweilerState {
  switch (event.type) {
    case "transcript_page_ready":
    case "transcript_content_ready":
      return state
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
    case "session_history_ready":
      return {
        ...state,
        historyReady: { sessionId: event.session_id, through: event.through_sequence ?? null },
        connection: { ...state.connection, phase: state.connection.gap === null ? "connected" : state.connection.phase, error: null },
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
        activeSessionId === null
          ? state.driverClientId === null
            ? undefined
            : event.sessions.find((session) => session.driver_client_id === state.driverClientId)
          : event.sessions.find((session) => session.session_id === activeSessionId)
      const activeSessionModelResolved = activeSession !== undefined && state.models.some(
        (model) =>
          model.available !== false &&
          (model.id === activeSession.model || model.aliases.includes(activeSession.model)),
      )
      return {
        ...state,
        ...(state.model !== null || activeSession === undefined ||
          (state.modelCatalogLoaded && !activeSessionModelResolved)
          ? {}
          : {
            model: activeSession.model,
            provider: activeSession.model.includes("/")
              ? activeSession.model.slice(0, activeSession.model.indexOf("/"))
              : null,
          }),
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
    case "modes_listed": {
      const currentModes = event.modes.filter((mode) => mode.current)
      return {
        ...state,
        ...(currentModes.length === 1 ? { mode: currentModes[0]!.id } : {}),
        modes: event.modes.map((mode) => ({
          id: mode.id,
          description: mode.description,
          current: mode.current,
        })),
        modesTruncated: event.truncated,
        commandAcks: responseAck(state, event.meta.request_id, event.type, event.session_id),
      }
    }
    case "models_listed":
      const currentModels = event.models.filter(
        (model) => model.current === true && model.available !== false,
      )
      const currentModel = currentModels.length === 1 ? currentModels[0] : undefined
      const hasReadyProvider = event.providers.some(
        (provider) => provider.configured && provider.authenticated && provider.reachable,
      )
      const freshUnresolvedSelection =
        !hasReadyProvider &&
        currentModel === undefined &&
        state.transcript.length === 0 &&
        state.streamingTail === null
      return {
        ...state,
        ...(state.model === null && currentModel !== undefined
          ? {
            model: currentModel.id,
            provider: currentModel.provider,
          }
          : freshUnresolvedSelection
            ? { model: null, provider: null }
            : {}),
        models: event.models.map((model) => ({
          id: model.id,
          displayName: model.display_name,
          provider: model.provider,
          aliases: model.aliases,
          current: model.current,
          available: model.available,
          status: model.status ?? null,
          vision: model.capabilities.vision,
          thinking: model.capabilities.thinking,
          toolCalling: model.capabilities.tool_calling,
        })),
        modelAliases: (event.aliases ?? []).map((alias) => ({
          alias: alias.alias,
          candidates: alias.candidates,
          current: alias.current,
        })),
        providers: event.providers.map((provider) => ({
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
        modelCatalogLoaded: true,
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
      const transcript = retainTranscriptEntry(
        state.transcript,
        {
          sequenceId: sequenceId ?? state.lastSequence ?? "0",
          agentTurn: event.agent_turn,
          turn: event.turn,
        },
      )
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
      const todos = target === null ? [] : deriveTodosFromTools(state.tools, target)
      const turns = target === null
        ? state.turns
        : Object.fromEntries(Object.entries(state.turns).filter(([turnId]) => {
          const turn = parseU64(turnId)
          return turn === null || turn <= target
        }))
      const tools = target === null
        ? state.tools
        : Object.fromEntries(Object.entries(state.tools).filter(([, tool]) => {
          const turn = parseU64(tool.turnId)
          return turn === null || turn <= target
        }))
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
        turns,
        tools,
        todos,
        subagents,
        subagentOrder: retainedSubagentIds,
      }
    }
    case "turn_started":
      return {
        ...state,
        errors: [],
        turns: retainRecentTurns(
          state.turns,
          event.turn_id,
          {
            turnId: event.turn_id,
            status: "running",
            usage: null,
            cost: null,
            timing: openActivityTiming(event.meta.emitted_at),
          },
        ),
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
        streamingTail: updateTail(state.streamingTail, event.turn_id, (tail) => appendTailText(tail, "text", event.text)),
      }
    case "thinking_delta":
      return {
        ...state,
        streamingTail: updateTail(state.streamingTail, event.turn_id, (tail) => appendTailText(tail, "thinking", event.text)),
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
    case "tool_progress":
      return state
    case "tool_call_started": {
      const tool: ToolProjection = {
        toolCallId: event.tool_call_id,
        invocationId: event.invocation_id,
        turnId: event.turn_id,
        name: event.name,
        args: event.args,
        status: "running",
        capabilities: [],
        rationale: null,
        diff: null,
        chunks: EMPTY_TOOL_OUTPUT,
        output: null,
        isError: null,
        callIndex: event.call_index,
        timing: openActivityTiming(event.meta.emitted_at),
      }
      return {
        ...state,
        errors: [],
        tools: retainRecentTools(state.tools, event.tool_call_id, tool),
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
      if (existing !== undefined && (existing.invocationId !== event.invocation_id || existing.turnId !== event.turn_id)) return state
      const tool: ToolProjection = {
        toolCallId: event.tool_call_id,
        invocationId: event.invocation_id,
        turnId: event.turn_id,
        name: event.name,
        args: event.args,
        status: "awaiting_approval",
        capabilities: event.capabilities,
        rationale: event.rationale,
        diff: event.diff ?? null,
        chunks: existing?.chunks ?? EMPTY_TOOL_OUTPUT,
        output: existing?.output ?? null,
        isError: existing?.isError ?? null,
        callIndex: existing?.callIndex ?? 0,
        timing: observeActivityTiming(existing?.timing, event.meta.emitted_at),
      }
      return {
        ...state,
        tools: updateTool(state.tools, event.tool_call_id, tool),
        streamingTail: attachToolToTail(state.streamingTail, event.turn_id, event.tool_call_id),
      }
    }
    case "tool_diff_ready": {
      const observed = state.tools[event.tool_call_id]
      if (observed !== undefined && (observed.invocationId !== event.invocation_id || observed.turnId !== event.turn_id)) return state
      const existing: ToolProjection = state.tools[event.tool_call_id] ?? {
        toolCallId: event.tool_call_id,
        invocationId: event.invocation_id,
        turnId: event.turn_id,
        name: "tool",
        args: null,
        status: "running",
        capabilities: [],
        rationale: null,
        diff: null,
        chunks: EMPTY_TOOL_OUTPUT,
        output: null,
        isError: null,
        callIndex: 0,
        timing: UNKNOWN_ACTIVITY_TIMING,
      }
      return {
        ...state,
        tools: updateTool(
          state.tools,
          event.tool_call_id,
          {
            ...existing,
            diff: event.diff,
            timing: observeActivityTiming(existing.timing, event.meta.emitted_at),
          },
        ),
        streamingTail: attachToolToTail(state.streamingTail, event.turn_id, event.tool_call_id),
      }
    }
    case "tool_output_delta": {
      const observed = state.tools[event.tool_call_id]
      if (observed !== undefined && (observed.invocationId !== event.invocation_id || observed.turnId !== event.turn_id)) return state
      const existing: ToolProjection = state.tools[event.tool_call_id] ?? {
        toolCallId: event.tool_call_id,
        invocationId: event.invocation_id,
        turnId: event.turn_id,
        name: "tool",
        args: null,
        status: "running",
        capabilities: [],
        rationale: null,
        diff: null,
        chunks: EMPTY_TOOL_OUTPUT,
        output: null,
        isError: null,
        callIndex: 0,
        timing: UNKNOWN_ACTIVITY_TIMING,
      }
      return {
        ...state,
        tools: updateTool(state.tools, event.tool_call_id, {
          ...existing,
          chunks: existing.chunks.append({ stream: event.stream, chunk: event.chunk }),
          timing: observeActivityTiming(existing.timing, event.meta.emitted_at),
        }),
        streamingTail: attachToolToTail(state.streamingTail, event.turn_id, event.tool_call_id),
      }
    }
    case "tool_call_finished": {
      const existing = state.tools[event.tool_call_id]
      if (existing !== undefined && (existing.invocationId !== event.invocation_id || existing.turnId !== event.turn_id)) return state
      const tool: ToolProjection = {
        toolCallId: event.tool_call_id,
        invocationId: event.invocation_id,
        turnId: event.turn_id,
        name: existing?.name ?? "unknown",
        args: existing?.args ?? null,
        status: "finished",
        capabilities: existing?.capabilities ?? [],
        rationale: existing?.rationale ?? null,
        diff: existing?.diff ?? null,
        chunks: EMPTY_TOOL_OUTPUT,
        output: event.output,
        isError: event.is_error,
        callIndex: event.call_index,
        timing: closeActivityTiming(existing?.timing, event.meta.emitted_at),
      }
      const todos =
        tool.name === "todo" && !event.is_error ? projectTodoOutput(event.output) : null
      return {
        ...state,
        tools: retainRecentTools(state.tools, event.tool_call_id, tool),
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
        turns: retainRecentTurns(
          state.turns,
          event.turn_id,
          {
            turnId: event.turn_id,
            status: event.status,
            usage: event.usage,
            cost: event.cost,
            timing: closeActivityTiming(
              state.turns[event.turn_id]?.timing,
              event.meta.emitted_at,
            ),
          },
        ),
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
      const commandResult = projectCommandResult(event.name, event.message, state)
      return {
        ...state,
        errors: [],
        transcript: retainTranscriptEntry(
          state.transcript,
          {
            sequenceId: commandSequence,
            agentTurn: `command:${event.name}:${commandSequence}`,
            turn: {
              role: "system",
              blocks: [],
              meta: { synthetic: true, summary: false },
            },
            presentation: "command_result",
            title: `/${event.name}`,
            commandResult,
          },
        ),
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
    case "extension_state_committed":
    case "provider_call_accounted":
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

function recordInvalid(state: RottweilerState): RottweilerState {
  return {
    ...state,
    protocol: { ...state.protocol, invalidEvents: state.protocol.invalidEvents + 1 },
  }
}
