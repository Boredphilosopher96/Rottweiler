import { createTestRenderer } from "@opentui/core/testing"
import { expect, test } from "bun:test"
import { FuzzyPickerRenderable, type PickerItem } from "../src/components"
import { ClientAllocationOwner } from "../src/client-allocation"
import { PickerController } from "../src/picker-controller"
import { kennelTheme } from "../src/theme"

async function fixture() {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  const picker = new FuzzyPickerRenderable<unknown>(setup.renderer, kennelTheme)
  setup.renderer.root.add(picker)
  const allocations = new ClientAllocationOwner()
  const controller = new PickerController({ allocations, picker: () => picker,
    terminalHeight: () => 24, statusHeight: () => 1, composerDockHeight: () => 4,
    focusComposer() {}, renderPicker() {}, withRefreshGuard: (_kind, action) => action(),
    onModalOpened() {}, onClosed() {},
  })
  controller.begin("agents")
  const callbacks: Array<(item: PickerItem<unknown>) => void> = []
  const original = picker.refresh.bind(picker)
  picker.refresh = (title, items, callback, compact) => { callbacks.push(callback); original(title, items, callback, compact) }
  return { setup, picker, allocations, controller, callbacks }
}
const item = (id: string, size = 4096) => ({ id, label: id, description: "", value: { source: "s".repeat(size) } })

test("picker retains item values after source release and through a callback that closes it", async () => {
  const f = await fixture()
  try {
    const row = item("first")
    const source = f.allocations.reserve("children", 8192)
    let selected = 0
    f.controller.show("Children", [row], () => {
      selected++
      f.controller.close()
      expect(f.allocations.usage.bytes).toBeGreaterThan(8192)
    })
    source.release()
    expect(f.allocations.usage.bytes).toBeGreaterThan(8192)
    f.callbacks[0]!(row)
    expect(selected).toBe(1)
    expect(f.allocations.usage.bytes).toBe(0)
    f.callbacks[0]!(row)
    expect(selected).toBe(1)
  } finally { f.controller.dispose(); f.setup.renderer.destroy() }
})

test("picker refusal preserves the mounted revision and successful replacement retires stale callbacks", async () => {
  const f = await fixture()
  try {
    let selected = ""
    const first = item("first")
    f.controller.show("Children", [first], row => { selected = row.id })
    const before = f.allocations.usage.bytes
    const pressure = f.allocations.reserve("live", f.allocations.limits.live - before)
    expect(() => f.controller.show("Refused", [item("second")], () => {})).toThrow("admission")
    expect(f.picker.select.options[0]?.name).toBe("first")
    expect(f.allocations.usage.bytes).toBe(before + pressure.bytes)
    f.callbacks[0]!(first)
    expect(selected).toBe("first")
    pressure.release()
    f.controller.show("Children", [item("second")], row => { selected = row.id })
    selected = ""
    f.callbacks[0]!(first)
    expect(selected).toBe("")
    expect(f.allocations.usage.bytes).toBeLessThan(before * 2)
    f.controller.close()
    expect(f.allocations.usage.bytes).toBe(0)
  } finally { f.controller.dispose(); f.setup.renderer.destroy() }
})

test("a failed native picker replacement pins both revisions and rejects further mutation until cleared", async () => {
  const f = await fixture()
  try {
    let selected = false
    const first = item("first")
    f.controller.show("Children", [first], () => { selected = true })
    const before = f.allocations.usage.bytes
    const refresh = f.picker.refresh.bind(f.picker)
    f.picker.refresh = (...args) => { refresh(...args); throw new Error("native replacement failed") }
    expect(() => f.controller.show("Children", [item("second")], () => {})).toThrow("native replacement failed")
    expect(f.allocations.usage.bytes).toBeGreaterThan(before)
    f.callbacks[0]!(first)
    expect(selected).toBe(false)
    const retained = f.allocations.usage.bytes
    expect(() => f.controller.show("Third", [item("third")], () => {})).toThrow("teardown")
    expect(f.allocations.usage.bytes).toBe(retained)
    f.controller.close()
    expect(f.picker.select.options).toHaveLength(0)
    expect(f.allocations.usage.bytes).toBe(0)
  } finally { f.controller.dispose(); f.setup.renderer.destroy() }
})
