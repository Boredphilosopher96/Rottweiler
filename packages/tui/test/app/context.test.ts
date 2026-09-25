import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import type { ClientCommand, CostSnapshot } from "../../src/protocol"
import { resolveSlashInput } from "../../src/session-commands"
import { emptySessionReader } from "../fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

test("opens context and cost screens without intercepting engine subcommands", () => {
  expect(resolveSlashInput("/context")).toEqual({ type: "screen", name: "context" })
  expect(resolveSlashInput("/cost")).toEqual({ type: "screen", name: "usage" })
  expect(resolveSlashInput("/context pin conversation:7")).toEqual({ type: "engine", content: "/context pin conversation:7" })
  expect(resolveSlashInput("/cost details")).toEqual({ type: "invalid", message: "usage: /usage" })
})

const contextItem = (item_id: string, kind: "system" | "tool_definitions" | "conversation", label: string, estimated_tokens: string) => ({
  item_id, kind, label, source: "fixture", machine_local_path: null, estimated_tokens,
  state: { pinned: false, evicted: false, summarized: false, pruned: false },
})

test("groups context by category with totals, a usage meter, and warning tiers", async () => {
  const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
  renderer = setup.renderer
  const commands: ClientCommand[] = []
  const context = {
    through: null, turn_id: null, stable_prefix_hash: "fixture", used_tokens: "7000", usable_tokens: "10000", reserved_tokens: "1000",
    context_window_known: true, cache_breakpoints: [], items: [
      contextItem("system:0", "system", "System prompt", "2000"),
      contextItem("tool:bash", "tool_definitions", "bash", "1000"),
      contextItem("conversation:1", "conversation", "User turn 1", "4000"),
    ],
  }
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: { ...createInitialState(), context },
    onCommand(command) { commands.push(command); return { type: "accepted" } } })
  renderer.root.add(app)
  app.openContextPicker()
  await setup.renderOnce()
  expect(commands.map(command => command.type)).toEqual(expect.arrayContaining(["list_commands", "get_context"]))
  expect(app.picker.items.map(item => [item.label, item.hint])).toEqual([
    ["System & instructions", "2.0k · 29%"],
    ["Tools", "1.0k · 14%"],
    ["Conversation", "4.0k · 57%"],
    ["Free", "3.0k · 30%"],
  ])
  expect(app.picker.items.some(item => item.label.includes("Refresh"))).toBe(false)
  const frame = setup.captureCharFrame()
  expect(frame).toContain("CONTEXT")
  expect(frame).toContain("70% · 7.0k/10k")
  expect(app.picker.footer.plainText).toContain("filling up")
  app.setState({ ...app.state, context: { ...context, used_tokens: "9000" } })
  expect(app.picker.footer.plainText).toContain("near limit")
  app.setState({ ...app.state, context: { ...context, used_tokens: "1000" } })
  expect(app.picker.footer.plainText).not.toContain("filling")

  app.picker.selectById("context.category.tools")
  app.picker.activateSelected()
  expect(app.picker.screenTitle).toBe("CONTEXT › Tools")
  expect(app.picker.items.map(item => item.label)).toEqual(["bash"])
  expect(app.picker.selectedItem?.primary).toBeNull()
  expect(app.picker.footer.plainText).not.toContain("remove")
  setup.mockInput.pressEscape()
  await Bun.sleep(30)
  expect(app.picker.screenTitle).toBe("CONTEXT")
  expect(app.picker.visible).toBe(true)
})

test("pins and removes conversation items directly and refuses edits while a turn runs", async () => {
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
  const openConversation = () => {
    app.openContextPicker()
    app.picker.selectById("context.category.conversation")
    app.picker.activateSelected()
  }
  openConversation()
  await setup.renderOnce()
  expect(commands.at(-1)?.type).toBe("get_context")
  expect(setup.captureCharFrame()).toContain("Keep the public API")
  expect(app.picker.footer.plainText).toBe("⏎ pin · ctrl+d remove · ctrl+k compact · esc back · filling up")
  app.picker.activateSelected()
  await Bun.sleep(0)
  expect(commands).toContainEqual(expect.objectContaining({ type: "pin_context", item_id: "conversation:7" }))
  expect(commands.at(-1)?.type).toBe("get_context")
  setup.mockInput.pressKey("d", { ctrl: true })
  await Bun.sleep(0)
  expect(commands).toContainEqual(expect.objectContaining({ type: "evict_context", item_id: "conversation:7" }))

  app.setState({ ...initial, turns: { turn: { turnId: "turn", status: "running", cost: null, usage: null, timing: { kind: "unknown" } } } })
  openConversation()
  await setup.renderOnce()
  expect(app.picker.selectedItem?.detail).toContain("Wait for the active response")
  const before = commands.filter(command => command.type === "pin_context" || command.type === "evict_context").length
  app.picker.activateSelected()
  setup.mockInput.pressKey("d", { ctrl: true })
  for (const reason of ["Finish the foreground terminal command first.", "Wait for the active child agent or background command to finish.", "Take control of this session first."]) {
    app.setState({ ...initial, availableActions: [{ action: "mutate_context", unavailable_reason: reason }] })
    openConversation()
    expect(app.picker.selectedItem?.detail).toContain(reason)
    app.picker.activateSelected()
    setup.mockInput.pressKey("d", { ctrl: true })
  }
  app.setState({ ...initial, context: { ...initial.context, items: [{ ...initial.context.items[0]!, item_id: "child_completion:42" }] } })
  openConversation()
  expect(app.picker.selectedItem?.detail).toContain("Managed by the engine")
  app.picker.activateSelected()
  expect(commands.filter(command => command.type === "pin_context" || command.type === "evict_context")).toHaveLength(before)
})

test("compacts from the context screen", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  const commands: ClientCommand[] = []
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: { ...createInitialState(), context: {
    through: null, turn_id: null, stable_prefix_hash: "fixture", used_tokens: "100", usable_tokens: "0", reserved_tokens: "0",
    context_window_known: false, cache_breakpoints: [], items: [contextItem("conversation:1", "conversation", "User turn 1", "100")],
  } }, onCommand(command) { commands.push(command); return { type: "accepted" } } })
  renderer.root.add(app)
  app.openContextPicker()
  await setup.renderOnce()
  expect(setup.captureCharFrame()).toContain("100 used · context limit unknown")
  expect(app.picker.items.map(item => item.label)).not.toContain("Free")
  setup.mockInput.pressKey("k", { ctrl: true })
  expect(commands.at(-1)).toMatchObject({ type: "compact", instructions: null })
  expect(app.picker.visible).toBe(false)
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
  expect(setup.captureCharFrame()).toContain("USAGE")
  expect(app.picker.sectionLabels).toEqual(["This session", "Charges", "Limits"])
  expect(app.picker.items.find(item => item.label === "Known USD charges")?.hint).toContain("incomplete")
  expect(app.picker.items.find(item => item.label === "Requests without a price")?.hint).toBe("1")
  expect(app.picker.items.some(item => item.label.startsWith("Refresh"))).toBe(false)
  app.picker.selectById("usage.budget")
  app.picker.activateSelected()
  expect(app.picker.screenTitle).toBe("BUDGET LIMITS")
})
