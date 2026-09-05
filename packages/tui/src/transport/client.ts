import type { ClientDiagnostics } from "../client-diagnostics"
import validateCommandReply from "../../../../protocol/command-reply-validator.js"
import { CLIENT_COMMAND_EXECUTION, ENGINE_EVENT_DELIVERY, MAX_COMMAND_REPLY_BYTES, PROTOCOL_VERSION } from "../protocol"
import { boundedJson } from "./json"
import type { ClientCommand, CommandReply } from "../protocol"
import {
  DEFAULT_BACKOFF_POLICY,
  backoffDelay,
  systemBackoffScheduler,
  validateBackoffPolicy,
  type BackoffPolicy,
  type BackoffScheduler,
} from "./backoff"
import { parseSseStream, SseLimitError, type SseParserOptions } from "./sse"
import {
  durableSequenceId,
  isRecord,
  normalizeWireEngineEvent,
  type AttachSessionCommand,
  type TransportConnectionUpdate,
  type WireEngineEvent,
} from "./types"

export interface EngineTransportOptions {
  readonly diagnostics?: ClientDiagnostics | undefined
  readonly socketPath: string
  readonly bootstrapToken: string | BootstrapTokenProvider
  readonly origin?: string
  readonly connectPath?: string
  readonly commandPath?: string
  readonly providerApiKeyPath?: string
  readonly eventsPath?: (sessionId: string, lastSeenSequence: string | null) => string
  readonly backoff?: BackoffPolicy
  readonly scheduler?: BackoffScheduler
  readonly sse?: SseParserOptions
  readonly fetch?: typeof fetch
}

export type BootstrapTokenProvider = () => string | Promise<string>

export type EngineStreamRestartMode = "immediate" | "backoff"

export interface EngineSubscriptionOptions {
  readonly attach: AttachSessionCommand
  readonly signal: AbortSignal
  readonly onEvent: (event: WireEngineEvent) => void | Promise<void>
  readonly onConnection?: (update: TransportConnectionUpdate) => void
  readonly onReconnect?: () => void | Promise<void>
  readonly onReplayCursorAhead?: () => void | Promise<void>
  readonly getLastSeenSequence?: () => string | null
  readonly requestId?: () => string
}

interface ActiveEventStream {
  readonly controller: AbortController
  restart: EngineStreamRestartMode | null
}

export class EngineHttpSseClient {
  readonly #diagnostics: ClientDiagnostics | undefined
  readonly #socketPath: string
  readonly #bootstrapToken: BootstrapTokenProvider
  readonly #origin: string
  readonly #connectPath: string
  readonly #commandPath: string
  readonly #providerApiKeyPath: string
  readonly #eventsPath: (sessionId: string, lastSeenSequence: string | null) => string
  readonly #backoff: BackoffPolicy
  readonly #scheduler: BackoffScheduler
  readonly #sse: SseParserOptions
  readonly #fetch: typeof fetch
  #clientAuth: ClientAuth | null = null
  #clientAuthRequest: Promise<ClientAuth> | null = null
  #activeEventStream: ActiveEventStream | null = null

  constructor(options: EngineTransportOptions) {
    if (options.socketPath.length === 0) {
      throw new TypeError("engine Unix socket path must not be empty")
    }
    if (typeof options.bootstrapToken === "string" && options.bootstrapToken.length === 0) {
      throw new TypeError("engine bootstrap token must not be empty")
    }
    this.#diagnostics = options.diagnostics
    this.#socketPath = options.socketPath
    this.#bootstrapToken =
      typeof options.bootstrapToken === "string"
        ? () => options.bootstrapToken as string
        : options.bootstrapToken
    this.#origin = (options.origin ?? "http://rottweiler.local").replace(/\/$/, "")
    this.#connectPath = options.connectPath ?? "/v1/connect"
    this.#commandPath = options.commandPath ?? "/v1/command"
    this.#providerApiKeyPath = options.providerApiKeyPath ?? "/v1/provider-api-key"
    this.#eventsPath = options.eventsPath ?? defaultEventsPath
    this.#backoff = options.backoff ?? DEFAULT_BACKOFF_POLICY
    this.#scheduler = options.scheduler ?? systemBackoffScheduler
    this.#sse = options.sse ?? {}
    this.#fetch = options.fetch ?? fetch
    validateBackoffPolicy(this.#backoff)
  }

  async postCommand(
    command: ClientCommand,
    signal?: AbortSignal,
  ): Promise<CommandReply | null> {
    const auth = await this.#ensureClientAuth(signal)
    const authenticatedCommand: ClientCommand = {
      ...command,
      meta: { ...command.meta, client_id: auth.clientId },
    }
    const response = await this.#fetch(this.#url(this.#commandPath), {
      unix: this.#socketPath,
      method: "POST",
      headers: this.#clientHeaders(auth, { "Content-Type": "application/json" }),
      body: JSON.stringify(authenticatedCommand),
      ...(signal === undefined ? {} : { signal }),
    })
    if (!response.ok) {
      if (response.status === 401 || response.status === 403) {
        this.#clientAuth = null
      }
      throw new EngineTransportError("engine command rejected", response.status)
    }
    if (response.status === 204 || response.headers.get("Content-Length") === "0") {
      return null
    }
    const contentType = response.headers.get("Content-Type") ?? ""
    if (!contentType.toLowerCase().includes("application/json")) {
      return null
    }
    const reply: unknown = await boundedJson(response, MAX_COMMAND_REPLY_BYTES, this.#diagnostics)
    const validatedAt = this.#diagnostics?.start()
    try {
      if (!validateCommandReply(reply)) {
        throw new EngineTransportError("engine returned an invalid command reply")
      }
      const expectedReply = CLIENT_COMMAND_EXECUTION[command.type] === "read" ? "read" : "command"
      if (reply.type !== expectedReply) {
        throw new EngineTransportError("engine reply class does not match the command")
      }
      if (reply.type === "read" && reply.events.some(event =>
        ENGINE_EVENT_DELIVERY[event.type] !== "connection" || !("meta" in event) || event.meta.protocol_version !== PROTOCOL_VERSION
        || !("client_id" in event.meta) || event.meta.client_id !== auth.clientId
        || !("request_id" in event.meta) || event.meta.request_id !== command.meta.request_id
        || ("session_id" in command && "session_id" in event && event.session_id !== command.session_id))) {
        throw new EngineTransportError("engine read reply contains non-query events")
      }
      return reply
    } finally { if (validatedAt !== undefined) this.#diagnostics?.finish("reply_validation", validatedAt) }
  }

  async submitProviderApiKey(
    sessionId: string,
    provider: string,
    apiKey: string,
    signal?: AbortSignal,
  ): Promise<{ readonly stored: true; readonly activated: boolean; readonly warnings: readonly string[] }> {
    const auth = await this.#ensureClientAuth(signal)
    const response = await this.#fetch(this.#url(this.#providerApiKeyPath), {
      unix: this.#socketPath,
      method: "POST",
      headers: this.#clientHeaders(auth, { "Content-Type": "application/json" }),
      body: JSON.stringify({ session_id: sessionId, provider, api_key: apiKey }),
      ...(signal === undefined ? {} : { signal }),
    })
    if (!response.ok) {
      if (response.status === 401 || response.status === 403) this.#clientAuth = null
      throw new EngineTransportError("provider credential submission failed", response.status)
    }
    const value: unknown = await response.json()
    if (!isRecord(value) || value.stored !== true || typeof value.activated !== "boolean" || !Array.isArray(value.warnings)
      || value.warnings.some((warning) => typeof warning !== "string")) {
      throw new EngineTransportError("engine returned an invalid credential result")
    }
    return { stored: true, activated: value.activated, warnings: value.warnings as string[] }
  }

  async activateProvider(
    sessionId: string,
    provider: string,
    signal?: AbortSignal,
  ): Promise<void> {
    const auth = await this.#ensureClientAuth(signal)
    const response = await this.#fetch(this.#url("/v1/activate-provider"), {
      unix: this.#socketPath,
      method: "POST",
      headers: this.#clientHeaders(auth, { "Content-Type": "application/json" }),
      body: JSON.stringify({ session_id: sessionId, provider }),
      ...(signal === undefined ? {} : { signal }),
    })
    if (!response.ok) throw new EngineTransportError("provider activation failed", response.status)
  }

  restartStream(mode: EngineStreamRestartMode = "immediate"): boolean {
    const active = this.#activeEventStream
    if (active === null || active.controller.signal.aborted) {
      return false
    }
    active.restart = mode
    active.controller.abort(
      new DOMException("engine event stream restart requested", "AbortError"),
    )
    return true
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    let attempt = 0
    let reconnecting = false
    let replayCursorReset = false
    let lastSeen = options.getLastSeenSequence?.() ?? options.attach.last_seen_sequence ?? null

    while (!options.signal.aborted) {
      if (
        this.#backoff.maximumAttempts !== undefined &&
        attempt > this.#backoff.maximumAttempts
      ) {
        throw new EngineTransportError("engine reconnect attempts exhausted")
      }
      options.onConnection?.({
        phase: reconnecting ? "reconnecting" : "connecting",
        attempt,
      })

      let receivedEvent = false
      const eventStream: ActiveEventStream = {
        controller: new AbortController(),
        restart: null,
      }
      const eventStreamController = eventStream.controller
      this.#activeEventStream = eventStream
      const abortEventStream = () => eventStreamController.abort(options.signal.reason)
      options.signal.addEventListener("abort", abortEventStream, { once: true })
      try {
        const attach: AttachSessionCommand = {
          ...options.attach,
          role: reconnecting ? "observer" : options.attach.role,
          meta: {
            ...options.attach.meta,
            request_id: options.requestId?.() ?? crypto.randomUUID(),
          },
          ...(lastSeen === null ? { last_seen_sequence: null } : { last_seen_sequence: lastSeen }),
        }
        const reply = await this.postCommand(attach, options.signal)
        const outcome = reply?.outcome
        if (outcome?.type === "rejected") {
          throw new EngineTransportError(`engine rejected session attach: ${outcome.error.message}`)
        }
        if (reconnecting) {
          await options.onReconnect?.()
        }

        const response = await this.#fetch(
          this.#url(this.#eventsPath(options.attach.session_id, lastSeen)),
          {
            unix: this.#socketPath,
            method: "GET",
            headers: this.#clientHeaders(await this.#ensureClientAuth(options.signal), {
              Accept: "text/event-stream",
            }),
            signal: eventStreamController.signal,
          },
        )
        if (!response.ok) {
          if (response.status === 401 || response.status === 403) {
            this.#clientAuth = null
          }
          if (await isReplayCursorAheadResponse(response)) {
            throw new ReplayCursorAheadError(response.status)
          }
          throw new EngineTransportError("engine event stream rejected", response.status)
        }
        if (!response.body) {
          throw new EngineTransportError("engine event stream has no response body")
        }
        const contentType = response.headers.get("Content-Type")?.toLowerCase() ?? ""
        if (!contentType.startsWith("text/event-stream")) {
          throw new EngineTransportError("engine event stream has an invalid content type")
        }
        options.onConnection?.({ phase: "connected", attempt })

        for await (const frame of parseSseStream(
          response.body,
          this.#sse,
          eventStreamController.signal,
        )) {
          const decodedAt = this.#diagnostics?.start()
          let value: WireEngineEvent | null
          try { value = normalizeWireEngineEvent(parseEventJson(frame.data)) }
          finally { if (decodedAt !== undefined) this.#diagnostics?.finish("event_decode", decodedAt) }
          if (value === null) {
            throw new EngineProtocolError("engine event stream emitted an invalid event")
          }
          await options.onEvent(value)
          receivedEvent = true
          const sequence = durableSequenceId(value)
          if (sequence !== null) {
            lastSeen = options.getLastSeenSequence?.() ?? sequence
          }
          if (eventStream.restart !== null) {
            break
          }
        }
        options.onConnection?.({ phase: "disconnected", attempt })
      } catch (error) {
        if (options.signal.aborted || (eventStream.restart === null && isAbortError(error))) {
          break
        }
        if (error instanceof ReplayCursorAheadError) {
          if (replayCursorReset || options.onReplayCursorAhead === undefined) {
            throw error
          }
          await options.onReplayCursorAhead()
          replayCursorReset = true
          lastSeen = null
          attempt = 0
          reconnecting = true
          options.onConnection?.({ phase: "disconnected", attempt })
          continue
        }
        if (eventStream.restart !== null) {
          options.onConnection?.({ phase: "disconnected", attempt })
        } else {
          options.onConnection?.({
            phase: "disconnected",
            attempt,
            error: transportErrorMessage(error),
          })
          // A bounded parser rejection is deterministic for the same replay
          // cursor. Retrying would request the identical poison event forever.
          if (error instanceof SseLimitError || error instanceof EngineProtocolError) {
            throw error
          }
        }
      } finally {
        options.signal.removeEventListener("abort", abortEventStream)
        if (this.#activeEventStream === eventStream) {
          this.#activeEventStream = null
        }
        if (!eventStreamController.signal.aborted) {
          eventStreamController.abort(new Error("engine event stream attempt ended"))
        }
      }

      if (options.signal.aborted) {
        break
      }
      if (eventStream.restart === "immediate") {
        attempt = 0
        reconnecting = true
        lastSeen = options.getLastSeenSequence?.() ?? lastSeen
        continue
      }
      const gapBackoff = eventStream.restart === "backoff"
      const delayAttempt = gapBackoff ? attempt : receivedEvent ? 0 : attempt
      try {
        await this.#scheduler.sleep(backoffDelay(this.#backoff, delayAttempt), options.signal)
      } catch (error) {
        if (options.signal.aborted || isAbortError(error)) {
          break
        }
        throw error
      }
      attempt = gapBackoff ? attempt + 1 : receivedEvent ? 0 : attempt + 1
      reconnecting = true
      lastSeen = options.getLastSeenSequence?.() ?? lastSeen
    }

    options.onConnection?.({ phase: "closed", attempt })
  }

  #url(path: string): string {
    if (!path.startsWith("/")) {
      throw new TypeError("engine transport paths must be absolute")
    }
    return `${this.#origin}${path}`
  }

  async #ensureClientAuth(signal?: AbortSignal): Promise<ClientAuth> {
    if (this.#clientAuth !== null) {
      return this.#clientAuth
    }
    if (this.#clientAuthRequest === null) {
      this.#clientAuthRequest = this.#mintClientAuth(signal).finally(() => {
        this.#clientAuthRequest = null
      })
    }
    const auth = await this.#clientAuthRequest
    this.#clientAuth = auth
    return auth
  }

  async #mintClientAuth(signal?: AbortSignal): Promise<ClientAuth> {
    const bootstrapToken = await this.#bootstrapToken()
    if (bootstrapToken.length === 0) {
      throw new EngineTransportError("engine bootstrap token source returned an empty token")
    }
    const response = await this.#fetch(this.#url(this.#connectPath), {
      unix: this.#socketPath,
      method: "POST",
      headers: this.#bootstrapHeaders(bootstrapToken),
      ...(signal === undefined ? {} : { signal }),
    })
    if (!response.ok) {
      throw new EngineTransportError("engine bootstrap connection rejected", response.status)
    }
    const value: unknown = await response.json()
    if (
      !isRecord(value) ||
      typeof value.client_id !== "string" ||
      value.client_id.length === 0 ||
      typeof value.token !== "string" ||
      value.token.length === 0
    ) {
      throw new EngineTransportError("engine returned invalid client credentials")
    }
    return { clientId: value.client_id, token: value.token }
  }

  #bootstrapHeaders(bootstrapToken: string): Headers {
    const headers = new Headers()
    headers.set("Authorization", `Bearer ${bootstrapToken}`)
    return headers
  }

  #clientHeaders(auth: ClientAuth, additional: Record<string, string>): Headers {
    const headers = new Headers(additional)
    headers.set("Authorization", `Bearer ${auth.token}`)
    headers.set("x-rottweiler-client", auth.clientId)
    return headers
  }
}

export class EngineTransportError extends Error {
  readonly status: number | undefined

  constructor(message: string, status?: number) {
    super(status === undefined ? message : `${message} (HTTP ${status})`)
    this.name = "EngineTransportError"
    this.status = status
  }
}

export class EngineProtocolError extends EngineTransportError {
  constructor(message: string) {
    super(message)
    this.name = "EngineProtocolError"
  }
}

class ReplayCursorAheadError extends EngineTransportError {
  constructor(status: number) {
    super("engine replay cursor is ahead of the durable log", status)
    this.name = "ReplayCursorAheadError"
  }
}

function defaultEventsPath(sessionId: string, lastSeenSequence: string | null): string {
  const query = new URLSearchParams({ session_id: sessionId })
  if (lastSeenSequence !== null) {
    query.set("last_seen_sequence", lastSeenSequence)
  }
  return `/v1/events?${query.toString()}`
}

async function isReplayCursorAheadResponse(response: Response): Promise<boolean> {
  const contentType = response.headers.get("Content-Type")?.toLowerCase() ?? ""
  if (!contentType.includes("application/json")) return false
  try {
    const value: unknown = await response.json()
    return isRecord(value) &&
      isRecord(value.error) &&
      value.error.code === "replay_cursor_ahead"
  } catch {
    return false
  }
}

function parseEventJson(data: string): unknown {
  try {
    return JSON.parse(data)
  } catch {
    throw new EngineProtocolError("engine event stream emitted invalid JSON")
  }
}

function isAbortError(error: unknown): boolean {
  return (
    (error instanceof DOMException && error.name === "AbortError") ||
    (isRecord(error) && error.name === "AbortError")
  )
}

function transportErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "unknown engine transport failure"
}

interface ClientAuth {
  readonly clientId: string
  readonly token: string
}
