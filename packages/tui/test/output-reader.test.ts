import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { OutputViewerRenderable } from "../src/components/output-viewer"
import { ToolOutputReader } from "../src/state/output-reader"
import { EMPTY_TOOL_OUTPUT, toolOutputBuffer } from "../src/state/display-buffer"
import type { ToolProjection } from "../src/state"
import { kennelTheme } from "../src/theme"

function live(chunks = EMPTY_TOOL_OUTPUT): ToolProjection {
  return { toolCallId: "provider", invocationId: "invocation", turnId: "1", name: "bash", args: {},
    status: "running", capabilities: [], rationale: null, diff: null, diffSource: null, chunks,
    display: null, source: null, isError: null, callIndex: 0, timing: { kind: "unknown" } }
}

test("preview cache retains bounded windows with correct interleaved immutable prefixes", () => {
  const old = toolOutputBuffer([{ stream: "stdout", chunk: "one\n" }])
  const oldPreview = old.preview()
  const newest = old.append({ stream: "stderr", chunk: "two\n" })
  const newestPreview = newest.preview()
  expect(old.preview()).toEqual(oldPreview)
  expect(newest.preview()).toBe(newestPreview)
  expect(old.append({ stream: "stdout", chunk: "branch\n" }).preview().tailLines).toEqual(["one", "branch"])
  expect(newest.preview().tailLines).toEqual(["one", "two"])
  expect(newest.preview()).not.toHaveProperty("plain")
  expect(newest.preview()).not.toHaveProperty("labeled")
  expect(newest.materializationWork.retainedVersions).toBe(1)
})

test("one full reader visits new chunks only and releases previous streams and explicit clears", () => {
  const reader = new ToolOutputReader()
  let buffer = EMPTY_TOOL_OUTPUT
  for (let i = 0; i < 1000; i++) {
    buffer = buffer.append({ stream: "stdout", chunk: `${i}\n` })
    const value = reader.read(buffer)
    expect(value.plain.endsWith(`${i}\n`)).toBeTrue()
    expect(reader.read(buffer)).toBe(value)
  }
  expect(reader.visitedChunks).toBe(1000)
  const other = toolOutputBuffer([{ stream: "stderr", chunk: "small" }])
  const current = reader.read(other)
  expect(current).toEqual({ plain: "small", labeled: "Error output\nsmall" })
  expect(reader.retainedCodeUnits).toBe(current.plain.length + current.labeled.length)
  expect(reader.read(other)).toBe(current)
  reader.clear()
  expect(reader.retainedCodeUnits).toBe(0)
  expect(reader.read(EMPTY_TOOL_OUTPUT)).toEqual({ plain: "", labeled: "" })
})

test("mounted output viewer replaces live text with final content and clears native text on close", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  const viewer = new OutputViewerRenderable(setup.renderer, kennelTheme)
  setup.renderer.root.add(viewer)
  try {
    const tool = live(toolOutputBuffer([{ stream: "stdout", chunk: "live first" }]))
    viewer.open(tool)
    await setup.renderOnce()
    expect(viewer.body.plainText).toContain("live first")
    viewer.update({ ...tool, status: "finished", chunks: EMPTY_TOOL_OUTPUT,
      display: { summary: "final", details: "authoritative final", truncated: false, subject: "", command: null, permissionDenied: false } })
    expect(viewer.body.plainText).toContain("authoritative final")
    expect(viewer.body.plainText).not.toContain("live first")
    viewer.closePresentation()
    expect(viewer.body.plainText).toBe("")
    expect(viewer.invocationId).toBeNull()
    viewer.open({ ...live(toolOutputBuffer([{ stream: "stderr", chunk: "other" }])), invocationId: "another" })
    expect(viewer.body.plainText).toContain("other")
    expect(viewer.body.plainText).not.toContain("live first")
  } finally { viewer.destroy(); setup.renderer.destroy() }
})
