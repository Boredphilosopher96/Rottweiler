import {
  PLUGIN_PROTOCOL_VERSION,
  PROTOCOL_LIMITS,
  RPC_METHODS,
  type CommandExecuteParams,
  type EventPublishParams,
  type HookDecision,
  type HookInvokeParams,
  type HookName,
  type JsonObject,
  type JsonValue,
  type PluginManifest,
  type PluginPushMethod,
  type ProviderCompleteParams,
  type ProviderEvent,
  type ProviderStream,
  type RpcId,
  type ToolCallParams,
  type ToolResponse,
} from "./protocol"
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
  injectMessage(sessionId: string, content: string): Promise<void>
  setStatus(sessionId: string, status: string): Promise<void>
  notify(title: string, message: string, sessionId?: string): Promise<void>
}

export interface HandlerContext {
  readonly push: PushApi
  /** Aborts on host shutdown, SIGINT/SIGTERM, provider cancellation, or a bounded non-provider handler timeout. */
  readonly signal: AbortSignal
  /** Writes only a bounded label to stderr. Never pass prompts, tool args, or credentials. */
  debug(label: string): void
}

export type ToolHandler = (params: ToolCallParams, context: HandlerContext) => ToolResponse | Promise<ToolResponse>
export type CommandHandler = (
  params: CommandExecuteParams,
  context: HandlerContext,
) => JsonValue | Promise<JsonValue>
export type HookHandler = (
  params: HookInvokeParams,
  context: HandlerContext,
) => HookDecision | Promise<HookDecision>
export type EventHandler = (params: EventPublishParams, context: HandlerContext) => void | Promise<void>
export type ProviderHandler = (
  params: ProviderCompleteParams,
  context: HandlerContext,
) => ProviderStream | Promise<ProviderStream>

export interface PluginHandlers {
  readonly tools?: Readonly<Record<string, ToolHandler>>
  readonly commands?: Readonly<Record<string, CommandHandler>>
  readonly hooks?: Partial<Readonly<Record<HookName, HookHandler>>>
  readonly events?: Readonly<Record<string, EventHandler>>
  readonly providers?: Readonly<Record<string, ProviderHandler>>
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

type CanonicalNameKind = "plugin" | "tool" | "command" | "event"

function requireCanonicalName(value: string, kind: CanonicalNameKind, label: string): void {
  const expression = kind === "tool"
    ? /^[a-z0-9_]+$/
    : kind === "event"
      ? /^[A-Z][A-Za-z0-9_]*$/
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

function validateDefinition(definition: PluginDefinition): void {
  const { manifest, handlers } = definition
  requireKeys(manifest, "manifest", ["name", "version", "protocol", "capabilities"])
  requireKeys(manifest.capabilities, "capabilities", [
    "tools", "commands", "hooks", "providers", "event_subscriptions", "push",
  ])
  if (manifest.protocol !== PLUGIN_PROTOCOL_VERSION) throw new Error("plugin manifest protocol must be 1")
  requireCanonicalName(manifest.name, "plugin", "plugin name")
  requireText(manifest.version, "plugin version", PROTOCOL_LIMITS.maxVersionBytes)
  if (byteLength(JSON.stringify(manifest)) > PROTOCOL_LIMITS.maxManifestBytes) {
    throw new Error("plugin manifest exceeds the protocol size limit")
  }

  const pairs: readonly [string, readonly string[], readonly string[]][] = [
    ["tool", (manifest.capabilities.tools ?? []).map((entry) => entry.name), Object.keys(handlers.tools ?? {})],
    ["command", (manifest.capabilities.commands ?? []).map((entry) => entry.name), Object.keys(handlers.commands ?? {})],
    [
      "hook",
      (manifest.capabilities.hooks ?? []).map((entry) => (typeof entry === "string" ? entry : entry.name)),
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
    if (declared.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) {
      throw new Error(`too many ${kind} capabilities`)
    }
    requireUnique(declared, kind)
    for (const name of implemented) {
      if (!declared.includes(name)) throw new Error(`${kind} handler ${name} exceeds the manifest`)
    }
    for (const name of declared) {
      if (!implemented.includes(name)) throw new Error(`${kind} capability ${name} has no handler`)
    }
  }
  const push = manifest.capabilities.push ?? []
  if (push.length > PROTOCOL_LIMITS.maxCapabilitiesPerKind) throw new Error("too many push capabilities")
  requireUnique(push, "push")
  const validPush = new Set<PluginPushMethod>([
    RPC_METHODS.injectMessage, RPC_METHODS.setStatus, RPC_METHODS.notify,
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
    requireKeys(provider, "provider", ["alias-prefix"])
    const prefix = provider["alias-prefix"]
    if (byteLength(prefix) < 2 || byteLength(prefix) > PROTOCOL_LIMITS.maxNameBytes || !/^[a-z0-9_.-]+\/$/.test(prefix)) {
      throw new Error("provider alias-prefix must be a bounded canonical prefix ending in /")
    }
  }
  const validToolCapabilities = new Set(["reads-fs", "writes-fs", "network", "exec"])
  for (const tool of manifest.capabilities.tools ?? []) {
    if (new Set(tool.caps).size !== tool.caps.length || tool.caps.some((capability) => !validToolCapabilities.has(capability))) {
      throw new Error(`tool ${tool.name} has invalid or duplicate capabilities`)
    }
  }
  for (const event of manifest.capabilities.event_subscriptions ?? []) requireCanonicalName(event, "event", "event subscription")
  for (const hook of manifest.capabilities.hooks ?? []) {
    const hookName = typeof hook === "string" ? hook : hook.name
    const validHooks = new Set([
      "session_start", "session_end", "user_prompt_submit", "pre_tool", "post_tool",
      "pre_compact", "turn_end", "permission_check",
    ])
    if (!validHooks.has(hookName)) throw new Error(`unknown hook capability ${hookName}`)
    if (typeof hook !== "string") requireKeys(hook, `hook ${hookName}`, ["name", "failure_policy"])
    if (typeof hook !== "string" && hook.failure_policy !== "fail-open" && hook.failure_policy !== "fail-closed") {
      throw new Error(`hook ${hook.name} has an invalid failure policy`)
    }
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
  #initialized = false
  #shuttingDown = false
  readonly #providerCalls = new Map<RpcId, AbortController>()
  readonly #providerTasks = new Set<Promise<void>>()

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
    this.#writer = new BoundedJsonWriter(transport.output, maxLineBytes)
  }

  async serve(
    input: AsyncIterable<Uint8Array>,
    maxLineBytes = DEFAULT_MAX_RPC_LINE_BYTES,
    signal?: AbortSignal,
  ): Promise<void> {
    const iterator = readBoundedLines(input, maxLineBytes)[Symbol.asyncIterator]()
    const abortLifetime = () => this.#lifetime.abort()
    signal?.addEventListener("abort", abortLifetime, { once: true })
    if (signal?.aborted === true) this.#lifetime.abort()
    let wakeAbort: (() => void) | undefined
    const aborted = new Promise<"aborted">((resolve) => {
      wakeAbort = () => resolve("aborted")
      signal?.addEventListener("abort", wakeAbort, { once: true })
    })
    try {
      while (!this.#shuttingDown && signal?.aborted !== true) {
        const next = await Promise.race([iterator.next(), aborted])
        if (next === "aborted" || next.done) break
        await this.handleLine(next.value)
      }
    } finally {
      if (wakeAbort !== undefined) signal?.removeEventListener("abort", wakeAbort)
      signal?.removeEventListener("abort", abortLifetime)
      void iterator.return?.(undefined)
      if (!this.#shuttingDown) await this.shutdown()
      await this.#writer.drain()
    }
  }

  async handleLine(line: string): Promise<void> {
    let message: unknown
    try {
      message = JSON.parse(line)
    } catch {
      await this.#failure(null, -32700, "parse error")
      return
    }
    if (message === null || typeof message !== "object" || Array.isArray(message)) {
      await this.#failure(null, -32600, "invalid request")
      return
    }
    const candidate = message as Record<string, unknown>
    const id = typeof candidate.id === "string" || typeof candidate.id === "number" ? candidate.id : undefined
    if (own(candidate, "id") && id === undefined) {
      await this.#failure(null, -32600, "invalid request")
      return
    }
    if (candidate.jsonrpc === "2.0" && typeof candidate.method !== "string" && id !== undefined) {
      return
    }
    if (candidate.jsonrpc !== "2.0" || typeof candidate.method !== "string") {
      await this.#failure(id ?? null, -32600, "invalid request")
      return
    }
    const isNotification = !own(candidate, "id")
    if (candidate.method === RPC_METHODS.providerCancel && isNotification) {
      try {
        this.#cancelProvider(candidate.params)
      } catch {
        this.#debug("notification provider/cancel failed")
      }
      return
    }
    if (candidate.method === RPC_METHODS.providerComplete && id !== undefined) {
      const task = this.#handleProvider(id, candidate.params)
      this.#providerTasks.add(task)
      void task.finally(() => this.#providerTasks.delete(task))
      return
    }
    try {
      const result = await this.#dispatch(candidate.method, candidate.params)
      if (!isNotification && id !== undefined) await this.#success(id, result)
    } catch (error) {
      if (isNotification) {
        this.#debug(`notification ${candidate.method} failed`)
      } else if (error instanceof SafeRpcError) {
        await this.#failure(id ?? null, error.code, error.safeMessage, error.safeData)
      } else {
        // Arbitrary exception messages, causes, stacks, and handler inputs can contain secrets.
        await this.#failure(id ?? null, -32603, "plugin handler failed")
      }
    }
  }

  async shutdown(): Promise<void> {
    if (this.#shuttingDown) return
    this.#shuttingDown = true
    this.#lifetime.abort()
    const handler = this.definition.handlers.shutdown
    if (handler === undefined) {
      await Promise.allSettled(this.#providerTasks)
      return
    }
    let timeout: ReturnType<typeof setTimeout> | undefined
    await Promise.race([
      Promise.resolve().then(() => handler(this.#lifetime.signal)).catch(() => this.#debug("shutdown handler failed")),
      new Promise<void>((resolve) => {
        timeout = setTimeout(() => {
          this.#debug("shutdown handler timed out")
          resolve()
        }, this.#handlerTimeoutMs)
      }),
    ])
    if (timeout !== undefined) clearTimeout(timeout)
    await Promise.allSettled(this.#providerTasks)
  }

  async #dispatch(method: string, rawParams: unknown): Promise<JsonValue> {
    if (method === RPC_METHODS.initialize) {
      if (this.#initialized) throw new SafeRpcError(-32600, "plugin is already initialized")
      const params = object(rawParams)
      requireRpcKeys(params, "initialize params", ["host", "protocol", "min_protocol", "max_frame_bytes"])
      if (
        params.host !== "rottweiler"
        || params.protocol !== PLUGIN_PROTOCOL_VERSION
        || typeof params.min_protocol !== "number"
        || !Number.isSafeInteger(params.min_protocol)
        || params.min_protocol < 1
        || params.min_protocol > PLUGIN_PROTOCOL_VERSION
        || typeof params.max_frame_bytes !== "number"
        || !Number.isSafeInteger(params.max_frame_bytes)
        || params.max_frame_bytes < 1
        || params.max_frame_bytes > PROTOCOL_LIMITS.maxLineBytes
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
    if (method === RPC_METHODS.toolCall) {
      const params = this.#toolParams(rawParams)
      const handler = this.definition.handlers.tools?.[params.name]
      if (handler === undefined) throw new SafeRpcError(-32601, "tool is not declared")
      return this.#runHandler((context) => handler(params, context)) as unknown as Promise<JsonValue>
    }
    if (method === RPC_METHODS.commandExecute) {
      const params = this.#commandParams(rawParams)
      const handler = this.definition.handlers.commands?.[params.name]
      if (handler === undefined) throw new SafeRpcError(-32601, "command is not declared")
      return this.#runHandler((context) => handler(params, context))
    }
    if (method === RPC_METHODS.hookInvoke) {
      const params = this.#hookParams(rawParams)
      const handler = this.definition.handlers.hooks?.[params.hook]
      if (handler === undefined) throw new SafeRpcError(-32601, "hook is not declared")
      return this.#runHandler((context) => handler(params, context)) as Promise<JsonValue>
    }
    if (method === RPC_METHODS.eventPublish) {
      const params = this.#eventParams(rawParams)
      const handler = this.definition.handlers.events?.[params.event]
      if (handler === undefined) throw new SafeRpcError(-32601, "event is not subscribed")
      await this.#runHandler((context) => handler(params, context))
      return null
    }
    throw new SafeRpcError(-32601, "method not found")
  }

  async #handleProvider(id: RpcId, rawParams: unknown): Promise<void> {
    if (!this.#initialized || this.#shuttingDown || this.#providerCalls.has(id) || this.#providerCalls.size >= 64) {
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
    let sawFinished = false
    try {
      const events = await handler(params, this.#context(call.signal))
      if (events === null || typeof events !== "object" || !(Symbol.asyncIterator in events)) {
        throw new SafeRpcError(-32603, "provider must return an async event stream")
      }
      for await (const event of events) {
        if (call.signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
        this.#validateProviderEvent(event, sawFinished)
        if (event.type === "finished") sawFinished = true
        await this.#writer.write({
          jsonrpc: "2.0",
          method: RPC_METHODS.providerEvent,
          params: { request_id: id, event } as unknown as JsonValue,
        })
      }
      if (!sawFinished) throw new SafeRpcError(-32603, "provider stream ended before finished")
      await this.#success(id, null)
    } catch (error) {
      if (error instanceof SafeRpcError) await this.#failure(id, error.code, error.safeMessage, error.safeData)
      else await this.#failure(id, -32603, "plugin provider failed")
    } finally {
      this.#providerCalls.delete(id)
      this.#lifetime.signal.removeEventListener("abort", cancel)
    }
  }

  #cancelProvider(rawParams: unknown): void {
    const value = object(rawParams)
    requireRpcKeys(value, "provider/cancel params", ["request_id"])
    const requestId = value.request_id
    if (typeof requestId !== "string" && typeof requestId !== "number") {
      throw new SafeRpcError(-32602, "invalid provider/cancel params")
    }
    this.#providerCalls.get(requestId)?.abort()
  }

  #validateProviderEvent(event: ProviderEvent, sawFinished: boolean): void {
    if (event === null || typeof event !== "object" || Array.isArray(event) || typeof event.type !== "string") {
      throw new SafeRpcError(-32603, "provider emitted an invalid event")
    }
    const allowed = new Set([
      "route_selected", "message_start", "text_delta", "thinking_delta", "tool_call_start",
      "tool_call_arguments_delta", "tool_call_end", "citation", "usage", "finished",
    ])
    if (!allowed.has(event.type) || sawFinished) {
      throw new SafeRpcError(-32603, "provider emitted an invalid event sequence")
    }
  }

  #toolParams(raw: unknown): ToolCallParams {
    const value = object(raw)
    requireRpcKeys(value, "tool/call params", ["name", "input"])
    return {
      name: string(value.name, "tool name"),
      input: object(value.input, "tool input"),
    }
  }

  #commandParams(raw: unknown): CommandExecuteParams {
    const value = object(raw)
    requireRpcKeys(value, "command/execute params", ["name", "arguments"])
    if (typeof value.arguments !== "string") throw new SafeRpcError(-32602, "invalid command arguments")
    return {
      name: string(value.name, "command name"),
      arguments: value.arguments,
    }
  }

  #hookParams(raw: unknown): HookInvokeParams {
    const value = object(raw)
    requireRpcKeys(value, "hook/invoke params", ["hook", "payload"])
    const hook = string(value.hook, "hook") as HookName
    return {
      hook,
      payload: object(value.payload, "hook payload"),
    }
  }

  #eventParams(raw: unknown): EventPublishParams {
    const value = object(raw)
    requireRpcKeys(value, "event/publish params", ["event", "payload"])
    return {
      event: string(value.event, "event"),
      payload: object(value.payload, "event payload"),
    }
  }

  #providerParams(raw: unknown): ProviderCompleteParams {
    const value = object(raw)
    requireRpcKeys(value, "provider/complete params", ["alias", "request"])
    return {
      alias: string(value.alias, "provider alias"),
      request: object(value.request, "provider request") as unknown as ProviderCompleteParams["request"],
    }
  }

  #debug(label: string): void {
    const safe = label.replace(/[\r\n\x00-\x1f\x7f]/g, " ").slice(0, 256)
    this.transport.error?.write(`[rottweiler-plugin:${this.definition.manifest.name}] ${safe}\n`)
  }

  #push(method: PluginPushMethod, params: JsonValue, signal: AbortSignal): Promise<void> {
    if (!this.#pushCapabilities.has(method)) {
      throw new SafeRpcError(-32003, "push method is not declared")
    }
    if (signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
    const id = `plugin-push-${this.#nextPushId}`
    this.#nextPushId += 1
    return this.#writer.write({ jsonrpc: "2.0", id, method, params })
  }

  #context(signal: AbortSignal): HandlerContext {
    return {
      signal,
      push: {
        injectMessage: (sessionId, content) => {
          requireText(sessionId, "session id", PROTOCOL_LIMITS.maxNameBytes)
          requireText(content, "injected message", PROTOCOL_LIMITS.maxHookPayloadBytes)
          return this.#push(RPC_METHODS.injectMessage, { session_id: sessionId, content }, signal)
        },
        setStatus: (sessionId, status) => {
          requireText(sessionId, "session id", PROTOCOL_LIMITS.maxNameBytes)
          requireText(status, "status", PROTOCOL_LIMITS.maxRpcMessageBytes)
          return this.#push(RPC_METHODS.setStatus, { session_id: sessionId, status }, signal)
        },
        notify: (title, message, sessionId) => {
          requireText(title, "notification title", PROTOCOL_LIMITS.maxNameBytes)
          requireText(message, "notification message", PROTOCOL_LIMITS.maxRpcMessageBytes)
          if (sessionId !== undefined) requireText(sessionId, "session id", PROTOCOL_LIMITS.maxNameBytes)
          return this.#push(RPC_METHODS.notify, {
            title,
            message,
            ...(sessionId === undefined ? {} : { session_id: sessionId }),
          }, signal)
        },
      },
      debug: (label) => this.#debug(label),
    }
  }

  async #runHandler<T>(invoke: (context: HandlerContext) => T | Promise<T>): Promise<T> {
    if (this.#lifetime.signal.aborted) throw new SafeRpcError(-32800, "plugin request cancelled")
    const call = new AbortController()
    let timedOut = false
    const cancel = () => call.abort()
    this.#lifetime.signal.addEventListener("abort", cancel, { once: true })
    if (this.#lifetime.signal.aborted) call.abort()
    let timer: ReturnType<typeof setTimeout> | undefined
    const cancelled = new Promise<never>((_resolve, reject) => {
      call.signal.addEventListener("abort", () => {
        reject(new SafeRpcError(
          timedOut ? -32004 : -32800,
          timedOut ? "plugin handler timed out" : "plugin request cancelled",
        ))
      }, { once: true })
    })
    timer = setTimeout(() => {
      timedOut = true
      call.abort()
    }, this.#handlerTimeoutMs)
    try {
      return await Promise.race([
        Promise.resolve().then(() => invoke(this.#context(call.signal))),
        cancelled,
      ])
    } finally {
      if (timer !== undefined) clearTimeout(timer)
      this.#lifetime.signal.removeEventListener("abort", cancel)
    }
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
