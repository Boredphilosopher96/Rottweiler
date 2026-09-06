import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { emptySessionReader, waitForHistory } from "../fixtures/history"
import { BootstrapPresentation } from "../../src/app/bootstrap"
import { ClientCache } from "../../src/history/cache"
import type { HistoryCacheValue } from "../../src/history/controller"
import { PROTOCOL_VERSION, type CommandReply } from "../../src/protocol"
import { collectSessionBootstrap, type BootstrapPost } from "../../src/runtime-bootstrap"
import { createInitialState, engineEvent, reduceRottweilerState } from "../../src/state"
import { ScriptedClient } from "./fixtures"
import { MemoryFiles, ReconnectingProjectionClient, TestApp, waitFor } from "./fixtures"
import { TuiEngineRuntime } from "../../src/runtime"

const meta = () => ({ protocol_version: PROTOCOL_VERSION, client_id: "c", request_id: crypto.randomUUID() })
const signal = () => new AbortController().signal
function source(offset = 0): BootstrapPost {
  const client = new ScriptedClient()
  return async (command, _signal, allocation): Promise<CommandReply> => {
    allocation.admit(8192)
    const reply = await client.postCommand(command)
    if (reply.type !== "read") throw new Error("fixture expected a read")
    const event = reply.events[0]!
    switch (event.type) {
      case "transcript_tail_ready":
        if (event.result.type === "ready") event.result.page.view.through = String(10 + offset)
        break
      case "session_state_ready":
        event.snapshot.through = String(12 + offset)
        event.snapshot.plugin_statuses = [{ plugin_id: "wasm:fixture", status: "Ready", source: String(11 + offset) }]
        break
      case "session_controls_ready":
        event.snapshot.through = String(14 + offset)
        event.snapshot.controls.questions = [{ question_id: "q", turn_id: "1", questions: [{ id: "q", prompt: "Choose", response_kind: "text", options: [] }] }]
        break
      case "session_children_ready":
        if (event.result.type === "ready") event.result.snapshot.through = String(15 + offset)
        break
      case "todos_read":
        if (event.result.type === "ready") {
          event.result.todos.through = String(16 + offset)
          event.result.todos.snapshot.items = [{ id: "task", content: "Keep", status: "pending" }]
        }
        break
    }
    return reply
  }
}

test("bootstrap retains every component while replay converges from the oldest independent source", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  const bootstrap = await collectSessionBootstrap(source(), meta, cache, "s", signal())
  let state = bootstrap.takeState()
  expect(state.lastSequence).toBe("10")
  expect(state.pluginStatuses).toEqual({ "wasm:fixture": "Ready" })
  expect(state.questions.q).toBeDefined()
  expect(cache.usage.pinnedEntries).toBe(4)
  expect(cache.allocations.usage.bytes).toBeGreaterThan(cache.usage.bytes)
  const durable = (sequence: string) => ({ protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: sequence, emitted_at: "2026-01-01T00:00:00Z" })
  state = reduceRottweilerState(state, engineEvent({ type: "plugin_status_changed", meta: durable("11"), plugin_id: "wasm:fixture", status: "Old" }), "s")
  state = reduceRottweilerState(state, engineEvent({ type: "question_answered", meta: durable("12"), turn_id: "1", question_id: "q", answers: [] }), "s")
  expect(state.pluginStatuses["wasm:fixture"]).toBe("Ready")
  expect(state.questions.q).toBeDefined()
  expect(state.lastSequence).toBe("12")
  bootstrap.release()
  expect(cache.allocations.usage.bytes).toBe(0)
  expect(() => bootstrap.takeState()).toThrow("released")
})

test("cancelled collection remains charged until the in-flight decoder actually settles", async () => {
  const cache = new ClientCache<HistoryCacheValue>(), controller = new AbortController(), read = source()
  let complete!: () => void, reached!: () => void
  const pending = new Promise<void>(resolve => { complete = resolve })
  const started = new Promise<void>(resolve => { reached = resolve })
  const work = collectSessionBootstrap(async (command, signal, allocation) => {
    if (command.type === "get_session_controls") {
      allocation.admit(65536); reached(); await pending
    }
    return read(command, signal, allocation)
  }, meta, cache, "s", controller.signal)
  await started
  controller.abort()
  expect(cache.allocations.usage.bytes).toBeGreaterThanOrEqual(65536)
  complete()
  await expect(work).rejects.toThrow()
  expect(cache.allocations.usage.bytes).toBe(0)
})

test.each(["session", "turn", "catchup"] as const)("a mismatched %s bootstrap releases all collected owners", async failure => {
  const cache = new ClientCache<HistoryCacheValue>(), read = source()
  await expect(collectSessionBootstrap(async (command, signal, allocation) => {
    const reply = await read(command, signal, allocation)
    const event = reply.type === "read" ? reply.events[0] : undefined
    if (event?.type === "session_state_ready") {
      if (failure === "session") event.session_id = "foreign"
      if (failure === "turn") event.snapshot.active_turn = { turn_id: "new", started: "17" }
    }
    if (event?.type === "todos_read" && failure === "catchup") event.result = { type: "catching_up", through: "1", target: "10" }
    return reply
  }, meta, cache, "s", signal())).rejects.toThrow()
  expect(cache.allocations.usage.bytes).toBe(0)
})

test("renderer handoff retains old and incoming owners through partial replacement failure", async () => {
  const cache = new ClientCache<HistoryCacheValue>()
  let state = createInitialState(), fail = false, seenCredit = 0
  const presentation = new BootstrapPresentation(next => {
    state = next
    seenCredit = cache.allocations.usage.bytes
    if (fail) throw new Error("component failed")
  })
  const first = await collectSessionBootstrap(source(), meta, cache, "s", signal())
  presentation.install(first)
  const firstCredit = seenCredit
  const second = await collectSessionBootstrap(source(), meta, cache, "s", signal())
  fail = true
  expect(() => presentation.install(second)).toThrow("component failed")
  expect(seenCredit).toBe(firstCredit * 2)
  expect(state.questions.q).toBeDefined()
  expect(cache.allocations.usage.bytes).toBe(seenCredit)
  state = createInitialState()
  presentation.dispose()
  expect(cache.allocations.usage.bytes).toBe(0)
})

test("the mounted app replaces its source model without losing drafts and releases owners on destruction", async () => {
  const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const app = createRottweilerApp(setup.renderer, { sessionId: "s", sessionReader: emptySessionReader })
  setup.renderer.root.add(app)
  app.composer.value = "draft stays editable"
  const cache = app.historyCache, read = source()
  const collect = () => collectSessionBootstrap(async (command, signal, allocation) => {
    const reply = await read(command, signal, allocation)
    const event = reply.type === "read" ? reply.events[0] : undefined
    if (event?.type === "session_controls_ready") event.snapshot.controls.questions[0]!.questions = [{
      id: "q", prompt: "Choose", response_kind: "select_one", options: [{ label: "Keep", value: "keep", description: "Keep" }],
    }]
    return reply
  }, meta, cache, "s", signal())
  try {
    app.installBootstrap(await collect())
    await waitForHistory(setup, () => setup.renderer.currentFocusedRenderable === app.interactionPanel.select)
    expect(app.composer.value).toBe("draft stays editable")
    const firstCredit = cache.allocations.usage.domains.controls
    app.installBootstrap(await collect())
    await setup.flush()
    expect(cache.allocations.usage.domains.controls).toBe(firstCredit)
    expect(app.state.pluginStatuses["wasm:fixture"]).toBe("Ready")
    expect(app.state.questions.q).toBeDefined()
  } finally { setup.renderer.destroy() }
  expect(app.state.questions).toEqual({})
  expect(app.state.pluginStatuses).toEqual({})
  expect(cache.allocations.usage.bytes).toBe(0)
})

test("fresh and rebound runtime cursors come from complete bootstrap cuts rather than a saved cursor", async () => {
  const client = new ReconnectingProjectionClient(), post = client.postCommand.bind(client)
  let read = source()
  client.postCommand = async command => {
    switch (command.type) {
      case "read_transcript_tail": case "get_session_state": case "get_session_controls": case "read_session_children": case "get_todos":
        return read(command, signal(), { admit() {} })
      default: return post(command)
    }
  }
  const runtime = new TuiEngineRuntime({ socketPath: "/private/bootstrap.sock", bootstrapToken: "secret", sessionId: "s",
    lastSeenSequence: "999", lastSeenFile: null, replayMode: false }, client, new MemoryFiles())
  const app = new TestApp(); runtime.bind(app)
  const running = runtime.start()
  try {
    await waitFor(() => client.subscription !== null)
    expect(client.subscription?.attach.last_seen_sequence).toBe("10")
    expect(app.state.questions.q).toBeDefined()
    const first = app.historyCache.allocations.usage.bytes
    read = source(10)
    await client.reconnect()
    expect(client.subscription?.getLastSeenSequence?.()).toBe("20")
    expect(app.state.controls.snapshotThrough).toBe("24")
    expect(app.historyCache.allocations.usage.bytes).toBe(first)
  } finally { await runtime.stop(); await running; app.bootstrap.dispose() }
  expect(app.historyCache.allocations.usage.bytes).toBe(0)
})
