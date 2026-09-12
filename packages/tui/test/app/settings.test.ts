import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import {
  createRottweilerApp
} from "../../src/app"
import type { ClientCommand, EngineEvent } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"
import { visionCapableState } from "./fixtures"

describe("Rottweiler settings", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  test("configures human-friendly budget limits from palette presets and custom prompts", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        settings: [
          {
            key: "budget.session_cost_cap_micros_usd",
            label: "Session cost cap",
            value: "$12.50",
            choices: [],
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "budget.daily_cost_cap_micros_usd",
            label: "Daily cost cap",
            value: "Unlimited",
            choices: [],
            provenance: "built-in",
            appliesImmediately: false,
          },
          {
            key: "budget.session_token_cap",
            label: "Session token cap",
            value: "250,000 tokens",
            choices: [],
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "budget.daily_token_cap",
            label: "Daily token cap",
            value: "1,000,000 tokens",
            choices: [],
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "budget.token_rate_alarm_per_minute",
            label: "Token rate alarm",
            value: "100,000 tokens/min",
            choices: [],
            provenance: "user",
            appliesImmediately: false,
          },
          {
            key: "budget.warn_at_percent",
            label: "Budget warning",
            value: "80%",
            choices: [],
            provenance: "user",
            appliesImmediately: false,
          },
        ],
      },
      onCommand(command) {
        emitted.push(command)
        if (command.type === "set_setting" && command.value === "0") {
          return {
            type: "rejected",
            error: {
              category: "config",
              code: "invalid_user_setting",
              message: "warning threshold must be an integer from 1 through 100",
              retryable: false,
            },
          }
        }
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openCommandPicker()
    const paletteOptions = app.commandPalette.itemIds
    const permissionsIndex = paletteOptions.indexOf("permissions.manage")
    const budgetIndex = paletteOptions.indexOf("budget.manage")
    expect(budgetIndex).toBe(permissionsIndex + 1)
    app.commandPalette.selectById("budget.manage")
    expect(app.commandPalette.detail.plainText).toContain("Budget limits")
    expect(app.commandPalette.detail.plainText).toContain("Set spend and subscription-token limits")
    app.commandPalette.activateSelected()

    expect(app.picker.title).toContain("Budget limits")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "Session limit · $12.50",
      "Daily limit · Unlimited",
      "Session tokens · 250,000 tokens",
      "Daily tokens · 1,000,000 tokens",
      "Token rate alarm · 100,000 tokens/min",
      "Warn at · 80%",
    ])
    expect(app.picker.select.options.map((option) => option.description)).toEqual([
      "Maximum spend for this session · user · next session",
      "Maximum spend per UTC day · built-in · next session",
      "Maximum subscription tokens for this session · user · next session",
      "Maximum subscription tokens per UTC day · user · next session",
      "Alert when one minute of subscription usage reaches this value · user · next session",
      "Warn when a configured cap reaches this percentage · user · next session",
    ])

    const sessionIndex = app.picker.select.options.findIndex(
      (option) => option.value === "budget.setting.budget.session_cost_cap_micros_usd",
    )
    app.picker.select.setSelectedIndex(sessionIndex)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Session limit")
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "$5",
      "$10",
      "$20",
      "$50",
      "$100",
      "Unlimited",
      "Custom amount…",
    ])
    const twentyIndex = app.picker.select.options.findIndex(
      (option) => option.value === "budget.preset.budget.session_cost_cap_micros_usd.20",
    )
    app.picker.select.setSelectedIndex(twentyIndex)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "budget.session_cost_cap_micros_usd",
      value: "20",
    }))

    app.openBudgetPicker()
    const warningIndex = app.picker.select.options.findIndex(
      (option) => option.value === "budget.setting.budget.warn_at_percent",
    )
    app.picker.select.setSelectedIndex(warningIndex)
    app.picker.select.selectCurrent()
    expect(app.picker.select.options.map((option) => option.name)).toEqual([
      "50%",
      "75%",
      "80%",
      "90%",
      "Custom…",
    ])
    const eightyIndex = app.picker.select.options.findIndex(
      (option) => option.value === "budget.preset.budget.warn_at_percent.80",
    )
    app.picker.select.setSelectedIndex(eightyIndex)
    app.picker.select.selectCurrent()
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "budget.warn_at_percent",
      value: "80",
    }))

    app.openBudgetPicker()
    const customWarningIndex = app.picker.select.options.findIndex(
      (option) => option.value === "budget.setting.budget.warn_at_percent",
    )
    app.picker.select.setSelectedIndex(customWarningIndex)
    app.picker.select.selectCurrent()
    const customWarningPreset = app.picker.select.options.findIndex(
      (option) => option.value === "budget.preset.budget.warn_at_percent.custom",
    )
    app.picker.select.setSelectedIndex(customWarningPreset)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Warning threshold as a percent, e.g. 70")
    await setup.mockInput.typeText("0")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(app.banner.plainText).toContain("warning threshold must be an integer from 1 through 100")

    app.openBudgetPicker()
    const dailyIndex = app.picker.select.options.findIndex(
      (option) => option.value === "budget.setting.budget.daily_cost_cap_micros_usd",
    )
    app.picker.select.setSelectedIndex(dailyIndex)
    app.picker.select.selectCurrent()
    const customIndex = app.picker.select.options.findIndex(
      (option) => option.value === "budget.preset.budget.daily_cost_cap_micros_usd.custom",
    )
    app.picker.select.setSelectedIndex(customIndex)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Daily limit in USD, e.g. 12.50")
    expect(app.picker.input.placeholder).toBe("12.50")
    await setup.mockInput.typeText("12.50")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "set_setting",
      key: "budget.daily_cost_cap_micros_usd",
      value: "12.50",
    }))
  })

  test("ignores an older settings response after a newer settings change response", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        settings: [{
          key: "compaction.auto",
          label: "Automatic compaction",
          value: "true",
          choices: ["true", "false"],
          provenance: "user",
          appliesImmediately: false,
        }],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openSettingsPicker()
    const olderRequest = emitted.findLast((command) => command.type === "list_settings")
    app.settingsBrowser.selectById("compaction.auto")
    app.settingsBrowser.activateSelected()
    const disabled = app.picker.select.options.findIndex((option) => option.value === "false")
    app.picker.select.setSelectedIndex(disabled)
    app.picker.select.selectCurrent()
    const newerRequest = emitted.findLast((command) => command.type === "set_setting")
    expect(olderRequest?.type).toBe("list_settings")
    expect(newerRequest?.type).toBe("set_setting")

    const settingsListed = (requestId: string, value: string): EngineEvent => ({
      type: "settings_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui",
        request_id: requestId,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      settings: [{
        key: "compaction.auto",
        label: "Automatic compaction",
        value,
        choices: ["true", "false"],
        provenance: "user",
        applies_immediately: false,
      }],
    })

    app.handleEvent(settingsListed(newerRequest!.meta.request_id, "false"))
    app.handleEvent(settingsListed(olderRequest!.meta.request_id, "true"))

    expect(app.state.settings).toEqual([
      expect.objectContaining({ key: "compaction.auto", value: "false" }),
    ])
  })

  test("opens the retained Settings browser and applies authoritative choices immediately", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        settings: [
          { key: "compaction.auto", label: "Automatic compaction", value: "true", choices: ["true", "false"], provenance: "user", appliesImmediately: false },
          { key: "ui.theme", label: "Theme", value: "opencode", choices: [], provenance: "user", appliesImmediately: false },
          { key: "project.models.default", label: "Project default model", value: "fast", choices: ["fast"], provenance: "private project preference", appliesImmediately: false },
        ],
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openSettingsPicker()
    await setup.renderOnce()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({ type: "list_settings" }))
    expect(app.settingsBrowser.visible).toBeTrue()
    expect(app.picker.visible).toBeFalse()
    expect(app.settingsBrowser.divider.x).toBe(30)
    expect(app.settingsBrowser.detailPane.x).toBe(31)

    app.settingsBrowser.selectById("compaction.auto")
    expect(app.settingsBrowser.activateSelected()).toBeTrue()
    expect(app.picker.visible).toBeTrue()
    const disabled = app.picker.select.options.findIndex((option) => option.value === "false")
    app.picker.select.setSelectedIndex(disabled)
    app.picker.select.selectCurrent()
    expect(emitted.at(-1)).toEqual(expect.objectContaining({
      type: "set_setting",
      key: "compaction.auto",
      value: "false",
    }))
    expect(app.settingsBrowser.visible).toBeTrue()
    const write = emitted.at(-1)
    if (write?.type !== "set_setting") throw new Error("missing settings write")
    app.handleEvent({
      type: "settings_listed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "ui",
        request_id: write.meta.request_id,
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-local",
      settings: [
        { key: "compaction.auto", label: "Automatic compaction", value: "false", choices: ["true", "false"], provenance: "user", applies_immediately: false },
        { key: "ui.theme", label: "Theme", value: "opencode", choices: [], provenance: "user", applies_immediately: false },
        { key: "project.models.default", label: "Project default model", value: "fast", choices: ["fast"], provenance: "private project preference", applies_immediately: false },
      ],
    })
    app.settingsBrowser.selectById("compaction.auto")
    expect(app.settingsBrowser.detail.plainText).toContain("current    false")

    const beforeInspect = emitted.length
    app.settingsBrowser.selectById("project.models.default")
    expect(app.settingsBrowser.footer.plainText).toContain("Read only")
    expect(app.settingsBrowser.activateSelected()).toBeTrue()
    expect(emitted).toHaveLength(beforeInspect)

    app.settingsBrowser.selectById("ui.theme")
    expect(app.settingsBrowser.activateSelected()).toBeTrue()
    expect(app.settingsBrowser.visible).toBeFalse()
    expect(app.themeBrowser.visible).toBeTrue()
  })

  test("keeps cached Settings truthful on rejection and retries a failed list", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    let listAttempts = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        settings: [{ key: "compaction.auto", label: "Automatic compaction", value: "true", choices: ["true", "false"], provenance: "user", appliesImmediately: false }],
      },
      onCommand(command) {
        if (command.type === "list_settings") {
          listAttempts += 1
          return {
            type: "rejected",
            error: { category: "protocol", code: "settings_unavailable", message: "settings unavailable", retryable: true },
          }
        }
        if (command.type === "set_setting") {
          return {
            type: "rejected",
            error: { category: "protocol", code: "setting_rejected", message: "setting rejected", retryable: true },
          }
        }
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)

    app.openSettingsPicker()
    await Bun.sleep(10)
    await setup.renderOnce()
    expect(app.settingsBrowser.itemIds).toContain("compaction.auto")
    expect(app.settingsBrowser.footer.plainText).toContain("settings unavailable")
    expect(listAttempts).toBe(1)
    setup.mockInput.pressKey("r", { ctrl: true })
    await Bun.sleep(0)
    expect(listAttempts).toBe(2)

    app.settingsBrowser.selectById("compaction.auto")
    app.settingsBrowser.activateSelected()
    const disabled = app.picker.select.options.findIndex((option) => option.value === "false")
    app.picker.select.setSelectedIndex(disabled)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(app.settingsBrowser.visible).toBeTrue()
    expect(app.settingsBrowser.detail.plainText).toContain("current    true")
    expect(app.banner.plainText).toContain("setting rejected")
  })

  test("keeps Settings full-primary, responsive, and explicit about Vim Escape", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      keybindings: { preset: "vim" },
      initialState: {
        ...createInitialState(),
        settings: [{ key: "compaction.auto", label: "Automatic compaction", value: "true", choices: ["true", "false"], provenance: "user", appliesImmediately: false }],
      },
    })
    renderer.root.add(app)
    app.openSettingsPicker()
    await setup.renderOnce()

    expect(app.settingsBrowser.x).toBe(0)
    expect(app.settingsBrowser.y).toBe(0)
    expect(app.settingsBrowser.height).toBe(app.main.height)
    expect(app.settingsBrowser.divider.x).toBe(30)
    expect(app.settingsBrowser.detailPane.x).toBe(31)
    expect(app.settingsBrowser.footer.plainText).toContain("Esc×2 close")

    setup.resize(89, 18)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.settingsBrowser.layoutMode).toBe("single")
    expect(app.settingsBrowser.divider.visible).toBeFalse()
    expect(app.settingsBrowser.detailPane.visible).toBeFalse()
    expect(app.settingsBrowser.compactDetail.plainText).toContain("current    true")

    setup.resize(72, 18)
    await setup.renderOnce()
    await setup.renderOnce()
    expect(app.settingsBrowser.listPane.width).toBe(70)
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.settingsBrowser.visible).toBeTrue()
    setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(app.settingsBrowser.visible).toBeFalse()
  })

  test("returns to the retained Settings browser when a value chooser is cancelled", async () => {
    const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        settings: [{
          key: "compaction.auto",
          label: "Automatic compaction",
          value: "true",
          choices: ["true", "false"],
          provenance: "user",
          appliesImmediately: false,
        }],
      },
    })
    renderer.root.add(app)

    app.openSettingsPicker()
    app.settingsBrowser.selectById("compaction.auto")
    app.settingsBrowser.activateSelected()
    expect(app.picker.visible).toBeTrue()
    expect(app.settingsBrowser.visible).toBeFalse()

    setup.mockInput.pressEscape()
    await Bun.sleep(30)

    expect(app.picker.visible).toBeFalse()
    expect(app.settingsBrowser.visible).toBeTrue()
    expect(renderer.currentFocusedRenderable).toBe(app.settingsBrowser.input)
    expect(app.settingsBrowser.selectedId).toBe("compaction.auto")
  })

  test("derives palette binding hints from custom compiled global bindings", () => {
    const setup = createTestRenderer({ width: 80, height: 18, useThread: false })
    return setup.then(({ renderer: testRenderer }) => {
      renderer = testRenderer
      const app = createRottweilerApp(testRenderer, {
        sessionReader: emptySessionReader,
        keybindings: {
          bindings: { global: { open_model_picker: "ctrl+k" } },
        },
      })
      testRenderer.root.add(app)
      app.openCommandPicker()
      app.commandPalette.selectById("model.list")
      expect(app.commandPalette.detail.plainText).toContain("Ctrl+K")
      expect(app.commandPalette.detail.plainText).not.toContain("Ctrl+M")
      expect(app.statusLine.plainText).toContain("model not selected · Ctrl+K")
    })
  })

  test("derives composer discovery hints and omits unbound actions", () => {
    const setup = createTestRenderer({ width: 80, height: 18, useThread: false })
    return setup.then(({ renderer: testRenderer }) => {
      renderer = testRenderer
      const app = createRottweilerApp(testRenderer, {
        sessionReader: emptySessionReader,
        initialState: visionCapableState(),
        keybindings: {
          bindings: {
            global: { paste_image: "ctrl+k" },
            standard: { open_external_editor: [] },
          },
        },
      })
      testRenderer.root.add(app)
      expect(app.composer.hintText.plainText).toContain("Ctrl+K image")
      expect(app.composer.hintText.plainText).not.toContain("Ctrl+V image")
      expect(app.composer.hintText.plainText).not.toContain("$EDITOR")
    })
  })

  test("marks the current permission mode and confirms yolo before sending", async () => {
    const setup = await createTestRenderer({ width: 80, height: 18, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      initialState: {
        ...createInitialState(),
        connection: { phase: "connected", attempt: 0, error: null, gap: null },
        permissions: {
          default: "ask",
          effective_rules: [],
          project_rules: [],
          session_rules: [],
          approvals: [],
          truncated: false,
          runtime_mode: "auto-safe",
        },
      },
      onCommand(command) {
        emitted.push(command)
        return { type: "accepted" }
      },
    })
    renderer.root.add(app)
    app.openPermissionModePicker()

    expect(app.picker.select.options.map((option) => option.value)).toEqual([
      "permissions.mode.strict",
      "permissions.mode.auto-safe",
      "permissions.mode.yolo",
      "permissions.mode.default",
    ])
    expect(app.picker.select.options.find(
      (option) => option.value === "permissions.mode.auto-safe",
    )?.name).toBe("● auto-safe")

    const yoloIndex = app.picker.select.options.findIndex(
      (option) => option.value === "permissions.mode.yolo",
    )
    app.picker.select.setSelectedIndex(yoloIndex)
    app.picker.select.selectCurrent()
    expect(app.picker.title).toContain("Run every tool without asking?")
    expect(emitted.some(
      (command) => command.type === "send_message" && command.content === "/permissions mode yolo",
    )).toBeFalse()

    const confirmIndex = app.picker.select.options.findIndex(
      (option) => option.value === "permissions.yolo.confirm",
    )
    app.picker.select.setSelectedIndex(confirmIndex)
    app.picker.select.selectCurrent()
    await Bun.sleep(0)
    expect(emitted).toContainEqual(expect.objectContaining({
      type: "send_message",
      content: "/permissions mode yolo",
    }))
  })
})
