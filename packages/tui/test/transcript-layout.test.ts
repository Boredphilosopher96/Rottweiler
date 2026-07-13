import { afterEach, describe, expect, mock, spyOn, test } from "bun:test"
import { mkdtempSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { TextAttributes, TreeSitterClient, getBaseAttributes } from "@opentui/core"
import { createTestRenderer, type TestRendererSetup } from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import { createInitialState } from "../src/state"

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
})
