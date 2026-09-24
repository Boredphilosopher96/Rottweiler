import { CliRenderEvents } from "@opentui/core"
import {
  createTestRenderer,
  type TestRenderer
} from "@opentui/core/testing"
import { afterEach, describe, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { AgentsStripRenderable, ContextPanelRenderable, SubagentPanelRenderable, agentsStripEntries, type AgentsStripInput } from "../../src/components"
import {
  type EngineEvent
} from "../../src/protocol"
import { stringCellWidth } from "../../src/render"
import { createInitialState, type RottweilerState } from "../../src/state"
import { createStreamingTail } from "../../src/state/model"
import { kennelTheme } from "../../src/theme"
import { emptySessionReader, sessionReaderFor, conversationItem, waitForHistory } from "../fixtures/history"
import { meta, neverUsage } from "./fixtures"

describe("subagents components", () => {
  let renderer: TestRenderer | undefined
  afterEach(() => { renderer?.destroy(); renderer = undefined })

  test("notifies only while terminal focus is away", async () => {
    const setup = await createTestRenderer({ width: 80, height: 20, useThread: false })
    renderer = setup.renderer
    const notifications: string[] = []
    const app = createRottweilerApp(renderer, {
      sessionReader: emptySessionReader,
      notifications: {
        notify(notification) {
          notifications.push(notification.kind)
        },
      },
    })
    renderer.root.add(app)
    renderer.emit(CliRenderEvents.BLUR)
    const events: EngineEvent[] = [
      { type: "turn_started", meta: meta("1"), turn_id: "1" },
      {
        type: "turn_finished",
        meta: meta("2"),
        turn_id: "1",
        status: "completed",
        usage: {
          input_tokens: "1",
          output_tokens: "1",
          cache_read_tokens: "0",
          cache_write_tokens: "0",
          reasoning_tokens: "0",
        },
        cost: { kind: "monetary", amount_micros: "1", currency: "USD" },
      },
    ]
    for (const event of events) {
      app.handleEvent(event)
    }
    app.handleEvent({
      type: "ui_notification",
      meta: meta("3"),
      plugin_id: "reviewer",
      title: "Review ready",
      message: "Open the result",
    })
    renderer.emit(CliRenderEvents.FOCUS)
    app.handleEvent({ type: "turn_started", meta: meta("4"), turn_id: "2" })
    app.handleEvent({
      type: "turn_finished",
      meta: meta("5"),
      turn_id: "2",
      status: "completed",
      usage: events[1]!.type === "turn_finished" ? events[1]!.usage : neverUsage(),
      cost:
        events[1]!.type === "turn_finished"
          ? events[1]!.cost
          : { kind: "unavailable", reason: "fixture" },
    })

    expect(notifications).toEqual(["turn_finished", "plugin"])
  })

  test("renders compact nested subagent progress without replacing retained rows", async () => {
    const setup = await createTestRenderer({ width: 92, height: 20, useThread: false })
    renderer = setup.renderer
    const items = [conversationItem(1, "assistant", "Coordinating the implementation.")]
    const initial: RottweilerState = {
      ...createInitialState(),
      streamingTail: createStreamingTail({
        turnId: "1",
        text: "Coordinating the implementation.",
        thinking: "",
        citations: [],
        toolInvocationIds: [],
        finished: null,
      }),
      subagentOrder: ["explore", "tests"],
      subagents: {
        explore: {
          projectionId: "explore",
          subagentId: "explore",
          parentTurnId: "1",
          task: "Inspect provider boundaries",
          spawnedAtMs: Date.now() - 83_000,
          status: "running",
          childSessionId: "session-explore",
          lastChildSequence: "4",
          activity: "using tool · read",
          summary: null,
          touchedFileCount: 0,
          diffArtifactId: null,
        },
        tests: {
          projectionId: "tests",
          subagentId: "tests",
          parentTurnId: "1",
          task: "Add orchestration tests",
          spawnedAtMs: Date.now() - 120_000,
          status: "completed",
          childSessionId: "session-tests",
          lastChildSequence: "8",
          activity: "finished",
          summary: "Added deterministic coverage",
          touchedFileCount: 2,
          diffArtifactId: "diff-tests",
        },
      },
    }
    const app = createRottweilerApp(renderer, { sessionReader: sessionReaderFor(items), initialState: initial })
    renderer.root.add(app)
    await setup.flush()

    const frame = setup.captureCharFrame()
    expect(frame).toContain("Ctrl+G agents")
    expect(frame).toContain("Inspect provider boundaries · using tool · read")
    expect(frame).toContain("1m23s")
    // The child that finished before this client attached is not replayed into the strip.
    expect([...app.agentsStrip.rows.keys()]).toEqual(["explore"])
    expect(
      app.transcript.streamingCard
        .getChildren()
        .some((child) => child instanceof SubagentPanelRenderable),
    ).toBeFalse()

    const retained = app.transcript.mountedCards.get("1")
    items.push({
      id: "9", ordinal: "1", revision: "10", agent_turn: "1", content: {
        type: "subagent", subagent_id: "tests", session_id: "session-tests",
        task: { text: "Add orchestration tests", format: "text", complete: true, source: { sequence: "9", selector: { type: "subagent_task" } } },
        status: {
          type: "finished", status: "completed", touched_file_count: 2,
          diff: { sequence: "10", selector: { type: "subagent_diff" } },
          result: { text: "Added deterministic coverage", format: "text", complete: true, source: { sequence: "10", selector: { type: "subagent_result" } } }
        },
      }
    })
    app.transcript.scrollTo(Infinity)
    await waitForHistory(setup, () => app.transcript.mountedCards.has("9"))
    app.setState({
      ...initial,
      streamingTail: null,
      subagentOrder: ["tests"],
      subagents: { tests: initial.subagents.tests! },
    })
    await setup.flush()
    // The child row stays one line; its report appears once, in the result card.
    expect(app.transcript.mountedCards.get("9")?.header.plainText).toBe("↳ Agent · Add orchestration tests · completed · 2 files · diff ready")
    expect(app.transcript.mountedCards.get("9")?.markdown.content).toBe("")

    expect(app.transcript.mountedCards.get("1")).toBe(retained)
    const many = Object.fromEntries(
      Array.from({ length: 20 }, (_, index) => [
        `child-${index}`,
        {
          projectionId: `child-${index}`,
          subagentId: `child-${index}`,
          parentTurnId: "2",
          task: `Bounded child ${index}`,
          spawnedAtMs: Date.now() - index * 1_000,
          status: index < 4 ? ("running" as const) : ("completed" as const),
          childSessionId: `session-${index}`,
          lastChildSequence: String(index),
          activity: index < 4 ? "working" : "finished",
          summary: index < 4 ? null : `result ${index}`,
          touchedFileCount: 0,
          diffArtifactId: null,
        },
      ]),
    )
    app.setState({
      ...initial,
      streamingTail: createStreamingTail({ ...initial.streamingTail!, turnId: "2" }),
      subagentOrder: Object.keys(many),
      subagents: many,
    })
    await setup.flush()
    expect([...app.agentsStrip.rows.keys()]).toEqual(["child-0", "child-1", "child-2", "child-3"])
    expect(app.agentsStrip.more.visible).toBeFalse()
  })

  test("opens an exact child transcript from a clicked tree row", async () => {
    const setup = await createTestRenderer({ width: 80, height: 12, useThread: false })
    renderer = setup.renderer
    const opened: string[] = []
    const panel = new SubagentPanelRenderable(renderer, kennelTheme, (subagentId) => {
      opened.push(subagentId)
    })
    panel.update([{
      projectionId: "child-row",
      subagentId: "child-exact",
      parentTurnId: "1",
      task: "Inspect the provider layer",
      spawnedAtMs: Date.now(),
      status: "running",
      childSessionId: "child-session",
      lastChildSequence: "3",
      activity: "reading files",
      summary: null,
      touchedFileCount: 0,
      diffArtifactId: null,
    }])
    renderer.root.add(panel)
    await setup.renderOnce()
    const row = panel.rows.get("child-row")!
    await setup.mockMouse.click(row.x + 2, row.y)
    expect(opened).toEqual(["child-exact"])
  })

  test("opens an exact child transcript from a clicked strip row", async () => {
    const setup = await createTestRenderer({ width: 100, height: 12, useThread: false })
    renderer = setup.renderer
    const opened: string[] = []
    const strip = new AgentsStripRenderable(renderer, kennelTheme, (subagentId) => {
      opened.push(subagentId)
    })
    strip.update(stripInput([child("child-row", "running", { subagentId: "child-exact", activity: "using tool · read" })]), 84_000)
    renderer.root.add(strip)
    await setup.renderOnce()
    expect(strip.rows.get("child-row")?.plainText).toBe("◌ explore · Inspect child-row · using tool · read · 1m23s")
    const row = strip.rows.get("child-row")!
    await setup.mockMouse.click(row.x + 2, row.y)
    expect(opened).toEqual(["child-exact"])
  })

  test("bounds the agents strip and keeps running children first", async () => {
    const setup = await createTestRenderer({ width: 100, height: 14, useThread: false })
    renderer = setup.renderer
    const strip = new AgentsStripRenderable(renderer, kennelTheme, () => { })
    const children = Array.from({ length: 9 }, (_, index) => child(`child-${index}`, index < 7 ? "running" : "completed"))
    const state = stateWith(children)
    const entries = agentsStripEntries(state, new Set(), new Set(children.map((subagent) => subagent.projectionId)))
    strip.update({ ...stripInput(entries), backgroundKey: "Ctrl+B" }, 84_000)
    renderer.root.add(strip)
    await setup.renderOnce()
    expect([...strip.rows.keys()]).toEqual(Array.from({ length: 5 }, (_, index) => `child-${index}`))
    expect(strip.more.plainText).toBe("… 4 more")
    expect(strip.footer.plainText).toBe("╰ Ctrl+B background · Ctrl+G agents")
  })

  test("keeps finished children dimmed until they are hidden", async () => {
    const setup = await createTestRenderer({ width: 100, height: 14, useThread: false })
    renderer = setup.renderer
    const strip = new AgentsStripRenderable(renderer, kennelTheme, () => {})
    const done = child("done", "completed", { cost: { kind: "monetary", amount_micros: "12500", currency: "USD" } })
    const state = stateWith([done])
    expect(agentsStripEntries(state, new Set(), new Set())).toEqual([])
    strip.update(stripInput(agentsStripEntries(state, new Set(), new Set(["done"]))))
    const panel = new ContextPanelRenderable(renderer, kennelTheme, {})
    panel.update(state)
    renderer.root.add(strip)
    await setup.renderOnce()
    expect(panel.agents.options[0]?.name).toContain("USD 0.0125")
    expect(panel.agentsTitle.plainText).toContain("1 finished")
    panel.destroy()
    expect(strip.visible).toBeTrue()
    expect(strip.rows.get("done")?.plainText).toContain("completed · USD 0.0125")
    expect(strip.footer.plainText).toContain("hidden after your next message")
    strip.update(stripInput(agentsStripEntries(state, new Set(["done"]), new Set(["done"]))))
    expect(strip.visible).toBeFalse()
  })

  test("bounds a composed strip row to its measured content width", async () => {
    const setup = await createTestRenderer({ width: 32, height: 12, useThread: false })
    renderer = setup.renderer
    const strip = new AgentsStripRenderable(renderer, kennelTheme, () => { })
    strip.update(stripInput([child("child-wide", "running", {
      task: "界".repeat(48),
      activity: "👨‍👩‍👧‍👦 reviewing the terminal layout with a long status",
    })]), 84_000)
    renderer.root.add(strip)
    await setup.renderOnce()

    const row = strip.rows.get("child-wide")!
    expect(stringCellWidth(row.plainText)).toBeLessThanOrEqual(28)
    expect(row.plainText.endsWith("…")).toBe(true)
  })
})

type Projection = RottweilerState["subagents"][string]

function child(id: string, status: Projection["status"], overrides: Partial<Projection> = {}): Projection {
  return {
    projectionId: id,
    subagentId: id,
    parentTurnId: "1",
    task: `Inspect ${id}`,
    spawnedAtMs: 1_000,
    status,
    childSessionId: `session-${id}`,
    lastChildSequence: "3",
    activity: status === "running" ? "working" : null,
    summary: null,
    touchedFileCount: 0,
    diffArtifactId: null,
    ...overrides,
  }
}

function stateWith(children: readonly Projection[]): RottweilerState {
  return {
    ...createInitialState(),
    subagentOrder: children.map((subagent) => subagent.projectionId),
    subagents: Object.fromEntries(children.map((subagent) => [subagent.projectionId, subagent])),
  }
}

function stripInput(entries: readonly Projection[]): AgentsStripInput {
  return { entries, agentName: () => "explore", agentsKey: "Ctrl+G", backgroundKey: null }
}
