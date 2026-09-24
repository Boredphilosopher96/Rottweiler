import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { PROTOCOL_VERSION } from "../../../../protocol/types"
import { createRottweilerApp, type RottweilerApp } from "../../src/app"
import type { ClientCommand, CommandOutcome, EngineEvent } from "../../src/protocol"
import type { SubagentDescriptor } from "../../src/subagent-state"
import { emptySessionReader } from "../fixtures/history"
import { options, select } from "../picker-screen"

const SESSION = "parent-session"

function eventMeta(sequence: number) {
  return { protocol_version: PROTOCOL_VERSION, session_id: SESSION, sequence_id: String(sequence), emitted_at: "2026-01-01T00:00:00Z" }
}

function descriptor(id: string, activity: SubagentDescriptor["activity"], task = `Task for ${id}`): SubagentDescriptor {
  return { subagent_id: id, child_session_id: `session-${id}`, task, agent: "reviewer", model: "fast", isolation: "shared", activity }
}

interface Harness {
  readonly app: RottweilerApp
  readonly emitted: ClientCommand[]
  readonly setup: Awaited<ReturnType<typeof createTestRenderer>>
  listChildren(children: readonly SubagentDescriptor[]): void
  emit(event: EngineEvent): void
}

describe("Rottweiler agents", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => {
    renderer?.destroy()
    renderer = undefined
  })

  async function harness(options: {
    readonly width?: number
    readonly height?: number
    readonly onCommand?: (command: ClientCommand) => CommandOutcome | Promise<CommandOutcome> | undefined
  } = {}): Promise<Harness> {
    const setup = await createTestRenderer({ width: options.width ?? 100, height: options.height ?? 24, useThread: false })
    renderer = setup.renderer
    const emitted: ClientCommand[] = []
    let request = 0
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      sessionId: SESSION,
      requestId: () => `request-${++request}`,
      onCommand(command) {
        emitted.push(command)
        return options.onCommand?.(command) ?? { type: "accepted" }
      },
    })
    renderer.root.add(app)
    return {
      app, emitted, setup,
      listChildren(children) {
        const list = emitted.findLast((command) => command.type === "list_subagents")!
        app.handleEvent({
          type: "subagents_listed",
          meta: { protocol_version: PROTOCOL_VERSION, client_id: "tui-client", request_id: list.meta.request_id, emitted_at: "2026-01-01T00:00:00Z" },
          session_id: SESSION,
          subagents: [...children],
        })
      },
      emit(event) {
        app.handleEvent(event)
      },
    }
  }

  function spawn(h: Harness, id: string, first = 1): void {
    h.emit({ type: "turn_started", meta: eventMeta(first), turn_id: "1" })
    h.emit({ type: "subagent_spawned", meta: eventMeta(first + 1), subagent_id: id, child_session_id: `session-${id}`, task: `Task for ${id}` })
  }

  function finish(h: Harness, id: string, sequence: number): void {
    h.emit({
      type: "subagent_finished", meta: eventMeta(sequence), subagent_id: id,
      result: {
        subagent_id: id, session_id: `session-${id}`, status: "completed", final_text: "Found the regression in the router",
        touched_files: [], usage: { input_tokens: "1", output_tokens: "1", cache_read_tokens: "0", cache_write_tokens: "0", reasoning_tokens: "0" },
        cost: { kind: "monetary", amount_micros: "12500", currency: "USD" }, turns: "1", duration_millis: "10",
      },
    })
  }

  test("Ctrl+G opens the Agents screen with running and finished children", async () => {
    const h = await harness()
    spawn(h, "child-running")
    h.setup.mockInput.pressKey("g", { ctrl: true })
    await Bun.sleep(0)
    expect(h.app.agentsBrowser.visible).toBeTrue()
    expect(h.app.agentsBrowser.heading.plainText).toContain("AGENTS")
    expect(h.emitted.at(-1)).toMatchObject({ type: "list_subagents", session_id: SESSION })
    h.listChildren([descriptor("child-running", "running"), descriptor("child-idle", "idle")])
    expect(h.app.agentsBrowser.sectionLabels).toEqual(["Running", "Finished"])
    expect(h.app.agentsBrowser.itemIds).toEqual(["agents.child.child-running", "agents.child.child-idle"])
    expect(h.app.agentsBrowser.detail.plainText).toContain("reviewer · running")
    h.setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(h.app.agentsBrowser.visible).toBeFalse()
  })

  test("viewing a child overlays it while the parent composer and stream stay intact", async () => {
    const h = await harness()
    h.app.composer.value = "parent draft stays private"
    h.app.composer.addAttachment({ name: "parent context.txt", media_type: "text/plain", data: { type: "text", content: "parent only" } })
    spawn(h, "child-running")
    h.app.openSubagentPicker()
    h.listChildren([descriptor("child-running", "running")])
    h.app.agentsBrowser.activateSelected()
    expect(options(h.app.picker).map((option) => option.value)).toEqual(["view", "stop", "close"])
    h.app.picker.activateSelected()
    await Bun.sleep(0)

    expect(h.app.activeSubagentId).toBe("child-running")
    expect(h.app.agentsBrowser.visible).toBeFalse()
    expect(h.app.composer.visible).toBeFalse()
    expect(h.app.banner.plainText).toContain("Agent · reviewer · running")
    expect(h.app.banner.plainText).toContain("Esc back")
    h.emit({ type: "text_delta", meta: eventMeta(3), turn_id: "1", text: "parent keeps streaming" })
    expect(h.app.state.streamingTail?.text).toContain("parent keeps streaming")

    h.setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(h.app.activeSubagentId).toBeNull()
    expect(h.app.composer.visible).toBeTrue()
    expect(h.app.composer.value).toBe("parent draft stays private")
    expect(h.app.composer.attachments.map((attachment) => attachment.name)).toEqual(["parent context.txt"])
    expect(h.emitted.some((command) => command.type === "interrupt_subagent")).toBeFalse()
    expect(h.app.banner.plainText).toContain("press Esc again to stop the child agent")
    h.setup.mockInput.pressKey("c", { ctrl: true })
    await Bun.sleep(30)
    expect(h.emitted.at(-1)).toMatchObject({ type: "interrupt_subagent", session_id: SESSION, subagent_id: "child-running" })
  })

  test("messages a finished child explicitly without touching the parent composer", async () => {
    const h = await harness()
    h.app.composer.value = "parent draft"
    h.app.openSubagentPicker()
    h.listChildren([descriptor("child-idle", "idle")])
    h.app.agentsBrowser.activateSelected()
    expect(options(h.app.picker).map((option) => option.value)).toEqual(["view", "message", "close"])
    select(h.app.picker, 1)
    h.app.picker.activateSelected()
    expect(h.app.picker.screenTitle).toContain("Message reviewer")
    h.app.setState(h.app.state)
    expect(h.app.picker.screenTitle).toContain("Message reviewer")
    await h.setup.mockInput.typeText("check the edge cases too")
    h.setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(h.emitted.at(-1)).toMatchObject({
      type: "continue_subagent", session_id: SESSION, subagent_id: "child-idle", content: "check the edge cases too",
    })
    expect(h.app.composer.value).toBe("parent draft")
    expect(h.app.activeSubagentId).toBeNull()
    expect(h.app.agentsBrowser.visible).toBeTrue()
  })

  test("child actions go back step by step: prompt to actions to list", async () => {
    const h = await harness()
    h.app.openSubagentPicker()
    h.listChildren([descriptor("child-idle", "idle")])
    h.app.agentsBrowser.activateSelected()
    expect(h.app.picker.footer.plainText).toContain("esc back")
    select(h.app.picker, 1)
    h.app.picker.activateSelected()
    expect(h.app.picker.screenTitle).toContain("Message reviewer")
    h.setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(options(h.app.picker).map((option) => option.value)).toEqual(["view", "message", "close"])
    h.setup.mockInput.pressEscape()
    await Bun.sleep(30)
    expect(h.app.agentsBrowser.visible).toBeTrue()
    expect(h.emitted.some((command) => command.type === "continue_subagent")).toBeFalse()
  })

  test("refuses to message a child that started working again", async () => {
    const h = await harness()
    h.app.openSubagentPicker()
    h.listChildren([descriptor("child-idle", "idle")])
    h.app.agentsBrowser.activateSelected()
    select(h.app.picker, 1)
    h.app.picker.activateSelected()
    await h.setup.mockInput.typeText("one more thing")
    spawn(h, "child-idle")
    h.setup.mockInput.pressEnter()
    await Bun.sleep(0)
    expect(h.emitted.some((command) => command.type === "continue_subagent")).toBeFalse()
    expect(h.app.state.errors.at(-1)).toMatchObject({ code: "subagent_still_running" })
  })

  test("stop and close are explicit actions that return to the list", async () => {
    const h = await harness()
    spawn(h, "child-running")
    h.app.openSubagentPicker()
    h.listChildren([descriptor("child-running", "running")])
    h.app.agentsBrowser.activateSelected()
    select(h.app.picker, 1)
    h.app.picker.activateSelected()
    await Bun.sleep(0)
    expect(h.emitted.at(-1)).toMatchObject({ type: "interrupt_subagent", subagent_id: "child-running" })
    expect(h.app.agentsBrowser.visible).toBeTrue()
  })

  test("keeps child-list failures retryable instead of claiming the list is empty", async () => {
    let attempts = 0
    const h = await harness({
      width: 72, height: 12,
      onCommand(command) {
        if (command.type !== "list_subagents") return undefined
        attempts += 1
        return { type: "rejected", error: { category: "protocol", code: "offline", message: "engine temporarily unavailable", retryable: true } }
      },
    })
    h.app.openSubagentPicker()
    await Bun.sleep(0)
    expect(h.app.agentsBrowser.itemIds).toEqual(["agents.retry"])
    h.app.agentsBrowser.activateSelected()
    await Bun.sleep(0)
    expect(attempts).toBe(2)
  })

  test("the strip lists running children and keeps finished ones until the next message", async () => {
    const h = await harness()
    expect(h.app.agentsStrip.visible).toBeFalse()
    spawn(h, "child-a")
    h.app.openSubagentPicker()
    h.listChildren([descriptor("child-a", "running")])
    h.app.closePicker()
    await h.setup.renderOnce()
    expect(h.app.agentsStrip.visible).toBeTrue()
    expect(h.app.agentsStrip.rows.get("child-a")?.plainText).toContain("◌ reviewer · Task for child-a")
    expect(h.app.statusLine.plainText).toContain("1 agent running")
    // The sidebar names the agent and its task, never the raw child id.
    expect(h.app.contextPanel.agents.options.map(option => option.name)).toEqual(["◌ reviewer · Task for child-a"])
    finish(h, "child-a", 3)
    h.emit({ type: "turn_finished", meta: eventMeta(4), turn_id: "1", status: "completed",
      usage: { input_tokens: "1", output_tokens: "1", cache_read_tokens: "0", cache_write_tokens: "0", reasoning_tokens: "0" },
      cost: { kind: "monetary", amount_micros: "1", currency: "USD" } })
    expect(h.app.agentsStrip.visible).toBeTrue()
    expect(h.app.agentsStrip.rows.get("child-a")?.plainText).toContain("completed · USD 0.0125")
    expect(h.app.statusLine.plainText).not.toContain("agent running")
    expect(h.app.agentsStrip.footer.plainText).toContain("hidden after your next message")

    // Clearing finished agents from the strip is a footer chord, not a list row.
    h.app.openSubagentPicker()
    expect(h.app.agentsBrowser.itemIds).toEqual(["agents.child.child-a"])
    expect(h.app.agentsBrowser.footer.plainText).toContain("Ctrl+D clear finished from strip")
    h.setup.mockInput.pressKey("d", { ctrl: true })
    expect(h.app.agentsStrip.visible).toBeFalse()
    expect(h.app.agentsBrowser.footer.plainText).not.toContain("Ctrl+D")
    h.app.closePicker()
    h.app.composer.value = "next task"
    await h.app.composer.submit()
    expect(h.app.agentsStrip.visible).toBeFalse()
    h.app.openSubagentPicker()
    expect(h.app.agentsBrowser.itemIds).toEqual(["agents.child.child-a"])
    expect(h.app.agentsBrowser.detail.plainText).toContain("Found the regression in the router")
  })

  test("queued children are live in the strip and the Agents screen", async () => {
    const h = await harness()
    spawn(h, "child-running")
    h.app.openSubagentPicker()
    h.listChildren([descriptor("child-running", "running"), descriptor("child-queued", "queued")])
    expect(h.app.agentsBrowser.sectionLabels).toEqual(["Running"])
    expect(h.app.agentsBrowser.footer.plainText).toContain("1 running · 1 queued · 0 finished")
    h.app.agentsBrowser.selectById("agents.child.child-queued")
    expect(h.app.agentsBrowser.detail.plainText).toContain("waiting for a free agent slot")
    h.app.closePicker()
    await h.setup.renderOnce()
    expect([...h.app.agentsStrip.rows.keys()]).toEqual(["child-running", "child-queued"])
    expect(h.app.agentsStrip.rows.get("child-queued")?.plainText).toBe("◷ reviewer · Task for child-queued · queued")
    expect(h.app.agentsStrip.footer.plainText).not.toContain("hidden after your next message")
  })

  test("Ctrl+B backgrounds only the child a foreground spawn is blocked on", async () => {
    const h = await harness()
    spawn(h, "child-bg")
    h.setup.mockInput.pressKey("b", { ctrl: true })
    await Bun.sleep(0)
    expect(h.emitted.some((command) => command.type === "background_subagent")).toBeFalse()
    expect(h.app.composer.hintText.plainText).not.toContain("background")

    h.emit({
      type: "tool_call_started", meta: eventMeta(3), turn_id: "1", tool_call_id: "call-wait", invocation_id: "inv-wait",
      name: "spawn_agent", args: { action: "wait", ids: ["child-bg"] }, call_index: 0,
    })
    expect(h.app.composer.hintText.plainText).toContain("Ctrl+B background")
    expect(h.app.agentsStrip.footer.plainText).toContain("Ctrl+B background")
    h.setup.mockInput.pressKey("b", { ctrl: true })
    await Bun.sleep(0)
    expect(h.emitted.at(-1)).toMatchObject({ type: "background_subagent", session_id: SESSION, subagent_id: "child-bg" })
  })
})
