import { expect, test } from "bun:test"
import { parsePluginManifest, type PluginManifest } from "../src/index.ts"
import type { UiContribution } from "../src/generated/ui-contract.ts"
import validateUiContribution from "../src/generated/ui-contribution-validator.js"

const contribution: UiContribution = {
  surface: "tool", id: "Result", tool_name: "inspect", title: "Inspection",
  fields: [{ kind: "text", id: "Summary", label: "Summary", path: [{ step: "field", name: "summary" }] }],
  actions: [{ id: "Open", label: "Open details", command: "inspect-details", arguments: { view: "summary" } }],
}
function manifest(ui: unknown): unknown {
  return {
    name: "inspector", version: "1.0.0", protocol: 3,
    capabilities: {
      tools: [{ name: "inspect", description: "Inspect", schema: { type: "object" }, caps: [] }],
      commands: [{ name: "inspect-details", description: "Inspect details" }],
      ui,
    },
  }
}

test("approved UI data shares generated bounds and freezes with its manifest", () => {
  expect(validateUiContribution(contribution)).toBe(true)
  const parsed = parsePluginManifest(manifest([structuredClone(contribution)]))
  expect(parsed.capabilities.ui).toEqual([contribution])
  expect(Object.isFrozen(parsed.capabilities.ui?.[0]?.fields)).toBe(true)
})

test("UI selectors and values reject unsupported shapes before code activation", () => {
  for (const invalid of [
    { ...contribution, script: "execute()" },
    { ...contribution, title: "é".repeat(65) },
    { ...contribution, fields: [{ ...contribution.fields[0], path: "$.summary" }] },
    { ...contribution, fields: [{ ...contribution.fields[0], path: Array(17).fill({ step: "index", index: 0 }) }] },
    { ...contribution, fields: [{ kind: "list", id: "items", label: "Items", path: [], max_items: 33 }] },
    { ...contribution, actions: [{ ...contribution.actions[0], arguments: "x".repeat(4096) }] },
  ]) {
    expect(validateUiContribution(invalid)).toBe(false)
    expect(() => parsePluginManifest(manifest([invalid]))).toThrow()
  }
})

test("UI declarations cannot route actions or presentations to undeclared capabilities", () => {
  for (const invalid of [
    [{ ...contribution, tool_name: "other" }],
    [{ ...contribution, actions: [{ ...contribution.actions[0], command: "other" }] }],
    [contribution, { ...contribution, id: "Second" }],
    [{ ...contribution, fields: [contribution.fields[0], contribution.fields[0]] }],
    [{ ...contribution, actions: [contribution.actions[0], contribution.actions[0]] }],
  ]) expect(() => parsePluginManifest(manifest(invalid))).toThrow()
  const { tool_name: _, ...common } = contribution
  expect(parsePluginManifest(manifest([{ ...common, surface: "panel" }])).capabilities.ui?.[0]?.surface).toBe("panel")
})

// @ts-expect-error UI capabilities are declarative field projections, not executable render callbacks.
const executable: NonNullable<PluginManifest["capabilities"]["ui"]>[number] = { render: () => "hello" }
void executable
