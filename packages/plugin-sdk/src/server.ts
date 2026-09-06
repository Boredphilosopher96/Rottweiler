import validateEffectCall from "./generated/extension-effect-call-validator.js"
import validateToolResponse from "./generated/tool-response-validator.js"
import validateInvocationId from "./generated/extension-invocation-id-validator.js"
import type { ExtensionInvocationId } from "./generated/extension-contract"
import validateUiPanelUpdate from "./generated/ui-panel-update-validator.js"
import validateUiPanelUpdated from "./generated/ui-panel-updated-validator.js"
import validateUiContribution from "./generated/ui-contribution-validator.js"
import { eventSourceReader, type EventHandlerContext } from "./host-events"
import validateEventNotice from "./generated/extension-event-notice-validator.js"
import validateEventOutcome from "./generated/extension-event-outcome-validator.js"
import validateEventKind from "./generated/extension-event-kind-validator.js"
import { hostStateContext, type HostSessionApi, type HostStateApi } from "./host-state"
import { invokeHook, type HookHandlers } from "./hooks"
import validateProviderRequest from "./generated/provider-request-validator.js"
import validateProviderEvent from "./generated/provider-event-validator.js"
import validateHookInput from "./generated/hook-input-validator.js"
import validateHookDirective from "./generated/hook-directive-validator.js"
import { ToolProgressReporter } from "./tool-progress"
import { StreamCredit } from "./stream-credit"
import {
  PLUGIN_PROTOCOL_VERSION,
  PLUGIN_HOST_ID,
  PROTOCOL_LIMITS,
  RPC_METHODS,
  type CommandExecuteParams,
  type ExtensionEventNotice,
  type ExtensionEventOutcome,
  type ExtensionEventKind,
  type HookInput,
  type InjectMessageResult,
  type JsonObject,
  type JsonValue,
  type PluginManifest,
  type PluginPushMethod,
  type ProviderCompleteParams,
  type ProviderEvent,
  type ProviderModelsParams,
  type ProviderModelsResponse,
  type ProviderHttpRequest,
  type ProviderHttpResponse,
  type ProviderStream,
  type RpcId,
  type ToolCallParams,
  type ToolResponse,
  type ToolProgress,
} from "./generated/protocol-3"
import {
  BoundedJsonWriter,
  DEFAULT_MAX_RPC_LINE_BYTES,
  readBoundedLines,
  readableStreamBytes,
  type RpcOutput,
} from "./transport"

export class SafeRpcError extends Error {
  constructor(
    readonly code: number,
    readonly safeMessage: string,
    readonly safeData?: JsonValue,
  ) {
    super(safeMessage)
    this.name = "SafeRpcError"
  }
}

export interface PushApi {
  publishPanel(id: string, data: JsonValue): Promise<number>
  injectMessage(sessionId: string, content: string): Promise<InjectMessageResult>
  setStatus(sessionId: string, status: string): Promise<void>
  notify(title: string, message: string, sessionId?: string): Promise<void>
}

export interface HandlerContext {
  readonly push: PushApi
  readonly session: HostSessionApi
  readonly state: HostStateApi
  /** Aborts on shutdown, provider cancellation, or the admitted operation's deadline. */
  readonly signal: AbortSignal
  /** Writes only a bounded label to stderr. Never pass prompts, tool args, or credentials. */
  debug(label: string): void
}

export interface ProviderHandlerContext extends HandlerContext {
  /** Host-owned authenticated HTTP. Credential values never enter this process. */
  readonly providerHttp: {
    request(credentialReference: string, request: ProviderHttpRequest): Promise<ProviderHttpResponse>
  }
}

export interface ToolHandlerContext extends HandlerContext {
  /** File and HTTP tools run inside this request's approved host scope. */
  readonly effects: { callTool(name: string, input: JsonValue): Promise<ToolResponse> }

  /** Replaces pending progress; delivery renews idle time within the immutable total deadline. */
  progress(update: ToolProgress): void
}

export type ToolHandler = (params: ToolCallParams, context: ToolHandlerContext) => ToolResponse | Promise<ToolResponse>
export type CommandHandler = (
  params: CommandExecuteParams,
  context: HandlerContext,
) => JsonValue | Promise<JsonValue>
export type EventHandler = (params: ExtensionEventNotice, context: EventHandlerContext) => ExtensionEventOutcome | Promise<ExtensionEventOutcome>
export type ProviderHandler = (
  params: ProviderCompleteParams,
  context: ProviderHandlerContext,
) => ProviderStream | Promise<ProviderStream>
export type ProviderModelsHandler = (
  params: ProviderModelsParams,
  context: ProviderHandlerContext,
) => ProviderModelsResponse | Promise<ProviderModelsResponse>

export interface PluginHandlers {
  readonly tools?: Readonly<Record<string, ToolHandler>>
  readonly commands?: Readonly<Record<string, CommandHandler>>
  readonly hooks?: HookHandlers
  readonly events?: Readonly<Partial<Record<ExtensionEventKind, EventHandler>>>
  readonly providers?: Readonly<Record<string, ProviderHandler>>
  readonly providerModels?: Readonly<Record<string, ProviderModelsHandler>>
  readonly shutdown?: (signal: AbortSignal) => void | Promise<void>
}

export interface PluginDefinition {
  readonly manifest: PluginManifest
  readonly handlers: PluginHandlers
}

export interface ServerTransport {
  readonly input: AsyncIterable<Uint8Array>
  readonly output: RpcOutput
  readonly error?: { write(message: string): unknown }
}

export interface RunOptions {
  readonly maxLineBytes?: number
  readonly handlerTimeoutMs?: number
  readonly transport?: ServerTransport
  readonly signal?: AbortSignal
}

const own = (value: object, key: string): boolean => Object.prototype.hasOwnProperty.call(value, key)
const byteLength = (value: string): number => new TextEncoder().encode(value).byteLength

class BoundedByteQueue implements AsyncIterable<Uint8Array> {
  readonly #items: Uint8Array[] = []
  readonly #readers: Array<(result: IteratorResult<Uint8Array>) => void> = []
  #bytes = 0
  #done = false
  #error: Error | undefined

  constructor(private readonly capacity: number, private readonly maxBytes: number, private readonly onCancel: () => void) {}

  push(item: Uint8Array): void {
    if (this.#done) return
    const reader = this.#readers.shift()
    if (reader !== undefined) reader({ done: false, value: item })
    else {
      if (this.#items.length >= this.capacity || this.#bytes + item.byteLength > this.maxBytes) {
        this.fail(new SafeRpcError(-32005, "provider HTTP response buffer exceeded"))
        this.onCancel()
        return
      }
      this.#items.push(item)
      this.#bytes += item.byteLength
    }
  }

  finish(): void {
    if (this.#done) return
    this.#done = true
    for (const reader of this.#readers.splice(0)) reader({ done: true, value: undefined })
  }

  fail(error: Error): void {
    this.#error ??= error
    this.#items.length = 0
    this.#bytes = 0
    this.finish()
  }

  async *[Symbol.asyncIterator](): AsyncIterator<Uint8Array> {
    try {
      while (true) {
        if (this.#items.length > 0) {
          const item = this.#items.shift()
          if (item !== undefined) {
            this.#bytes -= item.byteLength
            yield item
          }
          continue
        }
        if (this.#done) {
          if (this.#error !== undefined) throw this.#error
          return
        }
        const next = await new Promise<IteratorResult<Uint8Array>>((resolve) => this.#readers.push(resolve))
        if (next.done) {
          if (this.#error !== undefined) throw this.#error
          return
        }
        yield next.value
      }
    } finally {
      if (!this.#done) this.onCancel?.()
    }
  }
}

interface PendingProviderHttp {
  readonly cleanup: () => void
  readonly body: BoundedByteQueue
  readonly resolve: (response: ProviderHttpResponse) => void
  readonly reject: (error: Error) => void
  sawHead: boolean
  sawFinished: boolean
}

function requireText(value: string, label: string, max: number): void {
  if (byteLength(value) === 0 || byteLength(value) > max || /[\p{Cc}]/u.test(value)) {
    throw new Error(`${label} has an invalid length or contains control characters`)
  }
}

function requireKeys(value: object, label: string, allowed: readonly string[]): void {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) throw new Error(`${label} contains unknown field ${key}`)
  }
}

function requireRpcKeys(value: object, label: string, allowed: readonly string[]): void {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) throw new SafeRpcError(-32602, `invalid ${label}`)
  }
}

type CanonicalNameKind = "plugin" | "tool" | "command"

function requireCanonicalName(value: string, kind: CanonicalNameKind, label: string): void {
  const expression = kind === "tool"
    ? /^[a-z0-9_]+$/
    : /^[a-z0-9_.-]+$/
  if (byteLength(value) > PROTOCOL_LIMITS.maxNameBytes || !expression.test(value)) {
    throw new Error(`${label} must be a bounded canonical ${kind} name`)
  }
}

function jsonDepth(value: JsonValue, depth = 0): number {
  if (Array.isArray(value)) return value.reduce<number>((max, child) => Math.max(max, jsonDepth(child, depth + 1)), depth)
  if (value !== null && typeof value === "object") {
    return Object.values(value).reduce<number>((max, child) => Math.max(max, jsonDepth(child, depth + 1)), depth)
  }
  return depth
}

function requireUnique(values: readonly string[], label: string): void {
  if (new Set(values).size !== values.length) throw new Error(`duplicate ${label} capability`)
}

function object(value: unknown, label = "params"): JsonObject {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new SafeRpcError(-32602, `invalid ${label}`)
  }
  return value as JsonObject
}

function string(value: JsonValue | undefined, label: string): string {
  if (typeof value !== "string" || value.length === 0) throw new SafeRpcError(-32602, `invalid ${label}`)
  return value
}

function validateManifest(manifest: PluginManifest): void {
  requireKeys(manifest, "manifest", ["name", "version", "protocol", "capabilities"])
  requireKeys(manifest.capabilities, "capabilities", [
    "tools", "commands", "hooks", "providers", "event_subscriptions", "push", "ui",
  ])
  if (manifest.protocol !== PLUGIN_PROTOCOL_VERSION) {
    throw new Error(`plugin manifest protocol must be ${PLUGIN_PROTOCOL_VERSION}`)
  }
  requireCanonicalName(manifest.name, "plugin", "plugin name")
  requireText(manifest.version, "plugin version", PROTOCOL_LIMITS.maxVersionBytes)
  if (byteLength(JSON.stringify(manifest)) > PROTOCOL_LIMITS.maxManifestBytes) {
    throw new Error("plugin manifest exceeds the protocol size limit")
  }

  const declaredCapabilities: readonly [string, readonly string[]][] = [
    ["tool", (manifest.capabilities.tools ?? []).map((entry) => entry.name)],
    ["command", (manifest.capabilities.commands ?? []).map((entry) => entry.name)],
    [
      "hook",
      (manifest.capabilities.hooks ?? []).map((entry) => entry.name),
    ],
    [
      "provider",
      (manifest.capabilities.providers ?? []).map((entry) => entry["alias-prefix"]),
    ],
    ["event", [...(manifest.capabilities.event_subscriptions ?? [])]],
  ]
  for (const [kind, declared] of declaredCapabilities) {
    if (declared.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) {
      throw new Error(`too many ${kind} capabilities`)
    }
    requireUnique(declared, kind)
  }
  const ui = manifest.capabilities.ui ?? []
  if (ui.length > 128 || byteLength(JSON.stringify(ui)) > 256 * 1024) throw new Error("UI contribution admission limit")
  requireUnique(ui.map(item => item.id), "UI contribution")
  const uiTools: string[] = []
  for (const contribution of ui) {
    if (!validateUiContribution(contribution)) throw new Error("invalid UI contribution")
    requireText(contribution.title, "UI title", 128)
    if (contribution.fields.length > 32 || contribution.actions.length > 4) throw new Error("UI field/action limit")
    requireUnique(contribution.fields.map(field => field.id), "UI field")
    requireUnique(contribution.actions.map(action => action.id), "UI action")
    if (contribution.surface === "tool") {
      uiTools.push(contribution.tool_name)
      if (!manifest.capabilities.tools?.some(tool => tool.name === contribution.tool_name)) throw new Error("UI presenter requires its declared tool")
    }
    for (const action of contribution.actions) {
      if (!manifest.capabilities.commands?.some(command => command.name === action.command)) throw new Error("UI action requires its declared command")
      if (byteLength(JSON.stringify(action.arguments)) > 4096) throw new Error("UI action argument limit")
    }
  }
  requireUnique(uiTools, "UI tool presenter")
  const push = manifest.capabilities.push ?? []
  if (push.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) throw new Error("too many push capabilities")
  requireUnique(push, "push")
  const validPush = new Set<PluginPushMethod>([
    RPC_METHODS.injectMessage, RPC_METHODS.setStatus, RPC_METHODS.notify, RPC_METHODS.publishPanel,
    RPC_METHODS.sessionQuery, RPC_METHODS.contextRead, RPC_METHODS.sessionControl, RPC_METHODS.sessionToolCall, RPC_METHODS.effectToolCall, RPC_METHODS.stateRead, RPC_METHODS.stateCommit, RPC_METHODS.eventRead,
  ])
  if (push.some((method) => !validPush.has(method))) throw new Error("unknown push capability")

  for (const tool of manifest.capabilities.tools ?? []) {
    requireKeys(tool, `tool ${tool.name}`, ["name", "description", "schema", "caps"])
    requireCanonicalName(tool.name, "tool", "tool name")
    requireText(tool.description, `tool ${tool.name} description`, PROTOCOL_LIMITS.maxDescriptionBytes)
    if (tool.schema === null || typeof tool.schema !== "object" || Array.isArray(tool.schema)) {
      throw new Error(`tool ${tool.name} schema must be an object`)
    }
    const schemaBytes = byteLength(JSON.stringify(tool.schema))
    if (schemaBytes > PROTOCOL_LIMITS.maxSchemaBytes || jsonDepth(tool.schema) > PROTOCOL_LIMITS.maxSchemaDepth) {
      throw new Error(`tool ${tool.name} schema exceeds the size or depth limit`)
    }
    const schemaType = tool.schema.type
    if (schemaType !== undefined && typeof schemaType !== "string" && !Array.isArray(schemaType)) {
      throw new Error(`tool ${tool.name} schema type must be a string or array`)
    }
  }
  for (const command of manifest.capabilities.commands ?? []) {
    requireKeys(command, `command ${command.name}`, ["name", "description", "argument_hint", "allowed_tools"])
    requireCanonicalName(command.name, "command", "command name")
    requireText(command.description, `command ${command.name} description`, PROTOCOL_LIMITS.maxDescriptionBytes)
    if (command.argument_hint !== undefined) {
      requireText(command.argument_hint, `command ${command.name} argument_hint`, PROTOCOL_LIMITS.maxDescriptionBytes)
    }
    const allowedTools = command.allowed_tools ?? []
    if (allowedTools.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) throw new Error("too many command allowed_tools")
    requireUnique(allowedTools, "command allowed tool")
    for (const allowed of allowedTools) requireCanonicalName(allowed, "tool", "command allowed tool")
  }
  for (const provider of manifest.capabilities.providers ?? []) {
    requireKeys(provider, "provider", ["alias-prefix", "capabilities", "credential-references"])
    const prefix = provider["alias-prefix"]
    if (byteLength(prefix) < 2 || byteLength(prefix) > PROTOCOL_LIMITS.maxNameBytes || !/^[a-z0-9_.-]+\/$/.test(prefix)) {
      throw new Error("provider alias-prefix must be a bounded canonical prefix ending in /")
    }
    const capabilities = provider.capabilities ?? []
    if (capabilities.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) throw new Error("too many provider capabilities")
    requireUnique(capabilities, "provider")
    for (const capability of capabilities) requireCanonicalName(capability, "command", "provider capability")
    const credentialReferences = provider["credential-references"] ?? []
    if (credentialReferences.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) {
      throw new Error("too many provider credential references")
    }
    requireUnique(credentialReferences, "provider credential reference")
    for (const reference of credentialReferences) {
      requireCanonicalName(reference, "command", "provider credential reference")
    }
  }
  const validToolCapabilities = new Set(["reads-fs", "writes-fs", "network", "exec"])
  for (const tool of manifest.capabilities.tools ?? []) {
    if (new Set(tool.caps).size !== tool.caps.length || tool.caps.some((capability) => !validToolCapabilities.has(capability))) {
      throw new Error(`tool ${tool.name} has invalid or duplicate capabilities`)
    }
  }
  for (const event of manifest.capabilities.event_subscriptions ?? []) { if (!validateEventKind(event)) throw new Error("unknown event subscription") }
  for (const hook of manifest.capabilities.hooks ?? []) {
    const hookName = hook.name
    const validHooks = new Set([
      "session_start", "session_end", "user_prompt_submit", "pre_tool", "post_tool",
      "pre_compact", "turn_end", "permission_check",
    ])
    if (!validHooks.has(hookName)) throw new Error(`unknown hook capability ${hookName}`)
    requireKeys(hook, `hook ${hookName}`, ["name", "class", "failure_policy"])
    if (!["transform", "policy", "observer"].includes(hook.class)) throw new Error(`hook ${hookName} has an invalid class`)
    if (hook.class === "policy" && hook.failure_policy !== "fail-closed") throw new Error(`policy hook ${hookName} must fail closed`)
    if (hook.class === "transform" && !["pre_tool", "post_tool", "user_prompt_submit", "pre_compact"].includes(hookName)) throw new Error(`hook ${hookName} cannot transform input`)
    if (hook.failure_policy !== "fail-open" && hook.failure_policy !== "fail-closed") {
      throw new Error(`hook ${hook.name} has an invalid failure policy`)
    }
  }
}

function validateDefinition(definition: PluginDefinition): void {
  const { manifest, handlers } = definition
  validateManifest(manifest)
  const pairs: readonly [string, readonly string[], readonly string[]][] = [
    ["tool", (manifest.capabilities.tools ?? []).map((entry) => entry.name), Object.keys(handlers.tools ?? {})],
    ["command", (manifest.capabilities.commands ?? []).map((entry) => entry.name), Object.keys(handlers.commands ?? {})],
    [
      "hook",
      (manifest.capabilities.hooks ?? []).map((entry) => entry.name),
      Object.keys(handlers.hooks ?? {}),
    ],
    [
      "provider",
      (manifest.capabilities.providers ?? []).map((entry) => entry["alias-prefix"]),
      Object.keys(handlers.providers ?? {}),
    ],
    ["event", [...(manifest.capabilities.event_subscriptions ?? [])], Object.keys(handlers.events ?? {})],
  ]
  for (const [kind, declared, implemented] of pairs) {
    for (const name of implemented) {
      if (!declared.includes(name)) throw new Error(`${kind} handler ${name} exceeds the manifest`)
    }
    for (const name of declared) {
      if (!implemented.includes(name)) throw new Error(`${kind} capability ${name} has no handler`)
    }
  }
  const declaredModelProviders = (manifest.capabilities.providers ?? [])
    .filter((provider) => provider.capabilities?.includes("models") === true)
    .map((provider) => provider["alias-prefix"])
  const implementedModelProviders = Object.keys(handlers.providerModels ?? {})
  requireUnique(implementedModelProviders, "provider models handler")
  for (const prefix of implementedModelProviders) {
    if (!declaredModelProviders.includes(prefix)) throw new Error(`provider models handler ${prefix} exceeds the manifest`)
  }
  for (const prefix of declaredModelProviders) {
    if (!implementedModelProviders.includes(prefix)) throw new Error(`provider models capability ${prefix} has no handler`)
  }
}

function deepFreeze(value: unknown, seen = new WeakSet<object>()): void {
  if (value === null || typeof value !== "object" || seen.has(value)) return
  seen.add(value)
  for (const child of Object.values(value)) deepFreeze(child, seen)
  Object.freeze(value)
}

function lockDefinition(definition: PluginDefinition): void {
  deepFreeze(definition.manifest)
  deepFreeze(definition.handlers)
  Object.freeze(definition)
}

function validateProviderModelsResponse(response: ProviderModelsResponse): void {
  if (response === null || typeof response !== "object" || Array.isArray(response)) {
    throw new Error("provider model catalog must be an object")
  }
  requireKeys(response, "provider model catalog", ["models"])
  if (!Array.isArray(response.models) || response.models.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) {
    throw new Error("provider model catalog exceeds the entry limit")
  }
  const ids = new Set<string>()
  for (const model of response.models) {
    requireKeys(model, "provider model", [
      "id", "display_name", "capabilities", "max_context_tokens", "max_output_tokens", "pricing",
    ])
    requireText(model.id, "provider model id", PROTOCOL_LIMITS.maxNameBytes)
    if (ids.has(model.id)) throw new Error("provider model ids must be unique")
    ids.add(model.id)
    if (model.display_name !== undefined) {
      requireText(model.display_name, "provider model display name", PROTOCOL_LIMITS.maxNameBytes)
    }
    if (model.capabilities === null || typeof model.capabilities !== "object" || Array.isArray(model.capabilities)) {
      throw new Error("provider model capabilities must be an object")
    }
    requireKeys(model.capabilities, "provider model capabilities", [
      "tool_calling", "vision", "thinking", "cache_breakpoints",
    ])
    if (
      typeof model.capabilities.tool_calling !== "boolean"
      || typeof model.capabilities.vision !== "boolean"
      || typeof model.capabilities.thinking !== "boolean"
      || !["none", "explicit", "automatic"].includes(model.capabilities.cache_breakpoints)
    ) throw new Error("provider model capabilities are invalid")
    for (const limit of [model.max_context_tokens, model.max_output_tokens]) {
      if (limit !== undefined && (!Number.isSafeInteger(limit) || limit < 1 || limit > PROTOCOL_LIMITS.maxModelTokens)) {
        throw new Error("provider model token limit is invalid")
      }
    }
    if (model.pricing !== undefined) {
      requireKeys(model.pricing, "provider model pricing", [
        "input_per_million_micros_usd", "output_per_million_micros_usd",
        "cache_read_per_million_micros_usd", "cache_write_per_million_micros_usd",
        "reasoning_per_million_micros_usd",
      ])
      for (const price of [
        model.pricing.input_per_million_micros_usd,
        model.pricing.output_per_million_micros_usd,
        model.pricing.cache_read_per_million_micros_usd,
        model.pricing.cache_write_per_million_micros_usd,
        model.pricing.reasoning_per_million_micros_usd,
      ]) {
        if (price !== undefined && (!Number.isSafeInteger(price) || price < 0 || price > PROTOCOL_LIMITS.maxPriceMicrosUsd)) {
          throw new Error("provider model price is invalid")
        }
      }
    }
  }
}

function defaultTransport(signal?: AbortSignal): ServerTransport {
  return {
    input: readableStreamBytes(Bun.stdin.stream(), signal),
    output: {
      write(line) {
        return new Promise<void>((resolve, reject) => {
          process.stdout.write(line, (error?: Error | null) => (error == null ? resolve() : reject(error)))
        })
      },
    },
    error: { write: (message) => process.stderr.write(message) },
  }
}

export class PluginServer {
  readonly #writer: BoundedJsonWriter
  readonly #lifetime = new AbortController()
  readonly #pushCapabilities: ReadonlySet<PluginPushMethod>
  readonly #handlerTimeoutMs: number
  #nextPushId = 1
  #nextProviderHttpId = 1
  #initialized = false
  #shuttingDown = false
  #shutdownPromise: Promise<void> | undefined
  readonly #providerCalls = new Map<RpcId, AbortController>()
  readonly #providerCredits = new Map<RpcId, StreamCredit>()
  readonly #handlerTasks = new Set<Promise<void>>()
  #activeInvocations = 0
  readonly #providerHttp = new Map<RpcId, PendingProviderHttp>()
  readonly #hostCommands = new Map<RpcId, {
    resolve(value: JsonValue): void
    reject(error: Error): void
    cleanup(): void
  }>()

  constructor(
    private readonly definition: PluginDefinition,
    private readonly transport: ServerTransport,
    maxLineBytes = DEFAULT_MAX_RPC_LINE_BYTES,
    handlerTimeoutMs: number = PROTOCOL_LIMITS.defaultHandlerTimeoutMs,
  ) {
    validateDefinition(definition)
    lockDefinition(definition)
    if (!Number.isSafeInteger(handlerTimeoutMs) || handlerTimeoutMs < 1 || handlerTimeoutMs > 60_000) {
      throw new Error("handler timeout must be an integer from 1 to 60000 milliseconds")
    }
    this.#handlerTimeoutMs = handlerTimeoutMs
    this.#pushCapabilities = new Set(definition.manifest.capabilities.push ?? [])
    this.#writer = new BoundedJsonWriter(transport.output, maxLineBytes, {
      writeTimeoutMs: handlerTimeoutMs,
      onFailure: () => this.#lifetime.abort(),
    })
  }

  async serve(
    input: AsyncIterable<Uint8Array>,
    maxLineBytes = DEFAULT_MAX_RPC_LINE_BYTES,
    signal?: AbortSignal,
  ): Promise<void> {
    const iterator = readBoundedLines(input, maxLineBytes)[Symbol.asyncIterator]()
    const abortLifetime = () => {
      this.#writer.abort(new SafeRpcError(-32800, "plugin transport cancelled"))
      this.#lifetime.abort()
    }
    signal?.addEventListener("abort", abortLifetime, { once: true })
    if (signal?.aborted === true) abortLifetime()
    try {
      while (!this.#shuttingDown && !this.#lifetime.signal.aborted) {
        let cancelRead!: () => void
        const interrupted = new Promise<undefined>((resolve) => {
          cancelRead = () => resolve(undefined)
          this.#lifetime.signal.addEventListener("abort", cancelRead, { once: true })
        })
        let next: IteratorResult<string> | undefined
        try {
          next = await Promise.race([iterator.next(), interrupted])
        } finally {
          this.#lifetime.signal.removeEventListener("abort", cancelRead)
        }
        if (next === undefined || next.done) break
        await this.#routeLine(next.value, true)
      }
    } catch (error) {
      if (signal?.aborted !== true) throw error
    } finally {
      signal?.removeEventListener("abort", abortLifetime)
      void iterator.return?.(undefined).catch(() => undefined)
      if (signal?.aborted === true) this.#writer.abort(new SafeRpcError(-32800, "plugin transport cancelled"))
      await this.shutdown()
      if (signal?.aborted !== true) await this.#writer.drain()
    }
  }

  async handleLine(line: string): Promise<void> {
    await this.#routeLine(line, false)
  }

  async #routeLine(line: string, concurrent: boolean): Promise<void> {
    let message: unknown
    try {
      message = JSON.parse(line)
    } catch {
      await this.#controlReply(this.#failure(null, -32700, "parse error"), concurrent)
      return
    }
    if (message === null || typeof message !== "object" || Array.isArray(message)) {
      await this.#controlReply(this.#failure(null, -32600, "invalid request"), concurrent)
      return
    }
    const candidate = message as Record<string, unknown>
    const id = typeof candidate.id === "string" || typeof candidate.id === "number" ? candidate.id : undefined
    if (own(candidate, "result") || own(candidate, "error")) {
      try {
        requireRpcKeys(candidate, "response", own(candidate, "error")
          ? ["jsonrpc", "id", "error"] : ["jsonrpc", "id", "result"])
        if (candidate.jsonrpc !== "2.0" || id === undefined) throw new Error("response ID is missing")
        if (typeof id === "string") requireText(id, "response ID", PROTOCOL_LIMITS.maxNameBytes)
        else if (!Number.isSafeInteger(id)) throw new Error("response ID is not an integer")
        if (own(candidate, "error")) {
          const error = object(candidate.error, "response error")
          requireRpcKeys(error, "response error", ["code", "message", "data"])
          if (typeof error.code !== "number" || !Number.isSafeInteger(error.code)
            || typeof error.message !== "string") throw new Error("invalid response error")
          requireText(error.message, "response error", PROTOCOL_LIMITS.maxRpcMessageBytes)
        }
      } catch {
        this.#lifetime.abort()
        throw new SafeRpcError(-32600, "invalid host response")
      }
    }
    if (own(candidate, "id") && id === undefined) {
      await this.#controlReply(this.#failure(null, -32600, "invalid request"), concurrent)
      return
    }
    if (candidate.jsonrpc === "2.0" && typeof candidate.method !== "string" && id !== undefined) {
      if (!this.#handleHostCommandResponse(id, candidate)) this.#handleProviderHttpResponse(id, candidate)
      return
    }
    if (candidate.jsonrpc !== "2.0" || typeof candidate.method !== "string") {
      await this.#controlReply(this.#failure(id ?? null, -32600, "invalid request"), concurrent)
      return
    }
    const isNotification = !own(candidate, "id")
    if (candidate.method === RPC_METHODS.eventPublish && isNotification) {
      this.#debug("event delivery requires a correlated request")
      return
    }
    if (candidate.method === RPC_METHODS.providerHttpEvent && isNotification) {
      try {
        this.#handleProviderHttpEvent(candidate.params)
      } catch {
        this.#debug("notification provider/http_event failed")
        this.#lifetime.abort()
      }
      return
    }
    if (candidate.method === RPC_METHODS.providerCredit && isNotification) {
      try {
        const params = object(candidate.params, "provider credit")
        requireRpcKeys(params, "provider credit", ["request_id", "events", "bytes"])
        if ((typeof params.request_id !== "string" && typeof params.request_id !== "number")
          || typeof params.events !== "number" || typeof params.bytes !== "number") {
          throw new SafeRpcError(-32602, "invalid provider credit")
        }
        this.#providerCredits.get(params.request_id)?.grant(params.events, params.bytes)
      } catch {
        this.#lifetime.abort()
      }
      return
    }
    const lifecycle = candidate.method === RPC_METHODS.initialize
      || candidate.method === RPC_METHODS.shutdown || candidate.method === RPC_METHODS.exit
    if (lifecycle) {
      await this.#handleRequest(candidate.method, candidate.params, id)
      return
    }
    if (this.#shuttingDown || this.#lifetime.signal.aborted
      || this.#handlerTasks.size >= PROTOCOL_LIMITS.maxInFlightRequests || this.#activeInvocations >= PROTOCOL_LIMITS.maxInFlightRequests) {
      if (id !== undefined) await this.#controlReply(this.#failure(id, -32005, "plugin handler admission denied"), concurrent)
      return
    }
    const provider = candidate.method === RPC_METHODS.providerComplete && id !== undefined
    const task = provider
      ? this.#handleProvider(id, candidate.params)
      : candidate.method === RPC_METHODS.toolCall && id !== undefined
        ? this.#handleTool(id, candidate.params)
        : this.#handleRequest(candidate.method, candidate.params, id)
    this.#handlerTasks.add(task)
    void task.then(() => this.#handlerTasks.delete(task), () => {
      this.#handlerTasks.delete(task)
      this.#writer.abort(new Error("JSON-RPC response write failed"))
    })
    if (!concurrent && !provider) await task
  }

  async #controlReply(reply: Promise<void>, concurrent: boolean): Promise<void> {
    if (concurrent) void reply.catch(() => undefined)
    else await reply
  }

  async #handleRequest(method: string, params: unknown, id: RpcId | undefined): Promise<void> {
    try {
      const result = await this.#dispatch(id, method, params)
      if (id !== undefined) await this.#success(id, result)
    } catch (error) {
      if (id === undefined) {
        this.#debug(`notification ${method} failed`)
      } else if (error instanceof SafeRpcError) {
        await this.#failure(id ?? null, error.code, error.safeMessage, error.safeData)
      } else {
        // Arbitrary exception messages, causes, stacks, and handler inputs can contain secrets.
        await this.#failure(id ?? null, -32603, "plugin handler failed")
      }
    }
  }

  shutdown(): Promise<void> {
    this.#shutdownPromise ??= this.#finishShutdown()
    return this.#shutdownPromise
  }

  async #finishShutdown(): Promise<void> {
    this.#shuttingDown = true
    this.#lifetime.abort()
    for (const pending of this.#providerHttp.values()) {
      const error = new SafeRpcError(-32800, "plugin shutdown cancelled provider HTTP")
      pending.cleanup()
      pending.body.fail(error)
      pending.reject(error)
    }
    this.#providerHttp.clear()
    for (const pending of this.#hostCommands.values()) {
      pending.cleanup()
      pending.reject(new SafeRpcError(-32800, "plugin disconnected; host command outcome unknown"))
    }
    this.#hostCommands.clear()
    let timeout: ReturnType<typeof setTimeout> | undefined
    const deadline = new Promise<void>((resolve) => {
      timeout = setTimeout(() => {
        this.#debug("shutdown timed out")
        this.#writer.abort(new SafeRpcError(-32800, "plugin shutdown timed out"))
        resolve()
      }, this.#handlerTimeoutMs)
    })
    try {
      const handler = this.definition.handlers.shutdown
      if (handler !== undefined) {
        await Promise.resolve()
          .then(() => handler(this.#lifetime.signal))
          .catch(() => this.#debug("shutdown handler failed"))
      }
      await Promise.allSettled(this.#handlerTasks)
      await Promise.race([this.#writer.drain().catch(() => undefined), deadline])
    } finally {
      if (timeout !== undefined) clearTimeout(timeout)
    }
  }

  async #dispatch(id: RpcId | undefined, method: string, rawParams: unknown): Promise<JsonValue> {
    if (method === RPC_METHODS.initialize) {
      if (this.#initialized) throw new SafeRpcError(-32600, "plugin is already initialized")
      const params = object(rawParams)
      requireRpcKeys(params, "initialize params", ["host", "protocol", "max_frame_bytes", "capabilities"])
      const selectedProtocol = this.definition.manifest.protocol
      const hostCapabilities = params.capabilities
      const needsProviderModels = this.definition.manifest.capabilities.providers?.some(
        (provider) => provider.capabilities?.includes("models") === true,
      ) === true
      const needsProviderHttp = this.definition.manifest.capabilities.providers?.some(
        (provider) => (provider["credential-references"]?.length ?? 0) > 0,
      ) === true
      if (
        params.host !== PLUGIN_HOST_ID
        || params.protocol !== PLUGIN_PROTOCOL_VERSION
        || selectedProtocol !== PLUGIN_PROTOCOL_VERSION
        || typeof params.max_frame_bytes !== "number"
        || !Number.isSafeInteger(params.max_frame_bytes)
        || params.max_frame_bytes < 1
        || params.max_frame_bytes > PROTOCOL_LIMITS.maxLineBytes
        || (hostCapabilities !== undefined && (
          !Array.isArray(hostCapabilities)
          || hostCapabilities.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind
          || hostCapabilities.some((capability) => typeof capability !== "string")
        ))
        || (needsProviderModels && (
          !Array.isArray(hostCapabilities)
          || !hostCapabilities.includes("provider-models")
        ))
        || (needsProviderHttp && (
          !Array.isArray(hostCapabilities)
          || !hostCapabilities.includes("provider-http")
        ))
      ) {
        throw new SafeRpcError(-32001, "unsupported plugin protocol")
      }
      this.#initialized = true
      return this.definition.manifest as unknown as JsonValue
    }
    if (!this.#initialized) throw new SafeRpcError(-32002, "plugin is not initialized")
    if (method === RPC_METHODS.shutdown) {
      await this.shutdown()
      return null
    }
    if (method === RPC_METHODS.exit) {
      await this.shutdown()
      return null
    }
    if (method === RPC_METHODS.commandExecute) {
      const params = this.#commandParams(rawParams)
      const handler = this.definition.handlers.commands?.[params.name]
      if (handler === undefined) throw new SafeRpcError(-32601, "command is not declared")
      return this.#runHandler((context) => handler(params, context), params.invocation_id, Math.min(params.lifetime.total_ms, params.lifetime.idle_ms))
    }
    if (method === RPC_METHODS.hookInvoke) {
      const params = this.#hookParams(rawParams)
      const handlers = this.definition.handlers.hooks
      const declaration = this.definition.manifest.capabilities.hooks?.find(entry => entry.name === params.hook)
      if (handlers?.[params.hook] === undefined || declaration === undefined) throw new SafeRpcError(-32601, "hook is not declared")
      return this.#runHandler(async context => {
        const result = await invokeHook(handlers, params, context)
        if (!validateHookDirective(result)) throw new SafeRpcError(-32603, "invalid hook directive")
        if ((result.decision === "transform" && (declaration.class !== "transform" || result.change.hook !== params.hook))
          || (result.decision === "permission" && (declaration.class !== "policy" || params.hook !== "permission_check"))
          || (result.decision === "block" && declaration.class !== "policy")) {
          throw new SafeRpcError(-32603, "hook directive exceeds its declared class")
        }
        return result
      })
    }
    if (method === RPC_METHODS.eventPublish) {
      const params = this.#eventParams(rawParams)
      const handler = this.definition.handlers.events?.[params.event]
      if (handler === undefined) throw new SafeRpcError(-32601, "event is not subscribed")
      return this.#runHandler(async context => {
        const result = await handler(params, { ...context, readSource: eventSourceReader(params, (method, request) => this.#push(method, request, context.signal)) })
        if (!validateEventOutcome(result)) throw new SafeRpcError(-32603, "invalid event outcome")
        return result as JsonValue
      })
    }
    if (method === RPC_METHODS.providerModels) {
      if (id === undefined) throw new SafeRpcError(-32600, "provider catalog requires a request identity")
      const params = this.#providerModelsParams(rawParams)
      const handler = this.definition.handlers.providerModels?.[params.alias_prefix]
      if (handler === undefined) throw new SafeRpcError(-32601, "provider models are not declared")
      const response = await this.#runHandler(
        (context) => handler(params, this.#providerContext(context, id, params.alias_prefix)),
      )
      validateProviderModelsResponse(response)
      return response as unknown as JsonValue
    }
    throw new SafeRpcError(-32601, "method not found")
  }

  async #handleTool(id: RpcId, rawParams: unknown): Promise<void> {
    let reporter: ToolProgressReporter | undefined
    const call = new AbortController()
    let timedOut = false
    const cancel = () => call.abort()
    const expire = () => { timedOut = true; call.abort() }
    let totalTimer: ReturnType<typeof setTimeout> | undefined
    let idleTimer: ReturnType<typeof setTimeout> | undefined
    this.#lifetime.signal.addEventListener("abort", cancel, { once: true })
    try {
      if (!this.#initialized) throw new SafeRpcError(-32002, "plugin is not initialized")
      const params = this.#toolParams(rawParams)
      const handler = this.definition.handlers.tools?.[params.name]
      if (handler === undefined) throw new SafeRpcError(-32601, "tool is not declared")
      const { total_ms: totalMs, idle_ms: idleMs } = params.lifetime
      const deadlineAt = performance.now() + totalMs
      const renewIdle = () => {
        clearTimeout(idleTimer)
        if (!call.signal.aborted) idleTimer = setTimeout(expire, idleMs)
      }
      totalTimer = setTimeout(expire, totalMs)
      renewIdle()
      reporter = new ToolProgressReporter(
        (sequence, progress) => this.#writer.write({
          jsonrpc: "2.0", method: RPC_METHODS.toolProgress,
          params: { request_id: id, sequence, progress },
        }, "progress"), renewIdle, cancel,
      )
      const progress = reporter
      const result = await this.#invoke(() => {
          if (this.#lifetime.signal.aborted) call.abort()
          if (call.signal.aborted) throw new SafeRpcError(-32800, "plugin tool cancelled")
          return handler(params, { ...this.#context(call.signal, null),
            effects: { callTool: async (name, input) => {
              const request = { request_id: id, name, input }
              if (!validateEffectCall(request)) throw new SafeRpcError(-32602, "invalid host effect request")
              const result = await this.#push(RPC_METHODS.effectToolCall, request, call.signal, deadlineAt)
              if (!validateToolResponse(result)) throw new SafeRpcError(-32603, "invalid host effect result")
              return result as unknown as ToolResponse
            } }, progress: update => {
            if (call.signal.aborted) throw new SafeRpcError(-32800, "plugin tool cancelled")
            progress.report(update)
          } })
        })
      await reporter.finish()
      call.signal.throwIfAborted()
      if (!validateToolResponse(result)) throw new SafeRpcError(-32603, "invalid tool response")
      await this.#success(id, result as unknown as JsonValue)
    } catch (error) {
      await reporter?.finish()
      if (call.signal.aborted) await this.#failure(id, timedOut ? -32004 : -32800, timedOut ? "plugin tool deadline exceeded" : "plugin tool cancelled")
      else if (error instanceof SafeRpcError) await this.#failure(id, error.code, error.safeMessage, error.safeData)
      else await this.#failure(id, -32603, "plugin tool failed")
    } finally {
      call.abort()
      clearTimeout(totalTimer)
      clearTimeout(idleTimer)
      this.#lifetime.signal.removeEventListener("abort", cancel)
    }
  }

  #providerModelsParams(rawParams: unknown): ProviderModelsParams {
    const params = object(rawParams, "provider/models params")
    requireRpcKeys(params, "provider/models params", ["alias_prefix"])
    const aliasPrefix = string(params.alias_prefix, "provider alias prefix")
    if (!/^[a-z0-9_.-]+\/$/.test(aliasPrefix) || byteLength(aliasPrefix) > PROTOCOL_LIMITS.maxNameBytes) {
      throw new SafeRpcError(-32602, "invalid provider alias prefix")
    }
    return { alias_prefix: aliasPrefix }
  }

  async #handleProvider(id: RpcId, rawParams: unknown): Promise<void> {
    if (!this.#initialized || this.#shuttingDown || this.#providerCalls.has(id) || this.#providerCalls.size >= PROTOCOL_LIMITS.maxProviderStreams) {
      await this.#failure(id, -32005, "provider stream admission denied")
      return
    }
    let params: ProviderCompleteParams
    let handler: ProviderHandler | undefined
    try {
      params = this.#providerParams(rawParams)
      const prefix = Object.keys(this.definition.handlers.providers ?? {}).find((item) => params.alias.startsWith(item))
      handler = prefix === undefined ? undefined : this.definition.handlers.providers?.[prefix]
      if (handler === undefined) throw new SafeRpcError(-32601, "provider is not declared")
    } catch (error) {
      if (error instanceof SafeRpcError) await this.#failure(id, error.code, error.safeMessage, error.safeData)
      else await this.#failure(id, -32602, "invalid provider/complete params")
      return
    }
    const call = new AbortController()
    const cancel = () => call.abort()
    this.#lifetime.signal.addEventListener("abort", cancel, { once: true })
    if (this.#lifetime.signal.aborted) call.abort()
    this.#providerCalls.set(id, call)
    const credit = new StreamCredit(call.signal)
    this.#providerCredits.set(id, credit)
    const deadline = setTimeout(cancel, PROTOCOL_LIMITS.maxOperationDurationMs)
    const providerHandler = handler
    try {
      await this.#invoke(async () => {
        if (call.signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
        const events = await providerHandler(params, this.#providerContext(this.#context(call.signal, null), id, params.alias))
        if (events === null || typeof events !== "object" || !(Symbol.asyncIterator in events)) {
          throw new SafeRpcError(-32603, "provider must return an async event stream")
        }
        let sawFinished = false
        for await (const event of events) {
          if (call.signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
          this.#validateProviderEvent(event, sawFinished)
          if (event.type === "finished") sawFinished = true
          const frame: JsonValue = {
            jsonrpc: "2.0",
            method: RPC_METHODS.providerEvent,
            params: { request_id: id, event } as unknown as JsonValue,
          }
          if (event.type !== "finished") await credit.take(byteLength(JSON.stringify(frame)))
          await this.#writer.write(frame, "data")
        }
        if (!sawFinished) throw new SafeRpcError(-32603, "provider stream ended before finished")
      })
      if (call.signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
      await this.#success(id, null)
    } catch (error) {
      if (call.signal.aborted) await this.#failure(id, -32800, "plugin request cancelled")
      else if (error instanceof SafeRpcError) await this.#failure(id, error.code, error.safeMessage, error.safeData)
      else await this.#failure(id, -32603, "plugin provider failed")
    } finally {
      this.#providerCalls.delete(id)
      this.#providerCredits.delete(id)
      clearTimeout(deadline)
      credit.close()
      this.#lifetime.signal.removeEventListener("abort", cancel)
    }
  }

  #validateProviderEvent(event: ProviderEvent, sawFinished: boolean): void {
    if (!validateProviderEvent(event)) throw new SafeRpcError(-32603, "invalid provider event")
    if (sawFinished) {
      throw new SafeRpcError(-32603, "provider emitted an invalid event sequence")
    }
  }

  #toolParams(raw: unknown): ToolCallParams {
    const value = object(raw)
    requireRpcKeys(value, "tool/call params", ["name", "input", "lifetime"])
    const lifetime = this.#operationLifetime(value.lifetime, "tool")
    return {
      lifetime: { total_ms: lifetime.total_ms, idle_ms: lifetime.idle_ms },
      name: string(value.name, "tool name"),
      input: object(value.input, "tool input"),
    }
  }

  #operationLifetime(raw: unknown, label: string): ToolCallParams["lifetime"] {
    const lifetime = object(raw, `${label} lifetime`)
    requireRpcKeys(lifetime, `${label} lifetime`, ["total_ms", "idle_ms"])
    if (typeof lifetime.total_ms !== "number" || typeof lifetime.idle_ms !== "number"
      || !Number.isInteger(lifetime.total_ms) || !Number.isInteger(lifetime.idle_ms)
      || lifetime.idle_ms < 1 || lifetime.total_ms < lifetime.idle_ms
      || lifetime.total_ms > PROTOCOL_LIMITS.maxOperationDurationMs) {
      throw new SafeRpcError(-32602, `invalid ${label} lifetime`)
    }
    return { total_ms: lifetime.total_ms, idle_ms: lifetime.idle_ms }
  }

  #commandParams(raw: unknown): CommandExecuteParams {
    const value = object(raw)
    requireRpcKeys(value, "command/execute params", ["name", "arguments", "invocation_id", "lifetime"])
    const lifetime = this.#operationLifetime(value.lifetime, "command")
    if (value.invocation_id !== null && !validateInvocationId(value.invocation_id)) throw new SafeRpcError(-32602, "invalid command invocation identity")
    if (typeof value.arguments !== "string") throw new SafeRpcError(-32602, "invalid command arguments")
    return {
      lifetime,
      invocation_id: value.invocation_id,
      name: string(value.name, "command name"),
      arguments: value.arguments,
    }
  }

  #hookParams(raw: unknown): HookInput {
    if (byteLength(JSON.stringify(raw)) > PROTOCOL_LIMITS.maxHookPayloadBytes || !validateHookInput(raw)) throw new SafeRpcError(-32602, "invalid hook input")
    return raw
  }

  #eventParams(raw: unknown): ExtensionEventNotice {
    if (!validateEventNotice(raw)) throw new SafeRpcError(-32602, "invalid extension event notice")
    if (raw.content.storage === "inline" && byteLength(JSON.stringify(raw.content.data)) > 256 * 1024) throw new SafeRpcError(-32602, "extension inline event exceeds byte limit")
    return raw
  }

  #providerParams(raw: unknown): ProviderCompleteParams {
    const value = object(raw)
    requireRpcKeys(value, "provider/complete params", ["alias", "request"])
    if (!validateProviderRequest(value.request)) throw new SafeRpcError(-32602, "invalid provider request")
    return { alias: string(value.alias, "provider alias"), request: value.request }
  }

  #debug(label: string): void {
    const safe = label.replace(/[\r\n\x00-\x1f\x7f]/g, " ").slice(0, 256)
    this.transport.error?.write(`[rottweiler-plugin:${this.definition.manifest.name}] ${safe}\n`)
  }

  async #push(method: PluginPushMethod, params: JsonValue, signal: AbortSignal, deadlineAt?: number): Promise<JsonValue> {
    if (!this.#pushCapabilities.has(method)) throw new SafeRpcError(-32003, "push method is not declared")
    if (signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
    if (this.#hostCommands.size >= 64) throw new SafeRpcError(-32005, "host command admission denied")
    const id = `plugin-push-${this.#nextPushId++}`
    let resolve!: (value: JsonValue) => void
    let reject!: (error: Error) => void
    const result = new Promise<JsonValue>((yes, no) => { resolve = yes; reject = no })
    void result.catch(() => undefined)
    const fail = (error: Error) => {
      const pending = this.#hostCommands.get(id)
      if (pending === undefined) return
      this.#hostCommands.delete(id)
      pending.cleanup()
      pending.reject(error)
    }
    const cancel = () => fail(new SafeRpcError(-32800, "host command cancelled; outcome unknown"))
    const timer = setTimeout(() => fail(new SafeRpcError(-32004, "host command deadline exceeded; outcome unknown")), deadlineAt === undefined ? this.#handlerTimeoutMs : Math.max(0, deadlineAt - performance.now()))
    this.#hostCommands.set(id, {
      resolve, reject,
      cleanup: () => { clearTimeout(timer); signal.removeEventListener("abort", cancel) },
    })
    signal.addEventListener("abort", cancel, { once: true })
    try {
      await this.#writer.write({ jsonrpc: "2.0", id, method, params })
      return await result
    } catch (error) {
      fail(error instanceof Error ? error : new Error("host command transport failed"))
      throw error
    }
  }

  #handleHostCommandResponse(id: RpcId, message: Record<string, unknown>): boolean {
    const pending = this.#hostCommands.get(id)
    if (pending === undefined) return false
    this.#hostCommands.delete(id)
    pending.cleanup()
    try {
      if (own(message, "error")) {
        const error = object(message.error, "host command error")
        pending.reject(new SafeRpcError(
          typeof error.code === "number" ? error.code : -32603,
          typeof error.message === "string" ? error.message : "host command rejected",
        ))
      } else if (own(message, "result")) {
        // JSON.parse established JSON values; the command validates its result shape.
        pending.resolve(message.result as JsonValue)
      } else pending.reject(new SafeRpcError(-32603, "invalid host command response"))
    } catch {
      pending.reject(new SafeRpcError(-32603, "invalid host command response"))
    }
    return true
  }

  async #providerHttpRequest(
    invocationId: RpcId,
    alias: string,
    credentialReference: string,
    request: ProviderHttpRequest,
    signal: AbortSignal,
  ): Promise<ProviderHttpResponse> {
    requireText(credentialReference, "credential reference", PROTOCOL_LIMITS.maxNameBytes)
    requireText(request.url, "provider HTTP URL", PROTOCOL_LIMITS.maxHookPayloadBytes)
    requireText(request.credential_header, "provider HTTP credential header", PROTOCOL_LIMITS.maxNameBytes)
    if (request.credential_prefix !== undefined && byteLength(request.credential_prefix) > PROTOCOL_LIMITS.maxNameBytes) {
      throw new SafeRpcError(-32602, "provider HTTP credential prefix is invalid")
    }
    if (!(["GET", "POST", "DELETE"] as const).includes(request.method)) {
      throw new SafeRpcError(-32602, "provider HTTP method is invalid")
    }
    const headers = request.headers ?? []
    if (headers.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) {
      throw new SafeRpcError(-32602, "provider HTTP headers exceed the entry limit")
    }
    for (const header of headers) {
      requireText(header.name, "provider HTTP header name", PROTOCOL_LIMITS.maxNameBytes)
      if (byteLength(header.value) > PROTOCOL_LIMITS.maxRpcMessageBytes) {
        throw new SafeRpcError(-32602, "provider HTTP header value is invalid")
      }
    }
    const body = request.body ?? new Uint8Array()
    if (body.byteLength > PROTOCOL_LIMITS.maxLineBytes) {
      throw new SafeRpcError(-32602, "provider HTTP request body exceeds the limit")
    }
    if (signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
    if (this.#providerHttp.size >= 64) throw new SafeRpcError(-32005, "provider HTTP admission denied")
    const id = `plugin-http-${this.#nextProviderHttpId}`
    this.#nextProviderHttpId += 1
    let resolveResponse!: (response: ProviderHttpResponse) => void
    let rejectResponse!: (error: Error) => void
    const response = new Promise<ProviderHttpResponse>((resolve, reject) => {
      resolveResponse = resolve
      rejectResponse = reject
    })
    void response.catch(() => undefined)
    const cancel = () => {
      const pending = this.#providerHttp.get(id)
      if (pending === undefined) return
      this.#providerHttp.delete(id)
      pending.cleanup()
      pending.body.fail(new SafeRpcError(-32800, "provider HTTP request cancelled"))
      pending.reject(new SafeRpcError(-32800, "provider HTTP request cancelled"))
      void this.#writer.write({
        jsonrpc: "2.0",
        method: RPC_METHODS.providerHttpCancel,
        params: { request_id: id },
      }).catch(() => this.#debug("notification provider/http_cancel failed"))
    }
    const queue = new BoundedByteQueue(64, 4 * 1024 * 1024, cancel)
    this.#providerHttp.set(id, {
      cleanup: () => signal.removeEventListener("abort", cancel),
      body: queue,
      resolve: resolveResponse,
      reject: rejectResponse,
      sawHead: false,
      sawFinished: false,
    })
    signal.addEventListener("abort", cancel, { once: true })
    try {
      await this.#writer.write({
        jsonrpc: "2.0",
        id,
        method: RPC_METHODS.providerHttp,
        params: {
          invocation_id: invocationId,
          alias,
          credential_reference: credentialReference,
          request: {
            method: request.method,
            url: request.url,
            headers: headers as unknown as JsonValue,
            body_base64: Buffer.from(body).toString("base64"),
            credential_header: request.credential_header,
            credential_prefix: request.credential_prefix ?? "",
          },
        },
      })
      return await response
    } catch (error) {
      cancel()
      throw error
    }
  }

  #handleProviderHttpEvent(raw: unknown): void {
    const params = object(raw, "provider/http_event params")
    requireRpcKeys(params, "provider/http_event params", ["request_id", "event"])
    const requestId = params.request_id
    if (typeof requestId !== "string" && typeof requestId !== "number") {
      throw new SafeRpcError(-32602, "provider HTTP request id is invalid")
    }
    const pending = this.#providerHttp.get(requestId)
    if (pending === undefined) return
    const event = object(params.event, "provider HTTP event")
    const type = string(event.type, "provider HTTP event type")
    if (type === "head") {
      requireRpcKeys(event, "provider HTTP head", ["type", "status", "headers"])
      if (pending.sawHead || !Number.isSafeInteger(event.status) || (event.status as number) < 100 || (event.status as number) > 599) {
        throw new SafeRpcError(-32603, "provider HTTP response head is invalid")
      }
      if (!Array.isArray(event.headers)) throw new SafeRpcError(-32603, "provider HTTP response headers are invalid")
      const headers = event.headers.map((rawHeader) => {
        if (!Array.isArray(rawHeader) || rawHeader.length !== 2 || rawHeader.some((entry) => typeof entry !== "string")) {
          throw new SafeRpcError(-32603, "provider HTTP response header is invalid")
        }
        return { name: rawHeader[0] as string, value: rawHeader[1] as string }
      })
      pending.sawHead = true
      pending.resolve({ status: event.status as number, headers, body: pending.body })
      return
    }
    if (type === "body") {
      requireRpcKeys(event, "provider HTTP body", ["type", "data_base64"])
      if (!pending.sawHead || pending.sawFinished || typeof event.data_base64 !== "string") {
        throw new SafeRpcError(-32603, "provider HTTP body event is invalid")
      }
      pending.body.push(new Uint8Array(Buffer.from(event.data_base64, "base64")))
      return
    }
    if (type === "finished") {
      requireRpcKeys(event, "provider HTTP finished event", ["type"])
      if (!pending.sawHead || pending.sawFinished) {
        throw new SafeRpcError(-32603, "provider HTTP finished event is invalid")
      }
      pending.sawFinished = true
      return
    }
    throw new SafeRpcError(-32603, "provider HTTP event type is invalid")
  }

  #handleProviderHttpResponse(id: RpcId, message: Record<string, unknown>): void {
    const pending = this.#providerHttp.get(id)
    if (pending === undefined) return
    this.#providerHttp.delete(id)
    pending.cleanup()
    if (own(message, "error")) {
      let safeData: JsonValue | undefined
      if (message.error !== null && typeof message.error === "object" && !Array.isArray(message.error)) {
        const data = (message.error as Record<string, unknown>).data
        if (data !== null && typeof data === "object" && !Array.isArray(data)) {
          const code = (data as Record<string, unknown>).code
          if (typeof code === "string") safeData = { code }
        }
      }
      const error = new SafeRpcError(-32020, "host-mediated provider HTTP failed", safeData)
      pending.body.fail(error)
      pending.reject(error)
      return
    }
    if (!pending.sawHead || !pending.sawFinished || message.result !== null) {
      const error = new SafeRpcError(-32603, "host-mediated provider HTTP ended incorrectly")
      pending.body.fail(error)
      pending.reject(error)
      return
    }
    pending.body.finish()
  }

  #providerContext(context: HandlerContext, invocationId: RpcId, alias: string): ProviderHandlerContext {
    return {
      ...context,
      providerHttp: {
        request: (reference, request) => this.#providerHttpRequest(invocationId, alias, reference, request, context.signal),
      },
    }
  }

  #context(signal: AbortSignal, origin: ExtensionInvocationId | null, deadlineAt?: number): HandlerContext {
    return {
      signal,
      ...hostStateContext((method, params) => this.#push(method, params, signal, method === RPC_METHODS.sessionToolCall ? deadlineAt : undefined), origin),
      push: {
        publishPanel: async (id, data) => {
          const update = { id, data }
          if (!validateUiPanelUpdate(update)) throw new SafeRpcError(-32602, "invalid panel update")
          const result = await this.#push(RPC_METHODS.publishPanel, update, signal)
          if (!validateUiPanelUpdated(result)) throw new SafeRpcError(-32603, "invalid panel revision")
          return result.revision
        },
        injectMessage: async (sessionId, content) => {
          requireText(sessionId, "session id", PROTOCOL_LIMITS.maxNameBytes)
          requireText(content, "injected message", PROTOCOL_LIMITS.maxHookPayloadBytes)
          const result = object(await this.#push(RPC_METHODS.injectMessage, { session_id: sessionId, content }, signal), "injection result")
          requireRpcKeys(result, "injection result", ["disposition"])
          const disposition = result.disposition
          if (disposition !== "started" && disposition !== "queued" && disposition !== "command") {
            throw new SafeRpcError(-32603, "invalid injection disposition")
          }
          return { disposition }
        },
        setStatus: async (sessionId, status) => {
          requireText(sessionId, "session id", PROTOCOL_LIMITS.maxNameBytes)
          requireText(status, "status", PROTOCOL_LIMITS.maxRpcMessageBytes)
          if (await this.#push(RPC_METHODS.setStatus, { session_id: sessionId, status }, signal) !== null) {
            throw new SafeRpcError(-32603, "invalid status result")
          }
        },
        notify: async (title, message, sessionId) => {
          requireText(title, "notification title", PROTOCOL_LIMITS.maxNameBytes)
          requireText(message, "notification message", PROTOCOL_LIMITS.maxRpcMessageBytes)
          if (sessionId !== undefined) requireText(sessionId, "session id", PROTOCOL_LIMITS.maxNameBytes)
          const result = await this.#push(RPC_METHODS.notify, {
            title,
            message,
            ...(sessionId === undefined ? {} : { session_id: sessionId }),
          }, signal)
          if (result !== null) throw new SafeRpcError(-32603, "invalid notification result")
        },
      },
      debug: (label) => this.#debug(label),
    }
  }

  async #runHandler<T>(
    invoke: (context: HandlerContext) => T | Promise<T>,
    origin: ExtensionInvocationId | null = null,
    deadlineMs: number = this.#handlerTimeoutMs,
  ): Promise<T> {
    if (this.#lifetime.signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
    const call = new AbortController()
    let timedOut = false
    const cancel = () => call.abort()
    this.#lifetime.signal.addEventListener("abort", cancel, { once: true })
    if (this.#lifetime.signal.aborted) call.abort()
    const deadlineAt = performance.now() + deadlineMs
    let timer: ReturnType<typeof setTimeout> | undefined
    timer = setTimeout(() => {
      timedOut = true
      call.abort()
    }, deadlineMs)
    try {
      const result = await this.#invoke(() => invoke(this.#context(call.signal, origin, deadlineAt)))
      call.signal.throwIfAborted()
      return result
    } catch (error) {
      if (call.signal.aborted) throw new SafeRpcError(
        timedOut ? -32004 : -32800,
        timedOut ? "plugin handler timed out" : "plugin request cancelled",
      )
      throw error
    } finally {
      if (timer !== undefined) clearTimeout(timer)
      this.#lifetime.signal.removeEventListener("abort", cancel)
    }
  }

  #invoke<T>(invoke: () => T | Promise<T>): Promise<T> {
    this.#activeInvocations += 1
    return Promise.resolve().then(invoke).finally(() => { this.#activeInvocations -= 1 })
  }

  #success(id: RpcId, result: JsonValue): Promise<void> {
    return this.#writer.write({ jsonrpc: "2.0", id, result })
  }

  #failure(id: RpcId | null, code: number, message: string, data?: JsonValue): Promise<void> {
    return this.#writer.write({
      jsonrpc: "2.0",
      id,
      error: { code, message, ...(data === undefined ? {} : { data }) },
    })
  }
}

export function definePlugin(definition: PluginDefinition): PluginDefinition {
  validateDefinition(definition)
  lockDefinition(definition)
  return definition
}

/** Parse inert manifest data before any plugin code is approved or started. */
export function parsePluginManifest(value: unknown): PluginManifest {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("plugin manifest must be an object")
  }
  const manifest = value as PluginManifest
  if (
    manifest.capabilities === null
    || typeof manifest.capabilities !== "object"
    || Array.isArray(manifest.capabilities)
  ) {
    throw new Error("plugin manifest capabilities must be an object")
  }
  validateManifest(manifest)
  deepFreeze(manifest)
  return manifest
}

export async function runPlugin(definition: PluginDefinition, options: RunOptions = {}): Promise<void> {
  const controller = new AbortController()
  const abort = () => controller.abort()
  options.signal?.addEventListener("abort", abort, { once: true })
  if (options.signal?.aborted === true) controller.abort()
  process.once("SIGINT", abort)
  process.once("SIGTERM", abort)
  const transport = options.transport ?? defaultTransport(controller.signal)
  const maxLineBytes = options.maxLineBytes ?? DEFAULT_MAX_RPC_LINE_BYTES
  const server = new PluginServer(
    definition,
    transport,
    maxLineBytes,
    options.handlerTimeoutMs ?? PROTOCOL_LIMITS.defaultHandlerTimeoutMs,
  )
  try {
    await server.serve(transport.input, maxLineBytes, controller.signal)
  } finally {
    options.signal?.removeEventListener("abort", abort)
    process.off("SIGINT", abort)
    process.off("SIGTERM", abort)
  }
}
