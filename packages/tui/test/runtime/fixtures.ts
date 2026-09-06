import { ClientCache } from "../../src/history/cache"
import type { HistoryCacheValue } from "../../src/history/controller"
import { BootstrapPresentation } from "../../src/app/bootstrap"
import type { SessionBootstrap } from "../../src/runtime-bootstrap"
import { tailReply, todosReply } from "./snapshot-fixtures"
import type { EngineEvent } from "../../src/protocol"
import { PROTOCOL_VERSION, type ClientCommand, type CommandOutcome, type CommandReply } from "../../src/protocol"
import {
  TuiEngineRuntime,
  type RuntimeApp,
  type RuntimeEngineClient,
  type RuntimeFileSystem
} from "../../src/runtime"
import { createInitialState, engineEvent, reduceRottweilerState } from "../../src/state"
import { type EngineStreamRestartMode, type EngineSubscriptionOptions } from "../../src/transport"

export class MemoryFiles implements RuntimeFileSystem {
  readonly reads = new Map<string, string>()
  readonly writes: Array<{ path: string; content: string }> = []

  async readText(path: string, maximumBytes: number): Promise<string | null> {
    const value = this.reads.get(path) ?? null
    if (value !== null && new TextEncoder().encode(value).byteLength > maximumBytes) {
      throw new Error("test file exceeds limit")
    }
    return value
  }

  async writePrivateTextAtomic(path: string, content: string): Promise<void> {
    this.writes.push({ path, content })
    this.reads.set(path, content)
  }
}

export class TestApp implements RuntimeApp {
  state = createInitialState()
  readonly historyCache = new ClientCache<HistoryCacheValue>()
  readonly bootstrap = new BootstrapPresentation(state => this.setState(state))
  installBootstrap(value: SessionBootstrap): void { this.bootstrap.install(value) }
  sessionId = ""
  readonly connectionPhases: string[] = []
  initialReplayBatchesStarted = 0
  initialReplayBatchesFinished = 0
  connectionProjectionResets = 0

  beginInitialReplayBatch(): void {
    this.initialReplayBatchesStarted += 1
  }

  endInitialReplayBatch(): void {
    this.initialReplayBatchesFinished += 1
  }

  resetConnectionProjections(): void {
    this.connectionProjectionResets += 1
  }

  handleEvent(event: EngineEvent): void {
    this.state = reduceRottweilerState(this.state, engineEvent(event))
  }

  setState(state: ReturnType<typeof createInitialState>): void {
    this.state = state
    this.connectionPhases.push(state.connection.phase)
  }

  setSessionId(sessionId: string): void {
    this.sessionId = sessionId
  }
}

export class ScriptedClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  subscription: EngineSubscriptionOptions | null = null
  readonly outcomes: CommandOutcome[]

  constructor(outcomes: CommandOutcome[] = []) {
    this.outcomes = [...outcomes]
  }

  restartStream(): boolean {
    return false
  }

  async postCommand(command: ClientCommand): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command, "plan")
    return { type: "command", outcome: this.outcomes.shift() ?? { type: "accepted" } }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    this.subscription = options
    options.onConnection?.({ phase: "reconnecting", attempt: 2 })
    options.onConnection?.({ phase: "connected", attempt: 2 })
    await options.onEvent({ definition_fingerprint: "fixture",
      type: "mode_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: options.attach.session_id,
        sequence_id: "5",
        emitted_at: "2026-07-10T00:00:00Z",
      },
      mode: "plan",
    })
  }
}

export class BlockingPreparationClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  readonly resumeStarted: Promise<void>
  readonly #markResumeStarted: () => void
  readonly #resumeGate: Promise<void>
  readonly #releaseResume: () => void

  constructor() {
    let markResumeStarted!: () => void
    let releaseResume!: () => void
    this.resumeStarted = new Promise((resolve) => {
      markResumeStarted = resolve
    })
    this.#resumeGate = new Promise((resolve) => {
      releaseResume = resolve
    })
    this.#markResumeStarted = markResumeStarted
    this.#releaseResume = releaseResume
  }

  releaseResume(): void {
    this.#releaseResume()
  }

  restartStream(): boolean {
    return false
  }

  async postCommand(command: ClientCommand): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command)
    if (command.type === "resume_session") {
      this.#markResumeStarted()
      await this.#resumeGate
    }
    return { type: "command", outcome: { type: "accepted" } }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    options.onConnection?.({ phase: "connected", attempt: 0 })
  }
}

export class SwitchingClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  readonly subscriptions: EngineSubscriptionOptions[] = []
  readonly blockedResumes = new Map<string, () => void>()
  readonly rejectedSessions = new Set<string>()

  restartStream(): boolean {
    return false
  }

  async postCommand(command: ClientCommand, signal?: AbortSignal): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command)
    if (command.type === "resume_session" && this.rejectedSessions.has(command.session_id)) {
      return {
        type: "command", outcome: {
          type: "rejected",
          error: {
            category: "protocol",
            code: "session_not_found",
            message: "the selected session does not exist",
            retryable: false,
          },
        }
      }
    }
    if (command.type === "resume_session" && this.blockedResumes.has(command.session_id)) {
      await new Promise<void>((resolve, reject) => {
        this.blockedResumes.set(command.session_id, resolve)
        signal?.addEventListener(
          "abort",
          () => reject(signal.reason ?? new DOMException("aborted", "AbortError")),
          { once: true },
        )
      })
    }
    return { type: "command", outcome: { type: "accepted" } }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    this.subscriptions.push(options)
    options.onConnection?.({ phase: "connected", attempt: 0 })
    await new Promise<void>((resolve) => {
      if (options.signal.aborted) {
        resolve()
      } else {
        options.signal.addEventListener("abort", () => resolve(), {
          once: true,
        })
      }
    })
  }

  blockResume(sessionId: string): void {
    this.blockedResumes.set(sessionId, () => { })
  }
}

export class DelayedConnectionClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  subscription: EngineSubscriptionOptions | null = null
  readonly connected: Promise<void>
  readonly #markConnected: () => void

  constructor() {
    let markConnected!: () => void
    this.connected = new Promise((resolve) => {
      markConnected = resolve
    })
    this.#markConnected = markConnected
  }

  restartStream(): boolean {
    return false
  }

  async postCommand(command: ClientCommand): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command)
    return { type: "command", outcome: { type: "accepted" } }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    this.subscription = options
    await this.connected
    options.onConnection?.({ phase: "connected", attempt: 0 })
    await new Promise<void>((resolve) => {
      if (options.signal.aborted) resolve()
      else
        options.signal.addEventListener("abort", () => resolve(), {
          once: true,
        })
    })
  }

  connect(): void {
    this.#markConnected()
  }
}

export class ReconnectingProjectionClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  subscription: EngineSubscriptionOptions | null = null

  restartStream(): boolean {
    return false
  }

  async postCommand(command: ClientCommand): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command)
    return { type: "command", outcome: { type: "accepted" } }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    this.subscription = options
    options.onConnection?.({ phase: "connected", attempt: 0 })
    await new Promise<void>((resolve) => {
      if (options.signal.aborted) resolve()
      else options.signal.addEventListener("abort", () => resolve(), { once: true })
    })
  }

  async reconnect(): Promise<void> {
    await this.subscription?.onReconnect?.()
    this.subscription?.onConnection?.({ phase: "connected", attempt: 1 })
  }
}

export class CursorAheadClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []

  restartStream(): boolean {
    return false
  }

  async postCommand(command: ClientCommand): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command)
    return { type: "command", outcome: { type: "accepted" } }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    options.onConnection?.({ phase: "connected", attempt: 0 })
    await options.onEvent({
      type: "session_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "runtime-cursor-ahead",
        request_id: "initial-replay-complete",
        emitted_at: "2026-08-19T00:00:01Z",
      },
      session_id: options.attach.session_id,
      through_sequence: "9",
    })
    await options.onReplayCursorAhead?.()
    options.onConnection?.({ phase: "connected", attempt: 1 })
    await new Promise<void>((resolve) => {
      if (options.signal.aborted) resolve()
      else options.signal.addEventListener("abort", () => resolve(), { once: true })
    })
  }
}

export class BlockingShutdownClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  shutdownAborted = false

  restartStream(): boolean {
    return false
  }

  async postCommand(command: ClientCommand, signal?: AbortSignal): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command)
    await new Promise<void>((resolve) => {
      if (signal?.aborted) resolve()
      else signal?.addEventListener("abort", () => resolve(), { once: true })
    })
    this.shutdownAborted = signal?.aborted ?? false
    throw signal?.reason ?? new Error("shutdown request aborted")
  }

  async subscribe(): Promise<void> { }
}

export class CorrelatedForkClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  readonly subscriptions: EngineSubscriptionOptions[] = []
  forkSignalAborted = false

  restartStream(): boolean {
    return false
  }

  async postCommand(command: ClientCommand, signal?: AbortSignal): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command)
    if (command.type === "fork") {
      const current = this.subscriptions.at(-1)
      await current?.onEvent({
        type: "session_forked",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "bound-client",
          request_id: command.meta.request_id,
          emitted_at: "2026-07-11T00:00:00Z",
        },
        parent_session_id: command.session_id,
        child: { title: "Fixture",
          session_id: "fork-child",
          workspace_name: "workspace",
          model: "fast",
          driver_client_id: "bound-client",
          shell_active: false,
        },
        at_turn: command.at_turn ?? "0",
      })
      await Bun.sleep(0)
      this.forkSignalAborted = signal?.aborted ?? false
    }
    return { type: "command", outcome: { type: "accepted" } }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    this.subscriptions.push(options)
    options.onConnection?.({ phase: "connected", attempt: 0 })
    await new Promise<void>((resolve) => {
      if (options.signal.aborted) resolve()
      else
        options.signal.addEventListener("abort", () => resolve(), {
          once: true,
        })
    })
  }
}

export class RestartRecordingClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  readonly restarts: EngineStreamRestartMode[] = []
  subscription: EngineSubscriptionOptions | null = null

  async postCommand(command: ClientCommand): Promise<CommandReply> {
    this.commands.push(command)
    if (command.type === "read_transcript_tail") return tailReply(command)
    if (command.type === "get_todos") return todosReply(command)
    if (command.type === "read_session_children") return childrenReply(command)
    if (command.type === "get_session_controls") return controlsReply(command)
    if (command.type === "get_session_state") return stateReply(command)
    return { type: "command", outcome: { type: "accepted" } }
  }

  restartStream(mode: EngineStreamRestartMode = "immediate"): boolean {
    this.restarts.push(mode)
    return true
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    this.subscription = options
    options.onConnection?.({ phase: "connected", attempt: 0 })
    await new Promise<void>((resolve) => {
      if (options.signal.aborted) resolve()
      else options.signal.addEventListener("abort", () => resolve(), { once: true })
    })
  }
}

export class ForkSwitchingApp extends TestApp {
  runtime: TuiEngineRuntime | null = null

  override handleEvent(event: EngineEvent): void {
    super.handleEvent(event)
    if (event.type === "session_forked") {
      void this.runtime?.switchSession(event.child.session_id)
    }
  }
}

export async function waitFor(predicate: () => boolean, timeoutMs = 100): Promise<void> {
  const deadline = performance.now() + timeoutMs
  while (performance.now() < deadline) {
    if (predicate()) {
      return
    }
    await new Promise((resolve) => setTimeout(resolve, 1))
  }
  throw new Error("test condition was not reached")
}

function controlsReply(command: Extract<ClientCommand, { type: "get_session_controls" }>): CommandReply {
  return { type: "read", outcome: { type: "accepted" }, events: [{ type: "session_controls_ready",
    meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: command.session_id,
    snapshot: { through: "5", controls: { questions: [], approvals: [], pending_plan: null } },
  }] }

}

function stateReply(command: Extract<ClientCommand, { type: "get_session_state" }>, mode = "execute"): CommandReply {
  return { type: "read", outcome: { type: "accepted" }, events: [{ type: "session_state_ready",
    meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: command.session_id,
    snapshot: { through: "5", driver_client_id: command.meta.client_id, title: null, model_alias: "main",
      provider: null, thinking: "off", mode_id: mode, active_turn: null, completed_turns: "0", shell: null,
      compaction: null, plugin_statuses: [], queued_messages: [], budget: null },
  }] }
}

function childrenReply(command: Extract<ClientCommand, { type: "read_session_children" }>): CommandReply {
  return { type: "read", outcome: { type: "accepted" }, events: [{ type: "session_children_ready",
    meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: command.session_id,
    result: { type: "ready", snapshot: { through: null, children: [] } },
  }] }
}
