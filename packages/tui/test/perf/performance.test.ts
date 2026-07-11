import { afterEach, describe, expect, test } from "bun:test"
import {
  createTestRenderer,
  MockTreeSitterClient,
  type TestRenderer,
} from "@opentui/core/testing"

import { createRottweilerApp } from "../../src/app"
import { createInitialState, type RottweilerState } from "../../src/state"

describe("M4 executable TUI performance budgets", () => {
  let renderer: TestRenderer | undefined
  let treeSitter: MockTreeSitterClient | undefined

  afterEach(async () => {
    renderer?.destroy()
    renderer = undefined
    await treeSitter?.destroy()
    treeSitter = undefined
  })

  test("10MB transcript streaming frame compute stays inside p95/p99.9 budgets", async () => {
    // Perf files share a Bun process with the component suite. Collect before
    // allocating the 10MB fixture so unrelated suite garbage cannot become a
    // frame-compute outlier while preserving the production budget itself.
    Bun.gc(true)
    const setup = await createTestRenderer({
      width: 100,
      height: 30,
      useThread: false,
      gatherStats: true,
    })
    renderer = setup.renderer
    treeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
    treeSitter.setMockResult({ highlights: [] })
    const payload = "x".repeat(1_020)
    const transcript = Array.from({ length: 10_000 }, (_, index) => ({
      sequenceId: String(index + 1),
      agentTurn: String(index + 1),
      turn: {
        role: "assistant" as const,
        blocks: [{ type: "text" as const, text: `${index} ${payload}` }],
        meta: { synthetic: false, summary: false },
      },
    }))
    const base: RottweilerState = {
      ...createInitialState(),
      transcript,
      streamingTail: {
        turnId: "10001",
        text: "",
        thinking: "",
        citations: [],
        toolCallIds: [],
        finished: null,
      },
    }
    const app = createRottweilerApp(renderer, {
      initialState: base,
      treeSitterClient: treeSitter,
    })
    renderer.root.add(app)
    await setup.waitFor(() => treeSitter?.isHighlighting() === false)
    await setup.flush()
    expect(app.transcript.mountedEntryCount).toBeLessThan(24)

    for (let warmup = 0; warmup < 10; warmup += 1) {
      app.setState({
        ...base,
        streamingTail: { ...base.streamingTail!, text: `warmup ${warmup}\n` },
      })
      await setup.renderOnce()
    }
    Bun.gc(true)

    const samples: number[] = []
    let streamed = ""
    for (let line = 0; line < 200; line += 1) {
      streamed += `line ${line} streamed without re-laying out history\n`
      const started = Bun.nanoseconds()
      app.setState({
        ...base,
        streamingTail: { ...base.streamingTail!, text: streamed },
      })
      await setup.renderOnce()
      samples.push((Bun.nanoseconds() - started) / 1_000_000)
    }

    const p95 = percentile(samples.slice(10), 0.95)
    const p999 = percentile(samples.slice(10), 0.999)
    expect(p95).toBeLessThan(16)
    expect(p999).toBeLessThan(33)
    expect(app.transcript.mountedEntryCount).toBeLessThan(24)
    const native = setup.getNativeStats()
    // OpenTUI's native stats expose frame duration in microseconds.
    expect(native.nativeLastFrameTime).toBeLessThan(33_000)
  }, 20_000)

  test("focused composer input echo stays below 16ms p99", async () => {
    Bun.gc(true)
    const setup = await createTestRenderer({
      width: 80,
      height: 20,
      useThread: false,
      gatherStats: true,
    })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer)
    renderer.root.add(app)
    await setup.renderOnce()
    app.composer.focus()

    for (const key of "warmup") {
      setup.mockInput.pressKey(key)
      await setup.renderOnce()
    }
    app.composer.value = ""
    await setup.renderOnce()
    Bun.gc(true)

    const samples: number[] = []
    const input = "responsivetypingwithoutblockingtherenderloop"
    for (const key of input) {
      const started = Bun.nanoseconds()
      setup.mockInput.pressKey(key)
      await setup.renderOnce()
      setup.captureCharFrame()
      samples.push((Bun.nanoseconds() - started) / 1_000_000)
    }

    expect(app.composer.value).toBe(input)
    expect(percentile(samples.slice(5), 0.99)).toBeLessThan(16)
  })

  test("Vim mode dispatch and insert echo stay below 16ms p99", async () => {
    Bun.gc(true)
    const setup = await createTestRenderer({
      width: 80,
      height: 20,
      useThread: false,
      gatherStats: true,
    })
    renderer = setup.renderer
    const app = createRottweilerApp(renderer, { keybindings: { preset: "vim" } })
    renderer.root.add(app)
    await setup.renderOnce()
    setup.mockInput.pressKey("i")

    for (const key of "modalwarmup") {
      setup.mockInput.pressKey(key)
      await setup.renderOnce()
    }
    app.composer.value = ""
    await setup.renderOnce()
    Bun.gc(true)

    const samples: number[] = []
    const input = "vimmodestaysresponsiveundertyping"
    for (const key of input) {
      const started = Bun.nanoseconds()
      setup.mockInput.pressKey(key)
      await setup.renderOnce()
      setup.captureCharFrame()
      samples.push((Bun.nanoseconds() - started) / 1_000_000)
    }

    expect(app.composer.value).toBe(input)
    expect(percentile(samples.slice(5), 0.99)).toBeLessThan(16)
  })
})

function percentile(values: readonly number[], quantile: number): number {
  const sorted = [...values].sort((left, right) => left - right)
  const index = Math.min(sorted.length - 1, Math.ceil(sorted.length * quantile) - 1)
  return sorted[Math.max(0, index)] ?? Number.POSITIVE_INFINITY
}
