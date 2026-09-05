import { prepareToolDisplay } from "../../src/state/tool-display"
import { createTestRenderer, MockTreeSitterClient, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, spyOn, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand } from "../../src/protocol"
import { createInitialState, type ToolProjection } from "../../src/state"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { emptySessionReader, sessionReaderFor, conversationItem } from "../fixtures/history"
import { toolsAppState } from "./fixtures"

describe("Rottweiler tools-history", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("renders historical events in immutable replay presentation", async () => {
    const setup = await createTestRenderer({ width: 112, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: sessionReaderFor([
        conversationItem(1, "user", "Show the saved result."),
        conversationItem(2, "assistant", "Historical answer rendered through the retained tree."),
      ]),
      treeSitterClient: new MockTreeSitterClient({ autoResolveTimeout: 0 }),
      replaySessionId: "session-historical",
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.handleEvent({
      type: "session_history_ready",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "replay-client",
        request_id: "replay-request",
        emitted_at: "2026-01-01T00:00:01Z",
      },
      session_id: "session-historical",
      through_sequence: "2",
    })
    app.composer.value = "must not be sent"

    expect(await app.composer.submit()).toBeFalse()
    await setup.flush()
    expect(app.composer.visible).toBeFalse()
    expect(app.interactionPanel.visible).toBeFalse()
    expect(app.banner.plainText).toContain("Replay · session-historical · read-only")
    expect(app.banner.plainText).toContain("history available")
    expect(app.transcript.mountedEntryCount).toBe(2)
    expect(setup.captureCharFrame()).toContain("Historical answer rendered")
    expect(commands).toHaveLength(0)
  })

  test("defers hidden Tools binding and preserves its selection and current output on return", async () => {
    const setup = await createTestRenderer({ width: 110, height: 24, useThread: false })
    renderer = setup.renderer
    const initial = toolsAppState()
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: initial })
    renderer.root.add(app)
    await setup.renderOnce()
    const updates = spyOn(app.toolsWorkspace, "update")
    try {
      const original = initial.tools["tools-0"]
      if (original === undefined) throw new Error("fixture tool missing")
      const latest = {
        ...initial, tools: {
          ...initial.tools, "tools-0": {
            ...original, chunks: original.chunks.append({ stream: "stdout", chunk: "\nnewest retained output" }),
          }
        }
      }
      app.setState(latest)
      await setup.renderOnce()
      expect(updates).not.toHaveBeenCalled()
      app.showToolsView()
      await setup.renderOnce()
      expect(updates).toHaveBeenCalledTimes(1)
      const row = app.toolsWorkspace.rowForKey("tool:tools-0")
      expect(row).toBeDefined()
      expect(row?.output.plainText).toContain("newest retained output")
      app.toolsWorkspace.selectNextBlock()
      app.toolsWorkspace.toggleSelectedBlock()
      const selected = app.toolsWorkspace.captureClientState()
      app.showConversationView()
      updates.mockClear()
      app.setState({ ...latest })
      await setup.renderOnce()
      expect(updates).not.toHaveBeenCalled()
      app.showToolsView()
      await setup.renderOnce()
      expect(app.toolsWorkspace.captureClientState()).toEqual(selected)
    } finally { updates.mockRestore() }
  })

  test("projects Tools elapsed labels from an injected presentation clock", async () => {
    const setup = await createTestRenderer({ width: 110, height: 24, useThread: false })
    renderer = setup.renderer
    let nowMs = Date.parse("2026-01-01T12:00:41.000Z")
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: toolsAppState(),
      nowMs: () => nowMs,
    })
    renderer.root.add(app)
    app.showToolsView()
    await setup.renderOnce()

    expect(app.toolsWorkspace.rowForKey("tool:tools-0")?.header.plainText).toContain(
      "live · 00:41",
    )

    nowMs += 1_000
    app.setState({ ...app.state })
    await setup.renderOnce()
    expect(app.toolsWorkspace.rowForKey("tool:tools-0")?.header.plainText).toContain(
      "live · 00:42",
    )
  })

  test("switches mounted conversation and Tools views from the palette without sharing scroll state", async () => {
    const setup = await createTestRenderer({ width: 110, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: sessionReaderFor(Array.from({ length: 24 }, (_, index) => conversationItem(index + 1, "assistant", `Historical response ${index}\nsecond line`))), initialState: toolsAppState() })
    renderer.root.add(app)
    await setup.flush()
    app.transcript.scrollTo(app.transcript.scroller.scrollHeight)
    await setup.flush()
    const transcriptScroll = app.transcript.scroller.scrollTop
    expect(transcriptScroll).toBeGreaterThan(0)

    app.openCommandPicker()
    expect(app.commandPalette.itemIds).toContain("view.tools")
    expect(app.commandPalette.itemIds).toContain("view.conversation")
    app.commandPalette.selectById("view.tools")
    app.commandPalette.activateSelected()
    await setup.flush()

    expect(app.primaryView).toBe("tools")
    expect(app.toolsWorkspace.visible).toBeTrue()
    expect(app.transcript.visible).toBeFalse()
    expect(app.contextPanel.visible).toBeFalse()
    expect(app.toolsElapsedTimerActive).toBeTrue()
    expect(app.toolsWorkspace.activeStatus.plainText).toContain("Esc Esc to interrupt")
    expect(app.toolsWorkspace.queueBlock.plainText).toContain("Next sends when this turn ends")
    expect(app.toolsWorkspace.queueBlock.plainText).toContain("1 later message remains queued")

    app.toolsWorkspace.activityScroller.scrollTo(2)
    await setup.flush()
    const toolsScroll = app.toolsWorkspace.activityScroller.scrollTop
    app.openCommandPicker()
    app.commandPalette.selectById("view.conversation")
    app.commandPalette.activateSelected()
    await setup.flush()
    expect(app.primaryView).toBe("conversation")
    expect(app.transcript.scroller.scrollTop).toBe(transcriptScroll)
    expect(app.toolsElapsedTimerActive).toBeFalse()

    app.openCommandPicker()
    app.commandPalette.selectById("view.tools")
    app.commandPalette.activateSelected()
    await setup.flush()
    expect(app.toolsWorkspace.activityScroller.scrollTop).toBe(toolsScroll)

    setup.mockInput.pressKey("o", { ctrl: true })
    expect(app.picker.visible).toBeTrue()
    expect(app.picker.title).toContain("Modes")
  })

  test("an open live output reader follows completion into canonical content", async () => {
    const setup = await createTestRenderer({ width: 110, height: 24, useThread: false })
    renderer = setup.renderer
    let contentReads = 0
    const app = createRottweilerApp(renderer, {
      initialState: toolsAppState(), sessionId: "session-tools",
      sessionReader: {
        ...emptySessionReader,
        content: async (target, read) => {
          expect(target.sessionId).toBe("session-tools")
          expect(read.source).toEqual({ sequence: "100", selector: { type: "tool_output" } })
          contentReads++
          return { view: read.view, source: read.source, offset: 0, next_offset: null, total_bytes: 21, format: "text", text: "Canonical full output" }
        },
      },
    })
    renderer.root.add(app)
    app.showToolsView()
    await setup.renderOnce()
    expect(app.toolsWorkspace.rowForKey("tool:tools-0")?.openOutput()).toBeTrue()
    expect(app.outputViewer.visible).toBeTrue()
    const original = app.state.tools["tools-0"]!
    app.handleEvent({
      type: "tool_call_finished",
      meta: { protocol_version: PROTOCOL_VERSION, session_id: "session-tools", sequence_id: "100", emitted_at: "2026-01-01T12:01:00Z" },
      turn_id: original.turnId, tool_call_id: original.toolCallId, invocation_id: original.invocationId,
      output: { type: "text", text: "bounded preview" }, presentation: null, is_error: false, call_index: 0,
    })
    await setup.flush()
    expect(contentReads).toBe(1)
    expect(app.outputViewer.visible).toBeTrue()
    expect(setup.captureCharFrame()).toContain("Canonical full output")
    app.setState({ ...app.state })
    await setup.flush()
    expect(contentReads).toBe(1)
  })

  test("keeps approval focus above Tools and preserves existing output viewer lifecycle", async () => {
    const setup = await createTestRenderer({ width: 110, height: 24, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: toolsAppState() })
    renderer.root.add(app)
    app.showToolsView()
    await setup.renderOnce()

    const first = app.toolsWorkspace.rowForKey("tool:tools-0")
    expect(first?.openOutput()).toBeTrue()
    await setup.renderOnce()
    expect(app.outputViewer.visible).toBeTrue()
    app.outputViewer.scroller.scrollTo(2)
    const viewerScroll = app.outputViewer.scroller.scrollTop
    const changed = toolsAppState()
    const original = changed.tools["tools-0"]!
    app.setState({
      ...changed,
      tools: {
        ...changed.tools,
        "tools-0": {
          ...original,
          chunks: original.chunks.append({ stream: "stdout", chunk: "late line\n" }),
        },
      },
    })
    expect(app.outputViewer.scroller.scrollTop).toBe(viewerScroll)

    const withoutViewedTool = { ...app.state.tools }
    delete withoutViewedTool["tools-0"]
    app.setState({ ...app.state, tools: withoutViewedTool })
    expect(app.outputViewer.visible).toBeFalse()

    app.handleEvent({
      type: "tool_approval_needed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tools",
        sequence_id: "90",
        emitted_at: "2026-01-01T12:00:15.000Z",
      },
      turn_id: "turn-tools",
      tool_call_id: "approval-tools",
      invocation_id: "approval-tools",
      name: "edit",
      args: { path: "src/app.ts" },
      capabilities: ["write_filesystem"],
      rationale: "Apply the requested view",
    })
    expect(app.interactionPanel.capturesInput).toBeTrue()
    expect(renderer.currentFocusedRenderable).toBe(app.interactionPanel.select)
    expect(app.primaryView).toBe("tools")
  })

  test("reprojects Tools to the retained turn after conversation rewind", async () => {
    const setup = await createTestRenderer({ width: 110, height: 24, useThread: false })
    renderer = setup.renderer
    const toolForTurn = (turnId: string): ToolProjection => ({
      toolCallId: `tool-${turnId}`,
      invocationId: `tool-${turnId}`,
      turnId,
      name: "read",
      args: { path: `${turnId}.txt` },
      status: "finished",
      capabilities: [],
      rationale: null,
      diff: null,
      chunks: toolOutputBuffer([]),
      display: prepareToolDisplay({ type: "text", text: `turn ${turnId}` }, null, { path: `${turnId}.txt` }, false), source: null,
      isError: false,
      callIndex: 0,
      timing: { kind: "unknown" },
    })
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        turns: Object.fromEntries(["1", "2"].map((turnId) => [turnId, {
          turnId,
          status: "running" as const,
          usage: null,
          cost: null,
          timing: { kind: "unknown" as const },
        }])),
        tools: {
          "tool-1": toolForTurn("1"),
          "tool-2": toolForTurn("2"),
        },
      },
    })
    renderer.root.add(app)
    app.showToolsView()
    await setup.renderOnce()
    expect(app.toolsWorkspace.rowForKey("tool:tool-2")).toBeDefined()
    expect(app.toolsWorkspace.rowForKey("tool:tool-1")).toBeUndefined()

    app.handleEvent({
      type: "conversation_rewound",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        session_id: "session-tools",
        sequence_id: "1",
        emitted_at: "2026-01-01T12:00:00.000Z",
      },
      to_agent_turn: "1",
      operation_id: "rewind-tools-view",
      unrestorable_paths: [],
    })
    await setup.renderOnce()

    expect(app.state.turns["2"]).toBeUndefined()
    expect(app.state.tools["tool-2"]).toBeUndefined()
    expect(app.toolsWorkspace.rowForKey("tool:tool-1")).toBeDefined()
    expect(app.toolsWorkspace.rowForKey("tool:tool-2")).toBeUndefined()
  })

  test("allows read-only Tools inspection in replay without emitting a command or timer", async () => {
    const setup = await createTestRenderer({ width: 110, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const base = toolsAppState()
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...base,
        replay: { active: true, sessionId: "session-tools", completedThrough: "80" },
      },
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
    app.showToolsView()
    await setup.renderOnce()

    expect(app.primaryView).toBe("tools")
    expect(app.toolsElapsedTimerActive).toBeFalse()
    app.toolsWorkspace.selectNextBlock()
    app.toolsWorkspace.toggleSelectedBlock()
    expect(app.toolsWorkspace.openSelectedOutput()).toBeTrue()
    expect(app.outputViewer.visible).toBeTrue()
    expect(commands).toEqual([])
  })
})
