import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import type { ClientCommand, CommandOutcome } from "../../src/protocol"
import { kennelTheme, themeByName, themeCatalog } from "../../src/theme"
import { emptySessionReader } from "../fixtures/history"
import { expectCoherentTheme } from "./fixtures"

describe("theme confirmation lifetime", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  async function setup() {
    const native = await createTestRenderer({ width: 100, height: 24, useThread: false })
    renderer = native.renderer
    const pending = Promise.withResolvers<CommandOutcome>()
    const commands: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader, theme: kennelTheme,
      onCommand(command) {
        commands.push(command)
        return command.type === "set_setting" && command.key === "ui.theme"
          ? pending.promise : { type: "accepted" }
      },
    })
    renderer.root.add(app)
    const selected = themeByName("tokyonight")!
    app.openThemePicker()
    app.themeBrowser.selectById(`theme:${selected.name}`)
    app.themeBrowser.activateSelected()
    expect(app.themeBrowser.footer.plainText).toContain("Saving theme")
    return { app, pending, commands, selected }
  }

  test("one confirmation owns its selection until persistence settles", async () => {
    const { app, pending, commands, selected } = await setup()
    const other = themeCatalog.find(theme => theme.name !== selected.name && theme.name !== "system")!
    app.themeBrowser.selectById(`theme:${other.name}`)
    app.themeBrowser.activateSelected()
    expect(commands.filter(command => command.type === "set_setting")).toHaveLength(1)
    expectCoherentTheme(app, selected)
    pending.resolve({ type: "accepted" })
    await Bun.sleep(0)
    expectCoherentTheme(app, selected)
    expect(app.themeBrowser.visible).toBe(false)
  })

  for (const completion of ["accepted", "failed"] as const) {
    test(`replacing a theme interaction reverts its preview and ignores its ${completion} completion`, async () => {
      const { app, pending } = await setup()
      app.openSettingsPicker()
      expect(app.settingsBrowser.visible).toBe(true)
      expectCoherentTheme(app, kennelTheme)
      const errors = app.state.errors.length
      if (completion === "accepted") pending.resolve({ type: "accepted" })
      else pending.reject(new Error("persistence failed"))
      await Bun.sleep(0)
      expect(app.settingsBrowser.visible).toBe(true)
      expect(app.state.errors).toHaveLength(errors)
      expectCoherentTheme(app, kennelTheme)
    })
  }

  test("renderer destruction settles the preview owner without rebuilding on a late acknowledgement", async () => {
    const { app, pending } = await setup()
    const browser = app.themeBrowser
    renderer!.destroy()
    renderer = undefined
    pending.resolve({ type: "accepted" })
    await Bun.sleep(0)
    expect(browser.input.isDestroyed).toBe(true)
    expect(app.themeBrowser).toBe(browser)
  })
})
