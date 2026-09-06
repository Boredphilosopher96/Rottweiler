import { SessionSnapshotReader } from "./runtime-snapshots"
import { MAX_SESSION_CONTROLS_PREPARED_BYTES, MAX_SESSION_STATE_PREPARED_BYTES, MAX_SESSION_CHILDREN_PREPARED_BYTES } from "../../../protocol/types"
import type { ReplyAllocation } from "./transport/reply-allocation"
import type { EngineEvent } from "./protocol"
import type { ClientDiagnostics } from "./client-diagnostics"
import { CLIENT_COMMAND_EXECUTION } from "./protocol"
import type { SessionReader } from "./session-reader"
import { chmod, lstat, mkdir, open, readFile, rename, rm } from "node:fs/promises"
import { basename, dirname, isAbsolute, join } from "node:path"

import type { ClientCommand, CommandOutcome, CommandReply } from "./protocol"
import { PROTOCOL_VERSION } from "./protocol"
import {
  createInitialState,
  enterReplayMode,
  reduceRottweilerState,
  transportClosed,
  transportConnected,
  transportConnecting,
  transportDisconnected,
  transportReconnecting,
  type RottweilerAction,
  type RottweilerState,
} from "./state"
import { EngineHttpSseClient, durableSequenceId, isRecord, type EngineSubscriptionOptions, type EngineStreamRestartMode, type TransportConnectionUpdate } from "./transport"

const TOKEN_FILE_LIMIT = 64 * 1024
const CURSOR_FILE_LIMIT = 128
const FORK_OPERATION_FILE_LIMIT = 4 * 1024
const MAX_U64 = 18_446_744_073_709_551_615n
const SESSION_PREPARE_ATTEMPTS = 24
const SESSION_PREPARE_INITIAL_DELAY_MS = 10
const SESSION_PREPARE_MAXIMUM_DELAY_MS = 250
const HOST_SHUTDOWN_TIMEOUT_MS = 1_500

export interface EngineRuntimeEnvironment {
  readonly [name: string]: string | undefined
}

export interface RuntimeFileSystem {
  readText(path: string, maximumBytes: number): Promise<string | null>
  writePrivateTextAtomic(path: string, content: string): Promise<void>
}

export interface EngineRuntimeConfig {
  readonly socketPath: string
  readonly bootstrapToken: string
  readonly sessionId: string
  readonly lastSeenSequence: string | null
  readonly lastSeenFile: string | null
  readonly replayMode: boolean
  readonly forkOperationDirectory?: string | null
}

export interface RuntimeApp {
  readonly state: RottweilerState
  handleEvent(event: EngineEvent): void
  setState(state: RottweilerState): void
  setSessionId(sessionId: string): void
  beginInitialReplayBatch?(): void
  endInitialReplayBatch?(): void
  resetConnectionProjections?(): void
}

export interface RuntimeEngineClient {
  postCommand(command: ClientCommand, signal?: AbortSignal, allocation?: ReplyAllocation): Promise<CommandReply>
  submitProviderApiKey?(
    sessionId: string,
    provider: string,
    apiKey: string,
    signal?: AbortSignal,
  ): Promise<{
    readonly stored: true
    readonly activated: boolean
    readonly warnings: readonly string[]
  }>
  activateProvider?(sessionId: string, provider: string, signal?: AbortSignal): Promise<void>
  restartStream(mode?: EngineStreamRestartMode): boolean
  subscribe(options: EngineSubscriptionOptions): Promise<void>
}

export interface CreateEngineRuntimeOptions {
  readonly diagnostics?: ClientDiagnostics | undefined
  readonly environment?: EngineRuntimeEnvironment
  readonly files?: RuntimeFileSystem
  readonly fetch?: typeof fetch
  readonly client?: RuntimeEngineClient
  readonly requestId?: () => string
  readonly sleep?: RuntimeSleep
  readonly onDriverReady?: (sessionId: string) => void
}

export type RuntimeSleep = (delayMs: number, signal: AbortSignal) => Promise<void>

export const systemRuntimeFiles: RuntimeFileSystem = {
  async readText(path, maximumBytes) {
    const metadata = await lstat(path).catch((error: unknown) => {
      if (hasErrorCode(error, "ENOENT")) {
        return null
      }
      throw error
    })
    if (metadata === null) {
      return null
    }
    if (!metadata.isFile() || metadata.isSymbolicLink()) {
      throw new EngineRuntimeError("runtime handoff path is not a regular file")
    }
    assertOwnerPrivate(metadata.mode, metadata.uid, "runtime handoff file")
    if (metadata.size > maximumBytes) {
      throw new EngineRuntimeError("runtime handoff file exceeds its size limit")
    }
    return readFile(path, "utf8")
  },

  async writePrivateTextAtomic(path, content) {
    const parent = dirname(path)
    await mkdir(parent, { recursive: true, mode: 0o700 })
    const parentMetadata = await lstat(parent)
    if (!parentMetadata.isDirectory() || parentMetadata.isSymbolicLink()) {
      throw new EngineRuntimeError("runtime handoff parent is not a private directory")
    }
    assertOwnerPrivate(parentMetadata.mode, parentMetadata.uid, "runtime handoff parent")

    const temporary = join(parent, `.${basename(path)}.${crypto.randomUUID()}.tmp`)
    let handle: Awaited<ReturnType<typeof open>> | null = null
    try {
      handle = await open(temporary, "wx", 0o600)
      await handle.writeFile(content, { encoding: "utf8" })
      await handle.sync()
      await handle.close()
      handle = null
      await rename(temporary, path)
      await chmod(path, 0o600)
    } catch (error) {
      await handle?.close().catch(() => {})
      await rm(temporary, { force: true }).catch(() => {})
      throw error
    }
  },
}

export class EngineRuntimeError extends Error {
  constructor(message: string) {
    super(message)
    this.name = "EngineRuntimeError"
  }
}

/**
 * Owns the TUI's single authenticated engine connection. Rendering remains
 * synchronous; durable cursor persistence is coalesced in the background.
 */
export class TuiEngineRuntime {
  readonly #controls = new SessionSnapshotReader(
    MAX_SESSION_CONTROLS_PREPARED_BYTES,
    async (sessionId, signal, allocation) => {
      const generation = this.#sessionGeneration
      const reply = await this.#client.postCommand({ type: "get_session_controls", meta: this.#meta(), session_id: sessionId }, signal, allocation)
      signal.throwIfAborted()
      if (generation !== this.#sessionGeneration) throw new DOMException("session changed", "AbortError")
      const event = reply.type === "read" ? reply.events[0] : undefined
      if (reply.outcome.type === "rejected" || reply.type !== "read" || reply.events.length !== 1
        || event?.type !== "session_controls_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("session controls reply is missing its session-bound result")
      }
      return event
    },
    event => {
      const app = this.#requiredApp(), before = app.state.controls
      app.handleEvent(event)
      return app.state.controls !== before
    },
    (error, sessionId) => {
      if (sessionId !== this.#sessionId) return
      const app = this.#requiredApp()
      app.setState({ ...app.state, errors: [...app.state.errors.slice(-63), {
        category: "protocol", code: "session_controls_unavailable", message: safeErrorMessage(error), retryable: true,
      }] })
    },
  )

  readonly #metadata = new SessionSnapshotReader(
    MAX_SESSION_STATE_PREPARED_BYTES,
    async (sessionId, signal, allocation) => {
      const generation = this.#sessionGeneration
      const reply = await this.#client.postCommand({ type: "get_session_state", meta: this.#meta(), session_id: sessionId }, signal, allocation)
      signal.throwIfAborted()
      if (generation !== this.#sessionGeneration) throw new DOMException("session changed", "AbortError")
      const event = reply.type === "read" ? reply.events[0] : undefined
      if (reply.outcome.type === "rejected" || reply.type !== "read" || reply.events.length !== 1
        || event?.type !== "session_state_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("session metadata reply is missing its session-bound result")
      }
      return event
    },
    event => {
      const app = this.#requiredApp(), before = app.state.recovery
      app.handleEvent(event)
      return app.state.recovery !== before && app.state.recovery.compaction?.stale !== true
    },
    (error, sessionId) => {
      if (sessionId !== this.#sessionId) return
      const app = this.#requiredApp()
      app.setState({ ...app.state, errors: [...app.state.errors.slice(-63), {
        category: "protocol", code: "session_metadata_unavailable", message: safeErrorMessage(error), retryable: true,
      }] })
    },
  )

  readonly #children = new SessionSnapshotReader(
    MAX_SESSION_CHILDREN_PREPARED_BYTES,
    async (sessionId, signal, allocation) => {
      const generation = this.#sessionGeneration
      const reply = await this.#client.postCommand({ type: "read_session_children", meta: this.#meta(), session_id: sessionId, scope: { type: "session" } }, signal, allocation)
      signal.throwIfAborted()
      if (generation !== this.#sessionGeneration) throw new DOMException("session changed", "AbortError")
      const event = reply.type === "read" ? reply.events[0] : undefined
      if (reply.outcome.type === "rejected" || reply.type !== "read" || reply.events.length !== 1
        || event?.type !== "session_children_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("children reply is missing its session-bound result")
      }
      return event
    },
    event => {
      const app = this.#requiredApp(), before = app.state.recovery.children
      app.handleEvent(event)
      return app.state.recovery.children !== before
    },
    (error, sessionId) => {
      if (sessionId !== this.#sessionId) return
      const app = this.#requiredApp()
      app.setState({ ...app.state, errors: [...app.state.errors.slice(-63), {
        category: "protocol", code: "session_children_unavailable", message: safeErrorMessage(error), retryable: true,
      }] })
    },
  )

  readonly sessionReader: SessionReader = {
    children: async ({ sessionId, scope }, signal, allocation) => {
      const reply = await this.#readSession({ type: "read_session_children", meta: this.#meta(), session_id: sessionId, scope }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "session_children_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("children reply is missing its session-bound result")
      }
      return event.result
    },
    tail: async ({ sessionId, scope }, read, signal, allocation) => {
      const reply = await this.#readSession({ type: "read_transcript_tail", meta: this.#meta(), session_id: sessionId, scope, read }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "transcript_tail_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("live tail reply is missing its session-bound result")
      }
      return event.result
    },
    uiCatalog: async (sessionId, signal) => {
      const reply = await this.#readSession({ type: "get_ui_catalog", meta: this.#meta(), session_id: sessionId }, signal)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "ui_catalog_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("UI catalog reply is missing its session-bound result")
      }
      return event.catalog
    },
    uiPanels: async (sessionId, signal) => {
      const reply = await this.#readSession({ type: "get_ui_panels", meta: this.#meta(), session_id: sessionId }, signal)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "ui_panels_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("UI panels reply is missing its session-bound result")
      }
      return event.panels
    },
    todos: async ({ sessionId, scope }, signal) => {
      const reply = await this.#readSession({ type: "get_todos", meta: this.#meta(), session_id: sessionId, scope }, signal)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "todos_read" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("task reply is missing its session-bound result")
      }
      return event.result
    },
    page: async ({ sessionId, scope }, read, signal, allocation) => {
      const reply = await this.#readSession({ type: "read_transcript", meta: this.#meta(), session_id: sessionId, scope, read }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "transcript_page_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("transcript page reply is missing its result")
      }
      return event.result
    },
    content: async ({ sessionId, scope }, read, signal, allocation) => {
      const reply = await this.#readSession({ type: "read_transcript_content", meta: this.#meta(), session_id: sessionId, scope, read }, signal, allocation)
      const event = reply.events[0]
      if (reply.events.length !== 1 || event?.type !== "transcript_content_ready" || event.session_id !== sessionId) {
        throw new EngineRuntimeError("transcript content reply is missing its result")
      }
      return event.page
    },
  }
  readonly #config: EngineRuntimeConfig
  readonly #client: RuntimeEngineClient
  readonly #requestId: () => string
  readonly #controller = new AbortController()
  readonly #handoff: SequenceHandoff | null
  readonly #sleep: RuntimeSleep
  readonly #onDriverReady: ((sessionId: string) => void) | undefined
  readonly #forkOperations: ForkOperationHandoff | null
  readonly #ready: Promise<void>
  readonly #resolveReady: () => void
  readonly #rejectReady: (reason: unknown) => void
  #app: RuntimeApp | null = null
  #started = false
  #sessionId: string
  #sessionGeneration = 0
  #driverReady = false
  #transitionController: AbortController | null = null
  #subscriptionController: AbortController | null = null
  #subscription: Promise<void> | null = null
  #recoveringSequenceGap = false
  readonly #forkRequests = new Map<string, string>()

  constructor(
    config: EngineRuntimeConfig,
    client: RuntimeEngineClient,
    files: RuntimeFileSystem = systemRuntimeFiles,
    requestId: () => string = () => crypto.randomUUID(),
    sleep: RuntimeSleep = abortableSleep,
    onDriverReady?: (sessionId: string) => void,
  ) {
    this.#config = config
    this.#sessionId = config.sessionId
    this.#client = client
    this.#requestId = requestId
    this.#sleep = sleep
    this.#onDriverReady = onDriverReady
    this.#forkOperations =
      config.replayMode || config.forkOperationDirectory == null
        ? null
        : new ForkOperationHandoff(config.forkOperationDirectory, files)
    let resolveReady!: () => void
    let rejectReady!: (reason: unknown) => void
    this.#ready = new Promise<void>((resolve, reject) => {
      resolveReady = resolve
      rejectReady = reject
    })
    this.#resolveReady = resolveReady
    this.#rejectReady = rejectReady
    void this.#ready.catch(() => {
      // Command callers receive the same initialization failure when they
      // await readiness; this observer prevents a standalone rejected
      // readiness promise from becoming an unhandled process rejection.
    })
    this.#handoff =
      config.replayMode || config.lastSeenFile === null
        ? null
        : new SequenceHandoff(config.lastSeenFile, files)
  }

  bind(app: RuntimeApp): void {
    if (this.#app !== null && this.#app !== app) {
      throw new EngineRuntimeError("engine runtime is already bound to an application")
    }
    this.#app = app
    // A cursor is meaningful only together with the reducer projection that
    // produced it. A freshly spawned TUI has an empty projection, so importing
    // the supervisor's cursor here would permanently omit earlier transcript
    // events. Reconnects in this process still use app.state.lastSequence.
  }

  async start(): Promise<void> {
    this.#requiredApp()
    if (this.#started) {
      throw new EngineRuntimeError("engine runtime has already started")
    }
    this.#started = true
    this.#apply(transportConnecting(0))

    try {
      await this.#activateSession(this.#config.sessionId, false)
      this.#resolveReady()
      await this.#subscription
    } catch (error) {
      this.#rejectReady(error)
      if (!this.#controller.signal.aborted) {
        this.#apply(transportDisconnected(0, safeErrorMessage(error)))
        throw error
      }
    } finally {
      await this.#handoff?.close()
    await Promise.all([this.#controls.settle(), this.#metadata.settle(), this.#children.settle()])
    }
  }

  async sendCommand(command: ClientCommand): Promise<CommandOutcome | null> {
    try {
      await this.#ready
      if (!this.#driverReady || this.#subscriptionController === null) {
        return null
      }
      if (this.#config.replayMode && !isReplayReadOnlyCommand(command)) {
        return null
      }
      const generation = this.#sessionGeneration
      const sessionId = commandSessionId(command)
      if (sessionId !== null && sessionId !== this.#sessionId) {
        return null
      }
      let dispatched = command
      const fork = command.type === "fork"
      if (fork) {
        await this.#forkOperations?.prepare(
          command.session_id,
          command.at_turn ?? null,
          command.operation_id,
        )
        dispatched = command
        this.#forkRequests.set(command.meta.request_id, command.session_id)
      }
      const outcome = await this.#postCommand(
        dispatched,
        fork ? this.#controller.signal : this.#subscriptionController.signal,
      )
      if (fork) {
        if (
          outcome?.type === "rejected" &&
          ["host_protocol_failure", "session_not_loaded", "invalid_fork_operation_id"].includes(
            outcome.error.code,
          )
        ) {
          await this.#forkOperations?.complete(command.session_id)
          this.#forkRequests.delete(command.meta.request_id)
        }
        // A correlated SessionForked may switch sessions and abort the old
        // subscription before this POST returns. The fork transaction itself
        // remains authoritative across that transition.
        return outcome
      }
      return generation === this.#sessionGeneration && this.#driverReady ? outcome : null
    } catch (error) {
      if (!this.#controller.signal.aborted) {
        this.#apply(
          transportDisconnected(
            this.#requiredApp().state.connection.attempt,
            safeErrorMessage(error),
          ),
        )
      }
      return null
    }
  }

  async submitProviderApiKey(
    provider: string,
    apiKey: string,
  ): Promise<{
    readonly stored: true
    readonly activated: boolean
    readonly warnings: readonly string[]
  }> {
    await this.#ready
    if (!this.#driverReady || this.#subscriptionController === null) {
      throw new EngineRuntimeError("the session driver is not ready")
    }
    if (this.#config.replayMode || this.#client.submitProviderApiKey === undefined) {
      throw new EngineRuntimeError("provider credential submission is unavailable")
    }
    return await this.#client.submitProviderApiKey(
      this.#sessionId,
      provider,
      apiKey,
      this.#subscriptionController.signal,
    )
  }

  async activateProvider(provider: string): Promise<void> {
    await this.#ready
    if (
      !this.#driverReady ||
      this.#subscriptionController === null ||
      this.#client.activateProvider === undefined ||
      this.#config.replayMode
    ) {
      throw new EngineRuntimeError("provider activation is unavailable")
    }
    await this.#client.activateProvider(
      this.#sessionId,
      provider,
      this.#subscriptionController.signal,
    )
  }

  async stop(): Promise<void> {
    if (!this.#controller.signal.aborted) {
      this.#controller.abort(new DOMException("TUI engine runtime stopped", "AbortError"))
    }
    this.#driverReady = false
    this.#transitionController?.abort(this.#controller.signal.reason)
    this.#subscriptionController?.abort(this.#controller.signal.reason)
    await this.#handoff?.close()
    await Promise.all([this.#controls.settle(), this.#metadata.settle(), this.#children.settle()])
  }

  /**
   * Ask the authenticated host to shut down before the renderer releases its
   * process. This deliberately does not wait for session-driver readiness:
   * closing Rottweiler must also work while the initial session is still
   * opening. The independent deadline preserves the supervisor's process-exit
   * fallback when the transport is unavailable.
   */
  async shutdownHost(timeoutMs = HOST_SHUTDOWN_TIMEOUT_MS): Promise<boolean> {
    if (this.#config.replayMode || this.#controller.signal.aborted) return false
    const controller = new AbortController()
    const timer = setTimeout(
      () => controller.abort(new DOMException("host shutdown timed out", "TimeoutError")),
      timeoutMs,
    )
    try {
      const outcome = await this.#postCommand(
        { type: "shutdown_host", meta: this.#meta() },
        controller.signal,
      )
      return outcome?.type === "accepted"
    } catch {
      return false
    } finally {
      clearTimeout(timer)
    }
  }

  /**
   * Move the live client to another durable session without allowing a command
   * built for the previous session to cross the transition.
   */
  async switchSession(sessionId: string): Promise<boolean> {
    if (sessionId.length === 0) {
      return false
    }
    if (sessionId === this.#sessionId && this.#driverReady) {
      return true
    }
    try {
      await this.#activateSession(sessionId, true)
      return true
    } catch (error) {
      if (isAbortError(error) || this.#controller.signal.aborted) {
        return false
      }
      this.#apply(
        transportDisconnected(
          this.#requiredApp().state.connection.attempt,
          `session switch failed: ${safeErrorMessage(error)}`,
        ),
      )
      return false
    }
  }

  async #activateSession(sessionId: string, resetProjection: boolean): Promise<void> {
    const generation = ++this.#sessionGeneration
    this.#driverReady = false
    this.#recoveringSequenceGap = false
    this.#transitionController?.abort(
      new DOMException("session transition superseded", "AbortError"),
    )
    this.#subscriptionController?.abort(
      new DOMException("session subscription replaced", "AbortError"),
    )
    const previousSubscription = this.#subscription
    this.#subscription = null
    if (previousSubscription !== null) {
      await previousSubscription.catch(() => {})
    }
    if (generation !== this.#sessionGeneration) {
      throw new DOMException("session transition superseded", "AbortError")
    }

    if (resetProjection) {
      const initial = this.#config.replayMode
        ? enterReplayMode(createInitialState(), sessionId)
        : createInitialState()
      this.#requiredApp().setState(reduceRottweilerState(initial, transportConnecting(0)))
    }

    const transition = new AbortController()
    this.#transitionController = transition
    const abortTransition = () => transition.abort(this.#controller.signal.reason)
    this.#controller.signal.addEventListener("abort", abortTransition, {
      once: true,
    })
    try {
      // Historical replay must never run recovery or take a driver lease: both
      // can append events or update session/index state. The observer attach
      // below is the only session operation in replay mode.
      if (!this.#config.replayMode) {
        await this.#prepareSession(sessionId, transition.signal)
      }
      if (generation !== this.#sessionGeneration || transition.signal.aborted) {
        throw (
          transition.signal.reason ??
          new DOMException("session transition superseded", "AbortError")
        )
      }

      const subscriptionController = new AbortController()
      const abortSubscription = () => subscriptionController.abort(this.#controller.signal.reason)
      this.#controller.signal.addEventListener("abort", abortSubscription, {
        once: true,
      })
      this.#subscriptionController = subscriptionController
      this.#sessionId = sessionId
      const boundForBatch = this.#requiredApp()
      let initialReplayBatch = true
      const finishInitialReplayBatch = () => {
        if (!initialReplayBatch) return
        initialReplayBatch = false
        boundForBatch.endInitialReplayBatch?.()
      }
      const restartInitialReplayBatch = () => {
        if (initialReplayBatch) return
        initialReplayBatch = true
        boundForBatch.beginInitialReplayBatch?.()
      }
      boundForBatch.beginInitialReplayBatch?.()

      let subscriptionReady = false
      let resolveSubscriptionReady!: () => void
      let rejectSubscriptionReady!: (error: unknown) => void
      const ready = new Promise<void>((resolve, reject) => {
        resolveSubscriptionReady = () => {
          subscriptionReady = true
          resolve()
        }
        rejectSubscriptionReady = reject
      })
      const abortReady = () =>
        rejectSubscriptionReady(
          subscriptionController.signal.reason ??
            new DOMException("session transition superseded", "AbortError"),
        )
      subscriptionController.signal.addEventListener("abort", abortReady, {
        once: true,
      })

      const subscription = this.#client
        .subscribe({
          attach: {
            type: "attach_session",
            meta: this.#meta(),
            session_id: sessionId,
            last_seen_sequence: null,
            role: this.#config.replayMode ? "observer" : "driver",
          },
          signal: subscriptionController.signal,
          getLastSeenSequence: () => this.#requiredApp().state.lastSequence,
          requestId: this.#requestId,
          onReconnect: async () => {
            if (this.#config.replayMode) {
              return
            }
            const takeover = await this.#postCommand(
              {
                type: "take_driver",
                meta: this.#meta(),
                session_id: sessionId,
              },
              subscriptionController.signal,
            )
            if (takeover?.type === "rejected") {
              throw new EngineRuntimeError(
                `engine rejected reconnect driver takeover: ${takeover.error.message}`,
              )
            }
          },
          onConnection: (update) => {
            if (generation === this.#sessionGeneration) {
              this.#onConnection(update)
            }
            if (update.phase === "connected") {
              const reconnectReady = subscriptionReady
              resolveSubscriptionReady()
              if (reconnectReady && !this.#config.replayMode) {
                this.#requiredApp().resetConnectionProjections?.()
                void this.#requestInitialProjections(
                  sessionId,
                  subscriptionController.signal,
                )
              }
            }
          },
          onReplayCursorAhead: () => {
            if (generation !== this.#sessionGeneration) return
            const bound = this.#requiredApp()
            restartInitialReplayBatch()
            bound.resetConnectionProjections?.()
            const initial = this.#config.replayMode
              ? enterReplayMode(createInitialState(), sessionId)
              : createInitialState()
            bound.setState(
              reduceRottweilerState(
                initial,
                transportReconnecting(bound.state.connection.attempt),
              ),
            )
            this.#recoveringSequenceGap = false
          },
          onEvent: (event) => {
            if (
              generation !== this.#sessionGeneration ||
              !eventBelongsToSession(event, sessionId)
            ) {
              return
            }
            const bound = this.#requiredApp()
            if (
              event.type === "session_forked" &&
              this.#forkRequests.get(event.meta.request_id) === event.parent_session_id
            ) {
              this.#forkRequests.delete(event.meta.request_id)
              void this.#forkOperations?.complete(event.parent_session_id).catch(() => {
                // Leaving the stable handoff in place is fail-safe: a later
                // retry replays the same durable child instead of duplicating it.
              })
            }
            const previousGap = bound.state.connection.gap
            const previousSequence = bound.state.lastSequence
            bound.handleEvent(event)
            if (event.type === "conversation_rewound") void this.#children.refresh(sessionId, subscriptionController.signal)
            if (!this.#config.replayMode && bound.state.recovery.compaction?.stale === true) {
              void this.#metadata.refresh(sessionId, subscriptionController.signal)
            }
            if (event.type === "session_replay_completed" || event.type === "session_history_ready") finishInitialReplayBatch()
            const nextGap = bound.state.connection.gap
            if (nextGap === null) {
              this.#recoveringSequenceGap = false
            } else if (previousGap === null) {
              this.#recoveringSequenceGap = this.#client.restartStream("immediate")
            } else if (
              this.#recoveringSequenceGap &&
              bound.state.lastSequence === previousSequence &&
              durableSequenceId(event) === nextGap.received
            ) {
              this.#client.restartStream("backoff")
            }
            if (bound.state.lastSequence !== null) {
              this.#handoff?.record(bound.state.lastSequence)
            }
          },
        })
        .catch((error: unknown) => {
          if (!subscriptionReady) rejectSubscriptionReady(error)
          if (generation === this.#sessionGeneration && !subscriptionController.signal.aborted) {
            this.#apply(
              transportDisconnected(
                this.#requiredApp().state.connection.attempt,
                safeErrorMessage(error),
              ),
            )
          }
        })
        .finally(() => {
          finishInitialReplayBatch()
          if (!subscriptionReady && !subscriptionController.signal.aborted) {
            rejectSubscriptionReady(
              new EngineRuntimeError("engine event stream closed before becoming ready"),
            )
          }
        })
      this.#subscription = subscription

      // Projection replies are connection-scoped events. Do not expose driver
      // readiness or request initial state until the SSE stream can receive
      // those replies; otherwise a slow first GET loses models/status/context.
      await ready
      subscriptionController.signal.removeEventListener("abort", abortReady)
      if (generation !== this.#sessionGeneration || subscriptionController.signal.aborted) {
        throw (
          subscriptionController.signal.reason ??
          new DOMException("session transition superseded", "AbortError")
        )
      }
      this.#requiredApp().setSessionId(sessionId)
      this.#driverReady = true
      this.#onDriverReady?.(sessionId)
      if (!this.#config.replayMode) {
        // These projections populate secondary UI surfaces. Live model
        // discovery is deliberately requested only when its picker opens, and
        // no secondary projection may hold the writable driver lease—or every
        // composer submission—hostage.
        void this.#requestInitialProjections(sessionId, subscriptionController.signal)
      }
    } finally {
      this.#controller.signal.removeEventListener("abort", abortTransition)
      if (this.#transitionController === transition) {
        this.#transitionController = null
      }
    }
  }

  async #prepareSession(sessionId: string, signal: AbortSignal): Promise<void> {
    let delay = SESSION_PREPARE_INITIAL_DELAY_MS
    let attempt = 0
    while (!signal.aborted) {
      const resume = await this.#postCommand(
        {
          type: "resume_session",
          meta: this.#meta(),
          session_id: sessionId,
          last_seen_sequence: null,
          role: "observer",
        },
        signal,
      )
      if (resume.type === "accepted") {
        if (this.#config.replayMode) {
          return
        }
        const takeover = await this.#postCommand(
          {
            type: "take_driver",
            meta: this.#meta(),
            session_id: sessionId,
          },
          signal,
        )
        if (takeover.type === "accepted") {
          return
        }
        if (!isTransientSessionPreparationRejection(takeover)) {
          throw new EngineRuntimeError(`engine rejected driver takeover: ${takeover.error.message}`)
        }
      } else if (!isTransientSessionPreparationRejection(resume)) {
        throw new EngineRuntimeError(`engine rejected session resume: ${resume.error.message}`)
      }

      attempt = Math.min(attempt + 1, Number.MAX_SAFE_INTEGER)
      this.#onConnection({ phase: "reconnecting", attempt })
      await this.#sleep(delay, signal)
      delay = Math.min(delay * 2, SESSION_PREPARE_MAXIMUM_DELAY_MS)
    }
    throw signal.reason ?? new DOMException("session preparation aborted", "AbortError")
  }

  async #requestInitialProjections(sessionId: string, signal: AbortSignal): Promise<void> {
    void this.#controls.refresh(sessionId, signal)
    void this.#metadata.refresh(sessionId, signal)
    void this.#children.refresh(sessionId, signal)
    const commands: ClientCommand[] = [
      { type: "list_models", refresh: false, meta: this.#meta(), session_id: sessionId },
      { type: "list_modes", meta: this.#meta(), session_id: sessionId },
      { type: "list_sessions", meta: this.#meta() },
      { type: "get_context", meta: this.#meta(), session_id: sessionId },
      { type: "get_cost", meta: this.#meta(), session_id: sessionId },
      {
        type: "get_workspace_status",
        meta: this.#meta(),
        session_id: sessionId,
      },
      { type: "list_settings", meta: this.#meta(), session_id: sessionId },
      { type: "list_mcp_servers", meta: this.#meta(), session_id: sessionId },
      { type: "list_runtime_services", meta: this.#meta(), session_id: sessionId },
      { type: "list_permissions", meta: this.#meta(), session_id: sessionId },
      { type: "list_commands", meta: this.#meta(), session_id: sessionId },
    ]
    for (const command of commands) {
      if (signal.aborted || sessionId !== this.#sessionId) {
        return
      }
      try {
        await this.#postCommand(command, signal)
      } catch (error) {
        if (signal.aborted || isAbortError(error)) {
          return
        }
        // These read projections are opportunistic. Their individual command
        // acknowledgements carry actionable failures when the engine is live;
        // a missing panel must not discard the composer draft or driver lease.
      }
    }
  }

  async #postCommand(command: ClientCommand, signal?: AbortSignal): Promise<CommandOutcome> {
    const generation = this.#sessionGeneration
    const lifetime = signal === undefined ? this.#controller.signal : AbortSignal.any([signal, this.#controller.signal])
    lifetime.throwIfAborted()
    const reply = await this.#client.postCommand(command, lifetime)
    if (reply.type === "read" && generation === this.#sessionGeneration && !lifetime.aborted) {
      for (const event of reply.events) this.#requiredApp().handleEvent(event)
    }
    return reply.outcome
  }

  async #readSession(
    command: Extract<ClientCommand, { type: "read_session_children" | "read_transcript_tail" | "read_transcript" | "read_transcript_content" | "get_todos" | "get_ui_catalog" | "get_ui_panels" }>,
    signal: AbortSignal,
    allocation?: ReplyAllocation,
  ): Promise<Extract<CommandReply, { type: "read" }>> {
    await this.#ready
    if (!this.#driverReady || this.#subscriptionController === null) {
      throw new EngineRuntimeError("session read connection is unavailable")
    }
    const generation = this.#sessionGeneration
    const lifetime = AbortSignal.any([signal, this.#subscriptionController.signal])
    lifetime.throwIfAborted()
    const reply = await this.#client.postCommand(command, lifetime, allocation)
    lifetime.throwIfAborted()
    if (generation !== this.#sessionGeneration) throw new DOMException("session changed", "AbortError")
    if (reply.type !== "read") throw new EngineRuntimeError("session read has no typed reply")
    if (reply.outcome.type === "rejected") throw new EngineRuntimeError(reply.outcome.error.message)
    return reply
  }

  #meta() {
    return {
      protocol_version: PROTOCOL_VERSION,
      client_id: "tui-runtime",
      request_id: this.#requestId(),
    }
  }

  #onConnection(update: TransportConnectionUpdate): void {
    switch (update.phase) {
      case "connecting":
        this.#apply(transportConnecting(update.attempt))
        break
      case "reconnecting":
        this.#apply(transportReconnecting(update.attempt))
        break
      case "connected":
        this.#apply(transportConnected(update.attempt))
        break
      case "disconnected":
        this.#apply(transportDisconnected(update.attempt, update.error))
        break
      case "closed":
        this.#apply(transportClosed())
        break
    }
  }

  #apply(action: RottweilerAction): void {
    const app = this.#requiredApp()
    app.setState(reduceRottweilerState(app.state, action))
  }

  #requiredApp(): RuntimeApp {
    if (this.#app === null) {
      throw new EngineRuntimeError("engine runtime is not bound to an application")
    }
    return this.#app
  }
}

export async function createEngineRuntimeFromEnvironment(
  options: CreateEngineRuntimeOptions = {},
): Promise<TuiEngineRuntime | null> {
  const files = options.files ?? systemRuntimeFiles
  const environment = options.environment ?? process.env
  const config = await loadEngineRuntimeConfigWithHandoffRetry(
    environment,
    files,
    options.sleep ?? abortableSleep,
  )
  if (config === null) {
    return null
  }
  const client =
    options.client ??
    new EngineHttpSseClient({
      diagnostics: options.diagnostics,
      socketPath: config.socketPath,
      bootstrapToken: async () => {
        const tokenFile = nonEmpty(environment.ROTTWEILER_ENGINE_TOKEN_FILE)
        if (tokenFile === null) {
          throw new EngineRuntimeError("engine bootstrap token file is not configured")
        }
        return await readBootstrapToken(tokenFile, files)
      },
      ...(options.fetch === undefined ? {} : { fetch: options.fetch }),
    })
  return new TuiEngineRuntime(
    config,
    client,
    files,
    options.requestId,
    options.sleep,
    options.onDriverReady,
  )
}

async function loadEngineRuntimeConfigWithHandoffRetry(
  environment: EngineRuntimeEnvironment,
  files: RuntimeFileSystem,
  sleep: RuntimeSleep,
): Promise<EngineRuntimeConfig | null> {
  let delay = SESSION_PREPARE_INITIAL_DELAY_MS
  const signal = new AbortController().signal
  for (let attempt = 0; attempt < SESSION_PREPARE_ATTEMPTS; attempt += 1) {
    try {
      return await loadEngineRuntimeConfig(environment, files)
    } catch (error) {
      if (
        !(error instanceof EngineRuntimeError) ||
        error.message !== "engine bootstrap token file is missing or empty" ||
        attempt + 1 === SESSION_PREPARE_ATTEMPTS
      ) {
        throw error
      }
      await sleep(delay, signal)
      delay = Math.min(delay * 2, SESSION_PREPARE_MAXIMUM_DELAY_MS)
    }
  }
  throw new EngineRuntimeError("engine bootstrap token handoff did not become ready")
}

function isTransientSessionPreparationRejection(outcome: CommandOutcome): boolean {
  return (
    outcome.type === "rejected" &&
    (outcome.error.code === "session_not_loaded" ||
      outcome.error.code === "session_requires_recovery")
  )
}

async function abortableSleep(delayMs: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) {
    throw signal.reason
  }
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(resolve, delayMs)
    signal.addEventListener(
      "abort",
      () => {
        clearTimeout(timer)
        reject(signal.reason)
      },
      { once: true },
    )
  })
}

export async function loadEngineRuntimeConfig(
  environment: EngineRuntimeEnvironment,
  files: RuntimeFileSystem = systemRuntimeFiles,
): Promise<EngineRuntimeConfig | null> {
  const socketPath = nonEmpty(environment.ROTTWEILER_ENGINE_SOCKET)
  const tokenFile = nonEmpty(environment.ROTTWEILER_ENGINE_TOKEN_FILE)
  const sessionId = nonEmpty(environment.ROTTWEILER_SESSION_ID)
  const lastSeenFile = nonEmpty(environment.ROTTWEILER_LAST_SEEN_FILE)
  const forkOperationDirectory = nonEmpty(environment.ROTTWEILER_FORK_OPERATION_DIRECTORY)
  const replayMode = replayModeFromEnvironment(environment.ROTTWEILER_REPLAY_MODE)
  const lastSeenFromEnvironment = optionalSequence(
    environment.ROTTWEILER_LAST_SEEN_SEQUENCE,
    "ROTTWEILER_LAST_SEEN_SEQUENCE",
  )

  const configured =
    [socketPath, tokenFile, sessionId, lastSeenFile].some((value) => value !== null) ||
    lastSeenFromEnvironment !== null
  if (!configured) {
    return null
  }
  if (socketPath === null || tokenFile === null || sessionId === null) {
    throw new EngineRuntimeError(
      "engine runtime requires ROTTWEILER_ENGINE_SOCKET, ROTTWEILER_ENGINE_TOKEN_FILE, and ROTTWEILER_SESSION_ID",
    )
  }
  if (forkOperationDirectory !== null && !isAbsolute(forkOperationDirectory)) {
    throw new EngineRuntimeError("ROTTWEILER_FORK_OPERATION_DIRECTORY must be absolute")
  }

  const token = await readBootstrapToken(tokenFile, files)

  const lastSeenFromFile =
    lastSeenFile === null
      ? null
      : optionalSequence(
          (await files.readText(lastSeenFile, CURSOR_FILE_LIMIT))?.trim(),
          "ROTTWEILER_LAST_SEEN_FILE",
        )

  return {
    socketPath,
    bootstrapToken: token,
    sessionId,
    lastSeenSequence: newestSequence(lastSeenFromEnvironment, lastSeenFromFile),
    lastSeenFile,
    replayMode,
    forkOperationDirectory,
  }
}

function replayModeFromEnvironment(value: string | undefined): boolean {
  if (value === undefined || value === "" || value === "0") {
    return false
  }
  if (value === "1") {
    return true
  }
  throw new EngineRuntimeError("ROTTWEILER_REPLAY_MODE must be 0 or 1")
}

async function readBootstrapToken(tokenFile: string, files: RuntimeFileSystem): Promise<string> {
  const token = (await files.readText(tokenFile, TOKEN_FILE_LIMIT))?.trim() ?? ""
  if (token.length === 0) {
    throw new EngineRuntimeError("engine bootstrap token file is missing or empty")
  }
  return token
}

class SequenceHandoff {
  readonly #path: string
  readonly #files: RuntimeFileSystem
  #pending: string | null = null
  #written: string | null = null
  #flush: Promise<void> | null = null

  constructor(path: string, files: RuntimeFileSystem) {
    this.#path = path
    this.#files = files
  }

  record(sequence: string): void {
    if (parseSequence(sequence) === null) {
      return
    }
    this.#pending = newestSequence(this.#pending, sequence)
    this.#startFlush()
  }

  async close(): Promise<void> {
    while (this.#flush !== null) {
      await this.#flush
    }
  }

  #startFlush(): void {
    if (this.#flush !== null) {
      return
    }
    this.#flush = this.#drain()
      .catch(() => {
        // Cursor persistence must never crash or stall the render/event loop.
        // Reconnect still uses the last in-memory cursor when this optional
        // supervisor handoff cannot be written.
      })
      .finally(() => {
        this.#flush = null
        if (this.#pending !== null && this.#pending !== this.#written) {
          this.#startFlush()
        }
      })
  }

  async #drain(): Promise<void> {
    while (this.#pending !== null && this.#pending !== this.#written) {
      const next = this.#pending
      this.#pending = null
      await this.#files.writePrivateTextAtomic(this.#path, `${next}\n`)
      this.#written = next
    }
  }
}

interface PersistedForkOperation {
  readonly version: 1
  readonly session_id: string
  readonly at_turn: string | null
  readonly operation_id: string
}

class ForkOperationHandoff {
  readonly #directory: string
  readonly #files: RuntimeFileSystem

  constructor(directory: string, files: RuntimeFileSystem) {
    this.#directory = directory
    this.#files = files
  }

  async prepare(sessionId: string, atTurn: string | null, operationId: string): Promise<string> {
    const path = this.#path(sessionId)
    const existing = (await this.#files.readText(path, FORK_OPERATION_FILE_LIMIT))?.trim()
    if (existing !== undefined && existing !== null && existing.length > 0) {
      const operation = parseForkOperation(existing)
      if (operation.session_id !== sessionId || operation.at_turn !== atTurn) {
        throw new EngineRuntimeError(
          "another fork operation is pending for this session; retry its original boundary",
        )
      }
      if (operation.operation_id !== operationId) {
        throw new EngineRuntimeError(
          "another fork operation is pending for this session; retry its original boundary",
        )
      }
      return operation.operation_id
    }
    const operation: PersistedForkOperation = {
      version: 1,
      session_id: sessionId,
      at_turn: atTurn,
      operation_id: operationId,
    }
    await this.#files.writePrivateTextAtomic(path, `${JSON.stringify(operation)}\n`)
    return operation.operation_id
  }

  async complete(sessionId: string): Promise<void> {
    await this.#files.writePrivateTextAtomic(this.#path(sessionId), "")
  }

  #path(sessionId: string): string {
    if (sessionId.length === 0 || sessionId.length > 128 || !/^[A-Za-z0-9._-]+$/.test(sessionId)) {
      throw new EngineRuntimeError("fork session id is unsafe for durable handoff")
    }
    return join(this.#directory, `${sessionId}.json`)
  }
}

function parseForkOperation(value: string): PersistedForkOperation {
  let parsed: unknown
  try {
    parsed = JSON.parse(value)
  } catch {
    throw new EngineRuntimeError("pending fork operation handoff is corrupt")
  }
  if (
    !isRecord(parsed) ||
    parsed.version !== 1 ||
    typeof parsed.session_id !== "string" ||
    !(parsed.at_turn === null || typeof parsed.at_turn === "string") ||
    typeof parsed.operation_id !== "string" ||
    parsed.operation_id.length === 0 ||
    parsed.operation_id.length > 128 ||
    !/^[A-Za-z0-9._-]+$/.test(parsed.operation_id)
  ) {
    throw new EngineRuntimeError("pending fork operation handoff is corrupt")
  }
  return parsed as unknown as PersistedForkOperation
}

function optionalSequence(value: string | undefined, source: string): string | null {
  const normalized = nonEmpty(value)
  if (normalized === null) {
    return null
  }
  if (parseSequence(normalized) === null) {
    throw new EngineRuntimeError(`${source} must contain a decimal u64 sequence`)
  }
  return normalized
}

function parseSequence(value: string | null): bigint | null {
  if (value === null || !/^(0|[1-9][0-9]*)$/.test(value)) {
    return null
  }
  const parsed = BigInt(value)
  return parsed <= MAX_U64 ? parsed : null
}

function newestSequence(left: string | null, right: string | null): string | null {
  const leftValue = parseSequence(left)
  const rightValue = parseSequence(right)
  if (leftValue === null) {
    return rightValue === null ? null : right
  }
  if (rightValue === null || leftValue >= rightValue) {
    return left
  }
  return right
}

function nonEmpty(value: string | undefined): string | null {
  if (value === undefined || value.length === 0) {
    return null
  }
  return value
}

function safeErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "engine runtime failed"
}

function commandSessionId(command: ClientCommand): string | null {
  return "session_id" in command ? command.session_id : null
}

function isReplayReadOnlyCommand(command: ClientCommand): boolean {
  return CLIENT_COMMAND_EXECUTION[command.type] === "read"
}

function eventBelongsToSession(event: EngineEvent, sessionId: string): boolean {
  if ("meta" in event && isRecord(event.meta) && "session_id" in event.meta && typeof event.meta.session_id === "string") {
    return event.meta.session_id === sessionId
  }
  if (event.type === "subagent_progress") {
    return event.parent_session_id === sessionId
  }
  return (
    !("session_id" in event) || event.session_id === undefined || event.session_id === sessionId
  )
}

function isAbortError(error: unknown): boolean {
  return (
    (error instanceof DOMException && error.name === "AbortError") ||
    (isRecord(error) && error.name === "AbortError")
  )
}

function hasErrorCode(error: unknown, code: string): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    (error as { readonly code?: unknown }).code === code
  )
}

function assertOwnerPrivate(mode: number, uid: number, label: string): void {
  if ((mode & 0o077) !== 0) {
    throw new EngineRuntimeError(`${label} must not be accessible by group or other users`)
  }
  const effectiveUid = process.geteuid?.()
  if (effectiveUid !== undefined && uid !== effectiveUid) {
    throw new EngineRuntimeError(`${label} is not owned by the current user`)
  }
}
