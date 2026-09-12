import { afterEach, describe, expect, test } from "bun:test"
import { rm } from "node:fs/promises"
import { PROTOCOL_VERSION } from "../../src/protocol"
import {
  TuiEngineRuntime
} from "../../src/runtime"
import { MemoryFiles, SwitchingClient, TestApp, waitFor } from "./fixtures"

describe("runtime session-switching", () => {
  let temporaryDirectory: string | null = null
  afterEach(async () => {
    if (temporaryDirectory !== null) {
      await rm(temporaryDirectory, { recursive: true, force: true })
      temporaryDirectory = null
    }
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
        replayMode: false,
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
    }, { admit() {} })
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

    await client.subscriptions[0]?.onEvent({ definition_fingerprint: "fixture",
      type: "mode_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-old",
        sequence_id: "1",
        emitted_at: "2026-07-10T00:00:00Z",
      },
      mode: "stale-mode",
    })
    expect(app.state.mode).toBe("execute")
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
      type: "command_descriptors_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui",
        request_id: "stale-command-catalog",
        emitted_at: "2026-07-10T00:00:00Z",
      },
      session_id: "session-old",
      commands: [{ source: "builtin", name: "stale", description: "wrong session", usage: "" }],
      truncated: false,
    })
    expect(app.state.commands).toEqual([])
    await client.subscriptions[1]?.onEvent({
      type: "command_descriptors_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui",
        request_id: "current-command-catalog",
        emitted_at: "2026-07-10T00:00:00Z",
      },
      session_id: "session-new",
      commands: [{ source: "builtin", name: "current", description: "right session", usage: "" }],
      truncated: false,
    })
    expect(app.state.commands.map((command) => command.name)).toEqual(["current"])
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
        replayMode: false,
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
      }, { admit() {} }),
    ).toBeNull()
    expect(
      client.subscriptions.some(
        (subscription) => subscription.attach.session_id === "session-missing",
      ),
    ).toBeFalse()
    await runtime.stop()
    await starting
  })
})
