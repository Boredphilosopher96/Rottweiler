import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import { PROTOCOL_VERSION, type ClientCommand, type CommandOutcome, type EngineEvent } from "../../src/protocol"
import { emptySessionReader } from "../fixtures/history"
import { options } from "../picker-screen"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })
const reply = (request = "startup") => ({ protocol_version: PROTOCOL_VERSION, client_id: "tui-client", request_id: request, emitted_at: "2026-09-16T00:00:00Z" })
const sessions: EngineEvent = { type: "sessions_listed", meta: reply("sessions"), sessions: [] }
const emptyCatalog: EngineEvent = { type: "models_listed", meta: reply(), models: [], aliases: [], providers: [], cached: true, truncated: false }
const currentModel = { id: "fixture/gpt-5-mini", provider: "fixture", display_name: "gpt-5-mini", aliases: ["fast"], current: true, available: true, capabilities: { vision: false, thinking: false, tool_calling: true, cache_behavior: "none" as const, max_context_tokens: null, max_output_tokens: null } }

test("cached incomplete catalog cannot erase a hydrated configured selection or steal its first prompt", async () => {
  const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
  renderer = setup.renderer
  const commands: ClientCommand[] = []
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: {
    ...createInitialState(), model: "fast", connection: { phase: "connected", attempt: 0, error: null, gap: null },
  }, onCommand(command) { commands.push(command); return { type: "accepted" } } })
  renderer.root.add(app)
  app.handleEvent({ ...emptyCatalog, providers: [{ name: "fixture", auth_kind: "api_key", next_action: "select_models", configured: true, authenticated: true, reachable: false, model_count: 0 }] })
  app.handleEvent(sessions)
  expect(app.state.model).toBe("fast")
  expect(app.statusLine.plainText).toContain("loading models")
  expect(app.statusLine.plainText).not.toContain("fast")
  expect(app.picker.visible).toBeFalse()
  await setup.mockInput.typeText("First configured prompt")
  setup.mockInput.pressEnter()
  await Bun.sleep(0)
  expect(commands).toContainEqual(expect.objectContaining({ type: "send_message", content: "First configured prompt" }))
})

test("an automatically opened setup picker retires when the selected model resolves", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  const commands: ClientCommand[] = []
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: {
    ...createInitialState(), connection: { phase: "connected", attempt: 0, error: null, gap: null },
  }, onCommand(command) { commands.push(command); return { type: "accepted" } } })
  renderer.root.add(app)
  app.handleEvent(emptyCatalog)
  app.handleEvent(sessions)
  expect(app.picker.visible).toBeTrue()
  const request = commands.findLast(command => command.type === "list_models")!
  app.handleEvent({ ...emptyCatalog, meta: reply(request.meta.request_id), models: [currentModel], cached: false })
  expect(app.state.model).toBe(currentModel.id)
  expect(app.picker.visible).toBeFalse()
  expect(renderer.currentFocusedRenderable).toBe(app.composer.editor)
})

test("late startup projections cannot open onboarding while first submission owns the cleared draft", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  const submission = Promise.withResolvers<CommandOutcome>()
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: {
    ...createInitialState(), connection: { phase: "connected", attempt: 0, error: null, gap: null },
  }, onCommand(command) { return command.type === "send_message" ? submission.promise : { type: "accepted" } } })
  renderer.root.add(app)
  app.composer.value = "First prompt"
  await setup.flush()
  const sent = app.composer.submit()
  await Bun.sleep(0)
  expect(app.composer.value).toBe("")
  app.handleEvent(emptyCatalog)
  app.handleEvent(sessions)
  const visible = app.picker.visible
  submission.resolve({ type: "accepted" })
  await sent
  expect(visible).toBeFalse()
  expect(app.picker.visible).toBeFalse()
})

test("a user-opened model picker keeps ownership when a catalog resolves the current model", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  const commands: ClientCommand[] = []
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader,
    onCommand(command) { commands.push(command); return { type: "accepted" } } })
  renderer.root.add(app)
  app.openModelPicker()
  const request = commands.findLast(command => command.type === "list_models")!
  app.handleEvent({ ...emptyCatalog, meta: reply(request.meta.request_id), models: [currentModel], cached: false })
  expect(app.picker.visible).toBeTrue()
  expect(renderer.currentFocusedRenderable).toBe(app.picker.input)
})

test("fresh home clears the placeholder default and offers setup even when its empty catalog is cached", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  const commands: ClientCommand[] = []
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, initialState: {
    ...createInitialState(), model: "fast", connection: { phase: "connected", attempt: 0, error: null, gap: null },
  }, onCommand(command) { commands.push(command); return { type: "accepted" } } })
  renderer.root.add(app)
  app.handleEvent(emptyCatalog)
  app.handleEvent(sessions)
  expect(app.state.model).toBeNull()
  expect(app.picker.visible).toBeTrue()
  const request = commands.findLast(command => command.type === "list_models")!
  app.handleEvent({ ...emptyCatalog, meta: reply(request.meta.request_id), cached: false })
  expect(app.picker.screenTitle).toContain("WELCOME")
  expect(options(app.picker).some(option => option.value === "providers.compatible")).toBeTrue()
})
