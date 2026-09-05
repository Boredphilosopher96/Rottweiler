import { todoState } from "../fixtures/todos"
import { deferPresentationForEvent } from "../../src/presentation"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, EngineEvent } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptyHistoryReader } from "../fixtures/history"
import { initialEvent, ManualPresentationFrame } from "./fixtures"

describe("Rottweiler presentation", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("refreshes live runtime services around tool execution", async () => {
    const setup = await createTestRenderer({ width: 100, height: 18, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "session-services",
      requestId: () => `request-${commands.length + 1}`,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.handleEvent({
      type: "tool_call_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-services",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      turn_id: "turn-services",
      tool_call_id: "tool-services",
      invocation_id: "tool-services",
      name: "edit",
      args: { path: "src/lib.rs" },
      call_index: 0,
    })
    expect(commands.at(-1)).toMatchObject({ type: "list_runtime_services" })

    app.handleEvent({
      type: "tool_call_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-services",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      turn_id: "turn-services",
      tool_call_id: "tool-services",
      invocation_id: "tool-services",
      output: { type: "text", text: "done" },
      is_error: false,
      call_index: 0,
    })
    expect(commands.filter((command) => command.type === "list_runtime_services")).toHaveLength(2)
  })

  test("clears stale runtime services when the final activity refresh fails", async () => {
    const setup = await createTestRenderer({ width: 100, height: 18, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      sessionId: "session-services",
      initialState: {
        ...createInitialState(),
        runtimeServices: [{ kind: "formatter", name: "rustfmt" }],
      },
      onCommand(command) {
        if (command.type !== "list_runtime_services") return { type: "accepted" }
        return {
          type: "rejected",
          error: {
            category: "protocol",
            code: "services_unavailable",
            message: "service probe failed",
            retryable: true,
          },
        }
      },
    })
    renderer.root.add(app)

    app.handleEvent({
      type: "tool_call_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-services",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      turn_id: "turn-services",
      tool_call_id: "tool-services",
      invocation_id: "tool-services",
      name: "edit",
      args: { path: "src/lib.rs" },
      call_index: 0,
    })
    app.handleEvent({
      type: "tool_call_finished",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-services",
        sequence_id: "2",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      turn_id: "turn-services",
      tool_call_id: "tool-services",
      invocation_id: "tool-services",
      output: { type: "text", text: "done" },
      is_error: false,
      call_index: 0,
    })
    await Bun.sleep(0)

    expect(app.state.runtimeServices).toEqual([])
    expect(app.state.errors.at(-1)?.message).toContain("service probe failed")
  })

  test("renders into OpenTUI's inspectable in-memory cell buffer", async () => {
    const setup = await createTestRenderer({
      width: 72,
      height: 12,
      useThread: false,
    })
    renderer = setup.renderer
    renderer.root.add(createRottweilerApp(renderer, { historyReader: emptyHistoryReader, initialEvent }))

    await setup.renderOnce()

    const frame = setup.captureCharFrame()
    expect(frame).toContain("rottweiler")
    expect(frame).toContain("hello")
    expect(frame).toContain("model not selected · Alt+M")

    const cells = setup.captureSpans()
    expect(cells.cols).toBe(72)
    expect(cells.rows).toBe(12)
    expect(cells.lines).toHaveLength(12)
  })

  test("presents an intentional ready state without an empty context sidebar", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
      },
    })
    renderer.root.add(app)

    await setup.flush()

    const frame = setup.captureCharFrame()
    expect(frame).toContain("rottweiler")
    expect(frame).toContain("Describe a task, or press / for commands.")
    expect(frame).toContain("model not selected · Alt+M")
    expect(frame).not.toContain("No tasks")
    expect(frame).not.toContain("No changed files")
    expect(app.contextPanel.visible).toBeFalse()

    app.setState({
      ...app.state,
      todos: todoState([{ id: "first-task", content: "Inspect the workspace", status: "pending" }]),
    })
    await setup.flush()
    expect(app.contextPanel.visible).toBeTrue()
  })

  test("coalesces hundreds of ordered presentation deltas into one frame without losing protocol progress", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const frame = new ManualPresentationFrame()
    const app = createRottweilerApp(renderer, { historyReader: emptyHistoryReader, presentationFrame: frame })
    renderer.root.add(app)
    app.handleEvent({
      type: "turn_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      turn_id: "turn-stream",
    })
    let presentationUpdates = 0
    const update = app.transcript.update.bind(app.transcript)
    app.transcript.update = (state) => {
      presentationUpdates += 1
      update(state)
    }

    for (let index = 0; index < 300; index += 1) {
      const sequence = String(index + 2)
      const meta = {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: sequence,
        emitted_at: "2026-01-01T00:00:00Z",
      } as const
      if (index % 3 === 0) {
        app.handleEvent({ type: "text_delta", meta, turn_id: "turn-stream", text: "t" })
      } else if (index % 3 === 1) {
        app.handleEvent({ type: "thinking_delta", meta, turn_id: "turn-stream", text: "r" })
      } else {
        app.handleEvent({
          type: "citation_delta",
          meta,
          turn_id: "turn-stream",
          uri: `https://example.test/${index}`,
        })
      }
    }

    expect(frame.scheduled).toBe(1)
    expect(frame.delays).toEqual([16])
    expect(frame.callbacks.size).toBe(1)
    expect(presentationUpdates).toBe(0)
    expect(app.state.lastSequence).toBe("301")
    expect(app.state.streamingTail?.text).toHaveLength(100)
    expect(app.state.streamingTail?.thinking).toHaveLength(100)
    expect(app.state.streamingTail?.citations).toHaveLength(100)

    frame.flush()

    expect(presentationUpdates).toBe(1)
    expect(frame.callbacks.size).toBe(0)
    expect(app.transcript.streamingMarkdown.content).toHaveLength(100)
  })

  test("defers high-volume projections but presents interactive boundaries immediately", () => {
    for (const type of [
      "text_delta",
      "thinking_delta",
      "citation_delta",
      "tool_output_delta",
      "subagent_progress",
      "context_usage_updated",
    ] as const) {
      expect(deferPresentationForEvent({ type }), type).toBeTrue()
    }
    for (const type of [
      "tool_approval_needed",
      "question_asked",
      "user_shell_state_changed",
      "conversation_rewound",
      "turn_finished",
      "host_shutdown",
    ] as const) {
      expect(deferPresentationForEvent({ type }), type).toBeFalse()
    }
  })

  test("coalesces compaction text and thinking into one presentation frame", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    const frame = new ManualPresentationFrame()
    const app = createRottweilerApp(renderer, { historyReader: emptyHistoryReader, presentationFrame: frame })
    renderer.root.add(app)
    app.handleEvent({
      type: "compaction_started",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-local",
        sequence_id: "1",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      reason: "automatic",
    })
    app.handleEvent({
      type: "compaction_attempt_started",
      session_id: "session-local",
      summary_turn_id: "7",
      attempt: 0,
    })

    for (let index = 0; index < 200; index += 1) {
      app.handleEvent({
        type: index % 2 === 0 ? "compaction_text_delta" : "compaction_thinking_delta",
        session_id: "session-local",
        summary_turn_id: "7",
        attempt: 0,
        text: "x",
      })
    }

    expect(frame.scheduled).toBe(1)
    expect(frame.callbacks.size).toBe(1)
    frame.flush()
    expect(app.state.compaction?.text).toHaveLength(100)
    expect(app.state.compaction?.thinking).toHaveLength(100)
  })

  test("flushes queued stream content immediately before permission, question, and finish events", async () => {
    const terminalEvents: EngineEvent[] = [
      {
        type: "tool_approval_needed",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "3",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
        tool_call_id: "tool-1",
        invocation_id: "tool-1",
        name: "bash",
        args: { command: "pwd" },
        capabilities: ["execute"],
        rationale: "inspect the workspace",
      },
      {
        type: "question_asked",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "3",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
        question_id: "question-1",
        questions: [{
          id: "question-1",
          prompt: "Continue?",
          response_kind: "select_one",
          options: [{ value: "yes", label: "Yes" }],
        }],
      },
      {
        type: "turn_finished",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "3",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
        status: "completed",
        usage: {
          input_tokens: "1",
          output_tokens: "1",
          cache_read_tokens: "0",
          cache_write_tokens: "0",
          reasoning_tokens: "0",
        },
        cost: { kind: "unavailable", reason: "fixture" },
      },
    ]

    for (const terminalEvent of terminalEvents) {
      const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
      renderer = setup.renderer
      const frame = new ManualPresentationFrame()
      const app = createRottweilerApp(renderer, { historyReader: emptyHistoryReader, presentationFrame: frame, onCommand: () => ({ type: "accepted" }) })
      renderer.root.add(app)
      app.handleEvent({
        type: "turn_started",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "1",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
      })
      let presentationUpdates = 0
      const update = app.transcript.update.bind(app.transcript)
      app.transcript.update = (state) => {
        presentationUpdates += 1
        update(state)
      }
      app.handleEvent({
        type: "text_delta",
        meta: {
          protocol_version: PROTOCOL_VERSION,
          session_id: "session-local",
          sequence_id: "2",
          emitted_at: "2026-01-01T00:00:00Z",
        },
        turn_id: "turn-terminal",
        text: "ready",
      })

      app.handleEvent(terminalEvent)

      expect(presentationUpdates).toBe(1)
      expect(frame.callbacks.size).toBe(0)
      expect(app.state.lastSequence).toBe("3")
      expect(app.transcript.streamingMarkdown.content).toBe("ready")
      if (terminalEvent.type === "tool_approval_needed") expect(app.interactionPanel.visible).toBeTrue()
      if (terminalEvent.type === "question_asked") expect(app.interactionPanel.visible).toBeTrue()
      if (terminalEvent.type === "turn_finished") expect(app.state.turns["turn-terminal"]?.status).toBe("completed")
      renderer.destroy()
      renderer = undefined
    }
  })
})
