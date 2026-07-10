import type { ClientCommand } from "../protocol"
import {
  DEFAULT_BACKOFF_POLICY,
  backoffDelay,
  systemBackoffScheduler,
  validateBackoffPolicy,
  type BackoffPolicy,
  type BackoffScheduler,
} from "./backoff"
import { parseSseStream, type SseParserOptions } from "./sse"
import {
  durableSequenceId,
  isRecord,
  isWireEngineEvent,
  type AttachSessionCommand,
  type TransportConnectionUpdate,
  type WireEngineEvent,
} from "./types"

export interface EngineTransportOptions {
  readonly socketPath: string
  readonly bootstrapToken: string
  readonly origin?: string
  readonly connectPath?: string
  readonly commandPath?: string
  readonly eventsPath?: (sessionId: string) => string
  readonly backoff?: BackoffPolicy
  readonly scheduler?: BackoffScheduler
  readonly sse?: SseParserOptions
  readonly fetch?: typeof fetch
}

export interface EngineSubscriptionOptions {
  readonly attach: AttachSessionCommand
  readonly signal: AbortSignal
  readonly onEvent: (event: WireEngineEvent) => void | Promise<void>
  readonly onConnection?: (update: TransportConnectionUpdate) => void
  readonly getLastSeenSequence?: () => string | null
}

export class EngineHttpSseClient {
  readonly #socketPath: string
  readonly #bootstrapToken: string
  readonly #origin: string
  readonly #connectPath: string
  readonly #commandPath: string
  readonly #eventsPath: (sessionId: string) => string
  readonly #backoff: BackoffPolicy
  readonly #scheduler: BackoffScheduler
  readonly #sse: SseParserOptions
  readonly #fetch: typeof fetch
  #clientAuth: ClientAuth | null = null
  #clientAuthRequest: Promise<ClientAuth> | null = null

  constructor(options: EngineTransportOptions) {
    if (options.socketPath.length === 0) {
      throw new TypeError("engine Unix socket path must not be empty")
    }
    if (options.bootstrapToken.length === 0) {
      throw new TypeError("engine bootstrap token must not be empty")
    }
    this.#socketPath = options.socketPath
    this.#bootstrapToken = options.bootstrapToken
    this.#origin = (options.origin ?? "http://rottweiler.local").replace(/\/$/, "")
    this.#connectPath = options.connectPath ?? "/v1/connect"
    this.#commandPath = options.commandPath ?? "/v1/command"
    this.#eventsPath = options.eventsPath ?? defaultEventsPath
    this.#backoff = options.backoff ?? DEFAULT_BACKOFF_POLICY
    this.#scheduler = options.scheduler ?? systemBackoffScheduler
    this.#sse = options.sse ?? {}
    this.#fetch = options.fetch ?? fetch
    validateBackoffPolicy(this.#backoff)
  }

  async postCommand(command: ClientCommand, signal?: AbortSignal): Promise<unknown | null> {
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
    return contentType.toLowerCase().includes("application/json") ? response.json() : null
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    let attempt = 0
    let reconnecting = false
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
      try {
        const attach: AttachSessionCommand = {
          ...options.attach,
          ...(lastSeen === null ? { last_seen_sequence: null } : { last_seen_sequence: lastSeen }),
        }
        const acknowledgement = await this.postCommand(attach, options.signal)
        if (acknowledgement !== null) {
          if (!isWireEngineEvent(acknowledgement)) {
            throw new EngineTransportError("engine returned an invalid command acknowledgement")
          }
          await options.onEvent(acknowledgement)
        }

        const response = await this.#fetch(
          this.#url(this.#eventsPath(options.attach.session_id)),
          {
            unix: this.#socketPath,
            method: "GET",
            headers: this.#clientHeaders(await this.#ensureClientAuth(options.signal), {
              Accept: "text/event-stream",
            }),
            signal: options.signal,
          },
        )
        if (!response.ok) {
          if (response.status === 401 || response.status === 403) {
            this.#clientAuth = null
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

        for await (const frame of parseSseStream(response.body, this.#sse, options.signal)) {
          const value: unknown = parseEventJson(frame.data)
          if (!isWireEngineEvent(value)) {
            throw new EngineTransportError("engine event stream emitted an invalid event")
          }
          await options.onEvent(value)
          receivedEvent = true
          const sequence = durableSequenceId(value)
          if (sequence !== null) {
            lastSeen = options.getLastSeenSequence?.() ?? sequence
          }
        }
        options.onConnection?.({ phase: "disconnected", attempt })
      } catch (error) {
        if (options.signal.aborted || isAbortError(error)) {
          break
        }
        options.onConnection?.({
          phase: "disconnected",
          attempt,
          error: transportErrorMessage(error),
        })
      }

      if (options.signal.aborted) {
        break
      }
      const delayAttempt = receivedEvent ? 0 : attempt
      try {
        await this.#scheduler.sleep(backoffDelay(this.#backoff, delayAttempt), options.signal)
      } catch (error) {
        if (options.signal.aborted || isAbortError(error)) {
          break
        }
        throw error
      }
      attempt = receivedEvent ? 0 : attempt + 1
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
    const response = await this.#fetch(this.#url(this.#connectPath), {
      unix: this.#socketPath,
      method: "POST",
      headers: this.#bootstrapHeaders(),
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

  #bootstrapHeaders(): Headers {
    const headers = new Headers()
    headers.set("Authorization", `Bearer ${this.#bootstrapToken}`)
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

function defaultEventsPath(sessionId: string): string {
  void sessionId
  return "/v1/events"
}

function parseEventJson(data: string): unknown {
  try {
    return JSON.parse(data)
  } catch {
    throw new EngineTransportError("engine event stream emitted invalid JSON")
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
