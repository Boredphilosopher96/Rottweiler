import { afterAll, afterEach, describe, expect, test } from "bun:test"
import { mkdirSync, mkdtempSync, renameSync, rmSync, writeFileSync } from "node:fs"
import { dirname, join } from "node:path"
import { tmpdir } from "node:os"
import {
  createTestRenderer,
  MockTreeSitterClient,
  type TestRenderer,
} from "@opentui/core/testing"
import { TreeSitterClient } from "@opentui/core"

import { createRottweilerApp } from "../../src/app"
import {
  createInitialState,
  engineEvent,
  reduceRottweilerState,
  type RottweilerState,
} from "../../src/state"
import { PROTOCOL_VERSION } from "../../src/protocol"

const emittedMetrics: Record<string, number> = {}
const rawSamples: Record<string, { clock: string; warmup: number; trialsMs: number[][] }> = {}

function samplesFor(name: string, clock = "process_cpu", warmup = 10): number[] {
  const samples: number[] = []
  const record = rawSamples[name] ??= { clock, warmup, trialsMs: [] }
  record.trialsMs.push(samples)
  return samples
}

function writeReport(output: string, report: unknown): void {
  mkdirSync(dirname(output), { recursive: true })
  const temporary = join(dirname(output), `.${crypto.randomUUID()}.tmp`)
  writeFileSync(temporary, `${JSON.stringify(report)}\n`, { encoding: "utf8", mode: 0o600 })
  renameSync(temporary, output)
}

afterAll(() => {
  const output = process.env.ROTTWEILER_PERF_OUTPUT
  if (output === undefined || output === "") return
  const expected = [
    "tui_frame_p95_us",
    "tui_frame_p999_us",
    "tui_input_echo_best_p99_us",
    "tui_tool_output_frame_p95_us",
    "tui_tools_workspace_frame_p95_us",
    "tui_vim_echo_best_p99_us",
  ]
  writeReport(`${output}.samples.json`, {
    schema_version: 1,
    bun_version: Bun.version,
    platform: process.platform,
    arch: process.arch,
    samples: rawSamples,
  })
  writeReport(output, { schema_version: 1, metrics: emittedMetrics })
  expect(Object.keys(emittedMetrics).sort()).toEqual(expected)
})

describe("M4 executable TUI performance budgets", () => {
  const frameP95BudgetMs = process.platform === "linux" ? 40 : 20
  const frameP999BudgetMs = process.platform === "linux" ? 66 : 33
  let renderer: TestRenderer | undefined
  let treeSitter: MockTreeSitterClient | TreeSitterClient | undefined
  let parserDataPath: string | undefined

  afterEach(async () => {
    renderer?.destroy()
    renderer = undefined
    await treeSitter?.destroy()
    treeSitter = undefined
    if (parserDataPath !== undefined) rmSync(parserDataPath, { recursive: true, force: true })
    parserDataPath = undefined
  })

  test("retained transcript streaming frame compute stays inside p95/p99.9 budgets", async () => {
    // Perf files share a Bun process with the component suite. Collect before
    // allocating the 10MiB fixture so unrelated suite garbage cannot become a
    // frame-compute outlier while preserving the production budget itself.
    Bun.gc(true)
    const setup = await createTestRenderer({
      width: 100,
      height: 30,
      useThread: false,
      gatherStats: true,
    })
    renderer = setup.renderer
    const mockTreeSitter = new MockTreeSitterClient({ autoResolveTimeout: 0 })
    mockTreeSitter.setMockResult({ highlights: [] })
    treeSitter = mockTreeSitter
    const payload = "x".repeat(1_018)
    const transcript = Array.from({ length: 10_240 }, (_, index) => ({
      sequenceId: String(index + 1),
      agentTurn: String(index + 1),
      turn: {
        role: "assistant" as const,
        blocks: [{ type: "text" as const, text: `${String(index).padStart(5, "0")} ${payload}` }],
        meta: { synthetic: false, summary: false },
      },
    }))
    expect(transcript.reduce((bytes, entry) => bytes + entry.turn.blocks.reduce((sum, block) => sum + Buffer.byteLength(block.text), 0), 0)).toBe(10 * 1_024 * 1_024)
    const base: RottweilerState = {
      ...createInitialState(),
      transcript,
      streamingTail: {
        turnId: "10241",
        text: "",
        thinking: "",
        citations: [],
        toolCallIds: [],
        finished: null,
      },
    }
    const app = createRottweilerApp(renderer, {
      initialState: base,
      treeSitterClient: mockTreeSitter,
    })
    renderer.root.add(app)
    await setup.waitFor(() => mockTreeSitter.isHighlighting() === false)
    await setup.flush()
    expect(app.transcript.mountedEntryCount).toBe(16)

    for (let warmup = 0; warmup < 10; warmup += 1) {
      app.setState({
        ...base,
        streamingTail: { ...base.streamingTail!, text: `warmup ${warmup}\n` },
      })
      await setup.renderOnce()
    }
    Bun.gc(true)

    const samples = samplesFor("retained_transcript")
    let streamed = ""
    for (let line = 0; line < 200; line += 1) {
      streamed += `line ${line} streamed without re-laying out history\n`
      // This budget is explicitly frame compute, not wall-clock scheduling.
      // Hosted runners may deschedule the Bun process between the async render
      // call and its continuation; process CPU time retains renderer, native,
      // allocation, and GC work while excluding unrelated host contention.
      const started = process.cpuUsage()
      app.setState({
        ...base,
        streamingTail: { ...base.streamingTail!, text: streamed },
      })
      await setup.renderOnce()
      const used = process.cpuUsage(started)
      samples.push((used.user + used.system) / 1_000)
    }

    const p95 = percentile(samples.slice(10), 0.95)
    const p999 = percentile(samples.slice(10), 0.999)
    emittedMetrics.tui_frame_p95_us = Math.ceil(p95 * 1_000)
    emittedMetrics.tui_frame_p999_us = Math.ceil(p999 * 1_000)
    expect(p95).toBeLessThan(frameP95BudgetMs)
    expect(p999).toBeLessThan(frameP999BudgetMs)
    expect(app.transcript.mountedEntryCount).toBe(16)
    const native = setup.getNativeStats()
    // OpenTUI's native stats expose frame duration in microseconds.
    expect(native.nativeLastFrameTime).toBeLessThan(frameP999BudgetMs * 1_000)
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

    const trialP99s: number[] = []
    const input = "responsivetypingwithoutblockingtherenderloop".repeat(4)
    for (let trial = 0; trial < 7; trial += 1) {
      app.composer.value = ""
      await setup.renderOnce()
      Bun.gc(true)
      const samples = samplesFor("composer_input", inputLatencyClock(), 5)
      for (const key of input) {
        const elapsed = startInputLatencySample()
        setup.mockInput.pressKey(key)
        await setup.renderOnce()
        setup.captureCharFrame()
        samples.push(elapsed())
      }
      expect(samples.slice(5).length).toBeGreaterThanOrEqual(100)
      trialP99s.push(percentile(samples.slice(5), 0.99))
    }

    expect(app.composer.value).toBe(input)
    const bestP99 = Math.min(...trialP99s)
    emittedMetrics.tui_input_echo_best_p99_us = Math.ceil(bestP99 * 1_000)
    console.info(
      `Focused composer input echo (${inputLatencyClock()}): trial p99s=${trialP99s.map((value) => value.toFixed(3)).join(",")}ms; best=${bestP99.toFixed(3)}ms`,
    )
    // Every trial retains the 16ms hard ceiling. Protected runs use wall time;
    // shared-runner smoke uses process CPU time so descheduling is not charged
    // as input/render compute. A compute regression still moves every trial.
    for (const trialP99 of trialP99s) expect(trialP99).toBeLessThan(16)
  })

  test("mounted tool-output bursts stay inside the frame budget with live Tree-sitter", async () => {
    Bun.gc(true)
    parserDataPath = mkdtempSync(join(tmpdir(), "rottweiler-tool-output-perf-"))
    treeSitter = new TreeSitterClient({
      dataPath: parserDataPath,
      workerPath: join(import.meta.dir, "../../node_modules/@opentui/core/parser.worker.js"),
    })
    await treeSitter.initialize()
    const setup = await createTestRenderer({
      width: 100,
      height: 30,
      useThread: false,
      gatherStats: true,
    })
    renderer = setup.renderer
    const transcript = Array.from({ length: 40 }, (_, index) => ({
      sequenceId: String(index + 1),
      agentTurn: String(index + 1),
      turn: {
        role: "assistant" as const,
        blocks: [{
          type: "text" as const,
          text: `Result ${index}\n\n\`\`\`typescript\nconst value${index} = ${index}\n\`\`\``,
        }],
        meta: { synthetic: false, summary: false },
      },
    }))
    let state: RottweilerState = { ...createInitialState(), transcript }
    const meta = (sequence: number) => ({
      protocol_version: PROTOCOL_VERSION,
      session_id: "tool-output-performance",
      sequence_id: String(1_000 + sequence),
      emitted_at: "2026-08-22T00:00:00Z",
    })
    state = reduceRottweilerState(state, engineEvent({
      type: "turn_started",
      meta: meta(0),
      turn_id: "tool-output-turn",
    }))
    for (let index = 0; index < 16; index += 1) {
      state = reduceRottweilerState(state, engineEvent({
        type: "tool_call_started",
        meta: meta(index + 1),
        turn_id: "tool-output-turn",
        tool_call_id: `mounted-tool-${index}`,
        name: "bash",
        args: { command: `fixture-${index}` },
        call_index: index,
      }))
    }
    const app = createRottweilerApp(renderer, { initialState: state, treeSitterClient: treeSitter })
    renderer.root.add(app)
    await setup.flush()
    expect(app.transcript.mountedEntryCount).toBe(16)
    expect(app.transcript.streamingCard.visible).toBeTrue()
    Bun.gc(true)

    const samples = samplesFor("mounted_tool_output")
    const chunk = "output-line 0123456789abcdef\n".repeat(293).slice(0, 8 * 1_024)
    for (let index = 0; index < 120; index += 1) {
      const started = process.cpuUsage()
      state = reduceRottweilerState(state, engineEvent({
        type: "tool_output_delta",
        meta: meta(100 + index),
        turn_id: "tool-output-turn",
        tool_call_id: "mounted-tool-15",
        stream: "stdout",
        chunk,
      }))
      app.setState(state)
      await setup.renderOnce()
      const used = process.cpuUsage(started)
      samples.push((used.user + used.system) / 1_000)
    }

    const p95 = percentile(samples.slice(10), 0.95)
    emittedMetrics.tui_tool_output_frame_p95_us = Math.ceil(p95 * 1_000)
    expect(p95).toBeLessThan(frameP95BudgetMs)
  }, 20_000)

  test("visible Tools workspace streams bounded retained rows without identity churn", async () => {
    Bun.gc(true)
    const setup = await createTestRenderer({
      width: 110,
      height: 32,
      useThread: false,
      gatherStats: true,
    })
    renderer = setup.renderer
    const meta = (sequence: number) => ({
      protocol_version: PROTOCOL_VERSION,
      session_id: "tools-workspace-performance",
      sequence_id: String(10_000 + sequence),
      emitted_at: "2026-08-25T12:00:00Z",
    })
    let state = reduceRottweilerState(createInitialState(), engineEvent({
      type: "turn_started",
      meta: meta(0),
      turn_id: "tools-performance-turn",
    }))
    for (let index = 0; index < 16; index += 1) {
      state = reduceRottweilerState(state, engineEvent({
        type: "tool_call_started",
        meta: meta(index + 1),
        turn_id: "tools-performance-turn",
        tool_call_id: `tools-performance-${index}`,
        name: "bash",
        args: { command: `fixture-${index}` },
        call_index: index,
      }))
    }
    const app = createRottweilerApp(renderer, { initialState: state })
    renderer.root.add(app)
    app.showToolsView()
    await setup.renderOnce()
    expect(app.toolsWorkspace.mountedRowCount).toBe(16)
    expect(app.toolsElapsedTimerActive).toBeTrue()
    const rowIdentities = new Map(
      app.toolsWorkspace.mountedRowKeys.map((key) => [key, app.toolsWorkspace.rowForKey(key)]),
    )

    const samples = samplesFor("tools_workspace")
    const chunk = "tools-output-line 0123456789abcdef\n".repeat(240).slice(0, 8 * 1_024)
    for (let index = 0; index < 120; index += 1) {
      const started = process.cpuUsage()
      state = reduceRottweilerState(state, engineEvent({
        type: "tool_output_delta",
        meta: meta(100 + index),
        turn_id: "tools-performance-turn",
        tool_call_id: `tools-performance-${index % 16}`,
        stream: "stdout",
        chunk,
      }))
      app.setState(state)
      await setup.renderOnce()
      const used = process.cpuUsage(started)
      samples.push((used.user + used.system) / 1_000)
    }

    const p95 = percentile(samples.slice(10), 0.95)
    emittedMetrics.tui_tools_workspace_frame_p95_us = Math.ceil(p95 * 1_000)
    expect(p95).toBeLessThan(frameP95BudgetMs)
    expect(app.toolsWorkspace.mountedRowCount).toBe(16)
    for (const [key, identity] of rowIdentities) {
      expect(app.toolsWorkspace.rowForKey(key)).toBe(identity)
    }

    app.showConversationView()
    expect(app.toolsElapsedTimerActive).toBeFalse()
    app.setState({
      ...state,
      replay: {
        active: true,
        sessionId: "tools-workspace-performance",
        completedThrough: state.lastSequence,
      },
    })
    app.showToolsView()
    expect(app.toolsElapsedTimerActive).toBeFalse()
  }, 20_000)

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

    const trialP99s: number[] = []
    const input = "vimmodestaysresponsiveundertyping".repeat(4)
    for (let trial = 0; trial < 7; trial += 1) {
      app.composer.value = ""
      await setup.renderOnce()
      Bun.gc(true)
      const samples = samplesFor("vim_input", inputLatencyClock(), 5)
      for (const key of input) {
        const elapsed = startInputLatencySample()
        setup.mockInput.pressKey(key)
        await setup.renderOnce()
        setup.captureCharFrame()
        samples.push(elapsed())
      }
      expect(samples.slice(5).length).toBeGreaterThanOrEqual(100)
      trialP99s.push(percentile(samples.slice(5), 0.99))
    }

    expect(app.composer.value).toBe(input)
    const bestP99 = Math.min(...trialP99s)
    emittedMetrics.tui_vim_echo_best_p99_us = Math.ceil(bestP99 * 1_000)
    console.info(
      `Vim composer input echo (${inputLatencyClock()}): trial p99s=${trialP99s.map((value) => value.toFixed(3)).join(",")}ms; best=${bestP99.toFixed(3)}ms`,
    )
    for (const trialP99 of trialP99s) expect(trialP99).toBeLessThan(16)
  })
})

function percentile(values: readonly number[], quantile: number): number {
  const sorted = [...values].sort((left, right) => left - right)
  const index = Math.min(sorted.length - 1, Math.ceil(sorted.length * quantile) - 1)
  return sorted[Math.max(0, index)] ?? Number.POSITIVE_INFINITY
}

function inputLatencyClock(): "process CPU" | "wall" {
  return process.env.ROTTWEILER_PERF_SMOKE === "1" ? "process CPU" : "wall"
}

function startInputLatencySample(): () => number {
  if (process.env.ROTTWEILER_PERF_SMOKE === "1") {
    const started = process.cpuUsage()
    return () => {
      const used = process.cpuUsage(started)
      return (used.user + used.system) / 1_000
    }
  }
  const started = Bun.nanoseconds()
  return () => (Bun.nanoseconds() - started) / 1_000_000
}
