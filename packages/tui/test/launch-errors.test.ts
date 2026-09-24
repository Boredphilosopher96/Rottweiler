import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../src/app"
import { launchSummary } from "../src/render/launch"
import { appendSessionError, MAX_ERROR_HISTORY } from "../src/state/errors"
import { createInitialState, reduceRottweilerState, transportDisconnected } from "../src/state"
import { PROTOCOL_VERSION, type ContextSnapshot } from "../src/protocol"
import { meta, reduce } from "./state/fixtures"
import { emptySessionReader } from "./fixtures/history"
import { options } from "./picker-screen"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })
const failure = { category: "provider", code: "provider_failed", message: "Provider rejected the selected context length", retryable: false } as const
const context: ContextSnapshot = { through: null, turn_id: null, stable_prefix_hash: "", used_tokens: "20", usable_tokens: "100", reserved_tokens: "0", context_window_known: true, cache_breakpoints: [], items: [] }

test("launch is a compact welcome that shows only observed, non-empty inventories", () => {
  let state = createInitialState()
  expect(launchSummary(state, 80)).toBe("Rottweiler\nLoading models…\nDescribe a task, or press / for commands.")
  const reply = { protocol_version: PROTOCOL_VERSION, client_id: "test", request_id: "catalog", emitted_at: "2026-09-15T00:00:00Z" }
  state = reduce(state, { type: "command_descriptors_listed", meta: reply, session_id: "session-state", commands: [], available_actions: [], truncated: false })
  state = reduce(state, { type: "mcp_servers_listed", meta: reply, session_id: "session-state", servers: [] })
  state = { ...state, context, workspaceStatus: { workspaceName: "Rottweiler", branch: "feat/ux", changes: [], truncated: false } }
  const empty = launchSummary(state, 80)
  expect(empty).toContain("Rottweiler · feat/ux")
  expect(empty).not.toContain("Skill")
  expect(empty).not.toContain("MCP")
  expect(empty).not.toContain("Instructions")
  state = { ...state, context: { ...context, items: [{ item_id: "instructions", kind: "project_instructions", label: "AGENTS.md", source: "workspace", machine_local_path: null, estimated_tokens: "20", state: { pinned: false, evicted: false, summarized: false, pruned: false } }] }, commands: [{ name: "review", description: "Review", usage: "/review", source: "skill" }, { name: "plan", description: "Plan", usage: "/plan", source: "skill" }] }
  expect(launchSummary(state, 80)).toContain("AGENTS.md loaded · 2 skills")
  state = reduceRottweilerState(state, transportDisconnected(1))
  expect(launchSummary(state, 80)).not.toContain("MCP")
})

test("launch names the model by display name and gives one setup action otherwise", () => {
  const coding = { id: "openai/coding", provider: "openai", displayName: "Coding", aliases: ["fast"], available: true, current: true, status: null, vision: false, thinking: true, toolCalling: true, contextTokens: null }
  const state = { ...createInitialState(), model: "fast", models: [coding], modelCatalogLoaded: true }
  expect(launchSummary(state, 80)).toContain("Coding · OpenAI API")
  expect(launchSummary({ ...state, model: "openai_codex/gpt-5.6-terra", models: [] }, 80)).toContain("gpt-5.6-terra · OpenAI · ChatGPT")
  expect(launchSummary({ ...state, model: "missing" }, 80)).toContain("Connect a provider with /model to start")
  const connected = [{ name: "openai", authKind: "api_key" as const, nextAction: "select_models" as const, configured: true, authenticated: true, reachable: true, modelCount: 1, status: null }]
  expect(launchSummary({ ...state, model: null, providers: connected }, 80)).toContain("Choose a model with /model")
  expect(launchSummary({ ...state, model: null, providers: connected, modelCatalogCached: true }, 80)).toContain("Loading models…")
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
  expect(setup.captureCharFrame()).toContain("Loading models…")
  expect(setup.captureCharFrame()).not.toContain("not loaded")
  expect(app.composer.y + app.composer.height).toBeLessThan(height)
  expect(setup.captureCharFrame()).toMatchSnapshot("launch")
  app.composer.value = "Try this"
  await app.composer.submit()
  expect(app.state.errorHistory).toHaveLength(1)
  app.setState({ ...app.state, errors: [] })
  app.composer.value = "/errors"
  expect(await app.composer.submit()).toBeTrue()
  expect(app.picker.screenTitle).toContain("ERRORS")
  app.picker.activateSelected()
  await setup.flush()
  expect(app.picker.screenTitle).toBe("ERRORS › Error 1")
  expect(options(app.picker).some(option => option.name.includes("Provider rejected"))).toBeTrue()
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
  expect(options(app.picker).map(option => option.value)).toEqual(["error:1"])
  expect(app.picker.footer.plainText).toBe("⏎ details · ctrl+d dismiss notices · esc close · 1 notice on screen")
  setup.mockInput.pressKey("d", { ctrl: true })
  expect(app.state.errors).toEqual([])
  expect(app.picker.footer.plainText).toBe("⏎ details · esc close")
  expect(app.state.errorHistory).toHaveLength(1)
  expect(options(app.picker).some(option => option.value === "error:1")).toBeTrue()
})
