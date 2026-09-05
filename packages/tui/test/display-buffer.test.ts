import { describe, expect, spyOn, test } from "bun:test"
import {
  EMPTY_TOOL_OUTPUT, DISPLAY_TRUNCATION_MARKER, LIVE_OUTPUT_TRUNCATION_MARKER,
  MAX_TOOL_DISPLAY_BYTES, MAX_TOOL_DISPLAY_CHUNKS, MAX_TAIL_TEXT_BYTES,
  MAX_PREVIEW_LINE_CODE_UNITS, PREVIEW_LINE_TRUNCATION_MARKER,
  createInitialState, engineEvent, reduceRottweilerState, toolOutputBuffer, utf8Prefix,
  type ToolProjection,
} from "../src/state"
import { toolOutputContent, toolOutputPreview } from "../src/components/transcript/blocks"
import { projectToolActivity } from "../src/render/tools-workspace-presentation"
import { PROTOCOL_VERSION, type EngineEvent } from "../src/protocol"

function tool(chunks = EMPTY_TOOL_OUTPUT): ToolProjection {
  return {
    toolCallId: "stream", invocationId: "stream-1", turnId: "1", name: "bash", args: { command: "echo output" },
    status: "running", capabilities: [], rationale: null, diff: null, chunks, output: null,
    isError: null, callIndex: 0, timing: { kind: "unknown" },
  }
}
const meta = (sequence: number) => ({ protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: String(sequence), emitted_at: "2026-09-04T00:00:00Z" })
const reduce = (state: ReturnType<typeof createInitialState>, event: EngineEvent) => reduceRottweilerState(state, engineEvent(event))

describe("bounded immutable display streams", () => {
  test("actual Tools/transcript readers visit only appended nodes and reuse unchanged revisions", () => {
    let buffer = EMPTY_TOOL_OUTPUT
    let expected = ""
    for (let index = 0; index < 1000; index += 1) {
      const text = `line-${index}\n`
      buffer = buffer.append({ stream: index % 2 === 0 ? "stdout" : "stderr", chunk: text })
      expected += text
      const projection = tool(buffer)
      const activity = projectToolActivity(projection, index, false)
      const transcript = toolOutputPreview(projection).content
      expect(activity.output.kind).toBe("text")
      expect(transcript).toContain(`line-${index}`)
      expect(buffer.materializationWork.visitedNodes).toBe(index + 1)
    }
    expect(buffer.read().plain).toBe(expected)
    const before = buffer.materializationWork.visitedNodes
    const current = buffer.read()
    for (let index = 0; index < 100; index += 1) {
      projectToolActivity(tool(buffer), index, false)
      toolOutputPreview(tool(buffer))
      expect(buffer.read()).toBe(current)
    }
    expect(buffer.materializationWork.visitedNodes).toBe(before)
    expect(buffer.materializationWork.retainedVersions).toBe(1)
    expect(buffer.materializationWork.windowInputCodeUnits).toBeLessThan(expected.length * 5)
    expect(buffer.retainedBytes).toBe(Buffer.byteLength(expected))
  })

  test("production live previews split only new payload and fixed windows, without TextEncoder allocations", () => {
    const split = String.prototype.split
    let splitCodeUnits = 0
    let encodeCalls = 0
    const splitting = spyOn(String.prototype, "split").mockImplementation(function (this: string, separator, limit) {
      splitCodeUnits += this.length
      if (typeof separator === "string") return split.bind(this)(separator, limit)
      return split.bind(this)(separator, limit)
    })
    const encode = TextEncoder.prototype.encode
    const encoding = spyOn(TextEncoder.prototype, "encode").mockImplementation(function (this: TextEncoder, input) {
      encodeCalls += 1
      return encode.call(this, input)
    })
    try {
      let buffer = EMPTY_TOOL_OUTPUT
      const chunk = `${"x".repeat(1015)}\n`
      for (let index = 0; index < 1000; index += 1) {
        buffer = buffer.append({ stream: "stdout", chunk })
        projectToolActivity(tool(buffer), index, false)
        toolOutputPreview(tool(buffer))
      }
      expect(buffer.retainedBytes).toBe(chunk.length * 1000)
      expect(splitCodeUnits).toBeLessThan(buffer.retainedBytes * 4)
      expect(encodeCalls).toBe(0)
      const before = splitCodeUnits
      for (let index = 0; index < 100; index += 1) {
        projectToolActivity(tool(buffer), index, false)
        toolOutputPreview(tool(buffer))
      }
      expect(splitCodeUnits - before).toBeLessThan(10_000)
    } finally {
      splitting.mockRestore()
      encoding.mockRestore()
    }
  })

  test("single-line streams have bounded preview copies while complete retained output remains readable", () => {
    let buffer = EMPTY_TOOL_OUTPUT
    let presentedCodeUnits = 0
    for (let index = 0; index < 1000; index += 1) {
      buffer = buffer.append({ stream: "stdout", chunk: "x".repeat(1000) })
      const preview = toolOutputPreview({ ...tool(buffer), name: "other" })
      presentedCodeUnits += preview.content.length
      const output = projectToolActivity(tool(buffer), index, false).output
      if (output.kind !== "text") throw new Error("expected live output")
      expect(output.text.length).toBeLessThanOrEqual(MAX_PREVIEW_LINE_CODE_UNITS)
    }
    expect(presentedCodeUnits).toBeLessThan((MAX_PREVIEW_LINE_CODE_UNITS + 50) * 1000)
    expect(buffer.read().plain.length).toBe(1_000_000)
    expect(buffer.read().tailLines[0]?.startsWith(PREVIEW_LINE_TRUNCATION_MARKER)).toBe(true)
    expect(buffer.read().sourceTruncated).toBe(false)
    expect(toolOutputContent({ ...tool(buffer), name: "other" })).not.toContain(PREVIEW_LINE_TRUNCATION_MARKER)
  })

  test("older snapshots and branches remain correct without rolling back the forward cache", () => {
    const old = toolOutputBuffer([{ stream: "stdout", chunk: "one\r" }])
    const oldView = old.read()
    const next = old.append({ stream: "stderr", chunk: "\ntwo\n" })
    const nextView = next.read()
    expect(old.read().plain).toBe("one\r")
    expect(next.read()).toBe(nextView)
    expect(oldView.plain).toBe("one\r")
    expect(nextView.plain).toBe("one\r\ntwo\n")
    expect(nextView.tailLines).toEqual(["one", "two"])
    const branch = old.append({ stream: "stdout", chunk: "other" })
    expect(branch.read().plain).toBe("one\rother")
    expect(next.read().plain).toBe("one\r\ntwo\n")
    expect(old.read().plain).toBe("one\r")
    expect(next.materializationWork.retainedVersions).toBe(1)
  })

  test("incremental windows match CRLF normalization and trailing-empty-line semantics", () => {
    let buffer = EMPTY_TOOL_OUTPUT
    let full = ""
    const inputs = ["a\r", "\nb\r", "c\n\n", "\n".repeat(100), "tail", ...Array.from({ length: 80 }, (_, index) => `\n${index}`), "\n\n"]
    for (const chunk of inputs) {
      buffer = buffer.append({ stream: "stdout", chunk })
      full += chunk
      const lines = full.replaceAll("\r\n", "\n").replaceAll("\r", "\n").split("\n")
      while (lines.at(-1) === "") lines.pop()
      expect(buffer.read().lineCount).toBe(lines.length)
      expect(buffer.read().tailLines).toEqual(lines.slice(-32))
      const output = projectToolActivity(tool(buffer), 0, false).output
      if (output.kind !== "text") throw new Error("expected a text output window")
      expect(output.text).toBe(lines.slice(-8).join("\n"))
    }
  })

  test("mounted transcript preview matches full-body slicing without scanning earlier chunks", () => {
    for (const name of ["bash", "other"]) {
      let buffer = EMPTY_TOOL_OUTPUT
      const inputs = ["", "first\r", "\nsecond\n\n", "\n".repeat(100), "tail", ...Array.from({ length: 40 }, (_, index) => `\n${index}`)]
      for (const chunk of inputs) {
        buffer = buffer.append({ stream: "stdout", chunk })
        const projection = { ...tool(buffer), name, rationale: "Test preview" }
        const all = toolOutputContent(projection).split("\n")
        const preview = toolOutputPreview(projection)
        expect(preview.content).toBe((all.length <= 8 ? all : all.slice(-7)).join("\n"))
        expect(preview.hiddenLines).toBe(all.length <= 8 ? 0 : all.length - 7)
      }
    }
  })

  test("recognizes a host truncation marker fragmented across stream events", () => {
    let buffer = EMPTY_TOOL_OUTPUT
    for (const chunk of LIVE_OUTPUT_TRUNCATION_MARKER) buffer = buffer.append({ stream: "stdout", chunk })
    const output = projectToolActivity(tool(buffer), 0, false).output
    expect(output.kind === "text" && output.sourceTruncated).toBe(true)
  })

  test("byte and chunk limits report omissions without retaining further payload nodes", () => {
    let buffer = toolOutputBuffer([{ stream: "stdout", chunk: "🐕".repeat(MAX_TOOL_DISPLAY_BYTES / 4 + 1) }])
    expect(buffer.retainedBytes).toBeLessThanOrEqual(MAX_TOOL_DISPLAY_BYTES)
    expect(buffer.truncated).toBe(true)
    expect(buffer.omittedBytes).toBe(4)
    const initial = buffer.read()
    expect(initial.plain).toContain(DISPLAY_TRUNCATION_MARKER)
    for (let index = 0; index < 2000; index += 1) buffer = buffer.append({ stream: "stderr", chunk: "ignored" })
    expect(buffer.count).toBe(1)
    expect(buffer.retainedBytes).toBe(MAX_TOOL_DISPLAY_BYTES)
    expect(buffer.omittedBytes).toBe(4 + 2000 * 7)
    expect(buffer.materializationWork.visitedNodes).toBe(1)
    let tiny = EMPTY_TOOL_OUTPUT
    for (let index = 0; index < MAX_TOOL_DISPLAY_CHUNKS + 10; index += 1) tiny = tiny.append({ stream: "stdout", chunk: "x" })
    expect(tiny.count).toBe(MAX_TOOL_DISPLAY_CHUNKS)
    expect(tiny.omittedBytes).toBe(10)
    expect(tiny.truncated).toBe(true)
    expect(JSON.stringify(tiny).length).toBeLessThan(150)
  })

  test("an exhausted empty-chunk stream still shows a truthful omission marker", () => {
    let buffer = EMPTY_TOOL_OUTPUT
    for (let index = 0; index < MAX_TOOL_DISPLAY_CHUNKS; index += 1) buffer = buffer.append({ stream: "stdout", chunk: "" })
    buffer = buffer.append({ stream: "stdout", chunk: "not retained" })
    const output = projectToolActivity(tool(buffer), 0, false).output
    expect(output.kind === "text" && output.text).toBe(DISPLAY_TRUNCATION_MARKER)
    expect(output.kind === "text" && output.sourceTruncated).toBe(true)
  })

  test("final output releases live storage while prior immutable snapshots remain readable", () => {
    const running = reduce(createInitialState(), {
      type: "tool_output_delta", meta: meta(1), turn_id: "1", tool_call_id: "stream", invocation_id: "stream-1", stream: "stdout", chunk: "live prefix",
    })
    const before = running.tools["stream-1"]
    if (before === undefined) throw new Error("expected running projection")
    before.chunks.read()
    const finished = reduce(running, {
      type: "tool_call_finished", presentation: null, meta: meta(2), turn_id: "1", tool_call_id: "stream", invocation_id: "stream-1",
      output: { type: "text", text: "authoritative final output" }, is_error: false, call_index: 0,
    })
    expect(finished.tools["stream-1"]?.chunks).toBe(EMPTY_TOOL_OUTPUT)
    expect(before.chunks.read().plain).toBe("live prefix")
    expect(finished.tools["stream-1"]?.output).toEqual({ type: "text", text: "authoritative final output" })
  })

  test("text and reasoning obey independent display budgets with exact omitted-byte metadata", () => {
    let state = reduce(createInitialState(), { type: "text_delta", meta: meta(1), turn_id: "1", text: "x".repeat(MAX_TAIL_TEXT_BYTES - 1) })
    state = reduce(state, { type: "text_delta", meta: meta(2), turn_id: "1", text: "🐕" })
    state = reduce(state, { type: "thinking_delta", meta: meta(3), turn_id: "1", text: "y".repeat(MAX_TAIL_TEXT_BYTES + 12) })
    expect(state.streamingTail?.displayBudget).toEqual({
      text: { bytes: MAX_TAIL_TEXT_BYTES - 1, omittedBytes: 4 },
      thinking: { bytes: MAX_TAIL_TEXT_BYTES, omittedBytes: 12 },
    })
    expect(state.streamingTail?.text.endsWith("�")).toBe(false)
    expect(state.streamingTail?.thinking.length).toBe(MAX_TAIL_TEXT_BYTES)
    state = reduce(state, { type: "text_delta", meta: meta(4), turn_id: "1", text: "a" })
    expect(state.streamingTail?.displayBudget.text).toEqual({ bytes: MAX_TAIL_TEXT_BYTES - 1, omittedBytes: 5 })
    expect(state.streamingTail?.text.endsWith("a")).toBe(false)
    expect(utf8Prefix("a🐕b", 4)).toBe("a")
    expect(utf8Prefix("a🐕b", 5)).toBe("a🐕")
  })
})
