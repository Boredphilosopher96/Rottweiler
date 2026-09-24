import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../src/app"
import { launchSummary } from "../src/render/launch"
import { appendSessionError, MAX_ERROR_HISTORY } from "../src/state/errors"
import { createInitialState, reduceRottweilerState, transportDisconnected } from "../src/state"
import { PROTOCOL_VERSION, type ContextSnapshot } from "../src/protocol"
import { meta, reduce } from "./state/fixtures"
import { emptySessionReader } from "./fixtures/history"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })
const failure = { category: "provider", code: "provider_failed", message: "Provider rejected the selected context length", retryable: false } as const
const context: ContextSnapshot = { through: null, turn_id: null, stable_prefix_hash: "", used_tokens: "20", usable_tokens: "100", reserved_tokens: "0", context_window_known: true, cache_breakpoints: [], items: [] }

test("launch distinguishes unknown inventories, loaded empty inventories and observed instructions", () => {
  let state = createInitialState()
  expect(launchSummary(state, 80)).toContain("MCP · not loaded")
  expect(launchSummary(state, 80)).toContain("Skill commands · not loaded")
  const reply = { protocol_version: PROTOCOL_VERSION, client_id: "test", request_id: "catalog", emitted_at: "2026-09-15T00:00:00Z" }
  state = reduce(state, { type: "command_descriptors_listed", meta: reply, session_id: "session-state", commands: [], available_actions: [], truncated: false })
  state = reduce(state, { type: "mcp_servers_listed", meta: reply, session_id: "session-state", servers: [] })
  state = { ...state, context, workspaceStatus: { workspaceName: "Rottweiler", branch: "feat/ux", changedPaths: [], truncated: false } }
  expect(launchSummary(state, 80)).toContain("Workspace · Rottweiler · feat/ux")
  expect(launchSummary(state, 80)).toContain("Instructions · none active")
  expect(launchSummary(state, 80)).toContain("MCP · 0 ready / 0 configured")
  expect(launchSummary(state, 80)).toContain("Skill commands · 0")
  state = { ...state, context: { ...context, items: [{ item_id: "instructions", kind: "project_instructions", label: "AGENTS.md", source: "workspace", machine_local_path: null, estimated_tokens: "20", state: { pinned: false, evicted: false, summarized: false, pruned: false } }] }, commands: [{ name: "review", description: "Review", usage: "/review", source: "skill" }], commandsTruncated: true }
  expect(launchSummary(state, 80)).toContain("AGENTS.md (workspace)")
  expect(launchSummary(state, 80)).toContain("Skill commands · 1+ · catalog truncated · /review")
  state = reduceRottweilerState(state, transportDisconnected(1))
  expect(launchSummary(state, 80)).toContain("MCP · not loaded")
})

test("launch resolves an active alias without inventing an unresolved model", () => {
  const state = { ...createInitialState(), model: "fast", models: [{ id: "openai/coding", provider: "openai", displayName: "Coding", aliases: ["fast"], available: true, current: true, status: null, vision: false, thinking: true, toolCalling: true }] }
  expect(launchSummary(state, 80)).toContain("Model · Coding · openai")
  expect(launchSummary({ ...state, model: "missing" }, 80)).toContain("Choose a model")
})

test("error history retains bounded scalar details through successful activity", () => {
  let state = reduce(createInitialState(), { type: "error", meta: meta("1"), error: failure })
  state = reduce(state, { type: "turn_started", meta: meta("2"), turn_id: "turn" })
  expect(state.errors).toHaveLength(1)
  expect(state.errorHistory[0]?.message).toBe(failure.message)
  state = reduce(state, { type: "guard_triggered", meta: meta("3"), turn_id: "turn", guard: "test", message: "Guard stopped the turn" })
  expect(state.errorHistory.at(-1)?.message).toBe("Guard stopped the turn")
  for (let index = 0; index < 100; index++) state = appendSessionError(state, { ...failure, message: `\u001b[31m${"x".repeat(5000)}`, details: { huge: "discard".repeat(1000) } })
  expect(state.errorHistory).toHaveLength(MAX_ERROR_HISTORY)
  expect(state.errorHistory.every(error => Buffer.byteLength(error.message) <= 2048)).toBeTrue()
  expect(state.errorHistory.at(-1)).not.toHaveProperty("details")
  expect(state.errorHistory.at(-1)?.message).not.toContain("\u001b")
  expect(createInitialState().errorHistory).toEqual([])
})

for (const [width, height] of [[110, 32], [80, 24]] as const) test(`launch and retained error details at ${width}x${height}`, async () => {
  const setup = await createTestRenderer({ width, height, useThread: false })
  renderer = setup.renderer
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, onCommand(command) { return command.type === "send_message" ? { type: "rejected", error: failure } : { type: "accepted" } } })
  renderer.root.add(app)
  await setup.flush()
  expect(setup.captureCharFrame()).toContain("Workspace · not loaded")
  expect(setup.captureCharFrame()).toContain("Skill commands · not loaded")
  expect(app.composer.y + app.composer.height).toBeLessThan(height)
  expect(setup.captureCharFrame()).toMatchSnapshot("launch")
  app.composer.value = "Try this"
  await app.composer.submit()
  expect(app.state.errorHistory).toHaveLength(1)
  app.setState({ ...app.state, errors: [] })
  app.composer.value = "/errors"
  expect(await app.composer.submit()).toBeTrue()
  expect(app.picker.title).toContain("Recent errors")
  app.picker.select.selectCurrent()
  await setup.flush()
  expect(app.picker.title).toContain("details")
  expect(app.picker.select.options.some(option => option.name.includes("Provider rejected"))).toBeTrue()
  expect(setup.captureCharFrame()).toMatchSnapshot()
  app.setSessionId("another-session")
  expect(app.state.errorHistory).toEqual([])
})

test("dismisses notices explicitly without deleting retained failure details", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: appendSessionError(createInitialState(), failure) })
  renderer.root.add(app)
  app.openErrorsPicker()
  const dismiss = app.picker.select.options.findIndex(option => option.value === "dismiss")
  expect(dismiss).toBeGreaterThanOrEqual(0)
  app.picker.select.setSelectedIndex(dismiss)
  app.picker.select.selectCurrent()
  expect(app.state.errors).toEqual([])
  expect(app.state.errorHistory).toHaveLength(1)
  expect(app.picker.select.options.some(option => option.value === "error:1")).toBeTrue()
})
