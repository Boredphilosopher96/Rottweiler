import { writeFile } from "node:fs/promises"

import { createTestRenderer } from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import { createEngineRuntimeFromEnvironment, type TuiEngineRuntime } from "../src/runtime"

const reportFile = process.env.ROTTWEILER_TEST_REPORT_FILE
if (reportFile === undefined) throw new Error("ROTTWEILER_TEST_REPORT_FILE is required")

const setup = await createTestRenderer({ width: 100, height: 24, useThread: false })
let runtime: TuiEngineRuntime | null = null
const app = createRottweilerApp(setup.renderer, {
  onCommand(command) {
    return runtime?.sendCommand(command) ?? null
  },
})
setup.renderer.root.add(app)
runtime = await createEngineRuntimeFromEnvironment()
if (runtime === null) throw new Error("supervised engine runtime was not configured")
runtime.bind(app)
const running = runtime.start()

try {
  await waitFor(() => app.state.connection.phase === "connected")
  await waitFor(() => app.interactionPanel.visible)
  await setup.renderOnce()
  const waitingBanner = app.banner.plainText
  app.interactionPanel.select.selectCurrent()
  await waitFor(() => app.state.turns["turn-approval"]?.status === "completed")
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

async function waitFor(predicate: () => boolean, timeoutMs = 5_000): Promise<void> {
  const deadline = Bun.nanoseconds() + timeoutMs * 1_000_000
  while (!predicate()) {
    if (Bun.nanoseconds() >= deadline) throw new Error("approval worker timed out")
    await Bun.sleep(2)
  }
}
