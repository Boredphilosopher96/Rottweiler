import { afterEach, describe, expect, test } from "bun:test"
import { rm } from "node:fs/promises"
import { PROTOCOL_VERSION, type ClientCommand } from "../../src/protocol"
import {
  TuiEngineRuntime,
  type RuntimeEngineClient
} from "../../src/runtime"
import { BlockingPreparationClient, BlockingShutdownClient, DelayedConnectionClient, MemoryFiles, ScriptedClient, TestApp, waitFor } from "./fixtures"

describe("runtime lifecycle", () => {
  let temporaryDirectory: string | null = null
  afterEach(async () => {
    if (temporaryDirectory !== null) {
      await rm(temporaryDirectory, { recursive: true, force: true })
      temporaryDirectory = null
    }
  })
  test("requests typed host shutdown before stopping and keeps an unavailable host bounded", async () => {
    const config = {
      socketPath: "/private/engine.sock",
      bootstrapToken: "bootstrap-secret",
      sessionId: "session-runtime",
      lastSeenSequence: null,
      lastSeenFile: null,
      replayMode: false,
    }
    const acceptedClient = new ScriptedClient()
    const acceptedRuntime = new TuiEngineRuntime(config, acceptedClient)

    expect(await acceptedRuntime.shutdownHost()).toBeTrue()
    expect(acceptedClient.commands).toEqual([
      {
        type: "shutdown_host",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "tui-runtime",
          request_id: expect.any(String),
        },
      },
    ])
    await acceptedRuntime.stop()

    const blockedClient = new BlockingShutdownClient()
    const blockedRuntime = new TuiEngineRuntime(config, blockedClient)
    expect(await blockedRuntime.shutdownHost(5)).toBeFalse()
    expect(blockedClient.commands[0]?.type).toBe("shutdown_host")
    expect(blockedClient.shutdownAborted).toBeTrue()
    await blockedRuntime.stop()
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
        replayMode: false,
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
    await waitFor(() => client.commands.some((command) => command.type === "list_commands"))

    expect(delays).toEqual([10])
    expect(client.commands.map((command) => command.type)).toEqual([
      "resume_session",
      "resume_session",
      "take_driver",
      "list_models",
      "get_session_controls",
      "get_session_state",
      "read_session_children",
      "list_modes",
      "list_sessions",
      "get_context",
      "get_cost",
      "get_workspace_status",
      "list_settings",
      "list_mcp_servers",
      "list_runtime_services",
      "list_permissions",
      "list_commands",
    ])
    expect(client.subscription?.attach.role).toBe("driver")
  })

  test("waits for checkpoint recovery before exposing a writable driver", async () => {
    const client = new ScriptedClient([
      {
        type: "rejected",
        error: {
          category: "protocol",
          code: "session_requires_recovery",
          message: "session is fail-closed until checkpoint journal recovery completes",
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
        sessionId: "session-recovering",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
      new MemoryFiles(),
      () => `request-${client.commands.length + 1}`,
      async (delay) => {
        delays.push(delay)
      },
    )
    runtime.bind(new TestApp())
    await runtime.start()
    await waitFor(() => client.commands.some((command) => command.type === "list_commands"))

    expect(delays).toEqual([10])
    expect(client.commands.map((command) => command.type)).toEqual([
      "resume_session",
      "resume_session",
      "take_driver",
      "list_models",
      "get_session_controls",
      "get_session_state",
      "read_session_children",
      "list_modes",
      "list_sessions",
      "get_context",
      "get_cost",
      "get_workspace_status",
      "list_settings",
      "list_mcp_servers",
      "list_runtime_services",
      "list_permissions",
      "list_commands",
    ])
  })

  test("fails permanent session persistence preparation instead of retrying forever", async () => {
    const client = new ScriptedClient([
      {
        type: "rejected",
        error: {
          category: "internal",
          code: "host_persistence_failure",
          message: "session metadata is corrupt",
          retryable: false,
        },
      },
    ])
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-corrupt",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
      new MemoryFiles(),
    )
    const app = new TestApp()
    runtime.bind(app)

    await expect(runtime.start()).rejects.toThrow("session metadata is corrupt")
    expect(client.commands).toHaveLength(1)
    expect(app.state.connection.phase).toBe("disconnected")
  })

  test("keeps genuinely opening sessions retryable until runtime shutdown", async () => {
    const commands: ClientCommand[] = []
    const client: RuntimeEngineClient = {
      restartStream() {
        return false
      },
      async postCommand(command) {
        commands.push(command)
        return {
          type: "command", outcome: {
            type: "rejected",
            error: {
              category: "protocol",
              code: "session_not_loaded",
              message: "session is still opening",
              retryable: true,
            },
          }
        }
      },
      async subscribe() {
        throw new Error("subscription must not start before preparation")
      },
    }
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-opening",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
      new MemoryFiles(),
    )
    runtime.bind(new TestApp())
    const starting = runtime.start()
    await waitFor(() => commands.length >= 2)
    await runtime.stop()
    await starting
    expect(commands.every((command) => command.type === "resume_session")).toBeTrue()
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
        replayMode: false,
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
    await waitFor(() => client.commands.some((command) => command.type === "list_commands"))
    const commandTypes = client.commands.map((command) => command.type)
    expect(commandTypes[0]).toBe("resume_session")
    expect(commandTypes.indexOf("take_driver")).toBeLessThan(commandTypes.indexOf("send_message"))
    for (const expected of [
      "list_sessions",
      "get_context",
      "get_cost",
      "get_workspace_status",
      "list_settings",
      "list_mcp_servers",
      "list_runtime_services",
      "list_permissions",
      "list_commands",
      "send_message",
    ] satisfies ClientCommand["type"][]) expect(commandTypes).toContain(expected)
  })

  test("requests the cached model catalog without blocking startup submissions", async () => {
    const client = new ScriptedClient()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-slow-catalog",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
      new MemoryFiles(),
    )
    runtime.bind(new TestApp())

    await runtime.start()
    await waitFor(() => client.commands.some((command) => command.type === "list_commands"))
    expect(client.commands).toContainEqual({
      type: "list_models",
      refresh: false,
      meta: expect.objectContaining({
        protocol_version: PROTOCOL_VERSION,
        client_id: "tui-runtime",
      }),
      session_id: "session-slow-catalog",
    })
    expect(
      await runtime.sendCommand({
        type: "send_message",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "ui",
          request_id: "send-during-catalog",
        },
        session_id: "session-slow-catalog",
        content: "stay responsive",
        attachments: [],
      }),
    ).toEqual({ type: "accepted" })
    expect(client.commands.at(-1)?.type).toBe("send_message")
    await runtime.stop()
  })

  test("waits for the event stream before requesting connection-scoped projections", async () => {
    const client = new DelayedConnectionClient()
    const readySessions: string[] = []
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-delayed-events",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
      new MemoryFiles(),
      undefined,
      undefined,
      (sessionId) => readySessions.push(sessionId),
    )
    runtime.bind(new TestApp())

    const starting = runtime.start()
    await waitFor(() => client.subscription !== null)
    expect(client.commands.map((command) => command.type)).toEqual([
      "resume_session",
      "take_driver",
    ])
    expect(readySessions).toEqual([])

    client.connect()
    await waitFor(() => readySessions.length === 1)
    expect(readySessions).toEqual(["session-delayed-events"])
    expect(client.commands.map((command) => command.type)).toEqual([
      "resume_session",
      "take_driver",
      "list_models",
      "get_session_controls",
      "get_session_state",
      "read_session_children",
      "list_modes",
      "list_sessions",
      "get_context",
      "get_cost",
      "get_workspace_status",
      "list_settings",
      "list_mcp_servers",
      "list_runtime_services",
      "list_permissions",
      "list_commands",
    ])
    await runtime.stop()
    await starting
  })
})
