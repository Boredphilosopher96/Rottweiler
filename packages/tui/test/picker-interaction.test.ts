import { createTestRenderer } from "@opentui/core/testing"
import { expect, test } from "bun:test"
import { FuzzyPickerRenderable, type PickerItem } from "../src/components"
import { PickerController } from "../src/picker-controller"
import { kennelTheme } from "../src/theme"

test("picker transitions retire captured actions while refresh retains the active interaction", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  try {
    const picker = new FuzzyPickerRenderable<unknown>(setup.renderer, kennelTheme)
    setup.renderer.root.add(picker)
    const callbacks: Array<(item: PickerItem<unknown>) => void> = []
    const refresh = picker.refresh.bind(picker)
    picker.refresh = (title, items, callback, compact) => {
      callbacks.push(callback)
      refresh(title, items, callback, compact)
    }
    const controller = new PickerController({
      picker: () => picker, terminalHeight: () => 24, statusHeight: () => 1,
      composerDockHeight: () => 4, focusComposer() {}, renderPicker() {},
      withRefreshGuard: (_kind, action) => action(), onModalOpened() {}, onClosed() {},
    })
    const selected: string[] = []
    const item = { id: "choice", label: "Choice", description: "", value: "selected" }
    const show = () => controller.show("Choose", [item], choice => selected.push(choice.value))
    controller.begin("settings")
    const retiredRoutes: Array<string | null> = []
    const first = controller.interaction!
    first.onRetire(() => retiredRoutes.push(controller.kind))
    expect(() => first.onRetire(() => {})).toThrow("cleanup owner")
    show()
    controller.refresh()
    expect(controller.interaction).toBe(first)
    callbacks[0]!(item)
    expect(selected).toEqual(["selected"])
    controller.kind = "settingChoices"
    expect(first.active).toBe(false)
    expect(retiredRoutes).toEqual(["settingChoices"])
    callbacks[0]!(item)
    expect(selected).toHaveLength(1)
    show()
    const choices = controller.interaction!
    controller.begin("settingChoices")
    expect(choices.active).toBe(false)
    callbacks[1]!(item)
    expect(selected).toHaveLength(1)
    show()
    const current = controller.interaction!
    callbacks[2]!(item)
    expect(selected).toHaveLength(2)
    controller.close()
    expect(current.active).toBe(false)
    expect(controller.interaction).toBeNull()
    callbacks[2]!(item)
    expect(selected).toHaveLength(2)
    controller.begin("settings")
    const last = controller.interaction!
    controller.dispose()
    expect(last.active).toBe(false)
    expect(retiredRoutes).toEqual(["settingChoices"])
  } finally { setup.renderer.destroy() }
})
