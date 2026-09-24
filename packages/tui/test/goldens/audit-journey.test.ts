import { afterEach, expect, test } from "bun:test"
import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent } from "../../src/protocol"
import { emptySessionReader } from "../fixtures/history"
import { childResult } from "../state/fixtures"
import { options, select } from "../picker-screen"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })
const usage = { input_tokens: "20", output_tokens: "10", reasoning_tokens: "0", cache_read_tokens: "0", cache_write_tokens: "0" }
const cost = { kind: "monetary", amount_micros: "1000", currency: "USD" } as const

// Deterministic client journey through production events. Authentication and
// provider networking are intentionally exercised by separate integration gates.
for (const [width, height] of [[110, 32], [80, 24]] as const) {
  test(`audit journey setup through resume at ${width}x${height}`, async () => {
    const setup = await createTestRenderer({ width, height, useThread: false, exitOnCtrlC: false })
    renderer = setup.renderer
    const commands: ClientCommand[] = []
    const resumed: string[] = []
    let exits = 0
    let sequence = 0
    const meta = () => ({ protocol_version: PROTOCOL_VERSION, session_id: "journey", sequence_id: String(++sequence), emitted_at: "2026-09-15T12:00:00Z" })
    const reply = (request: string) => ({ protocol_version: PROTOCOL_VERSION, client_id: "journey-client", request_id: request, emitted_at: "2026-09-15T12:00:00Z" })
    const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, sessionId: "journey", clientId: "journey-client",
      initialState: { ...createInitialState(), connection: { phase: "connected", attempt: 0, error: null, gap: null }, providers: [{
        name: "openai", configured: false, authenticated: false, reachable: false, authKind: "api_key", nextAction: "configure", modelCount: 0, status: "Set up provider",
      }] },
      onCommand(command) { commands.push(command); return { type: "accepted" } },
      onSessionSelect(session) { resumed.push(session) },
      onExit() { exits += 1 },
    })
    renderer.root.add(app)
    const deliver = async (event: EngineEvent) => { app.handleEvent(event); await setup.flush() }
    app.openProviderPicker()
    await setup.flush()
    expect(app.picker.visible).toBeTrue()
    expect(app.state.model).toBeNull()
    await deliver({ type: "provider_activation_finished", meta: reply("activation"), session_id: "journey", provider: "openai", success: true, message: "Connected" })
    const catalogRequest = commands.findLast(command => command.type === "list_models")!
    await deliver({ type: "models_listed", meta: reply(catalogRequest.meta.request_id), aliases: [], cached: false, truncated: false,
      providers: [{ name: "openai", auth_kind: "api_key", next_action: "select_models", configured: true, authenticated: true, reachable: true, model_count: 2 }],
      models: ["coding", "reasoning"].map(name => ({ id: `openai/${name}`, display_name: name, provider: "openai", aliases: [], current: false, available: true,
        capabilities: { tool_calling: true, vision: false, thinking: true, cache_behavior: "none", max_context_tokens: "32000", max_output_tokens: "4000" } })),
    })
    expect(commands).toContainEqual(expect.objectContaining({ type: "switch_model", model: "openai/coding" }))
    await deliver({ type: "model_changed", meta: meta(), model: "openai/coding", provider: "openai" })
    expect(app.statusLine.plainText).toContain("coding")
    app.closePicker()
    await setup.mockInput.typeText("Fix the regression in src/main.rs and run its tests.")
    setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(commands).toContainEqual(expect.objectContaining({ type: "send_message", content: "Fix the regression in src/main.rs and run its tests." }))
    expect(app.composer.value).toBe("")
    await deliver({ type: "turn_started", meta: meta(), turn_id: "turn" })
    await deliver({ type: "tool_approval_needed", meta: meta(), turn_id: "turn", tool_call_id: "edit", invocation_id: "edit", name: "edit",
      args: { path: "src/main.rs" }, capabilities: ["write_filesystem"], rationale: "Apply the reviewed correction", diff: null })
    expect(app.interactionPanel.visible).toBeTrue()
    expect(app.composer.visible).toBeTrue()
    expect(app.interactionPanel.y + app.interactionPanel.height).toBeLessThanOrEqual(app.composer.y)
    expect(setup.captureCharFrame()).toMatchSnapshot("approval")
    setup.mockInput.pressKey("y")
    await Bun.sleep(0)
    expect(commands).toContainEqual(expect.objectContaining({ type: "approve_tool", decision: "allow_once" }))
    await deliver({ type: "tool_approval_resolved", meta: meta(), turn_id: "turn", tool_call_id: "edit", invocation_id: "edit", decision: "allow_once" })
    await deliver({ type: "tool_call_finished", meta: meta(), turn_id: "turn", tool_call_id: "edit", invocation_id: "edit", payloads: [], presentation: null,
      output: { type: "text", text: "Updated src/main.rs" }, is_error: false, call_index: 0 })
    await deliver({ type: "tool_call_started", meta: meta(), turn_id: "turn", tool_call_id: "test", invocation_id: "test", name: "bash", args: { command: "cargo test" }, call_index: 1 })
    await deliver({ type: "tool_call_finished", meta: meta(), turn_id: "turn", tool_call_id: "test", invocation_id: "test", payloads: [], presentation: null, output: { type: "text", text: "test result: ok. 4 passed; 0 failed" }, is_error: false, call_index: 1 })
    expect(app.state.tools.test?.status).toBe("finished")
    expect(setup.captureCharFrame()).toContain("cargo test")
    await deliver({ type: "text_delta", meta: meta(), turn_id: "turn", text: "The edit is ready for review." })
    setup.mockInput.pressKey("c", { ctrl: true })
    await Bun.sleep(0)
    expect(commands.at(-1)?.type).toBe("interrupt")
    await deliver({ type: "turn_finished", meta: meta(), turn_id: "turn", status: "interrupted", usage, cost })
    expect(setup.captureCharFrame()).toContain("Interrupted")
    await deliver({ type: "subagent_spawned", meta: meta(), subagent_id: "child", child_session_id: "child-session", task: "Review the tests" })
    await deliver({ type: "subagent_finished", meta: meta(), subagent_id: "child", result: childResult("child", "child-session", "Tests reviewed") })
    expect(app.state.subagents.child?.status).toBe("completed")
    await deliver({ type: "compaction_started", meta: meta(), reason: "manual" })
    await deliver({ type: "compaction_finished", meta: meta(), summary_turn_id: "summary", reclaimed_tokens: "12000", usage, cost })
    expect(setup.captureCharFrame()).toContain("12000 tokens reclaimed")
    expect(setup.captureCharFrame()).toMatchSnapshot("interrupted with child completion and compaction")
    app.openSessionPicker()
    const list = commands.findLast(command => command.type === "list_sessions")!
    await deliver({ type: "sessions_listed", meta: reply(list.meta.request_id), sessions: [{ session_id: "past", title: "Previous work", workspace_name: "fixture", model: "openai/coding", driver_client_id: null, shell_active: false }] })
    select(app.picker, options(app.picker).findIndex(option => option.value === "past"))
    app.picker.activateSelected()
    expect(resumed).toEqual(["past"])
    expect(app.picker.visible).toBeFalse()
    setup.mockInput.pressKey("c", { ctrl: true })
    expect(exits).toBe(0)
    setup.mockInput.pressKey("c", { ctrl: true })
    expect(exits).toBe(1)
  })
}
