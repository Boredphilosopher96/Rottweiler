import { afterEach, describe, expect, test } from "bun:test"
import { rm } from "node:fs/promises"
import { PROTOCOL_VERSION, type ClientCommand } from "../../src/protocol"
import {
  TuiEngineRuntime
} from "../../src/runtime"
import { CorrelatedForkClient, ForkSwitchingApp, MemoryFiles, ScriptedClient, TestApp, waitFor } from "./fixtures"

describe("runtime forks", () => {
  let temporaryDirectory: string | null = null
  afterEach(async () => {
    if (temporaryDirectory !== null) {
      await rm(temporaryDirectory, { recursive: true, force: true })
      temporaryDirectory = null
    }
  })
  test("persists one fork operation across a TUI restart and clears it only on completion", async () => {
    const files = new MemoryFiles()
    const config = {
      socketPath: "/private/engine.sock",
      bootstrapToken: "secret",
      sessionId: "fork-parent",
      lastSeenSequence: null,
      lastSeenFile: null,
      replayMode: false,
      forkOperationDirectory: "/private/pending-forks",
    } as const
    const firstClient = new ScriptedClient()
    const first = new TuiEngineRuntime(config, firstClient, files)
    first.bind(new TestApp())
    await first.start()
    expect(
      await first.sendCommand({
        type: "fork",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "first-client",
          request_id: "first-request",
        },
        session_id: "fork-parent",
        at_turn: "7",
        operation_id: "first-operation",
      }),
    ).toEqual({ type: "accepted" })
    const firstFork = firstClient.commands.find((command) => command.type === "fork")
    expect(firstFork?.type).toBe("fork")
    if (firstFork?.type !== "fork") throw new Error("first fork command missing")
    expect(firstFork.operation_id).toBeString()
    await first.stop()

    const secondClient = new ScriptedClient()
    const second = new TuiEngineRuntime(config, secondClient, files)
    second.bind(new TestApp())
    await second.start()
    expect(
      await second.sendCommand({
        type: "fork",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "second-client",
          request_id: "different-boundary-request",
        },
        session_id: "fork-parent",
        at_turn: "8",
        operation_id: "different-operation",
      }),
    ).toBeNull()
    expect(secondClient.commands.some((command) => command.type === "fork")).toBeFalse()
    expect(
      await second.sendCommand({
        type: "fork",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "second-client",
          request_id: "second-request",
        },
        session_id: "fork-parent",
        at_turn: "7",
        operation_id: "first-operation",
      }),
    ).toEqual({ type: "accepted" })
    const secondFork = secondClient.commands.find((command) => command.type === "fork")
    expect(secondFork?.type).toBe("fork")
    if (secondFork?.type !== "fork") throw new Error("second fork command missing")
    expect(secondFork.operation_id).toBe(firstFork.operation_id)
    await secondClient.subscription?.onEvent({
      type: "session_forked",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "bound-client",
        request_id: "second-request",
        emitted_at: "2026-07-11T00:00:00Z",
      },
      parent_session_id: "fork-parent",
      child: {
        session_id: "fork-child",
        workspace_name: "workspace",
        model: "fast",
        driver_client_id: "bound-client",
        shell_active: false,
      },
      at_turn: "7",
    })
    await Bun.sleep(0)
    expect(files.reads.get("/private/pending-forks/fork-parent.json")).toBe("")
    await second.stop()
  })

  test("keeps a correlated fork POST alive while its own event switches sessions", async () => {
    const files = new MemoryFiles()
    const client = new CorrelatedForkClient()
    const app = new ForkSwitchingApp()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "fork-parent",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
        forkOperationDirectory: "/private/pending-forks",
      },
      client,
      files,
    )
    app.runtime = runtime
    runtime.bind(app)
    const running = runtime.start()
    while (client.subscriptions.length === 0) await Bun.sleep(1)

    const outcome = await runtime.sendCommand({
      type: "fork",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui-client",
        request_id: "fork-request",
      },
      session_id: "fork-parent",
      at_turn: null,
      operation_id: "fork-operation",
    })
    expect(outcome).toEqual({ type: "accepted" })
    expect(client.forkSignalAborted).toBeFalse()
    while (app.sessionId !== "fork-child") await Bun.sleep(1)
    expect(files.reads.get("/private/pending-forks/fork-parent.json")).toBe("")
    await runtime.stop()
    await running
  })

  test("retains the stable fork identity across capacity rejection", async () => {
    const files = new MemoryFiles()
    const client = new ScriptedClient()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/engine.sock",
        bootstrapToken: "secret",
        sessionId: "fork-parent",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
        forkOperationDirectory: "/private/pending-forks",
      },
      client,
      files,
    )
    runtime.bind(new TestApp())
    await runtime.start()
    await waitFor(() => client.commands.some((command) => command.type === "list_commands"))
    client.outcomes.push({
      type: "rejected",
      error: {
        category: "protocol",
        code: "session_capacity",
        message: "retry after another session closes",
        retryable: false,
      },
    })
    const command = (requestId: string): ClientCommand => ({
      type: "fork",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui-client",
        request_id: requestId,
      },
      session_id: "fork-parent",
      at_turn: "3",
      operation_id: "capacity-operation",
    })
    expect(await runtime.sendCommand(command("capacity-request"))).toMatchObject({
      type: "rejected",
      error: { code: "session_capacity" },
    })
    const firstFork = client.commands.find((candidate) => candidate.type === "fork")
    if (firstFork?.type !== "fork") throw new Error("capacity fork command missing")
    expect(files.reads.get("/private/pending-forks/fork-parent.json")).not.toBe("")

    expect(await runtime.sendCommand(command("capacity-retry"))).toEqual({
      type: "accepted",
    })
    const forks = client.commands.filter((candidate) => candidate.type === "fork")
    expect(forks).toHaveLength(2)
    expect(forks[1]?.type === "fork" ? forks[1].operation_id : null).toBe(firstFork.operation_id)
    await runtime.stop()
  })
})
