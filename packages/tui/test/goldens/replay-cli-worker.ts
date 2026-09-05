import { writeFileSync } from "node:fs"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"

import { createRottweilerApp } from "../../src/app"
import { createEngineRuntimeFromEnvironment } from "../../src/runtime"

const reportFile = process.env.ROTTWEILER_TEST_REPORT_FILE
const sessionId = process.env.ROTTWEILER_SESSION_ID
if (reportFile === undefined || sessionId === undefined) {
  throw new Error("replay CLI worker requires report and session environment")
}

const setup = await createTestRenderer({ width: 112, height: 32, useThread: false })
const treeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
treeSitter.setMockResult({ highlights: [] })
const runtime = await createEngineRuntimeFromEnvironment()
if (runtime === null) throw new Error("replay CLI worker requires an engine runtime")
const app = createRottweilerApp(setup.renderer, { historyReader: runtime.historyReader,
  sessionId,
  replaySessionId: sessionId,
  treeSitterClient: treeSitter,
})
setup.renderer.root.add(app)

runtime.bind(app)
const running = runtime.start()

try {
  await waitFor(() => app.state.historyReady !== null && app.transcript.mountedEntryCount === 4)
  await setup.renderOnce()
  await waitFor(() => treeSitter.isHighlighting() === false)
  await setup.flush()
  const frame = setup
    .captureCharFrame()
    .split("\n")
    .map((line) => line.trimEnd())
    .join("\n")
  const styled = setup.captureSpans().lines.map((line) =>
    line.spans
      .filter((span) => span.text.trim().length > 0)
      .map((span) => [span.text, span.fg.toInts(), span.bg.toInts(), span.attributes]),
  )
  writeFileSync(
    reportFile,
    `${JSON.stringify({
      frame,
      styledDigest: stableDigest(JSON.stringify(styled)),
      styledSpanCount: styled.reduce((total, line) => total + line.length, 0),
      historyThrough: app.state.historyReady?.through,
      mountedItems: app.transcript.mountedEntryCount,
      completedThrough: app.state.replay.completedThrough,
      lastSequence: app.state.lastSequence,
      invalidEvents: app.state.protocol.invalidEvents,
    })}\n`,
    { encoding: "utf8", mode: 0o600 },
  )
} finally {
  await runtime.stop()
  await running
  setup.renderer.destroy()
  await treeSitter.destroy()
}

async function waitFor(predicate: () => boolean, timeoutMs = 5_000): Promise<void> {
  const deadline = performance.now() + timeoutMs
  while (!predicate()) {
    if (performance.now() >= deadline) throw new Error("timed out waiting for CLI replay")
    await Bun.sleep(5)
    await setup.renderOnce()
    await setup.flush()
  }
}

function stableDigest(value: string): string {
  let hash = 2_166_136_261
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index)
    hash = Math.imul(hash, 16_777_619)
  }
  return (hash >>> 0).toString(16).padStart(8, "0")
}
