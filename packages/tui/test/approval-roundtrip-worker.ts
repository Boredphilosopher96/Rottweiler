import { emptySessionReader } from "./fixtures/history"
import { writeFile } from "node:fs/promises"

import { parseKeypress } from "@opentui/core"
import { createTestRenderer } from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import { createEngineRuntimeFromEnvironment, type TuiEngineRuntime } from "../src/runtime"

const reportFile = process.env.ROTTWEILER_TEST_REPORT_FILE
if (reportFile === undefined) throw new Error("ROTTWEILER_TEST_REPORT_FILE is required")

const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
let runtime: TuiEngineRuntime | null = null
const app = createRottweilerApp(setup.renderer, { sessionReader: emptySessionReader,
  onCommand(command, allocation) {
    return runtime?.sendCommand(command, allocation) ?? null
  },
})
setup.renderer.root.add(app)
runtime = await createEngineRuntimeFromEnvironment({ allocations: app.historyCache.allocations })
if (runtime === null) throw new Error("supervised engine runtime was not configured")
runtime.bind(app)
const running = runtime.start()

try {
  await waitFor("connection", () => app.state.connection.phase === "connected")
  await waitFor("approval panel", () => app.interactionPanel.visible)
  await setup.renderOnce()
  const waitingBanner = app.banner.plainText
  const enter = parseKeypress("\n", { useKittyKeyboard: true })
  if (enter === null) throw new Error("could not parse terminal line-feed")
  setup.renderer.keyInput.processParsedKey(enter)
  await waitFor("completion", () => app.state.turns["turn-approval"]?.status === "completed")
  await setup.renderOnce()
  await writeFile(reportFile, JSON.stringify({
    waitingBanner,
    panelVisibleAfterCompletion: app.interactionPanel.visible,
    turnStatus: app.state.turns["turn-approval"]?.status ?? null,
    errors: app.state.errors.map((error) => error.code),
  }))
} finally {
  await runtime.stop()
  await running.catch(() => {})
  setup.renderer.destroy()
}

async function waitFor(stage: string, predicate: () => boolean, timeoutMs = 5_000): Promise<void> {
  const deadline = Bun.nanoseconds() + timeoutMs * 1_000_000
  while (!predicate()) {
    if (Bun.nanoseconds() >= deadline) throw new Error(`approval worker timed out at ${stage}: connection=${app.state.connection.phase}, cursor=${app.state.lastSequence}, tools=${Object.values(app.state.tools).map(tool => tool.status)}, invalid=${app.state.protocol.invalidEvents}, panel=${app.interactionPanel.visible}`)
    await Bun.sleep(2)
    await setup.renderOnce()
    await setup.flush()
  }
}
