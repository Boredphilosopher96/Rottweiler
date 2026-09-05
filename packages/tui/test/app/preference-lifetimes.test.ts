import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import type { TextPromptOptions } from "../../src/components"
import type { ClientCommand } from "../../src/protocol"
import { createInitialState } from "../../src/state"
import { emptyHistoryReader } from "../fixtures/history"

const budgetKeys = ["budget.session_cost_cap_micros_usd", "budget.daily_cost_cap_micros_usd", "budget.session_token_cap", "budget.daily_token_cap", "budget.token_rate_alarm_per_minute", "budget.warn_at_percent"]

describe("preference interaction ownership", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  for (const feature of ["budget", "permission"] as const) {
    test(`${feature} prompt rejects a captured callback after session change and destruction`, async () => {
      const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
      renderer = setup.renderer
      const emitted: ClientCommand[] = []
      const app = createRottweilerApp(renderer, {
        historyReader: emptyHistoryReader, sessionId: "first",
        initialState: { ...createInitialState(),
          settings: budgetKeys.map(key => ({ key, label: key, value: "10", choices: [], provenance: "user", appliesImmediately: true })),
          permissions: { default: "ask", effective_rules: [], project_rules: [], session_rules: [], approvals: [], truncated: false },
        },
        onCommand: command => { emitted.push(command); return { type: "accepted" } },
      })
      renderer.root.add(app)
      const prompts: TextPromptOptions[] = []
      const openPrompt = app.picker.openTextPrompt.bind(app.picker)
      app.picker.openTextPrompt = options => { prompts.push(options); openPrompt(options) }
      const select = (id: string) => {
        const index = app.picker.select.options.findIndex(option => option.value === id)
        expect(index).toBeGreaterThanOrEqual(0)
        app.picker.select.setSelectedIndex(index)
        app.picker.select.selectCurrent()
      }
      const open = () => {
        if (feature === "budget") {
          app.openBudgetPicker()
          select("budget.setting.budget.session_cost_cap_micros_usd")
          select("budget.preset.budget.session_cost_cap_micros_usd.custom")
        } else {
          app.openPermissionPicker()
          select("permissions.add.allow")
        }
      }
      open()
      const replaced = prompts.at(-1)!
      app.openModePicker()
      const replacementCommands = emitted.length
      replaced.onSubmit(feature === "budget" ? "25" : "bash(*)")
      expect(emitted).toHaveLength(replacementCommands)
      open()
      const abandoned = prompts.at(-1)!
      app.picker.input.value = "unfinished"
      app.setSessionId("second")
      expect(app.picker.visible).toBe(false)
      open()
      const before = emitted.length
      abandoned.onSubmit(feature === "budget" ? "25" : "bash(*)")
      expect(emitted).toHaveLength(before)
      const current = prompts.at(-1)!
      current.onSubmit(feature === "budget" ? "25" : "bash(cargo test*)")
      expect(emitted.at(-1)).toMatchObject({ type: feature === "budget" ? "set_setting" : "add_session_permission_rule", session_id: "second" })
      open()
      const destroyed = prompts.at(-1)!
      app.destroy()
      const settled = emitted.length
      destroyed.onSubmit(feature === "budget" ? "50" : "bash(*)")
      expect(emitted).toHaveLength(settled)
    })
  }
})
