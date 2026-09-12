import { expect, test } from "bun:test"
import { plugin } from "../fixtures/conformance/rich-workflow"
import { parsePluginManifest } from "../src/index"

test("native rich fixture declares both action routes and its canonical tool grant", () => {
  const manifest = parsePluginManifest(plugin.manifest)
  expect(manifest.capabilities.commands?.[0]?.allowed_tools).toEqual(["rich_artifact"])
  expect(manifest.capabilities.ui?.map(surface => surface.id)).toEqual(["artifact", "result"])
  for (const surface of manifest.capabilities.ui ?? []) {
    expect(surface.actions?.[0]).toEqual({ id: "advance", label: "Advance workflow", command: "rich-workflow", arguments: { action: "advance" } })
  }
})

test("native rich fixture cannot redirect an action to an undeclared command", () => {
  const manifest = structuredClone(plugin.manifest)
  const surface = manifest.capabilities.ui![0]!
  const changed = { ...surface, actions: [{ ...surface.actions![0]!, command: "unowned-command" }] }
  expect(() => parsePluginManifest({ ...manifest, capabilities: { ...manifest.capabilities, ui: [changed] } })).toThrow()
})
