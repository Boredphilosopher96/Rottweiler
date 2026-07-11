import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, readFile, rm, stat } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { PROTOCOL_VERSION, type ClientCommand, type CommandOutcome } from "../src/protocol"
import {
  EngineRuntimeError,
  TuiEngineRuntime,
  createEngineRuntimeFromEnvironment,
  loadEngineRuntimeConfig,
  systemRuntimeFiles,
  type RuntimeApp,
  type RuntimeEngineClient,
  type RuntimeFileSystem,
} from "../src/runtime"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
import type { EngineSubscriptionOptions, WireEngineEvent } from "../src/transport"

class MemoryFiles implements RuntimeFileSystem {
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

class TestApp implements RuntimeApp {
  state = createInitialState()
  sessionId = ""
  readonly connectionPhases: string[] = []

  handleEvent(event: WireEngineEvent): void {
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

class ScriptedClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  subscription: EngineSubscriptionOptions | null = null
  readonly outcomes: CommandOutcome[]

  constructor(outcomes: CommandOutcome[] = []) {
    this.outcomes = [...outcomes]
  }

  async postCommand(command: ClientCommand): Promise<CommandOutcome> {
    this.commands.push(command)
    return this.outcomes.shift() ?? { type: "accepted" }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    this.subscription = options
    options.onConnection?.({ phase: "reconnecting", attempt: 2 })
    options.onConnection?.({ phase: "connected", attempt: 2 })
    await options.onEvent({
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

class BlockingPreparationClient implements RuntimeEngineClient {
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

  async postCommand(command: ClientCommand): Promise<CommandOutcome> {
    this.commands.push(command)
    if (command.type === "resume_session") {
      this.#markResumeStarted()
      await this.#resumeGate
    }
    return { type: "accepted" }
  }

  async subscribe(_options: EngineSubscriptionOptions): Promise<void> {}
}

class SwitchingClient implements RuntimeEngineClient {
  readonly commands: ClientCommand[] = []
  readonly subscriptions: EngineSubscriptionOptions[] = []
  readonly blockedResumes = new Map<string, () => void>()
  readonly rejectedSessions = new Set<string>()

  async postCommand(command: ClientCommand, signal?: AbortSignal): Promise<CommandOutcome> {
    this.commands.push(command)
    if (command.type === "resume_session" && this.rejectedSessions.has(command.session_id)) {
      return {
        type: "rejected",
        error: {
          category: "protocol",
          code: "session_not_found",
          message: "the selected session does not exist",
          retryable: false,
        },
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
    return { type: "accepted" }
  }

  async subscribe(options: EngineSubscriptionOptions): Promise<void> {
    this.subscriptions.push(options)
    await new Promise<void>((resolve) => {
      if (options.signal.aborted) {
        resolve()
      } else {
        options.signal.addEventListener("abort", () => resolve(), { once: true })
      }
    })
  }

  blockResume(sessionId: string): void {
    this.blockedResumes.set(sessionId, () => {})
  }
}

describe("OpenTUI engine runtime", () => {
  let temporaryDirectory: string | null = null

  afterEach(async () => {
    if (temporaryDirectory !== null) {
      await rm(temporaryDirectory, { recursive: true, force: true })
      temporaryDirectory = null
    }
  })

  test("leaves the visual shell offline when no engine environment is present", async () => {
    const files = new MemoryFiles()
    expect(await loadEngineRuntimeConfig({}, files)).toBeNull()
    expect(await createEngineRuntimeFromEnvironment({ environment: {}, files })).toBeNull()
    expect(files.reads.size).toBe(0)
  })

  test("reads the token once and chooses the newest valid replay cursor", async () => {
    const files = new MemoryFiles()
    files.reads.set("/private/token", "bootstrap-secret\n")
    files.reads.set("/private/cursor", "12\n")

    const config = await loadEngineRuntimeConfig(
      {
        ROTTWEILER_ENGINE_SOCKET: "/private/engine.sock",
        ROTTWEILER_ENGINE_TOKEN_FILE: "/private/token",
        ROTTWEILER_SESSION_ID: "session-runtime",
        ROTTWEILER_LAST_SEEN_SEQUENCE: "9",
        ROTTWEILER_LAST_SEEN_FILE: "/private/cursor",
      },
      files,
    )

    expect(config).toEqual({
      socketPath: "/private/engine.sock",
      bootstrapToken: "bootstrap-secret",
      sessionId: "session-runtime",
      lastSeenSequence: "12",
      lastSeenFile: "/private/cursor",
    })
  })

  test("waits for the supervisor token handoff before constructing the runtime", async () => {
    const files = new MemoryFiles()
    const delays: number[] = []
    const runtime = await createEngineRuntimeFromEnvironment({
      environment: {
        ROTTWEILER_ENGINE_SOCKET: "/private/engine.sock",
        ROTTWEILER_ENGINE_TOKEN_FILE: "/private/token",
        ROTTWEILER_SESSION_ID: "session-runtime",
      },
      files,
      client: new ScriptedClient(),
      sleep: async (delay) => {
        delays.push(delay)
        files.reads.set("/private/token", "bootstrap-after-spawn\n")
      },
    })

    expect(runtime).toBeInstanceOf(TuiEngineRuntime)
    expect(delays).toEqual([10])
  })

  test("rejects partial runtime configuration and malformed cursors", async () => {
    const files = new MemoryFiles()
    expect(
      loadEngineRuntimeConfig({ ROTTWEILER_ENGINE_SOCKET: "/private/engine.sock" }, files),
    ).rejects.toEqual(
      new EngineRuntimeError(
        "engine runtime requires ROTTWEILER_ENGINE_SOCKET, ROTTWEILER_ENGINE_TOKEN_FILE, and ROTTWEILER_SESSION_ID",
      ),
    )
    expect(
      loadEngineRuntimeConfig(
        {
          ROTTWEILER_LAST_SEEN_SEQUENCE: "-1",
        },
        files,
      ),
    ).rejects.toEqual(
      new EngineRuntimeError(
        "ROTTWEILER_LAST_SEEN_SEQUENCE must contain a decimal u64 sequence",
      ),
    )
  })

  test("resumes, attaches, projects connection state, and persists durable progress", async () => {
    const files = new MemoryFiles()
    const client = new ScriptedClient()
    const app = new TestApp()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret-never-rendered",
        sessionId: "session-runtime",
        lastSeenSequence: "4",
        lastSeenFile: "/private/cursor",
      },
      client,
      files,
      () => `request-${client.commands.length + 1}`,
    )

    runtime.bind(app)
    await runtime.start()

    expect(client.commands[0]).toMatchObject({
      type: "resume_session",
      session_id: "session-runtime",
      last_seen_sequence: null,
      role: "observer",
    })
    expect(client.commands[1]).toMatchObject({
      type: "take_driver",
      session_id: "session-runtime",
    })
    expect(client.subscription?.attach).toMatchObject({
      type: "attach_session",
      session_id: "session-runtime",
      last_seen_sequence: null,
      role: "driver",
    })
    expect(app.state.connection.phase).toBe("connected")
    expect(app.state.connection.attempt).toBe(2)
    expect(app.connectionPhases).toContain("reconnecting")
    expect(app.state.mode).toBe("plan")
    expect(app.state.lastSequence).toBe("5")
    expect(files.writes).toEqual([{ path: "/private/cursor", content: "5\n" }])
    expect(JSON.stringify(app.state)).not.toContain("secret-never-rendered")

    await client.subscription?.onReconnect?.()
    expect(client.commands.at(-1)?.type).toBe("take_driver")

    await runtime.sendCommand({
      type: "get_context",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui-client",
        request_id: "ui-request",
      },
      session_id: "session-runtime",
    })
    expect(client.commands.at(-1)?.type).toBe("get_context")
  })

  test("retries bounded session preparation before taking the driver lease", async () => {
    const files = new MemoryFiles()
    const client = new ScriptedClient([
      {
        type: "rejected",
        error: {
          category: "protocol",
          code: "session_not_loaded",
          message: "initial session is still opening",
          retryable: true,
        },
      },
      { type: "accepted" },
      { type: "accepted" },
    ])
    const delays: number[] = []
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-preparing",
        lastSeenSequence: null,
        lastSeenFile: null,
      },
      client,
      files,
      () => `request-${client.commands.length + 1}`,
      async (delay) => {
        delays.push(delay)
      },
    )
    const app = new TestApp()
    runtime.bind(app)
    await runtime.start()

    expect(delays).toEqual([10])
    expect(client.commands.map((command) => command.type)).toEqual([
      "resume_session",
      "resume_session",
      "take_driver",
      "get_context",
      "get_cost",
      "get_workspace_status",
      "list_models",
      "list_commands",
    ])
    expect(client.subscription?.attach.role).toBe("driver")
  })

  test("holds a first-paint submit until driver takeover is complete", async () => {
    const client = new BlockingPreparationClient()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-startup-race",
        lastSeenSequence: null,
        lastSeenFile: null,
      },
      client,
      new MemoryFiles(),
    )
    runtime.bind(new TestApp())
    const starting = runtime.start()
    await client.resumeStarted
    const sending = runtime.sendCommand({
      type: "send_message",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui",
        request_id: "first-paint-submit",
      },
      session_id: "session-startup-race",
      content: "do not reject this race",
      attachments: [],
    })
    await Promise.resolve()
    expect(client.commands.map((command) => command.type)).toEqual(["resume_session"])

    client.releaseResume()
    await starting
    expect(await sending).toEqual({ type: "accepted" })
    expect(client.commands.map((command) => command.type)).toEqual([
      "resume_session",
      "take_driver",
      "get_context",
      "get_cost",
      "get_workspace_status",
      "list_models",
      "list_commands",
      "send_message",
    ])
  })

  test("switches sessions atomically and suppresses old-session commands and events", async () => {
    const client = new SwitchingClient()
    const app = new TestApp()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-old",
        lastSeenSequence: null,
        lastSeenFile: null,
      },
      client,
      new MemoryFiles(),
    )
    runtime.bind(app)
    const starting = runtime.start()
    await waitFor(
      () =>
        client.subscriptions.length === 1 &&
        client.commands.some((command) => command.type === "list_commands"),
    )
    expect(app.sessionId).toBe("session-old")

    client.blockResume("session-middle")
    const middleSwitch = runtime.switchSession("session-middle")
    await waitFor(() =>
      client.commands.some(
        (command) => command.type === "resume_session" && command.session_id === "session-middle",
      ),
    )
    const oldCommand = await runtime.sendCommand({
      type: "send_message",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui",
        request_id: "must-not-cross",
      },
      session_id: "session-old",
      content: "stale",
      attachments: [],
    })
    expect(oldCommand).toBeNull()

    const finalSwitch = runtime.switchSession("session-new")
    expect(await middleSwitch).toBeFalse()
    expect(await finalSwitch).toBeTrue()
    expect(app.sessionId).toBe("session-new")
    expect(app.state.lastSequence).toBeNull()
    expect(client.subscriptions.map((subscription) => subscription.attach.session_id)).toEqual([
      "session-old",
      "session-new",
    ])
    expect(
      client.commands.some(
        (command) => command.type === "send_message" && command.session_id === "session-old",
      ),
    ).toBeFalse()

    await client.subscriptions[0]?.onEvent({
      type: "mode_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-old",
        sequence_id: "1",
        emitted_at: "2026-07-10T00:00:00Z",
      },
      mode: "stale-mode",
    })
    expect(app.state.mode).toBeNull()
    await client.subscriptions[0]?.onEvent({
      type: "subagent_progress",
      parent_session_id: "session-old",
      subagent_id: "stale-child",
      child_session_id: "stale-child-session",
      child_sequence: "1",
      event: { type: "thinking_delta", text: "stale" },
    })
    expect(app.state.subagentOrder).toEqual([])
    await client.subscriptions[1]?.onEvent({
      type: "subagent_spawned",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-new",
        sequence_id: "1",
        emitted_at: "2026-07-10T00:00:00Z",
      },
      subagent_id: "current-child",
      child_session_id: "current-child-session",
      task: "Current child",
    })
    await client.subscriptions[1]?.onEvent({
      type: "subagent_progress",
      parent_session_id: "session-new",
      subagent_id: "current-child",
      child_session_id: "current-child-session",
      child_sequence: "1",
      event: { type: "thinking_delta", text: "current" },
    })
    expect(app.state.subagentOrder).toEqual(["current-child"])
    await runtime.stop()
    await starting
  })

  test("keeps the new projection command-gated when session takeover is rejected", async () => {
    const client = new SwitchingClient()
    client.rejectedSessions.add("session-missing")
    const app = new TestApp()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-old",
        lastSeenSequence: null,
        lastSeenFile: null,
      },
      client,
      new MemoryFiles(),
    )
    runtime.bind(app)
    const starting = runtime.start()
    await waitFor(
      () =>
        client.subscriptions.length === 1 &&
        client.commands.some((command) => command.type === "list_commands"),
    )

    expect(await runtime.switchSession("session-missing")).toBeFalse()
    expect(app.sessionId).toBe("session-old")
    expect(app.state.connection.phase).toBe("disconnected")
    expect(app.state.connection.error).toContain("session switch failed")
    expect(
      await runtime.sendCommand({
        type: "get_context",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "ui",
          request_id: "blocked-after-rejection",
        },
        session_id: "session-old",
      }),
    ).toBeNull()
    expect(
      client.subscriptions.some(
        (subscription) => subscription.attach.session_id === "session-missing",
      ),
    ).toBeFalse()
    await runtime.stop()
    await starting
  })

  test("writes the optional supervisor cursor handoff with mode 0600", async () => {
    temporaryDirectory = await mkdtemp(join(tmpdir(), "rw-tui-runtime-"))
    const cursorPath = join(temporaryDirectory, "last-seen")

    await systemRuntimeFiles.writePrivateTextAtomic(cursorPath, "42\n")

    expect(await readFile(cursorPath, "utf8")).toBe("42\n")
    expect((await stat(cursorPath)).mode & 0o777).toBe(0o600)
  })
})

async function waitFor(predicate: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (predicate()) {
      return
    }
    await new Promise((resolve) => setTimeout(resolve, 1))
  }
  throw new Error("test condition was not reached")
}
