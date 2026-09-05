import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import {
  TuiEngineRuntime,
  type RuntimeEngineClient,
} from "../../src/runtime"
import type { EngineSubscriptionOptions } from "../../src/transport"
import { emptyHistoryReader, historyReaderFor, waitForHistory, shellItem } from "../fixtures/history"

describe("Rottweiler terminal", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("suspends before requesting !python and resumes only on durable inactive", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const ordering: string[] = []
    const commands: ClientCommand[] = []
    const items: import("../../src/protocol").TranscriptItem[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: historyReaderFor(items),
      sessionId: "session-tui-test",
      terminalHandover: {
        suspend: () => ordering.push("suspend"),
        resume: () => ordering.push("resume"),
      },
      onCommand: (command) => {
        ordering.push("command")
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.composer.value = "!python -q"
    expect(app.composer.shellMode).toBeTrue()
    expect(app.composer.title).toBe(" Shell ")
    expect(app.composer.editor.placeholder).toContain("Shell command")
    expect(await app.composer.submit()).toBeTrue()
    expect(app.composer.shellMode).toBeFalse()
    expect(ordering).toEqual(["suspend", "command"])
    expect(commands).toHaveLength(1)
    expect(commands[0]).toMatchObject({
      type: "user_shell_started",
      session_id: "session-tui-test",
      command: "python -q",
    })

    items[0] = shellItem(1, "python -q")
    app.handleEvent({
      type: "user_shell_state_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tui-test",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      shell_id: "shell-1",
      command: "python -q",
      active: true,
    })
    expect(ordering).toEqual(["suspend", "command"])
    await waitForHistory(setup, () => app.transcript.mountedCards.get("1")?.item.revision === items[0]?.revision)
    expect(app.transcript.mountedCards).toHaveLength(1)
    expect([...app.transcript.mountedCards.values()][0]?.header.plainText).toContain("Terminal · running")
    expect(app.transcript.mountedCards.get("1")?.shellCommand?.plainText).toContain("python -q")

    items[0] = shellItem(1, "python -q", "hello from shell")
    app.handleEvent({
      type: "user_shell_state_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tui-test",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      shell_id: "shell-1",
      command: "python -q",
      active: false,
      status: 0,
      captured_output: "hello from shell",
    })
    expect(ordering).toEqual(["suspend", "command", "resume", "command"])
    expect(commands.at(-1)).toMatchObject({
      type: "get_workspace_status",
      session_id: "session-tui-test",
    })
    await waitForHistory(setup, () => app.transcript.mountedCards.get("1")?.item.revision === items[0]?.revision)
    expect(app.transcript.mountedCards).toHaveLength(1)
    expect([...app.transcript.mountedCards.values()][0]?.header.plainText).toContain("Terminal · done")
    await waitForHistory(setup, () => setup.captureCharFrame().includes("hello from shell"))
  })

  test("reconciles foreground-shell terminal ownership after coalesced replay", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const ordering: string[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      terminalHandover: {
        suspend: () => ordering.push("suspend"),
        resume: () => ordering.push("resume"),
      },
    })
    renderer.root.add(app)

    app.beginInitialReplayBatch()
    app.handleEvent({
      type: "user_shell_state_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tui-test",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      shell_id: "shell-replayed",
      command: "python -q",
      active: true,
    })
    expect(ordering).toEqual([])

    app.endInitialReplayBatch()
    expect(ordering).toEqual(["suspend"])

    app.handleEvent({
      type: "user_shell_state_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tui-test",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      shell_id: "shell-replayed",
      command: "python -q",
      active: false,
      status: 23,
      captured_output: "interrupted",
    })
    expect(ordering).toEqual(["suspend", "resume"])
  })

  test("keeps slow historical replay side effects suppressed until the replay marker", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const transitions: string[] = []
    let releaseReplay!: () => void
    let markHistoricalEventDelivered!: () => void
    const replayGate = new Promise<void>((resolve) => {
      releaseReplay = resolve
    })
    const historicalEventDelivered = new Promise<void>((resolve) => {
      markHistoricalEventDelivered = resolve
    })
    const client: RuntimeEngineClient = {
      async postCommand() {
        return { type: "command", outcome: { type: "accepted" } }
      },
      restartStream() {
        return false
      },
      async subscribe(options: EngineSubscriptionOptions) {
        options.onConnection?.({ phase: "connected", attempt: 0 })
        await Bun.sleep(275)
        await options.onEvent({
          type: "session_forked",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            client_id: "slow-replay-client",
            request_id: "slow-replay-fork",
            emitted_at: "2026-08-19T00:00:00Z",
          },
          parent_session_id: options.attach.session_id,
          child: { title: "Fixture",
            session_id: "historical-child",
            workspace_name: "Historical fork",
            model: "fast",
            driver_client_id: null,
            shell_active: false,
          },
          at_turn: "4",
        })
        markHistoricalEventDelivered()
        await replayGate
        await options.onEvent({
          type: "session_replay_completed",
          meta: {
            protocol_version: PROTOCOL_VERSION,
            client_id: "slow-replay-client",
            request_id: "slow-replay-complete",
            emitted_at: "2026-08-19T00:00:01Z",
          },
          session_id: options.attach.session_id,
          through_sequence: null,
        })
        await new Promise<void>((resolve) => {
          if (options.signal.aborted) resolve()
          else options.signal.addEventListener("abort", () => resolve(), { once: true })
        })
      },
    }
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "slow-replay-parent",
      requestId: () => "slow-replay-fork",
      onCommand: () => ({ type: "accepted" }),
      onSessionSelect(sessionId) {
        transitions.push(sessionId)
      },
    })
    renderer.root.add(app)
    app.composer.value = "/fork 4"
    expect(await app.composer.submit()).toBeTrue()
    const runtime = new TuiEngineRuntime(
      {
        socketPath: "/private/slow-replay.sock",
        bootstrapToken: "secret",
        sessionId: "slow-replay-parent",
        lastSeenSequence: null,
        lastSeenFile: null,
        replayMode: false,
      },
      client,
    )
    runtime.bind(app)

    const running = runtime.start()
    await historicalEventDelivered
    await Bun.sleep(0)
    expect(transitions).toEqual([])

    releaseReplay()
    await Bun.sleep(0)
    expect(transitions).toEqual([])
    await runtime.stop()
    await running
  })

  test("does not hand terminal ownership to a historical replay shell", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const ordering: string[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      replaySessionId: "historical-session",
      terminalHandover: {
        suspend: () => ordering.push("suspend"),
        resume: () => ordering.push("resume"),
      },
    })
    renderer.root.add(app)

    app.beginInitialReplayBatch()
    app.handleEvent({
      type: "user_shell_state_changed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "historical-session",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      shell_id: "historical-shell",
      command: "python -q",
      active: true,
    })
    app.endInitialReplayBatch()

    expect(app.state.replay.active).toBeTrue()
    expect(ordering).toEqual([])
  })
})
