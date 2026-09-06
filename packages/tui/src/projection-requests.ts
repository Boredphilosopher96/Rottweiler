import { commandReplyDomain, type ClientAllocationOwner, type ClientAllocationLease } from "./client-allocation"
import type { ReplyAllocation } from "./transport/reply-allocation"
import type { EngineEvent } from "./protocol"
import {
  PROTOCOL_VERSION,
  CLIENT_COMMAND_EXECUTION,
  type ClientCommand,
  type CommandOutcome,
  type PermissionDecision,
  type PermissionApprovalScope,
} from "./protocol"
import { isRecord } from "./transport"

export type ProjectionKind =
  | "commands"
  | "modes"
  | "models"
  | "sessions"
  | "files"
  | "settings"
  | "permissions"
  | "mcp"
  | "runtime_services"

export type ProjectionRequestKind =
  | ProjectionKind
  | "workspace_status"
  | "workspace_diff"
  | "review"
  | "mcp_review"
  | "settings_pending"
  | "subagents"
  | "provider_activation_models"

export interface PendingFilePreview {
  readonly path: string
  readonly requestId: string
  readonly draft: string
  readonly mention: { readonly start: number; readonly end: number } | null
}

export type ProjectionCommand =
  | { readonly type: "search_workspace_files"; readonly query: string; readonly limit: number }
  | { readonly type: "preview_workspace_file"; readonly path: string; readonly max_bytes: number }
  | { readonly type: "switch_model"; readonly model: string; readonly provider?: string | null }
  | { readonly type: "get_session_review" | "get_workspace_status" | "get_context" | "get_cost" }
  | { readonly type: "get_workspace_diff"; readonly path: string; readonly max_bytes: number }
  | { readonly type: "search_sessions"; readonly query: string; readonly limit: number }
  | { readonly type: "rename_session"; readonly sessionId: string; readonly title: string }
  | { readonly type: "list_models"; readonly refresh: boolean }
  | { readonly type: "list_modes" }
  | { readonly type: "list_settings" }
  | { readonly type: "set_setting"; readonly key: string; readonly value: string }
  | { readonly type: "list_mcp_servers" }
  | { readonly type: "list_runtime_services" }
  | { readonly type: "add_mcp_http_server"; readonly name: string; readonly endpoint: string }
  | { readonly type: "add_mcp_stdio_server"; readonly name: string; readonly executable: string; readonly args: string[]; readonly environment: Array<{ readonly key: string; readonly value: string }> }
  | { readonly type: "remove_mcp_server"; readonly name: string }
  | { readonly type: "review_mcp_server"; readonly name: string }
  | { readonly type: "approve_mcp_server"; readonly name: string; readonly fingerprint: string }
  | { readonly type: "set_mcp_server_enabled"; readonly name: string; readonly enabled: boolean }
  | { readonly type: "list_permissions" }
  | { readonly type: "add_session_permission_rule"; readonly pattern: string; readonly action: PermissionDecision }
  | { readonly type: "remove_session_permission_rule"; readonly ruleId: string }
  | { readonly type: "revoke_permission_approval"; readonly approvalId: string; readonly scope: PermissionApprovalScope }
  | { readonly type: "remove_queued_message"; readonly position: string }
  | { readonly type: "clear_queued_messages" }
  | { readonly type: "begin_provider_auth"; readonly provider: string }
  | { readonly type: "configure_builtin_provider"; readonly provider: string }
  | { readonly type: "complete_provider_auth" | "cancel_provider_auth"; readonly provider: string; readonly attemptId: string }
  | { readonly type: "list_commands" | "list_sessions" }

type RequestMeta = ClientCommand["meta"]

interface ProjectionRequestBrokerOptions {
  readonly allocations: ClientAllocationOwner
  readonly clientId: () => string
  readonly sessionId: () => string
  readonly requestId: () => string
  readonly replayActive: () => boolean
  readonly emit: (command: ClientCommand, allocation: ReplyAllocation) => void | CommandOutcome | null | Promise<void | CommandOutcome | null>
  readonly onProjectionFailure: (
    kind: ProjectionKind,
    type: ClientCommand["type"],
    requestId: string,
    message: string,
  ) => void
  readonly onCommandFailure: (
    type: ClientCommand["type"],
    requestId: string,
    outcome: Extract<CommandOutcome, { type: "rejected" }> | null,
    message: string,
    failure: "rejected" | "unavailable" | "exception",
  ) => void
}

const MAX_PENDING_MODEL_SWITCH_REQUESTS = 128
const MAX_PENDING_SETTING_REQUESTS = 128

export class ProjectionRequestBroker {
  readonly #options: ProjectionRequestBrokerOptions
  readonly #settingPredecessors = new Map<string, string | null>()
  readonly #latestRequests: Record<ProjectionRequestKind, string | null> = {
    commands: null,
    modes: null,
    models: null,
    sessions: null,
    files: null,
    settings: null,
    permissions: null,
    mcp: null,
    runtime_services: null,
    workspace_status: null,
    workspace_diff: null,
    review: null,
    mcp_review: null,
    settings_pending: null,
    subagents: null,
    provider_activation_models: null,
  }
  readonly #pendingRequests: Record<ProjectionRequestKind, string | null> = {
    commands: null,
    modes: null,
    models: null,
    sessions: null,
    files: null,
    settings: null,
    permissions: null,
    mcp: null,
    runtime_services: null,
    workspace_status: null,
    workspace_diff: null,
    review: null,
    mcp_review: null,
    settings_pending: null,
    subagents: null,
    provider_activation_models: null,
  }
  #workspaceDiffPath: string | null = null
  #filePreview: PendingFilePreview | null = null
  readonly #forkRequests = new Set<string>()
  readonly #modelSwitchRequests = new Set<string>()

  constructor(options: ProjectionRequestBrokerOptions) {
    this.#options = options
  }

  meta(): RequestMeta {
    return {
      protocol_version: PROTOCOL_VERSION,
      client_id: this.#options.clientId(),
      request_id: this.#options.requestId(),
    }
  }

  issue(kind: ProjectionRequestKind): RequestMeta {
    const meta = this.meta()
    this.#track(kind, meta.request_id)
    return meta
  }

  current(kind: ProjectionRequestKind): string | null {
    return this.#pendingRequests[kind]
  }

  accepts(kind: ProjectionRequestKind, requestId: string | null): boolean {
    const latest = this.#latestRequests[kind]
    return latest === null || requestId === latest
  }

  matches(kind: ProjectionRequestKind, requestId: string | null): boolean {
    const pending = this.#pendingRequests[kind]
    return pending !== null && requestId === pending
  }

  clear(kind: ProjectionRequestKind): void {
    this.#pendingRequests[kind] = null
    if (kind === "workspace_diff") this.#workspaceDiffPath = null
  }

  clearAll(): void {
    for (const kind of Object.keys(this.#latestRequests) as ProjectionRequestKind[]) {
      this.#forget(kind)
    }
    this.#settingPredecessors.clear()
    this.#workspaceDiffPath = null
    this.#filePreview = null
    this.#forkRequests.clear()
    this.#modelSwitchRequests.clear()
  }

  clearForSessionChange(): void {
    for (const kind of [
      "workspace_status",
      "review",
      "mcp_review",
      "commands",
      "modes",
      "models",
      "sessions",
      "settings",
      "settings_pending",
      "permissions",
      "mcp",
      "runtime_services",
      "subagents",
    ] as const) this.#forget(kind)
    this.#modelSwitchRequests.clear()
  }

  clearForReconnect(): void {
    for (const kind of [
      "workspace_status",
      "workspace_diff",
      "review",
      "mcp_review",
      "commands",
      "modes",
      "models",
      "sessions",
      "files",
      "settings",
      "settings_pending",
      "permissions",
      "mcp",
      "runtime_services",
      "subagents",
      "provider_activation_models",
    ] as const) this.#forget(kind)
    this.#filePreview = null
  }

  setFilePreview(pending: PendingFilePreview | null): void {
    this.#filePreview = pending
  }

  filePreview(): PendingFilePreview | null {
    return this.#filePreview
  }

  markProviderActivationModels(): void {
    const requestId = this.#latestRequests.models
    this.#latestRequests.provider_activation_models = requestId
    this.#pendingRequests.provider_activation_models = requestId
  }

  consumeProviderActivationModels(requestId: string | null): boolean {
    if (this.#latestRequests.provider_activation_models !== requestId) return false
    this.#forget("provider_activation_models")
    return true
  }

  trackFork(requestId: string): void {
    this.#forkRequests.add(requestId)
  }

  acceptsFork(requestId: string): boolean {
    return this.#forkRequests.has(requestId)
  }

  clearForks(): void {
    this.#forkRequests.clear()
  }

  discardFork(requestId: string): void {
    this.#forkRequests.delete(requestId)
  }

  consumeModelSwitch(requestId: string): boolean {
    return this.#modelSwitchRequests.delete(requestId)
  }

  acceptsEvent(event: EngineEvent): boolean {
    const record = event as unknown as Record<string, unknown>
    const requestId = requestIdFrom(record)
    switch (event.type) {
      case "todos_read":
        return false // Direct task reads settle through their session capability.
      case "workspace_status_ready":
        return this.accepts("workspace_status", requestId)
      case "runtime_services_listed":
        return this.accepts("runtime_services", requestId)
      case "workspace_diff_ready": {
        const path = isRecord(record.diff) && typeof record.diff.path === "string"
          ? record.diff.path
          : null
        return this.matches("workspace_diff", requestId) && path === this.#workspaceDiffPath
      }
      case "session_review_ready":
        return this.accepts("review", requestId)
      case "workspace_files_found":
        return this.accepts("files", requestId)
      case "workspace_file_preview_ready": {
        const path = isRecord(record.preview) && typeof record.preview.path === "string"
          ? record.preview.path
          : null
        return this.#filePreview !== null &&
          requestId === this.#filePreview.requestId &&
          path === this.#filePreview.path
      }
      case "command_descriptors_listed":
        return this.accepts("commands", requestId)
      case "modes_listed":
        return this.accepts("modes", requestId)
      case "models_listed":
        return this.accepts("models", requestId)
      case "settings_listed":
        return this.accepts("settings", requestId)
      case "permissions_listed":
        return this.accepts("permissions", requestId)
      case "mcp_servers_listed":
        return record.session_id === this.#options.sessionId() &&
          this.matches("mcp", requestId)
      case "mcp_server_approval_reviewed":
        return record.session_id === this.#options.sessionId() &&
          this.matches("mcp_review", requestId)
      case "sessions_listed":
      case "sessions_search_ready":
        return this.accepts("sessions", requestId)
      default:
        return true
    }
  }

  completeEvent(event: EngineEvent): ProjectionKind | null {
    switch (event.type) {
      case "command_descriptors_listed":
        this.clear("commands")
        return "commands"
      case "modes_listed":
        this.clear("modes")
        return "modes"
      case "models_listed":
        this.clear("models")
        return "models"
      case "sessions_listed":
      case "sessions_search_ready":
        this.clear("sessions")
        return "sessions"
      case "settings_listed":
        // The latest settings id intentionally survives. A set_setting response
        // supersedes an older list_settings response while only the loading flag clears.
        this.#settingPredecessors.clear()
        this.clear("settings_pending")
        return "settings"
      case "permissions_listed":
        this.clear("permissions")
        return "permissions"
      case "mcp_servers_listed":
        this.clear("mcp")
        return "mcp"
      case "mcp_server_approval_reviewed":
        this.clear("mcp_review")
        return "mcp"
      case "runtime_services_listed":
        this.clear("runtime_services")
        return "runtime_services"
      case "workspace_files_found":
        this.clear("files")
        return "files"
      default:
        return null
    }
  }

  command(command: ProjectionCommand): string | null {
    if (
      this.#options.replayActive() &&
      CLIENT_COMMAND_EXECUTION[command.type] !== "read"
    ) return null

    const meta = this.meta()
    this.#trackCommand(command, meta.request_id)
    const dispatched = dispatchCommand(command, meta, this.#options.sessionId())
    void this.#emitProjectionCommand(command.type, dispatched, meta.request_id)
    return meta.request_id
  }

  allocate(): ClientAllocationLease { return this.#options.allocations.reserve("decoding", 0) }

  async emit(command: ClientCommand, allocation: ClientAllocationLease): Promise<void | CommandOutcome | null> {
    allocation.moveTo(commandReplyDomain(command.type))
    return this.#options.emit(command, allocation)
  }

  /** Input handlers that ignore results still own decoding through rejection projection. */
  dispatch(command: ClientCommand): void {
    void this.#emitProjectionCommand(command.type, command, command.meta.request_id)
  }

  async consume(command: ClientCommand, consume: (outcome: void | CommandOutcome | null) => void | Promise<void>): Promise<void> {
    using allocation = this.allocate()
    await consume(await this.emit(command, allocation))
  }

  #trackCommand(command: ProjectionCommand, requestId: string): void {
    switch (command.type) {
      case "get_workspace_status":
        this.#track("workspace_status", requestId)
        break
      case "get_workspace_diff":
        this.#track("workspace_diff", requestId)
        this.#workspaceDiffPath = command.path
        break
      case "get_session_review":
        this.#track("review", requestId)
        break
      case "switch_model": {
        if (this.#modelSwitchRequests.size >= MAX_PENDING_MODEL_SWITCH_REQUESTS) {
          const oldest = this.#modelSwitchRequests.values().next().value
          if (oldest !== undefined) this.#modelSwitchRequests.delete(oldest)
        }
        this.#modelSwitchRequests.add(requestId)
        break
      }
      case "list_sessions":
      case "search_sessions":
        this.#track("sessions", requestId)
        break
      case "list_settings":
        this.#settingPredecessors.clear()
        this.#latestRequests.settings = requestId
        this.#track("settings_pending", requestId)
        break
      case "set_setting": {
        if (this.#settingPredecessors.size >= MAX_PENDING_SETTING_REQUESTS) {
          const oldest = this.#settingPredecessors.keys().next().value
          if (oldest !== undefined) this.#settingPredecessors.delete(oldest)
        }
        this.#settingPredecessors.set(requestId, this.#latestRequests.settings)
        this.#latestRequests.settings = requestId
        break
      }
      case "list_permissions":
        this.#track("permissions", requestId)
        break
      case "add_session_permission_rule":
      case "remove_session_permission_rule":
      case "revoke_permission_approval":
        this.#latestRequests.permissions = requestId
        break
      case "list_mcp_servers":
        this.#track("mcp", requestId)
        break
      case "review_mcp_server":
        this.#track("mcp_review", requestId)
        break
      case "add_mcp_http_server":
      case "add_mcp_stdio_server":
      case "remove_mcp_server":
      case "approve_mcp_server":
      case "set_mcp_server_enabled":
        this.#track("mcp", requestId)
        break
      case "list_runtime_services":
        this.#track("runtime_services", requestId)
        break
      case "search_workspace_files":
        this.#track("files", requestId)
        break
      case "list_commands":
        this.#track("commands", requestId)
        break
      case "list_modes":
        this.#track("modes", requestId)
        break
      case "list_models":
        this.#track("models", requestId)
        break
    }
  }

  #track(kind: ProjectionRequestKind, requestId: string): void {
    this.#latestRequests[kind] = requestId
    this.#pendingRequests[kind] = requestId
  }

  #forget(kind: ProjectionRequestKind): void {
    if (kind === "settings") this.#settingPredecessors.clear()
    this.#latestRequests[kind] = null
    this.clear(kind)
  }

  async #emitProjectionCommand(
    type: ClientCommand["type"],
    command: ClientCommand,
    requestId: string,
  ): Promise<void> {
    using allocation = this.allocate()
    try {
      const outcome = await this.emit(command, allocation)
      if (outcome?.type === "rejected") {
        this.#handleFailure(type, requestId, outcome, outcome.error.message, "rejected")
      } else if (outcome === null) {
        this.#handleFailure(
          type,
          requestId,
          null,
          "the engine did not acknowledge the request",
          "unavailable",
        )
      }
    } catch (error) {
      this.#handleFailure(type, requestId, null, safeErrorMessage(error), "exception")
    }
  }

  #handleFailure(
    type: ClientCommand["type"],
    requestId: string,
    outcome: Extract<CommandOutcome, { type: "rejected" }> | null,
    message: string,
    failure: "rejected" | "unavailable" | "exception",
  ): void {
    const kind = projectionKind(type)
    if (kind === null) {
      if (type === "switch_model") this.#modelSwitchRequests.delete(requestId)
      if (type === "review_mcp_server" && this.matches("mcp_review", requestId)) {
        this.clear("mcp_review")
      }
      if (
        (
          type === "add_mcp_http_server" ||
          type === "add_mcp_stdio_server" ||
          type === "remove_mcp_server" ||
          type === "approve_mcp_server" ||
          type === "set_mcp_server_enabled"
        ) &&
        this.matches("mcp", requestId)
      ) {
        this.clear("mcp")
      }
      if (type === "set_setting") {
        if (this.#latestRequests.settings === requestId) {
          this.#latestRequests.settings = this.#settingPredecessors.get(requestId) ?? null
        }
        this.#settingPredecessors.delete(requestId)
      }
      this.#options.onCommandFailure(type, requestId, outcome, message, failure)
      return
    }
    if (kind === "settings") {
      if (!this.matches("settings_pending", requestId) || !this.accepts("settings", requestId)) return
    } else if (!this.matches(kind, requestId)) return
    if (kind === "commands" || kind === "modes" || kind === "models" || kind === "permissions" || kind === "mcp" || kind === "runtime_services") {
      this.clear(kind)
    } else if (kind === "settings") {
      this.clear("settings_pending")
    }
    this.#options.onProjectionFailure(kind, type, requestId, message)
  }
}

export function projectionKind(type: ClientCommand["type"]): ProjectionKind | null {
  switch (type) {
    case "list_commands": return "commands"
    case "list_modes": return "modes"
    case "list_models": return "models"
    case "list_sessions":
    case "search_sessions": return "sessions"
    case "search_workspace_files": return "files"
    case "list_settings": return "settings"
    case "list_permissions": return "permissions"
    case "list_mcp_servers": return "mcp"
    case "list_runtime_services": return "runtime_services"
    default: return null
  }
}

function requestIdFrom(record: Record<string, unknown>): string | null {
  return isRecord(record.meta) && typeof record.meta.request_id === "string"
    ? record.meta.request_id
    : null
}

function dispatchCommand(
  command: ProjectionCommand,
  meta: RequestMeta,
  sessionId: string,
): ClientCommand {
  switch (command.type) {
    case "list_models": return { ...command, meta, session_id: sessionId }
    case "list_sessions": return { type: command.type, meta }
    case "list_commands":
    case "list_modes":
    case "list_settings":
    case "list_mcp_servers":
    case "list_runtime_services":
    case "list_permissions":
    case "clear_queued_messages":
      return { type: command.type, meta, session_id: sessionId }
    case "set_setting":
    case "add_mcp_http_server":
    case "add_mcp_stdio_server":
    case "remove_mcp_server":
    case "review_mcp_server":
    case "approve_mcp_server":
    case "set_mcp_server_enabled":
    case "add_session_permission_rule":
    case "begin_provider_auth":
    case "configure_builtin_provider":
      return { ...command, meta, session_id: sessionId }
    case "remove_session_permission_rule":
      return { type: command.type, meta, session_id: sessionId, rule_id: command.ruleId }
    case "remove_queued_message":
      return { type: command.type, meta, session_id: sessionId, position: command.position }
    case "revoke_permission_approval":
      return {
        type: command.type,
        meta,
        session_id: sessionId,
        approval_id: command.approvalId,
        scope: command.scope,
      }
    case "complete_provider_auth":
    case "cancel_provider_auth":
      return {
        type: command.type,
        meta,
        session_id: sessionId,
        provider: command.provider,
        attempt_id: command.attemptId,
      }
    case "search_sessions": return { ...command, meta }
    case "rename_session":
      return { type: command.type, meta, session_id: command.sessionId, title: command.title }
    case "get_session_review":
    case "get_workspace_status":
    case "get_context":
    case "get_cost":
      return { type: command.type, meta, session_id: sessionId }
    case "get_workspace_diff":
    case "search_workspace_files":
    case "preview_workspace_file":
    case "switch_model":
      return { ...command, meta, session_id: sessionId }
  }
}

function safeErrorMessage(error: unknown): string {
  return error instanceof Error && error.message.length > 0
    ? error.message
    : "the request could not be delivered to the engine"
}
