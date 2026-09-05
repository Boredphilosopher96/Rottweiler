import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, readFile, rm, stat } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { PROTOCOL_VERSION } from "../../src/protocol"
import {
  TuiEngineRuntime,
  systemRuntimeFiles
} from "../../src/runtime"
import { CursorAheadClient, MemoryFiles, ReconnectingProjectionClient, RestartRecordingClient, ScriptedClient, TestApp, waitFor } from "./fixtures"

describe("runtime subscriptions", () => {
  let temporaryDirectory: string | null = null
  afterEach(async () => {
    if (temporaryDirectory !== null) {
      await rm(temporaryDirectory, { recursive: true, force: true })
      temporaryDirectory = null
    }
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
        replayMode: false,
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
    expect(app.initialReplayBatchesStarted).toBe(1)
    expect(app.initialReplayBatchesFinished).toBe(1)
    expect(files.writes).toEqual([{ path: "/private/cursor", content: "5\n" }])
    expect(JSON.stringify(app.state)).not.toContain("secret-never-rendered")

    await waitFor(() => client.commands.some((command) => command.type === "list_commands"))
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

  test("restarts gap recovery immediately and backs off when the replay is also gapped", async () => {
    const client = new RestartRecordingClient()
    const app = new TestApp()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-runtime",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
      new MemoryFiles(),
    )
    runtime.bind(app)
    const running = runtime.start()
    await waitFor(() => client.subscription !== null)
    const emit = async (sequence: string, mode: "plan" | "default") => {
      await client.subscription?.onEvent({ definition_fingerprint: "fixture",
        type: "mode_changed",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-runtime",
          sequence_id: sequence,
          emitted_at: "2026-07-17T00:00:00Z",
        },
        mode,
      })
    }

    await emit("1", "plan")
    await emit("3", "default")
    expect(app.state.lastSequence).toBe("1")
    expect(app.state.connection.gap).toEqual({ expected: "2", received: "3" })
    expect(client.restarts).toEqual(["immediate"])

    await emit("3", "default")
    expect(client.restarts).toEqual(["immediate", "backoff"])

    await emit("2", "default")
    await emit("3", "default")
    await emit("4", "plan")
    expect(app.state.lastSequence).toBe("4")
    expect(app.state.connection.gap).toBeNull()
    expect(app.state.connection.phase).toBe("connected")

    await runtime.stop()
    await running
  })

  test("replay attaches as an observer without recovery, takeover, or projection writes", async () => {
    const files = new MemoryFiles()
    const client = new ScriptedClient()
    const app = new TestApp()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-replay",
        lastSeenSequence: null,
        lastSeenFile: "/private/replay-cursor",
        replayMode: true,
      },
      client,
      files,
    )
    runtime.bind(app)
    await runtime.start()

    expect(client.commands).toEqual([])
    expect(client.subscription?.attach).toMatchObject({
      type: "attach_session",
      session_id: "session-replay",
      role: "observer",
      last_seen_sequence: null,
    })
    expect(files.writes).toEqual([])

    expect(
      await runtime.sendCommand({
        type: "send_message",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "replay-client",
          request_id: "forbidden",
        },
        session_id: "session-replay",
        content: "do not mutate replay",
        attachments: [],
      }),
    ).toBeNull()
    expect(client.commands).toEqual([])

    expect(await runtime.switchSession("session-replay-two")).toBeTrue()
    expect(client.commands).toEqual([])
    expect(client.subscription?.attach).toMatchObject({
      session_id: "session-replay-two",
      role: "observer",
    })
    expect(app.state.replay).toEqual({
      active: true,
      sessionId: "session-replay-two",
      completedThrough: null,
    })
    expect(files.writes).toEqual([])
  })

  test("refreshes read projections after a ready reconnect without retrying mutations", async () => {
    const client = new ReconnectingProjectionClient()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-reconnect-projections",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
      new MemoryFiles(),
    )
    runtime.bind(new TestApp())

    const running = runtime.start()
    await waitFor(() => client.commands.some((command) => command.type === "list_commands"))
    const beforeReconnect = client.commands.length

    await client.reconnect()
    await waitFor(
      () => client.commands
        .slice(beforeReconnect)
        .filter((command) => command.type === "list_commands").length === 1,
    )

    const reconnectedTypes = client.commands
      .slice(beforeReconnect)
      .map((command) => command.type)
    expect(reconnectedTypes).toEqual([
      "take_driver",
      "list_models",
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
    expect(reconnectedTypes).not.toContain("resume_session")
    expect(reconnectedTypes).not.toContain("send_message")

    await runtime.stop()
    await running
  })

  test("resets the session projection when the durable log rejects its cursor", async () => {
    const client = new CursorAheadClient()
    const app = new TestApp()
    app.handleEvent({ definition_fingerprint: "fixture",
      type: "mode_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-cursor-reset",
        sequence_id: "9",
        emitted_at: "2026-08-19T00:00:00Z",
      },
      mode: "plan",
    })
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "session-cursor-reset",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
      new MemoryFiles(),
    )
    runtime.bind(app)

    const running = runtime.start()
    await waitFor(() => client.commands.some((command) => command.type === "list_commands"))

    expect(app.connectionProjectionResets).toBe(2)
    expect(app.initialReplayBatchesStarted).toBe(2)
    expect(app.initialReplayBatchesFinished).toBe(1)
    expect(app.state.lastSequence).toBeNull()
    expect(app.state.mode).toBe("execute")
    expect(app.state.connection.phase).toBe("connected")

    await runtime.stop()
    await running
  })

  test("writes the optional supervisor cursor handoff with mode 0600", async () => {
    temporaryDirectory = await mkdtemp(join(tmpdir(), "rw-tui-runtime-"))
    const cursorPath = join(temporaryDirectory, "last-seen")

    await systemRuntimeFiles.writePrivateTextAtomic(cursorPath, "42\n")

    expect(await readFile(cursorPath, "utf8")).toBe("42\n")
    expect((await stat(cursorPath)).mode & 0o777).toBe(0o600)
  })
})
