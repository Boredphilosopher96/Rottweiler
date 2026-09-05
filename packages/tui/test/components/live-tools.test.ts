import { DiffRenderable, SyntaxStyle } from "@opentui/core"
import {
  createTestRenderer,
  type TestRenderer
} from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { formatElapsed, ReasoningBlockRenderable, ToolBlockRenderable, toolOutputContent } from "../../src/components"
import { formatToolArguments } from "../../src/render"
import { createInitialState, type RottweilerState } from "../../src/state"
import { toolOutputBuffer } from "../../src/state/display-buffer"
import { createStreamingTail } from "../../src/state/model"
import { kennelTheme } from "../../src/theme"
import { emptySessionReader } from "../fixtures/history"

describe("live-tools components", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("bounds tool arguments and redacts credential-shaped fields", () => {
    const rendered = formatToolArguments({
      path: "approval.txt",
      api_key: "must-not-render",
      content: "x".repeat(1_000),
    }, 120)
    expect(rendered).toContain("approval.txt")
    expect(rendered).toContain("[redacted]")
    expect(rendered).not.toContain("must-not-render")
    expect(rendered.length).toBeLessThanOrEqual(120)
  })

  test("formats elapsed reasoning durations", () => {
    expect(formatElapsed(0)).toBe("briefly")
    expect(formatElapsed(999)).toBe("briefly")
    expect(formatElapsed(12_000)).toBe("12s")
    expect(formatElapsed(83_000)).toBe("1m23s")
  })

  test("freezes a live reasoning duration when streaming ends", async () => {
    const originalNow = Date.now
    let now = 1_000
    Date.now = () => now
    try {
      const setup = await createTestRenderer({ width: 86, height: 16, useThread: false })
      renderer = setup.renderer
      const initial = {
        ...createInitialState(),
        streamingTail: createStreamingTail({
          turnId: "1",
          text: "",
          thinking: "Inspecting the workspace",
          citations: [],
          toolInvocationIds: [],
          finished: null,
        }),
      } satisfies RottweilerState
      const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: initial })
      renderer.root.add(app)
      await setup.renderOnce()
      const reasoning = app.transcript.streamingCard
        .getChildren()
        .find((child): child is ReasoningBlockRenderable => child instanceof ReasoningBlockRenderable)

      now = 13_000
      reasoning?.update(initial.streamingTail.thinking, false, 86)
      expect(reasoning?.header.plainText).toStartWith("reasoning · 12s")
      expect(reasoning?.header.plainText).toEndWith("⌄")
    } finally {
      Date.now = originalNow
    }
  })

  test("shows elapsed time only for running tools after three seconds", async () => {
    const originalNow = Date.now
    let now = 1_000
    Date.now = () => now
    try {
      const tool = {
        toolCallId: "elapsed-tool",
        invocationId: "elapsed-tool",
        turnId: "1",
        name: "read",
        args: { path: "src/main.rs" },
        status: "running" as const,
        capabilities: ["read_filesystem" as const],
        rationale: null,
        diff: null,
        chunks: toolOutputBuffer([]),
        output: null,
        isError: null,
        callIndex: 0,
        timing: { kind: "unknown" as const },
      }
      const setup = await createTestRenderer({ width: 86, height: 16, useThread: false })
      renderer = setup.renderer
      const card = new ToolBlockRenderable(renderer, kennelTheme, tool)
      expect(card.header.plainText).not.toContain("1s")
      const header = card.header.content
      card.update({ ...tool, chunks: toolOutputBuffer([{ stream: "stdout", chunk: "new output" }]) })
      expect(card.header.content).toBe(header)
      now = 5_000
      card.update(tool)
      expect(card.header.plainText).toEndWith(" · 4s")
    } finally {
      Date.now = originalNow
    }
  })

  test("does not repeat a collapsed tool subject in its summary", async () => {
    const setup = await createTestRenderer({ width: 86, height: 16, useThread: false })
    renderer = setup.renderer
    const card = new ToolBlockRenderable(renderer, kennelTheme, {
      toolCallId: "dedupe-tool",
      invocationId: "dedupe-tool",
      turnId: "1",
      name: "custom_tool",
      args: { path: "src/main.rs" },
      status: "finished",
      capabilities: [],
      rationale: null,
      diff: null,
      chunks: toolOutputBuffer([]),
      output: { type: "text", text: "Path=src/main.rs" },
      isError: false,
      callIndex: 0,
      timing: { kind: "unknown" as const },
    })
    renderer.root.add(card)
    await setup.renderOnce()

    expect(card.header.plainText).toStartWith("▸ custom-tool")
    expect(card.header.plainText).toEndWith("✓ Path=src/main.rs")
    expect(card.header.plainText.match(/src\/main\.rs/g)).toHaveLength(1)
  })

  test("expands a successful file edit and shows its diff by default", async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const card = new ToolBlockRenderable(renderer, kennelTheme, {
      toolCallId: "edit-expanded",
      invocationId: "edit-expanded",
      turnId: "1",
      name: "edit",
      args: { path: "src/main.rs" },
      status: "finished",
      capabilities: ["write_filesystem"],
      rationale: "Apply the requested change",
      diff: {
        proposal_id: "proposal",
        path: "src/main.rs",
        unified_diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old();\n+new();\n",
        arguments_hash: "arguments",
        base_hash: "base",
        diff_hash: "diff",
        truncated: false,
      },
      chunks: toolOutputBuffer([]),
      output: { type: "text", text: "1 change applied" },
      isError: false,
      callIndex: 0,
      timing: { kind: "unknown" },
    })
    renderer.root.add(card)
    await setup.renderOnce()

    expect(card.header.plainText).toStartWith("⌄ edit  src/main.rs")
    expect(card.header.plainText).toContain("✓")
    expect(card.diff).not.toBeNull()
    expect(card.diff?.visible).toBeTrue()
  })

  test("expands a live edit when its completed diff arrives", async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const running = {
      toolCallId: "edit-live-expanded",
      invocationId: "edit-live-expanded",
      turnId: "1",
      name: "edit",
      args: { path: "src/live.rs" },
      status: "running" as const,
      capabilities: ["write_filesystem" as const],
      rationale: "Apply the requested change",
      diff: null,
      chunks: toolOutputBuffer([]),
      output: null,
      isError: null,
      callIndex: 0,
      timing: { kind: "unknown" as const },
    }
    const card = new ToolBlockRenderable(renderer, kennelTheme, running)
    renderer.root.add(card)
    await setup.renderOnce()
    expect(card.header.plainText).toStartWith("▸ edit  src/live.rs")
    expect(card.header.plainText).toContain("◌")

    card.update({
      ...running,
      status: "finished",
      diff: {
        proposal_id: "proposal-live",
        path: "src/live.rs",
        unified_diff: "--- a/src/live.rs\n+++ b/src/live.rs\n@@ -1 +1 @@\n-old();\n+new();\n",
        arguments_hash: "arguments",
        base_hash: "base",
        diff_hash: "diff",
        truncated: false,
      },
      output: { type: "text", text: "1 change applied" },
      isError: false,
    })
    await setup.renderOnce()

    expect(card.header.plainText).toStartWith("⌄ edit  src/live.rs")
    expect(card.header.plainText).toContain("✓")
    expect(card.diff?.visible).toBeTrue()
    const renderedDiff = card.diff instanceof DiffRenderable
      ? card.diff.diff
      : card.diff?.plainText
    expect(renderedDiff).toContain("+new();")
  })

  test("keeps the newest live tool progress and lets header drags select without toggling", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const tool = {
      toolCallId: "bash-live-tail",
      invocationId: "bash-live-tail",
      turnId: "1",
      name: "bash",
      args: { command: "cargo test --workspace" },
      status: "running" as const,
      capabilities: ["execute" as const],
      rationale: null,
      diff: null,
      chunks: toolOutputBuffer(Array.from({ length: 12 }, (_, index) => ({
        stream: "stdout" as const,
        chunk: `progress-${index + 1}\n`,
      }))),
      output: null,
      isError: null,
      callIndex: 0,
      timing: { kind: "unknown" as const },
    }
    const card = new ToolBlockRenderable(renderer, kennelTheme, tool, true)
    renderer.root.add(card)
    await setup.renderOnce()

    expect(card.truncationMarker.plainText).toMatch(
      /^… \d+ more lines · click to view all$/,
    )
    expect(card.body.plainText).toContain("progress-12")
    expect(card.body.plainText).not.toContain("progress-1\n")

    await setup.mockMouse.drag(
      card.header.x + 1,
      card.header.y,
      card.header.x + "Terminal".length,
      card.header.y,
    )
    expect(renderer.getSelection()?.getSelectedText().trim()).not.toBe("")
    expect(card.body.visible).toBeTrue()

    renderer.clearSelection()
    await setup.mockMouse.click(card.header.x + 1, card.header.y)
    expect(card.body.visible).toBeFalse()
  })

  test("opens complete tool output, refreshes streaming content, and restores focus on close", async () => {
    const setup = await createTestRenderer({ width: 90, height: 24, useThread: false })
    renderer = setup.renderer
    const tool = {
      toolCallId: "shared-provider-call",
      invocationId: "full-output",
      turnId: "1",
      name: "read",
      args: { path: "logs/full-output.log" },
      status: "running" as const,
      capabilities: ["read_filesystem" as const],
      rationale: null,
      diff: null,
      chunks: toolOutputBuffer(Array.from({ length: 12 }, (_, index) => ({
        stream: "stdout" as const,
        chunk: `line-${index + 1}\n`,
      }))),
      output: null,
      isError: null,
      callIndex: 0,
      timing: { kind: "unknown" as const },
    }
    const sibling = {
      ...tool, invocationId: "other-output", callIndex: 1,
      args: { path: "logs/other.log" }, chunks: toolOutputBuffer([{ stream: "stdout", chunk: "other invocation" }]),
    }
    const initial: RottweilerState = {
      ...createInitialState(),
      tools: { [tool.invocationId]: tool, [sibling.invocationId]: sibling },
      streamingTail: createStreamingTail({
        turnId: tool.turnId,
        text: "",
        thinking: "",
        citations: [],
        toolInvocationIds: [tool.invocationId, sibling.invocationId],
        finished: null,
      }),
    }
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: initial })
    renderer.root.add(app)
    await setup.renderOnce()
    const card = app.transcript.streamingCard
      .getChildren()
      .flatMap((child) => child.getChildren())
      .find((child): child is ToolBlockRenderable => child instanceof ToolBlockRenderable)!
    card.toggle()
    await setup.renderOnce()

    expect(card.body.plainText.split("\n")).toHaveLength(7)
    expect(card.truncationMarker.plainText).toMatch(
      /^… \d+ more lines · click to view all$/,
    )
    await setup.mockMouse.click(
      card.truncationMarker.x + 2,
      card.truncationMarker.y,
    )
    await setup.renderOnce()

    expect(app.outputViewer.visible).toBeTrue()
    expect(app.outputViewer.invocationId).toBe("full-output")
    expect(app.outputViewer.body.plainText).not.toContain("other invocation")
    expect(app.outputViewer.header.plainText).toBe("Read file · logs/full-output.log")
    expect(app.outputViewer.body.plainText).toBe(toolOutputContent(tool))
    expect(app.outputViewer.body.plainText).toContain("line-1")
    expect(app.outputViewer.body.plainText).toContain("line-12")
    expect(renderer.currentFocusedRenderable).toBe(app.outputViewer.scroller)
    expect(app.composer.visible).toBeFalse()

    const streamingTool = {
      ...tool,
      chunks: tool.chunks.append({ stream: "stdout", chunk: "line-13\n" }),
    }
    const streaming: RottweilerState = {
      ...initial,
      tools: { ...initial.tools, [streamingTool.invocationId]: streamingTool },
    }
    app.setState(streaming)
    await setup.renderOnce()
    expect(app.outputViewer.body.plainText).toBe(toolOutputContent(streamingTool))
    expect(app.outputViewer.body.plainText).toContain("line-13")

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    await setup.renderOnce()
    expect(app.outputViewer.visible).toBeFalse()
    expect(app.composer.visible).toBeTrue()
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)

    await setup.mockMouse.click(
      card.truncationMarker.x + 2,
      card.truncationMarker.y,
    )
    await setup.renderOnce()
    expect(app.outputViewer.visible).toBeTrue()
    app.setState({
      ...streaming,
      tools: {},
      streamingTail: createStreamingTail({ ...streaming.streamingTail!, toolInvocationIds: [] }),
    })
    await setup.renderOnce()
    expect(app.outputViewer.visible).toBeFalse()
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
  })

  test("does not open full tool output when the marker click completes a selection", async () => {
    const setup = await createTestRenderer({ width: 90, height: 20, useThread: false })
    renderer = setup.renderer
    const opened: string[] = []
    const card = new ToolBlockRenderable(renderer, kennelTheme, {
      toolCallId: "selected-marker",
      invocationId: "selected-marker",
      turnId: "1",
      name: "read",
      args: { path: "selection.log" },
      status: "running",
      capabilities: ["read_filesystem"],
      rationale: null,
      diff: null,
      chunks: toolOutputBuffer(Array.from({ length: 12 }, (_, index) => ({
        stream: "stdout" as const,
        chunk: `selection-${index + 1}\n`,
      }))),
      output: null,
      isError: null,
      callIndex: 0,
      timing: { kind: "unknown" },
    }, true, undefined, {
      syntaxStyle: SyntaxStyle.create(),
      onOpenToolOutput: (toolCallId) => opened.push(toolCallId),
    })
    renderer.root.add(card)
    await setup.renderOnce()

    await setup.mockMouse.drag(
      card.truncationMarker.x + 1,
      card.truncationMarker.y,
      card.truncationMarker.x + 8,
      card.truncationMarker.y,
    )
    expect(renderer.getSelection()?.getSelectedText().trim()).not.toBe("")
    expect(opened).toEqual([])

    renderer.clearSelection()
    await setup.mockMouse.click(
      card.truncationMarker.x + 2,
      card.truncationMarker.y,
    )
    expect(opened).toEqual(["selected-marker"])
  })
})
