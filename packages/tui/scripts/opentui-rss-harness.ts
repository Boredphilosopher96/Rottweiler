import { observedResidentBytes } from "../src/process-memory"
import { BoxRenderable, TextRenderable } from "@opentui/core"
import { createTestRenderer } from "@opentui/core/testing"

interface Sample {
  readonly cycle: number
  readonly rssBytes: number
  readonly maxRssBytes: number
}

const cycles = positiveInteger(process.argv[2], 200)
const renderablesPerCycle = positiveInteger(process.argv[3], 200)
const setup = await createTestRenderer({ width: 100, height: 30, useThread: false })
const samples: Sample[] = []

try {
  for (let cycle = 0; cycle < cycles; cycle += 1) {
    const group = new BoxRenderable(setup.renderer, {
      id: `cycle-${cycle}`,
      width: "100%",
      flexDirection: "column",
    })
    for (let index = 0; index < renderablesPerCycle; index += 1) {
      group.add(new TextRenderable(setup.renderer, {
        id: `cycle-${cycle}-row-${index}`,
        content: `${cycle}:${index} ${"render graph payload ".repeat(8)}`,
      }))
    }
    setup.renderer.root.add(group)
    await setup.renderOnce()
    setup.renderer.root.remove(group)
    group.destroyRecursively()
    await setup.renderOnce()
    if (cycle % 10 === 0 || cycle === cycles - 1) {
      Bun.gc(true)
      samples.push({
        cycle,
        rssBytes: process.memoryUsage.rss(),
        maxRssBytes: observedResidentBytes(),
      })
    }
  }
} finally {
  setup.renderer.destroy()
}

const first = samples[0]
const last = samples.at(-1)
if (first === undefined || last === undefined) throw new Error("RSS harness produced no samples")
process.stdout.write(`${JSON.stringify({
  schemaVersion: 1,
  cycles,
  renderablesPerCycle,
  totalRenderables: cycles * renderablesPerCycle,
  rssGrowthBytes: last.rssBytes - first.rssBytes,
  maxRssBytes: Math.max(...samples.map((sample) => sample.maxRssBytes)),
  samples,
})}\n`)

function positiveInteger(value: string | undefined, fallback: number): number {
  if (value === undefined) return fallback
  const parsed = Number.parseInt(value, 10)
  if (!Number.isSafeInteger(parsed) || parsed < 1) {
    throw new Error(`expected a positive integer, received ${value}`)
  }
  return parsed
}
