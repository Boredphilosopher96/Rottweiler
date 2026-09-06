import { expect, test } from "bun:test"
import { ClientCache } from "../src/history/cache"
import type { HistoryCacheValue } from "../src/history/controller"
import { UiCatalogController, UI_COLLECTION_ALLOCATION_FACTOR } from "../src/ui/catalog"
import { UiActionController } from "../src/ui/actions"
import { prepareUiPanels, prepareUiSurface, uiIdentity } from "../src/ui/presentation"
import { retainedJsonBytes } from "../src/retained-json"
import { MAX_UI_CONTRIBUTIONS, MAX_UI_DESCRIPTOR_BYTES, MAX_UI_ACTIONS, MAX_UI_FIELDS, MAX_UI_TABLE_ROWS, MAX_UI_TABLE_COLUMNS, MAX_UI_PANEL_SLOTS, MAX_UI_PANELS_BYTES, MAX_UI_SURFACE_BYTES, type UiPanels } from "../src/protocol"
import validateSurface from "../../../protocol/ui-presentation-validator.js"
import { fixturePresentation } from "./fixtures/ui"

function panelFixture(): UiPanels {
  const presentation = fixturePresentation()
  presentation.descriptor.surface = { surface: "panel" }
  presentation.descriptor.actions = [{ id: "inspect", label: "Inspect" }]
  return { panels: [{ revision: 1, presentation }] }
}
async function settle(): Promise<void> { for (let i = 0; i < 8; i++) await Promise.resolve() }

test("catalog and panel admission charges before reads, closes late sources, and owns exact action revision", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  let panels = panelFixture()
  let finish!: () => void
  let blocked = false
  const controller = new UiCatalogController({
    uiCatalog: async () => {
      expect(cache.usage.bytes).toBeGreaterThan(0)
      return { entries: panels.panels.map(({ presentation }) => ({ owner: presentation.owner, descriptor: presentation.descriptor })) }
    },
    uiPanels: async () => {
      expect(cache.usage.bytes).toBeGreaterThanOrEqual(MAX_UI_PANELS_BYTES * UI_COLLECTION_ALLOCATION_FACTOR)
      if (blocked) await new Promise<void>(resolve => { finish = resolve })
      return panels
    },
  }, cache, () => {})
  controller.open("session", "panels")
  await settle()
  const key = uiIdentity(panels.panels[0]!.presentation)
  const lease = controller.pinPanel(key)!
  expect(lease.target).toEqual({ surface: "panel", revision: 1 })
  panels = { panels: [{ ...panels.panels[0]!, revision: 2 }] }
  controller.refresh()
  await settle()
  expect(controller.snapshot.panels[0]?.revision).toBe(2)
  expect(lease.target).toEqual({ surface: "panel", revision: 1 })
  expect(lease.model.presentation.descriptor.title).toBe("Inspection result")
  blocked = true
  controller.refresh()
  await settle()
  controller.close()
  cache.clear()
  expect(cache.usage.bytes).toBeGreaterThan(MAX_UI_PANELS_BYTES * UI_COLLECTION_ALLOCATION_FACTOR)
  finish()
  await settle()
  expect(controller.snapshot.panels).toHaveLength(0)
  expect(cache.usage.pinnedEntries).toBe(1)
  lease.release()
  expect(() => lease.model).toThrow("released")
  expect(cache.usage.bytes).toBe(0)
})

test("action settlement holds an old panel source across close and limits command fanout", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  const panels = panelFixture()
  const catalog = new UiCatalogController({ uiCatalog: async () => ({ entries: [] }), uiPanels: async () => panels }, cache, () => {})
  catalog.open("session", "panels")
  await settle()
  let finish!: () => void
  let submitted = 0
  const action = new UiActionController({ allowed: () => true, changed: () => {}, failed: () => {}, execute: async (_session, request) => {
    submitted++
    expect(request).toEqual({ owner: panels.panels[0]!.presentation.owner, contribution_id: "result", action_id: "inspect", target: { surface: "panel", revision: 1 } })
    await new Promise<void>(resolve => { finish = resolve })
    return { type: "accepted" }
  } })
  const key = uiIdentity(panels.panels[0]!.presentation)
  const pending = action.invoke(catalog.pinPanel(key)!, "inspect")
  expect(await action.invoke(catalog.pinPanel(key)!, "inspect")).toBeFalse()
  expect(submitted).toBe(1)
  catalog.close(); action.reset(); cache.clear()
  expect(cache.usage.pinnedEntries).toBe(1)
  expect(action.pending).toBeTrue()
  finish()
  expect(await pending).toBeTrue()
  expect(cache.usage.bytes).toBe(0)
})

test("maximal closed table cardinalities fit reserved collection and prepared native allocation", () => {
  const panels = panelFixture()
  const surface = panels.panels[0]!.presentation
  surface.descriptor.fields = Array.from({ length: MAX_UI_FIELDS }, (_, index) => ({ kind: "table", id: `f${index}`, label: "T", columns: Array(MAX_UI_TABLE_COLUMNS).fill("C"), max_rows: MAX_UI_TABLE_ROWS }))
  surface.projected.fields = surface.descriptor.fields.map(field => ({ kind: "table", id: field.id, rows: Array.from({ length: MAX_UI_TABLE_ROWS }, () => Array(MAX_UI_TABLE_COLUMNS).fill("x")) }))
  expect(validateSurface(surface)).toBeTrue()
  expect(Buffer.byteLength(JSON.stringify(surface))).toBeLessThanOrEqual(MAX_UI_SURFACE_BYTES)
  panels.panels = Array.from({ length: MAX_UI_PANEL_SLOTS }, (_, index) => ({ revision: 1, presentation: { ...surface, descriptor: { ...surface.descriptor, id: `panel-${index}` } } }))
  const sourceBytes = Buffer.byteLength(JSON.stringify(panels))
  expect(sourceBytes).toBeLessThanOrEqual(MAX_UI_PANELS_BYTES)
  const value: HistoryCacheValue = { kind: "ui_panels", panels: prepareUiPanels(panels) }
  const retained = retainedJsonBytes(value, Number.MAX_SAFE_INTEGER)
  // Includes encoded collection plus a separate UTF-16 JSON decode string.
  expect(retained + sourceBytes * 3).toBeLessThan(MAX_UI_PANELS_BYTES * UI_COLLECTION_ALLOCATION_FACTOR)
  const cache = new ClientCache<HistoryCacheValue>()
  const lease = cache.reserve(MAX_UI_PANELS_BYTES * UI_COLLECTION_ALLOCATION_FACTOR)!.commit("panels", value)
  const next = cache.reserve(MAX_UI_PANELS_BYTES * UI_COLLECTION_ALLOCATION_FACTOR)
  expect(next).not.toBeNull()
  next!.release()
  lease.release()
  cache.clear()
  expect(cache.usage.bytes).toBe(0)
})


test("unchanged panel revisions preserve prepared model identity and failed action admission releases its lease", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  const panels = panelFixture()
  const catalog = new UiCatalogController({ uiCatalog: async () => ({ entries: [] }), uiPanels: async () => structuredClone(panels) }, cache, () => {})
  catalog.open("session", "panels")
  await settle()
  const model = catalog.snapshot.panels[0]!.model
  catalog.refresh()
  await settle()
  expect(catalog.snapshot.panels[0]!.model).toBe(model)
  const action = new UiActionController({ allowed: () => { throw new Error("retired authority") },
    changed: () => {}, failed: () => {}, execute: async () => { throw new Error("must not dispatch") },
  })
  const lease = catalog.pinPanel(uiIdentity(model.presentation))!
  expect(await action.invoke(lease, "inspect")).toBeFalse()
  expect(() => lease.model).toThrow("released")
  expect(action.pending).toBeFalse()
  catalog.close(); cache.clear()
  expect(cache.usage.bytes).toBe(0)
})


test("prepared fields reject descriptor/projection identity and declared collection mismatch", () => {
  const duplicate = fixturePresentation()
  duplicate.projected.fields[1] = duplicate.projected.fields[0]!
  expect(() => prepareUiSurface(duplicate)).toThrow("duplicate presentation field")
  const list = fixturePresentation()
  const field = list.descriptor.fields.find(field => field.kind === "list")!
  if (field.kind !== "list") throw new Error("fixture")
  field.max_items = 1
  expect(() => prepareUiSurface(list)).toThrow("list bound")
  const table = fixturePresentation()
  const values = table.projected.fields.find(field => field.kind === "table")!
  if (values.kind !== "table") throw new Error("fixture")
  values.rows[0]!.push("undeclared column")
  expect(() => prepareUiSurface(table)).toThrow("table bound")
})


test("maximum catalog cardinality fits decoded collection admission and duplicate owners are rejected", async () => {
  const entries = Array.from({ length: MAX_UI_CONTRIBUTIONS }, (_, index) => {
    const presentation = fixturePresentation()
    presentation.descriptor.id = `panel-${index}`
    presentation.descriptor.fields = Array.from({ length: MAX_UI_FIELDS }, (_, field) => ({ kind: "badge", id: `f${field}`, label: "T" }))
    presentation.descriptor.actions = Array.from({ length: MAX_UI_ACTIONS }, (_, action) => ({ id: `a${action}`, label: "Run" }))
    return { owner: presentation.owner, descriptor: presentation.descriptor }
  })
  const catalog = { entries }
  const encoded = Buffer.byteLength(JSON.stringify(catalog))
  expect(encoded).toBeLessThanOrEqual(MAX_UI_DESCRIPTOR_BYTES)
  const retained = retainedJsonBytes({ kind: "ui_catalog", catalog }, Number.MAX_SAFE_INTEGER)
  expect(retained + encoded * 3).toBeLessThan(MAX_UI_DESCRIPTOR_BYTES * UI_COLLECTION_ALLOCATION_FACTOR)
  const cache = new ClientCache<HistoryCacheValue>()
  const controller = new UiCatalogController({ uiCatalog: async () => ({ entries: [entries[0]!, entries[0]!] }), uiPanels: async () => ({ panels: [] }) }, cache, () => {})
  controller.open("session", "catalog")
  await settle()
  expect(controller.snapshot.error).not.toBeNull()
  expect(controller.snapshot.entries).toHaveLength(0)
  controller.close(); cache.clear()
  expect(cache.usage.bytes).toBe(0)
})


test("catalog decoders receive the cache reservation and cannot decode beyond shared admission", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  let decoded = false
  const controller = new UiCatalogController({
    async uiCatalog(_session, _signal, allocation) {
      allocation.admit(cache.capacityBytes + 1)
      decoded = true
      return { entries: [] }
    },
    async uiPanels() { return { panels: [] } },
  }, cache, () => {})
  controller.open("session", "catalog")
  await settle()
  expect(decoded).toBeFalse()
  expect(controller.snapshot.error).not.toBeNull()
  expect(cache.usage.bytes).toBe(0)
  controller.close()
})
