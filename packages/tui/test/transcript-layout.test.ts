import { afterEach, describe, expect, mock, spyOn, test } from "bun:test"
import { mkdtempSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { RGBA, TextAttributes, TreeSitterClient, getBaseAttributes } from "@opentui/core"
import { createTestRenderer, type TestRendererSetup } from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import { createInitialState } from "../src/state"
import { nordTheme } from "../src/theme"

describe("render-aware transcript virtualization", () => {
  let renderer: TestRendererSetup | undefined
  let treeSitter: TreeSitterClient | undefined
  let parserDataPath: string | undefined

  afterEach(async () => {
    renderer?.renderer.destroy()
    renderer = undefined
    await Bun.sleep(50)
    await treeSitter?.destroy()
    treeSitter = undefined
    if (parserDataPath !== undefined) rmSync(parserDataPath, { recursive: true, force: true })
    parserDataPath = undefined
    mock.restore()
  })

  test("keeps mixed Markdown cards visible while real Tree-sitter heights settle during wheel scrolling", async () => {
    const renderDiagnostics: string[] = []
    spyOn(console, "error").mockImplementation((...arguments_: unknown[]) => {
      renderDiagnostics.push(arguments_.map(String).join(" "))
    })
    parserDataPath = mkdtempSync(join(tmpdir(), "rottweiler-transcript-layout-"))
    treeSitter = new TreeSitterClient({
      dataPath: parserDataPath,
      workerPath: join(import.meta.dir, "../node_modules/@opentui/core/parser.worker.js"),
    })
    await treeSitter.initialize()
    renderer = await createTestRenderer({ width: 88, height: 22, useThread: false })

    const transcript = Array.from({ length: 24 }, (_, index) => ({
      sequenceId: String(index + 1),
      agentTurn: String(index + 1),
      turn: {
        role: "assistant" as const,
        blocks: [{
          type: "text" as const,
          text: [
            `## CARD_${index}_BEGIN`,
            "",
            `CARD_${index} has a **formatted** response with a table and code sample.`,
            "",
            "| Item | Value |",
            "| --- | --- |",
            `| CARD_${index} | ${index} |`,
            "",
            "```typescript",
            `const CARD_${index} = ${index}`,
            "```",
            "",
            `CARD_${index}_END`,
          ].join("\n"),
        }],
        meta: { synthetic: false, summary: false },
      },
    }))
    const app = createRottweilerApp(renderer.renderer, {
      treeSitterClient: treeSitter,
      initialState: { ...createInitialState(), transcript },
    })
    renderer.renderer.root.add(app)
    for (let attempt = 0; attempt < 30; attempt += 1) {
      await Bun.sleep(10)
      await renderer.renderOnce()
    }

    let frame = renderer.captureCharFrame()
    expect(frame).toContain("CARD_23")
    expect(frame).not.toContain("## CARD_23_BEGIN")
    expect(frame).not.toContain("**formatted**")
    const bottomSpans = renderer.captureSpans().lines.flatMap((line) => line.spans)
    const strong = bottomSpans.find((span) => span.text.includes("formatted"))
    const heading = bottomSpans.find((span) => span.text.includes("CARD_23_BEGIN"))
    expect(getBaseAttributes(strong?.attributes ?? 0) & TextAttributes.BOLD).not.toBe(0)
    expect(getBaseAttributes(heading?.attributes ?? 0) & TextAttributes.BOLD).not.toBe(0)

    let priorTop = app.transcript.scroller.scrollTop
    for (let attempt = 0; attempt < 500 && priorTop > 0; attempt += 1) {
      await renderer.mockMouse.scroll(
        app.transcript.scroller.x + 2,
        app.transcript.scroller.y + 2,
        "up",
      )
      await Bun.sleep(0)
      await renderer.renderOnce()
      const nextTop = app.transcript.scroller.scrollTop
      if (nextTop !== priorTop) {
        frame = renderer.captureCharFrame()
        expect(frame).toMatch(/CARD_[0-9]+/)
      }
      priorTop = nextTop
    }
    expect(app.transcript.scroller.scrollTop).toBe(0)
    expect(renderer.captureCharFrame()).toContain("CARD_0")

    let priorBottomTop = app.transcript.scroller.scrollTop
    const bottom = () => Math.max(
      0,
      app.transcript.scroller.scrollHeight - app.transcript.scroller.viewport.height,
    )
    for (let attempt = 0; attempt < 500 && priorBottomTop < bottom(); attempt += 1) {
      await renderer.mockMouse.scroll(
        app.transcript.scroller.x + 2,
        app.transcript.scroller.y + 2,
        "down",
      )
      await Bun.sleep(0)
      await renderer.renderOnce()
      const nextTop = app.transcript.scroller.scrollTop
      if (nextTop !== priorBottomTop) {
        frame = renderer.captureCharFrame()
        expect(frame).toMatch(/CARD_[0-9]+/)
      }
      priorBottomTop = nextTop
    }
    expect(app.transcript.scroller.scrollTop).toBeGreaterThanOrEqual(bottom() - 1)
    expect(renderer.captureCharFrame()).toContain("CARD_23")
    expect(renderDiagnostics.join("\n")).not.toMatch(/Invalid dimensions|NaNxNaN|Failed to create frame buffer/)
  }, 20_000)

  test("uses intrinsic Markdown rows for a long streaming answer instead of clipping it", async () => {
    parserDataPath = mkdtempSync(join(tmpdir(), "rottweiler-streaming-layout-"))
    treeSitter = new TreeSitterClient({
      dataPath: parserDataPath,
      workerPath: join(import.meta.dir, "../node_modules/@opentui/core/parser.worker.js"),
    })
    await treeSitter.initialize()
    renderer = await createTestRenderer({ width: 52, height: 18, useThread: false })
    const text = [
      "## Live result",
      "",
      `A **wrapped** paragraph ${"with enough words to require visual wrapping ".repeat(8)}`,
      "",
      "| Item | Value |",
      "| --- | --- |",
      "| live | ready |",
      "",
      "```typescript",
      "const streamed = true",
      "```",
      "",
      "STREAMING_TAIL_SENTINEL",
    ].join("\n")
    const app = createRottweilerApp(renderer.renderer, {
      treeSitterClient: treeSitter,
      initialState: {
        ...createInitialState(),
        streamingTail: {
          turnId: "1",
          text,
          thinking: "",
          citations: [],
          toolCallIds: [],
          finished: null,
        },
      },
    })
    renderer.renderer.root.add(app)
    for (let attempt = 0; attempt < 40; attempt += 1) {
      await Bun.sleep(10)
      await renderer.renderOnce()
    }

    expect(app.transcript.streamingMarkdown.height).toBeGreaterThan(text.split("\n").length)
    expect(app.transcript.streamingCard.height).toBeGreaterThan(20)
    expect(renderer.captureCharFrame()).toContain("STREAMING_TAIL_SENTINEL")
    expect(renderer.captureCharFrame()).not.toContain("## Live result")
    expect(renderer.captureCharFrame()).not.toContain("**wrapped**")
  }, 20_000)

  test("incrementally retains Markdown blocks and keeps a fenced text diagram intact", async () => {
    const renderDiagnostics: string[] = []
    spyOn(console, "error").mockImplementation((...arguments_: unknown[]) => {
      renderDiagnostics.push(arguments_.map(String).join(" "))
    })
    parserDataPath = mkdtempSync(join(tmpdir(), "rottweiler-streaming-diagram-"))
    treeSitter = new TreeSitterClient({
      dataPath: parserDataPath,
      workerPath: join(import.meta.dir, "../node_modules/@opentui/core/parser.worker.js"),
    })
    await treeSitter.initialize()
    renderer = await createTestRenderer({ width: 70, height: 22, useThread: false })
    const initial = {
      ...createInitialState(),
      streamingTail: {
        turnId: "diagram",
        text: "",
        thinking: "",
        citations: [],
        toolCallIds: [],
        finished: null,
      },
    }
    const app = createRottweilerApp(renderer.renderer, {
      treeSitterClient: treeSitter,
      initialState: initial,
    })
    renderer.renderer.root.add(app)
    const markdown = app.transcript.streamingMarkdown
    const answer = [
      "## Architecture",
      "",
      "The stable block stays formatted while the diagram streams:",
      "",
      "```text",
      "┌──────────┐",
      "│ rw-core  │",
      "└────┬─────┘",
      "     ▼",
      "┌──────────┐",
      "│ OpenTUI  │",
      "└──────────┘",
      "```",
      "",
      "DIAGRAM_STREAM_COMPLETE",
    ].join("\n")

    for (let end = 1; end <= answer.length; end += 4) {
      app.setState({
        ...initial,
        streamingTail: { ...initial.streamingTail, text: answer.slice(0, end) },
      })
      await renderer.renderOnce()
      expect(app.transcript.streamingMarkdown).toBe(markdown)
      const frame = renderer.captureCharFrame()
      if (frame.includes("Architecture")) expect(frame).not.toContain("## Architecture")
      if (frame.includes("stable block")) expect(frame).not.toContain("```text")
    }
    app.setState({
      ...initial,
      streamingTail: { ...initial.streamingTail, text: answer },
    })
    for (let attempt = 0; attempt < 20; attempt += 1) {
      await Bun.sleep(5)
      await renderer.renderOnce()
    }

    const frame = renderer.captureCharFrame()
    expect(frame).toContain("┌──────────┐")
    expect(frame).toContain("│ rw-core  │")
    expect(frame).toContain("└────┬─────┘")
    expect(frame).toContain("│ OpenTUI  │")
    expect(frame).toContain("DIAGRAM_STREAM_COMPLETE")
    expect(frame).not.toContain("```text")
    expect(renderDiagnostics.join("\n")).not.toMatch(
      /Invalid dimensions|NaNxNaN|Failed to create frame buffer/,
    )
  }, 20_000)

  test("separates consecutive answers, renders Mermaid, and keeps prose on the primary foreground", async () => {
    parserDataPath = mkdtempSync(join(tmpdir(), "rottweiler-consecutive-mermaid-"))
    treeSitter = new TreeSitterClient({
      dataPath: parserDataPath,
      workerPath: join(import.meta.dir, "../node_modules/@opentui/core/parser.worker.js"),
    })
    await treeSitter.initialize()
    renderer = await createTestRenderer({ width: 82, height: 40, useThread: false })
    const app = createRottweilerApp(renderer.renderer, {
      theme: nordTheme,
      treeSitterClient: treeSitter,
      initialState: {
        ...createInitialState(),
        transcript: [
          {
            sequenceId: "1",
            agentTurn: "1",
            turn: {
              role: "assistant",
              blocks: [{
                type: "text",
                text: [
                  "## Previous answer",
                  "",
                  "Main prose stays readable.",
                  "",
                  "```mermaid",
                  "flowchart TB",
                  "  UI[OpenTUI] --> CORE[Rust core]",
                  "```",
                  "",
                  "PREVIOUS_ANSWER_END",
                ].join("\n"),
              }],
              meta: { synthetic: false, summary: false },
            },
          },
          {
            sequenceId: "2",
            agentTurn: "2",
            turn: {
              role: "user",
              blocks: [{ type: "text", text: "FOLLOW_UP_QUESTION_START\nCan you continue?" }],
              meta: { synthetic: false, summary: false },
            },
          },
        ],
      },
    })
    renderer.renderer.root.add(app)
    for (let attempt = 0; attempt < 30; attempt += 1) {
      await Bun.sleep(5)
      await renderer.renderOnce()
    }

    const cards = [...app.transcript.mountedCards.values()].sort((left, right) => left.y - right.y)
    expect(cards).toHaveLength(2)
    expect(cards[0]!.y + cards[0]!.height).toBeLessThanOrEqual(cards[1]!.y)
    const frame = renderer.captureCharFrame()
    expect(frame).toContain("PREVIOUS_ANSWER_END")
    expect(frame).toContain("FOLLOW_UP_QUESTION_START")
    expect(frame).toContain("OpenTUI")
    expect(frame).toContain("Rust core")
    expect(frame).not.toContain("flowchart TB")

    const prose = renderer.captureSpans().lines
      .flatMap((line) => line.spans)
      .find((span) => span.text.includes("Main prose stays readable"))
    expect(prose?.fg.toInts()).toEqual(RGBA.fromHex(nordTheme.foreground).toInts())
  }, 20_000)

  test("follows a growing answer at the bottom but preserves deliberate scrollback", async () => {
    renderer = await createTestRenderer({ width: 62, height: 16, useThread: false })
    const lines = Array.from({ length: 36 }, (_, index) => `Streaming line ${index + 1}`)
    const initial = {
      ...createInitialState(),
      streamingTail: {
        turnId: "follow",
        text: lines.join("\n"),
        thinking: "",
        citations: [],
        toolCallIds: [],
        finished: null,
      },
    }
    const app = createRottweilerApp(renderer.renderer, { initialState: initial })
    renderer.renderer.root.add(app)
    await renderer.flush()

    app.setState({
      ...initial,
      streamingTail: {
        ...initial.streamingTail,
        text: `${initial.streamingTail.text}\nAUTO_FOLLOW_SENTINEL`,
      },
    })
    await renderer.flush()
    expect(renderer.captureCharFrame()).toContain("AUTO_FOLLOW_SENTINEL")

    for (let attempt = 0; attempt < 8; attempt += 1) {
      await renderer.mockMouse.scroll(
        app.transcript.scroller.x + 2,
        app.transcript.scroller.y + 2,
        "up",
      )
      await renderer.renderOnce()
    }
    const scrollbackTop = app.transcript.scroller.scrollTop
    expect(scrollbackTop).toBeLessThan(
      app.transcript.scroller.scrollHeight - app.transcript.scroller.viewport.height,
    )
    app.setState({
      ...initial,
      streamingTail: {
        ...initial.streamingTail,
        text: `${initial.streamingTail.text}\nAUTO_FOLLOW_SENTINEL\nPRESERVE_SCROLLBACK_SENTINEL`,
      },
    })
    await renderer.flush()
    expect(app.transcript.scroller.scrollTop).toBeLessThanOrEqual(scrollbackTop + 1)
    expect(renderer.captureCharFrame()).not.toContain("PRESERVE_SCROLLBACK_SENTINEL")

    app.transcript.scrollTo(Number.MAX_SAFE_INTEGER)
    await renderer.flush()
    app.setState({
      ...initial,
      streamingTail: {
        ...initial.streamingTail,
        text: `${initial.streamingTail.text}\nAUTO_FOLLOW_SENTINEL\nPRESERVE_SCROLLBACK_SENTINEL\nFOLLOW_REENGAGED_SENTINEL`,
      },
    })
    await renderer.flush()
    expect(renderer.captureCharFrame()).toContain("FOLLOW_REENGAGED_SENTINEL")
  }, 20_000)
})
