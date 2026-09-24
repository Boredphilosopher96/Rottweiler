import { createTestRenderer, type TestRenderer } from "@opentui/core/testing"
import { afterEach, expect, test } from "bun:test"
import { createRottweilerApp } from "../../src/app"
import { createInitialState } from "../../src/state"
import { emptySessionReader } from "../fixtures/history"
import type { ClientCommand } from "../../src/protocol"

let renderer: TestRenderer | undefined
afterEach(() => { renderer?.destroy(); renderer = undefined })

test("Ctrl+C requires two consecutive idle presses to exit", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  let exits = 0
  const app = createRottweilerApp(renderer, { sessionReader: emptySessionReader, onExit: () => { exits++ } })
  renderer.root.add(app)
  setup.mockInput.pressKey("c", { ctrl: true })
  expect(exits).toBe(0)
  await setup.renderOnce()
  expect(setup.captureCharFrame()).toContain("again to exit")
  setup.mockInput.pressKey("x")
  setup.mockInput.pressKey("c", { ctrl: true })
  expect(exits).toBe(0)
  setup.mockInput.pressKey("c", { ctrl: true })
  expect(exits).toBe(1)
})

test("Ctrl+C interrupts active compaction without exiting", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  renderer = setup.renderer
  const state = createInitialState()
  const emitted: ClientCommand[] = []
  let exits = 0
  const app = createRottweilerApp(renderer, {
    sessionReader: emptySessionReader,
    initialState: { ...state, compaction: { ...state.compaction, active: true } },
    onExit: () => { exits++ },
    onCommand: command => { emitted.push(command); return { type: "accepted" } },
  })
  renderer.root.add(app)
  setup.mockInput.pressKey("c", { ctrl: true })
  setup.mockInput.pressKey("c", { ctrl: true })
  await Promise.resolve()
  expect(exits).toBe(0)
  expect(emitted.filter(command => command.type === "interrupt")).toHaveLength(2)
})
