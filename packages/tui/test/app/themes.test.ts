import { CliRenderEvents } from "@opentui/core"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, EngineEvent } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import {
  daylightTheme,
  kennelTheme,
  systemThemeFor,
  themeByName,
  themeCatalog
} from "../../src/theme"
import { emptyHistoryReader } from "../fixtures/history"
import { expectCoherentTheme, rgba } from "./fixtures"

describe("Rottweiler themes", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("constructs the complete app with the persisted startup theme", async () => {
    const setup = await createTestRenderer({ width: 72, height: 12, useThread: false })
    renderer = setup.renderer
    renderer.root.add(createRottweilerApp(renderer, { historyReader: emptyHistoryReader, theme: daylightTheme }))

    await setup.renderOnce()

    const backgrounds = setup.captureSpans().lines.flatMap((line) =>
      line.spans.map((span) => span.bg.toInts())
    )
    expect(backgrounds).toContainEqual(rgba(daylightTheme.background))
    expect(backgrounds).not.toContainEqual(rgba(kennelTheme.background))
  })

  test("previews the dynamic theme catalog coherently, reverts on Escape, and persists on confirm", async () => {
    const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      theme: kennelTheme,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "draft survives retheme"

    app.openThemePicker()
    expect(app.themeBrowser.footer.plainText).toBe("↑↓ preview · ⏎ apply · esc cancel")
    expect(app.themeBrowser.itemIds).toEqual(
      themeCatalog.map((theme) => `theme:${theme.name}`),
    )
    const previewTheme = themeByName("tokyonight")!
    const pickerBeforePreview = app.themeBrowser
    app.themeBrowser.selectById(`theme:${previewTheme.name}`)
    await setup.renderOnce()
    // Theme preview rebuilds the themed render tree while preserving picker
    // query, selection, focus, and the composer draft.
    expect(app.themeBrowser === pickerBeforePreview).toBeFalse()
    expect(pickerBeforePreview.input.isDestroyed).toBeTrue()
    expect(app.themeBrowser.input.isDestroyed).toBeFalse()
    expect(renderer.currentFocusedRenderable).toBe(app.themeBrowser.input)
    expect(setup.captureCharFrame()).toContain("THEME   34 themes   /theme")
    expectCoherentTheme(app, previewTheme)
    expect(app.composer.value).toBe("draft survives retheme")

    setup.mockInput.pressEscape()
    await Bun.sleep(100)
    await setup.renderOnce()
    expect(app.themeBrowser.visible).toBeFalse()
    expect(renderer.currentFocusedRenderable?.id).toBe("composer-editor")
    expectCoherentTheme(app, kennelTheme)
    expect(app.composer.value).toBe("draft survives retheme")
    expect(commands).toHaveLength(0)

    app.openThemePicker()
    app.themeBrowser.selectById(`theme:${previewTheme.name}`)
    app.themeBrowser.activateSelected()
    await Bun.sleep(10)
    expect(commands).toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "ui.theme",
      value: previewTheme.name,
    }))
    expect(app.themeBrowser.visible).toBeFalse()
    expectCoherentTheme(app, previewTheme)

    setup.resize(64, 14)
    app.openModePicker()
    await setup.renderOnce()
    expect(app.picker.visible).toBeTrue()
    expectCoherentTheme(app, previewTheme)
    expect(setup.captureCharFrame()).toContain("Modes")
  })

  test("keeps the active System theme and its picker preview synchronized with terminal mode", async () => {
    const setup = await createTestRenderer({ width: 90, height: 22, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      theme: systemThemeFor("dark"),
      systemThemeMode: "dark",
    })
    renderer.root.add(app)
    await setup.renderOnce()
    expectCoherentTheme(app, systemThemeFor("dark"))

    renderer.emit(CliRenderEvents.THEME_MODE, "light")
    await setup.renderOnce()
    expectCoherentTheme(app, systemThemeFor("light"))

    app.openThemePicker()
    expect(app.themeBrowser.selectedId).toBe("theme:system")
    expect(app.themeBrowser.detail.plainText).toContain(daylightTheme.background)
    const systemRow = app.themeBrowser.rowViews.find((row) => row.plainText.includes("system"))
    expect(systemRow?.plainText).toEndWith(" ansi")
  })

  test("refreshes System theme ownership through a non-System preview before cancel", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      theme: systemThemeFor("dark"),
      systemThemeMode: "dark",
    })
    renderer.root.add(app)

    app.openThemePicker()
    app.themeBrowser.selectById("theme:tokyonight")
    renderer.emit(CliRenderEvents.THEME_MODE, "light")
    await setup.renderOnce()
    expectCoherentTheme(app, themeByName("tokyonight", "light")!)

    app.themeBrowser.selectById("theme:system")
    await setup.renderOnce()
    expect(app.themeBrowser.detail.plainText).toContain("system  light")
    expect(app.themeBrowser.detail.plainText).toContain(daylightTheme.background)
    expectCoherentTheme(app, systemThemeFor("light"))

    app.themeBrowser.selectById("theme:tokyonight")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    await setup.renderOnce()
    expect(app.themeBrowser.visible).toBeFalse()
    expectCoherentTheme(app, systemThemeFor("light"))
  })

  test("resolves a captured non-System theme in the current terminal mode on cancel", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      theme: themeByName("tokyonight", "dark")!,
      systemThemeMode: "dark",
    })
    renderer.root.add(app)

    app.openThemePicker()
    app.themeBrowser.selectById("theme:nord")
    renderer.emit(CliRenderEvents.THEME_MODE, "light")
    await setup.renderOnce()
    expectCoherentTheme(app, themeByName("nord", "light")!)

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    await setup.renderOnce()
    expect(app.themeBrowser.visible).toBeFalse()
    expectCoherentTheme(app, themeByName("tokyonight", "light")!)
  })

  test("previews every built-in theme in the terminal's current light variant", async () => {
    const setup = await createTestRenderer({ width: 90, height: 22, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      theme: daylightTheme,
      systemThemeMode: "light",
    })
    renderer.root.add(app)
    app.openThemePicker()
    const preview = themeByName("tokyonight", "light")!
    app.themeBrowser.selectById("theme:tokyonight")
    await setup.renderOnce()
    expectCoherentTheme(app, preview)
  })

  test("uses the split theme browser and retains its query, selection, and viewport through preview", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      theme: kennelTheme,
      onCommand(command) {
        commands.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.composer.value = "theme browser keeps this draft"

    app.openThemePicker()
    const beforePreview = app.themeBrowser
    expect(beforePreview.visible).toBeTrue()
    expect(app.picker.visible).toBeFalse()
    expect(beforePreview.itemIds).toEqual(themeCatalog.map((theme) => `theme:${theme.name}`))
    beforePreview.scrollViewport(10)
    beforePreview.selectById("theme:tokyonight")
    await setup.renderOnce()

    expect(app.themeBrowser).not.toBe(beforePreview)
    expect(app.themeBrowser.visible).toBeTrue()
    expect(app.themeBrowser.selectedId).toBe("theme:tokyonight")
    expect(app.themeBrowser.scrollOffset).toBeGreaterThan(0)
    expect(app.themeBrowser.detail.plainText).toContain("tokyonight  dark · 52 roles resolved · live sample")
    expect(app.composer.value).toBe("theme browser keeps this draft")
    expectCoherentTheme(app, themeByName("tokyonight")!)

    await setup.mockInput.typeText("tok")
    expect(app.themeBrowser.input.value).toBe("tok")
    expect(app.themeBrowser.selectedId).toBe("theme:tokyonight")
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.themeBrowser.visible).toBeFalse()
    expect(commands).toEqual([])
    expectCoherentTheme(app, kennelTheme)

    app.openThemePicker()
    app.themeBrowser.selectById("theme:tokyonight")
    expect(app.themeBrowser.activateSelected()).toBeTrue()
    await Bun.sleep(10)
    expect(commands).toEqual([expect.objectContaining({
      type: "set_setting",
      key: "ui.theme",
      value: "tokyonight",
    })])
    expect(app.themeBrowser.visible).toBeFalse()
    expectCoherentTheme(app, themeByName("tokyonight")!)
  })

  test("restores the prior theme when browser persistence is rejected", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      theme: kennelTheme,
      onCommand: () => ({
        type: "rejected",
        error: {
          category: "protocol",
          code: "setting_rejected",
          message: "theme persistence rejected",
          retryable: true,
        },
      }),
    })
    renderer.root.add(app)

    app.openThemePicker()
    app.themeBrowser.selectById("theme:tokyonight")
    expect(app.themeBrowser.activateSelected()).toBeTrue()
    await Bun.sleep(10)

    expect(app.themeBrowser.visible).toBeFalse()
    expectCoherentTheme(app, kennelTheme)
    expect(app.state.errors.at(-1)?.code).toBe("setting_rejected")
  })

  test("keeps System synchronized and the theme browser usable in narrow Vim layout", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      historyReader: emptyHistoryReader,
      theme: systemThemeFor("dark"),
      systemThemeMode: "dark",
      keybindings: { preset: "vim" },
    })
    renderer.root.add(app)

    app.openThemePicker()
    await setup.renderOnce()
    expect(renderer.currentFocusedRenderable).toBe(app.themeBrowser.input)
    expect(app.themeBrowser.footer.plainText).toBe("↑↓ preview  ⏎ apply  esc×2 cancel")
    const fullSurface = setup.captureCharFrame()
    expect(app.themeBrowser.x).toBe(0)
    expect(app.themeBrowser.y).toBe(0)
    expect(app.themeBrowser.height).toBe(app.main.height)
    expect(app.themeBrowser.divider.x).toBe(34)
    expect(fullSurface).not.toContain("AGENTS")
    expect(fullSurface).not.toContain("▌ you")
    expect(fullSurface).not.toContain("● rottweiler")
    expect(fullSurface).not.toContain("\n╎")
    expect(app.themeBrowser.detail.plainText).toContain("system  dark · 52 roles resolved · live sample")
    renderer.emit(CliRenderEvents.THEME_MODE, "light")
    await setup.renderOnce()
    expectCoherentTheme(app, systemThemeFor("light"))
    expect(app.themeBrowser.detail.plainText).toContain("system  light · 52 roles resolved · live sample")

    setup.resize(99, 32)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.themeBrowser.layoutMode).toBe("single")
    expect(app.themeBrowser.listPane.width).toBe(97)
    expect(app.themeBrowser.detailPane.visible).toBeFalse()
    expect(app.themeBrowser.compactDetail.visible).toBeTrue()
    expect(app.themeBrowser.compactDetail.plainText).toBe(
      "custom ~/.rottweiler/themes/ · data only, never executed",
    )

    setup.resize(100, 32)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.themeBrowser.layoutMode).toBe("split")
    expect(app.themeBrowser.listPane.width).toBe(33)
    expect(app.themeBrowser.detailPane.x).toBe(35)
    expect(app.themeBrowser.detailPane.width).toBe(64)

    setup.resize(64, 14)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.themeBrowser.layoutMode).toBe("single")
    expect(app.themeBrowser.divider.visible).toBeFalse()
    expect(app.themeBrowser.detailPane.visible).toBeFalse()
    expect(app.themeBrowser.listPane.width).toBeGreaterThan(50)
    expect(app.themeBrowser.compactDetail.visible).toBeTrue()
    expect(app.themeBrowser.compactDetail.plainText).toBe(
      "custom ~/.rottweiler/themes/ · data only, never executed",
    )

    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.themeBrowser.visible).toBeTrue()
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.themeBrowser.visible).toBeFalse()
    expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
  })
})
