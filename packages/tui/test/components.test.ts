import { afterEach, describe, expect, test } from "bun:test"
import { CliRenderEvents } from "@opentui/core"
import {
  createTestRenderer,
  MockTreeSitterClient,
  setRendererCapabilities,
  type TestRenderer,
} from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import { ImageAttachmentRenderable, fuzzyScore } from "../src/components"
import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent } from "../src/protocol"
import { createInitialState, type RottweilerState } from "../src/state"
import { kennelTheme } from "../src/theme"

function meta(sequence: string) {
  return {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-components",
    sequence_id: sequence,
    emitted_at: "2026-01-01T00:00:00Z",
  }
}

describe("M4 retained components", () => {
  let renderer: TestRenderer | undefined
  let treeSitter: MockTreeSitterClient | undefined

  afterEach(async () => {
    renderer?.destroy()
    renderer = undefined
    await treeSitter?.destroy()
    treeSitter = undefined
  })

  test("mounts only visible transcript rows and preserves the streaming markdown instance", async () => {
    const setup = await createTestRenderer({ width: 86, height: 24, useThread: false })
    renderer = setup.renderer
    treeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
    treeSitter.setMockResult({ highlights: [] })
    const transcript = Array.from({ length: 10_000 }, (_, index) => ({
      sequenceId: String(index + 1),
      agentTurn: String(index + 1),
      turn: {
        role: "assistant" as const,
        blocks: [{ type: "text" as const, text: `Turn ${index} stayed virtualized.` }],
        meta: { synthetic: false, summary: false },
      },
    }))
    const initial: RottweilerState = {
      ...createInitialState(),
      transcript,
      streamingTail: {
        turnId: "10001",
        text: "first",
        thinking: "",
        citations: [],
        toolCallIds: [],
        finished: null,
      },
    }
    const app = createRottweilerApp(renderer, {
      initialState: initial,
      treeSitterClient: treeSitter,
    })
    renderer.root.add(app)
    await setup.waitFor(() => treeSitter?.isHighlighting() === false)
    await setup.flush()

    expect(app.transcript.mountedEntryCount).toBeLessThan(20)
    const streamingMarkdown = app.transcript.streamingMarkdown
    app.setState({
      ...initial,
      streamingTail: { ...initial.streamingTail!, text: "first second" },
    })
    await setup.renderOnce()
    expect(app.transcript.streamingMarkdown).toBe(streamingMarkdown)
    expect(app.transcript.mountedEntryCount).toBeLessThan(20)

    app.transcript.setScrollOffset(5_000_000)
    await setup.flush()
    expect(app.transcript.mountedEntryCount).toBeLessThan(20)
    expect(app.transcript.mountedKeys.at(-1)).not.toContain(":0:")
  })

  test("routes diff approval and context surgery through generated commands", async () => {
    const setup = await createTestRenderer({ width: 112, height: 30, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const state: RottweilerState = {
      ...createInitialState(),
      tools: {
        edit: {
          toolCallId: "edit",
          turnId: "1",
          name: "edit",
          args: { path: "src/main.rs" },
          status: "awaiting_approval",
          capabilities: ["write_filesystem"],
          rationale: "Apply change",
          diff: {
            proposal_id: "proposal-hash",
            path: "src/main.rs",
            unified_diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n",
            arguments_hash: "arguments-hash",
            base_hash: "base-hash",
            diff_hash: "diff-hash",
            truncated: false,
          },
          chunks: [],
          output: null,
          isError: null,
          callIndex: 0,
        },
      },
      context: {
        turn_id: "1",
        stable_prefix_hash: "hash",
        used_tokens: "1",
        usable_tokens: "10",
        reserved_tokens: "1",
        cache_breakpoints: [],
        items: [
          {
            item_id: "context-1",
            kind: "conversation",
            label: "Turn one",
            source: "session",
            machine_local_path: null,
            estimated_tokens: "10",
            state: { pinned: false, evicted: false, summarized: false, pruned: false },
          },
        ],
      },
    }
    const app = createRottweilerApp(renderer, {
      initialState: state,
      sessionId: "session-components",
      clientId: "client-components",
      requestId: () => "request-components",
      onCommand(command) {
        commands.push(command)
      },
    })
    renderer.root.add(app)
    await setup.renderOnce()
    app.interactionPanel.select.selectCurrent()
    app.contextPanel.items.focus()
    setup.mockInput.pressKey("p")

    expect(commands).toContainEqual({
      type: "approve_tool",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-components",
        request_id: "request-components",
      },
      session_id: "session-components",
      tool_call_id: "edit",
      decision: "allow_once",
      binding: {
        proposal_id: "proposal-hash",
        arguments_hash: "arguments-hash",
        base_hash: "base-hash",
        diff_hash: "diff-hash",
      },
    })
    expect(commands).toContainEqual({
      type: "pin_context",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "client-components",
        request_id: "request-components",
      },
      session_id: "session-components",
      item_id: "context-1",
    })

    commands.length = 0
    app.setState({
      ...state,
      tools: {
        edit: {
          ...state.tools.edit!,
          diff: { ...state.tools.edit!.diff!, truncated: true },
        },
      },
    })
    await setup.renderOnce()
    expect(app.interactionPanel.select.options.map((option) => option.value)).toEqual(["deny"])
    app.interactionPanel.select.selectCurrent()
    expect(commands).toContainEqual(
      expect.objectContaining({
        type: "approve_tool",
        tool_call_id: "edit",
        decision: "deny",
      }),
    )
  })

  test("notifies only while terminal focus is away", async () => {
    const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
    renderer = setup.renderer
    const notifications: string[] = []
    const app = createRottweilerApp(renderer, {
      notifications: {
        notify(notification) {
          notifications.push(notification.kind)
        },
      },
    })
    renderer.root.add(app)
    renderer.emit(CliRenderEvents.BLUR)
    const events: EngineEvent[] = [
      { type: "turn_started", meta: meta("1"), turn_id: "1" },
      {
        type: "turn_finished",
        meta: meta("2"),
        turn_id: "1",
        status: "completed",
        usage: {
          input_tokens: "1",
          output_tokens: "1",
          cache_read_tokens: "0",
          cache_write_tokens: "0",
          reasoning_tokens: "0",
        },
        cost: { kind: "monetary", amount_micros: "1", currency: "USD" },
      },
    ]
    for (const event of events) {
      app.handleEvent(event)
    }
    renderer.emit(CliRenderEvents.FOCUS)
    app.handleEvent({ type: "turn_started", meta: meta("3"), turn_id: "2" })
    app.handleEvent({
      type: "turn_finished",
      meta: meta("4"),
      turn_id: "2",
      status: "completed",
      usage: events[1]!.type === "turn_finished" ? events[1]!.usage : neverUsage(),
      cost:
        events[1]!.type === "turn_finished"
          ? events[1]!.cost
          : { kind: "unavailable", reason: "fixture" },
    })

    expect(notifications).toEqual(["turn_finished"])
  })

  test("fuzzy matching is ordered and image fallback is capability gated", async () => {
    expect(fuzzyScore("ctx", "context inspect")).toBeGreaterThan(
      fuzzyScore("ctx", "long command text x") ?? -1,
    )
    expect(fuzzyScore("zzz", "context inspect")).toBeNull()

    const setup = await createTestRenderer({ width: 50, height: 8, useThread: false })
    renderer = setup.renderer
    setRendererCapabilities(renderer, { kitty_graphics: false, sixel: false })
    const image = new ImageAttachmentRenderable(renderer, kennelTheme, {
      name: "screen.png",
      media_type: "image/png",
      data: { type: "inline_base64", data: "AA==" },
    })
    renderer.root.add(image)
    await setup.renderOnce()
    expect(image.height).toBe(2)
    expect(setup.captureCharFrame()).toContain("screen.png")
  })
})

function neverUsage() {
  return {
    input_tokens: "0",
    output_tokens: "0",
    cache_read_tokens: "0",
    cache_write_tokens: "0",
    reasoning_tokens: "0",
  }
}
