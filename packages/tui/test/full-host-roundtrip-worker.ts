import { writeFile } from "node:fs/promises"

import { createTestRenderer } from "@opentui/core/testing"

import { createRottweilerApp } from "../src/app"
import { commandResultMarkdown } from "../src/render"
import { createEngineRuntimeFromEnvironment } from "../src/runtime"

const reportFile = process.env.ROTTWEILER_TEST_REPORT_FILE
if (reportFile === undefined) throw new Error("ROTTWEILER_TEST_REPORT_FILE is required")

const setup = await createTestRenderer({ width: 100, height: 28, useThread: false })
const runtime = await createEngineRuntimeFromEnvironment()
if (runtime === null) throw new Error("supervised engine runtime was not configured")
const app = createRottweilerApp(setup.renderer, { historyReader: runtime.historyReader,
  onCommand(command) {
    return runtime.sendCommand(command)
  },
})
setup.renderer.root.add(app)
runtime.bind(app)
const running = runtime.start()

try {
  await waitFor("engine connection", () => app.state.connection.phase === "connected")

  app.composer.value = "/status"
  await app.composer.submit()
  await waitFor("human-readable status result", () => commandResult(app).includes("**Idle**"))

  app.composer.value = "Create approval.txt with the acceptance canary."
  await app.composer.submit()
  await waitFor("tool approval panel", () => app.interactionPanel.visible)
  await setup.renderOnce()
  const approvalBanner = app.banner.plainText
  const approvalPanel = app.interactionPanel.prompt.plainText
  app.interactionPanel.select.selectCurrent()

  await waitFor("completed turn", () => Object.values(app.state.turns).some((turn) => turn.status === "completed"))
  await waitFor("finished tool", () => Object.values(app.state.tools).some((tool) => tool.status === "finished"))
  await setup.renderOnce()
  const tool = Object.values(app.state.tools).find((candidate) => candidate.name === "write")
  await writeFile(reportFile, JSON.stringify({
    commandResult: commandResult(app),
    approvalBanner,
    approvalPanel,
    toolStatus: tool?.status ?? null,
    toolOutput: tool?.output ?? null,
    errors: app.state.errors.map((error) => error.code),
    errorDetails: app.state.errors.map((error) => error.message),
  }))
} finally {
  await runtime.stop()
  await running.catch(() => {})
  setup.renderer.destroy()
}

function commandResult(app: ReturnType<typeof createRottweilerApp>): string {
  return app.state.transcript
    .filter((entry) => entry.presentation === "command_result" && entry.title === "/status")
    .flatMap((entry) => entry.commandResult === undefined
      ? []
      : [commandResultMarkdown(entry.commandResult)])
    .join("\n")
}

async function waitFor(label: string, predicate: () => boolean, timeoutMs = 10_000): Promise<void> {
  const deadline = Bun.nanoseconds() + timeoutMs * 1_000_000
  while (!predicate()) {
    if (Bun.nanoseconds() >= deadline) throw new Error(`full-host roundtrip timed out waiting for ${label}`)
    await Bun.sleep(2)
    await setup.renderOnce()
    await setup.flush()
  }
}
