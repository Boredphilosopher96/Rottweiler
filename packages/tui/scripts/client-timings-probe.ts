import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { createRottweilerApp } from "../src/app"
import { ClientDiagnostics } from "../src/client-diagnostics"
import { boundedJson } from "../src/transport/json"
import { emptyHistoryReader, fixturePage } from "../test/fixtures/history"

// Diagnostic comparison, not a replacement statistic or threshold for the M4 gates.
const WARMUP = 200
const SAMPLES = 400
const payload = JSON.stringify({ text: "a".repeat(256 * 1024) })
const trials: Array<{ enabled: boolean; decodeCpuMs: number; historyFrameCpuMs: number }> = []
for (const enabled of [false, true, true, false, true, false, false, true]) {
  const diagnostics = enabled ? new ClientDiagnostics() : undefined
  let decodeCpuMs = 0
  for (let index = 0; index < WARMUP + SAMPLES; index += 1) {
    const response = new Response(payload)
    const started = process.cpuUsage()
    await boundedJson(response, 1024 * 1024, diagnostics)
    if (index >= WARMUP) {
      const elapsed = process.cpuUsage(started)
      decodeCpuMs += (elapsed.user + elapsed.system) / 1000
    }
  }
  const setup = await createTestRenderer({ width: 110, height: 32, useThread: false })
  const treeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
  treeSitter.setMockResult({ highlights: [] })
  const app = createRottweilerApp(setup.renderer, { diagnostics, historyReader: emptyHistoryReader, treeSitterClient: treeSitter })
  setup.renderer.root.add(app)
  await setup.flush()
  let historyFrameCpuMs = 0
  try {
    const source = fixturePage("timing-probe", { known_view: null, position: { type: "latest" }, max_items: 32, max_bytes: 256 * 1024 })
    for (let index = 0; index < WARMUP + SAMPLES; index += 1) {
      const page = {
        ...source, items: source.items.map(item => ({ ...item, revision: String(2000 + index) })),
        view: { ...source.view, through: String(2000 + index) }
      }
      const started = process.cpuUsage()
      app.transcript.setHistory({
        sessionId: "timing-probe", page, total: 1000n, loading: false, error: null,
        following: true, selection: null, anchor: null
      })
      await setup.renderOnce()
      if (index >= WARMUP) {
        const elapsed = process.cpuUsage(started)
        historyFrameCpuMs += (elapsed.user + elapsed.system) / 1000
      }
    }
    trials.push({ enabled, decodeCpuMs: decodeCpuMs / SAMPLES, historyFrameCpuMs: historyFrameCpuMs / SAMPLES })
  } finally {
    setup.renderer.destroy()
    await treeSitter.destroy()
  }
}
process.stdout.write(`${JSON.stringify({
  version: 1, bun: Bun.version, platform: process.platform, arch: process.arch,
  warmup: WARMUP, samples: SAMPLES, decodedBytes: payload.length, mountedRows: 16, trials
}, null, 2)}\n`)
