import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import type { ClientCommand, CostSnapshot } from "../../src/protocol"
import { parseSessionAction } from "../../src/session-commands"
import { emptySessionReader } from "../fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

test("opens context and cost screens without intercepting engine subcommands", () => {
  expect(parseSessionAction("/context")).toEqual({ type: "context" })
  expect(parseSessionAction("/cost")).toEqual({ type: "cost" })
  expect(parseSessionAction("/context pin conversation:7")).toBeNull()
  expect(parseSessionAction("/cost details")).toBeNull()
})

test("pins the selected context item and refuses edits while a turn runs", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  const commands: ClientCommand[] = []
  const initial = { ...createInitialState(), availableActions: [{ action: "mutate_context" as const, unavailable_reason: null }], context: {
    through: null, turn_id: null, stable_prefix_hash: "fixture", used_tokens: "7000", usable_tokens: "10000", reserved_tokens: "1000", context_window_known: true,
    cache_breakpoints: [], items: [{ item_id: "conversation:7", kind: "conversation" as const, label: "Keep the public API", source: "conversation", machine_local_path: null,
      estimated_tokens: "700", state: { pinned: false, evicted: false, summarized: false, pruned: false } }],
  } }
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: initial,
    onCommand(command) { commands.push(command); return { type: "accepted" } },
  })
  renderer.root.add(app)
  app.openContextPicker()
  await setup.renderOnce()
  expect(commands.at(-1)?.type).toBe("get_context")
  expect(setup.captureCharFrame()).toContain("Keep the public API")
  expect(setup.captureCharFrame()).toContain("Context filling")
  app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "item:conversation:7"))
  app.picker.select.selectCurrent()
  app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "pin"))
  app.picker.select.selectCurrent()
  await Bun.sleep(0)
  expect(commands).toContainEqual(expect.objectContaining({ type: "pin_context", item_id: "conversation:7" }))
  expect(commands.at(-1)?.type).toBe("get_context")
  app.setState({ ...initial, turns: { turn: { turnId: "turn", status: "running", cost: null, usage: null, timing: { kind: "unknown" } } } })
  app.openContextPicker()
  app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "item:conversation:7"))
  app.picker.select.selectCurrent()
  await setup.renderOnce()
  expect(setup.captureCharFrame()).toContain("Wait for the active response")
  const before = commands.filter(command => command.type === "pin_context").length
  app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "pin"))
  app.picker.select.selectCurrent()
  expect(commands.filter(command => command.type === "pin_context")).toHaveLength(before)
  for (const reason of ["Finish the foreground terminal command first.", "Wait for the active child agent or background command to finish.", "Take control of this session first."]) {
    app.setState({ ...initial, availableActions: [{ action: "mutate_context", unavailable_reason: reason }] })
    app.openContextPicker()
    app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "item:conversation:7"))
    app.picker.select.selectCurrent()
    app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "pin"))
    app.picker.select.selectCurrent()
    expect(commands.filter(command => command.type === "pin_context")).toHaveLength(before)
  }
  app.setState({ ...initial, context: { ...initial.context, items: [{ ...initial.context.items[0]!, item_id: "child_completion:42" }] } })
  app.openContextPicker()
  app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "item:child_completion:42"))
  app.picker.select.selectCurrent()
  expect(app.picker.select.options.find(option => option.value === "pin")?.description).toContain("managed by the engine")
  app.picker.select.setSelectedIndex(app.picker.select.options.findIndex(option => option.value === "pin"))
  app.picker.select.selectCurrent()
  expect(commands.filter(command => command.type === "pin_context")).toHaveLength(before)
})

test("cost screen keeps incomplete accounting and token usage visible", async () => {
  const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
  renderer = setup.renderer
  const cost: CostSnapshot = {
      utc_day: "2026-08-22",
      subscription_quota: null,
      session_usage: {
        input_tokens: "0",
        output_tokens: "0",
        cache_read_tokens: "0",
        cache_write_tokens: "0",
        reasoning_tokens: "0",
      },
      session_cost_micros_usd: "0",
      session_ai_credit_micros: "0",
      session_subscription_tokens: "0",
      daily_cost_micros_usd: "0",
      daily_ai_credit_micros: "0",
      daily_subscription_tokens: "0",
      trailing_minute_cost_micros_usd: "0",
      trailing_minute_ai_credit_micros: "0",
      trailing_minute_subscription_tokens: "0",
      cache_hit_basis_points: 0,
      session_cost_cap_micros_usd: null,
      daily_cost_cap_micros_usd: null,
      session_ai_credit_cap_micros: null,
      daily_ai_credit_cap_micros: null,
      session_token_cap: null,
      daily_token_cap: null,
      spend_rate_alarm_micros_usd_per_minute: null,
      ai_credit_rate_alarm_micros_per_minute: null,
      token_rate_alarm_per_minute: null,
      hard_cap_reached: false,
      session_monetary_accounting_complete: true,
      daily_monetary_accounting_complete: true,
      session_subscription_quota_entries: "0",
      session_cost_unavailable_entries: "0",
      session_non_usd_monetary_entries: "0",
      daily_subscription_quota_entries: "0",
      daily_cost_unavailable_entries: "0",
      daily_non_usd_monetary_entries: "0",
    }
  const commands: ClientCommand[] = []
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: {
    ...createInitialState(), cost: { ...cost, session_monetary_accounting_complete: false, session_cost_unavailable_entries: "1" },
  }, onCommand(command) { commands.push(command); return { type: "accepted" } } })
  renderer.root.add(app)
  app.openCostPicker()
  await setup.renderOnce()
  expect(commands.at(-1)?.type).toBe("get_cost")
  expect(setup.captureCharFrame()).toContain("Usage & cost")
  expect(app.picker.select.options.find(option => option.name === "Known USD charges")?.description).toContain("incomplete accounting")
  expect(app.picker.select.options.find(option => option.name === "Unavailable pricing entries")?.description).toBe("1")
})
