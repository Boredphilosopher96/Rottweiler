import { describe, expect, test } from "bun:test"
import { McpEnvironmentDraft, MCP_ENVIRONMENT_DRAFT_LIMITS } from "../src/app/mcp-draft"

describe("MCP environment draft ownership", () => {
  test("UTF-8 admission is atomic and replacing a value releases its byte charge", () => {
    const draft = new McpEnvironmentDraft()
    const value = "界".repeat(Math.floor((MCP_ENVIRONMENT_DRAFT_LIMITS.bytes - 1) / 3))
    expect(draft.set("A", value)).toBe(true)
    expect(draft.set("B", "界")).toBe(false)
    expect(draft.set("A", "small")).toBe(true)
    expect(draft.set("B", "界")).toBe(true)
    expect(draft.take()).toEqual([{ key: "A", value: "small" }, { key: "B", value: "界" }])
  })

  test("one value per key and a fixed entry cap bound small-entry allocation", () => {
    const draft = new McpEnvironmentDraft()
    for (let i = 0; i < MCP_ENVIRONMENT_DRAFT_LIMITS.entries; i += 1) expect(draft.set(`K${i}`, "")).toBe(true)
    expect(draft.set("extra", "")).toBe(false)
    expect(draft.set("K0", "replacement")).toBe(true)
    const entries = draft.take()
    expect(entries).toHaveLength(MCP_ENVIRONMENT_DRAFT_LIMITS.entries)
    expect(entries[0]).toEqual({ key: "K0", value: "replacement" })
    expect(draft.set("new", "value")).toBe(true)
    expect(entries).toHaveLength(MCP_ENVIRONMENT_DRAFT_LIMITS.entries)
    draft.clear()
    expect(draft.take()).toEqual([])
  })
})
