import { formatToolArguments, formatToolSubject } from "../src/tool-arguments"
import { describe, expect, test } from "bun:test"
import { displayPath, presentTool, setWorkspaceRoots } from "../src/render"
import { createInitialState, MAX_TOOL_RESULT_PREVIEW_BYTES, prepareToolDisplay } from "../src/state"
import { fixturePresentation } from "./fixtures/ui"
import { meta, reduce } from "./state/fixtures"

function finish(output: import("../src/protocol").ToolOutput, presentation: import("../src/protocol").UiPresentation | null = null) {
  return reduce(createInitialState(), {
    type: "tool_call_finished", meta: meta("1"), turn_id: "turn", tool_call_id: "provider",
    invocation_id: "invocation", output, presentation, is_error: false, call_index: 0,
  }).tools.invocation!
}

describe("source-owned tool presentation", () => {
  test("uses persisted descriptor fields for every tool without interpreting raw result keys", () => {
    const surface = fixturePresentation()
    const output = { type: "structured" as const, value: { diagnostics: [{ message: "not a declared display value" }], machine_local_path: "/private/secret" } }
    const tool = finish(output, surface)
    const display = presentTool(tool)
    expect(display.summary).toBe("Inspection result")
    expect(display.details).toContain("Summary · Native, source-backed presentation")
    expect(display.details).toContain("engine.rs │ Ready")
    expect(display.details).not.toContain("not a declared")
    expect(display.details).not.toContain("/private/secret")
    expect(presentTool({ ...tool, name: "unrelated_extension_tool" })).toBe(tool.display!)
    expect(Object.hasOwn(tool, "output")).toBe(false)
    expect(tool.source).toEqual({ sequence: "1", selector: { type: "tool_output" } })
  })

  test("large final bodies leave only a bounded copied preview and source reference", () => {
    const text = "🐕".repeat(1024 * 1024)
    const tool = finish({ type: "text", text })
    expect(Buffer.byteLength(tool.display!.details)).toBeLessThanOrEqual(MAX_TOOL_RESULT_PREVIEW_BYTES)
    expect(tool.display?.truncated).toBe(true)
    expect(JSON.stringify(tool).length).toBeLessThan(10_000)
    expect(tool.args).toBeNull()
    expect(tool.display?.details).not.toContain("�")
  })

  test("undeclared structured output and protected model framing require the canonical content reader", () => {
    const display = prepareToolDisplay({ type: "mixed", parts: [
      { type: "text", text: "<rottweiler_untrusted_result>private model body</rottweiler_untrusted_result>" },
      { type: "structured", value: { private: "must not be formatted", huge: "x".repeat(4 * 1024 * 1024) } },
    ] }, null, null, false)
    expect(display.details).toBe("Result available in full output.")
    expect(display.details).not.toContain("private")
  })

  test("errors remain plain, bounded, and actionable", () => {
    const denied = prepareToolDisplay({ type: "text", text: "permission denied for tool bash" }, null, null, true)
    expect(denied.permissionDenied).toBe(true)
    expect(denied.details).toBe("Permission denied. The tool was not run.")
    const invalid = prepareToolDisplay({ type: "text", text: "error parsing diff: line count did not match for hunk" }, null, null, true)
    expect(invalid.details).toBe("Couldn't apply the requested change.")
  })

  test("presentation truncation applies before retained display strings are built", () => {
    const presentation = fixturePresentation()
    presentation.projected.fields = [{ kind: "text", id: "summary", value: "🐕".repeat(1024) }]
    presentation.descriptor.fields = [{ kind: "text", id: "summary", label: "Summary" }]
    const display = prepareToolDisplay({ type: "text", text: "unused source" }, presentation, null, false)
    expect(Buffer.byteLength(display.details)).toBeLessThanOrEqual(MAX_TOOL_RESULT_PREVIEW_BYTES)
    expect(display.truncated).toBe(true)
    expect(display.details).not.toContain("unused source")
  })

  test("workspace paths remain relative to the longest owning root", () => {
    setWorkspaceRoots(["/repo", "/repo/nested"])
    expect(displayPath("/repo/nested/file.ts")).toBe("file.ts")
    expect(displayPath("/repo2/file.ts")).toBe("/repo2/file.ts")
    setWorkspaceRoots([])
  })
})


test("invalid descriptor pairing fails closed without crashing the event reducer", () => {
  const presentation = fixturePresentation()
  presentation.projected.fields[0] = { kind: "badge", id: "summary", value: "wrong kind" }
  const tool = finish({ type: "text", text: "private fallback" }, presentation)
  expect(tool.display?.summary).toBe("Presentation unavailable")
  expect(tool.display?.details).not.toContain("private fallback")
  expect(tool.source?.sequence).toBe("1")
})


test("argument subjects omit internal metadata and preserve Unicode at truncation boundaries", () => {
  expect(formatToolSubject({ machine_local_path: "/private/source" })).toBe("")
  expect(formatToolSubject({ api_key: "secret" })).toContain("[redacted]")
  const subject = formatToolSubject({ path: "a".repeat(78) + "🐕".repeat(100) })
  expect(subject.length).toBeLessThanOrEqual(80)
  expect(subject.isWellFormed()).toBe(true)
  expect(formatToolArguments({ body: { secret: "never traverse" } })).toBe("Body=structured value")
})


test("completed and late diffs release large approval metadata while preserving canonical sources", () => {
  const diff = { proposal_id: "proposal", path: "file.rs", unified_diff: "+" + "x".repeat(256 * 1024),
    arguments_hash: "args", base_hash: "base", diff_hash: "diff", truncated: false }
  let state = reduce(createInitialState(), { type: "tool_approval_needed", meta: meta("1"), turn_id: "turn",
    tool_call_id: "provider", invocation_id: "invocation", name: "edit", args: { path: "file.rs" },
    capabilities: ["write_filesystem"], rationale: "r".repeat(64 * 1024), diff })
  expect(state.tools.invocation?.diff).toBe(diff)
  expect(state.tools.invocation?.rationale).toHaveLength(64 * 1024)
  state = reduce(state, { type: "tool_call_finished", meta: meta("2"), turn_id: "turn", tool_call_id: "provider",
    invocation_id: "invocation", output: { type: "text", text: "updated" }, presentation: null, is_error: false, call_index: 0 })
  expect(state.tools.invocation?.diff).toBeNull()
  expect(state.tools.invocation?.rationale).toBeNull()
  expect(state.tools.invocation?.capabilities).toEqual([])
  expect(state.tools.invocation?.diffSource).toEqual({ sequence: "1", selector: { type: "tool_diff" } })
  state = reduce(state, { type: "tool_diff_ready", meta: meta("3"), turn_id: "turn", tool_call_id: "provider", invocation_id: "invocation", diff })
  expect(state.tools.invocation?.diff).toBeNull()
  expect(state.tools.invocation?.diffSource?.sequence).toBe("3")
  expect(JSON.stringify(state.tools.invocation).length).toBeLessThan(2000)
})
