import { writeFile } from "node:fs/promises"
import { join } from "node:path"
import { createRottweilerApp } from "../app"
import { ClientAllocationOwner } from "../client-allocation"
import { MAX_COMPOSER_TEXT_BYTES } from "../composer-drafts"
import { createInitialState } from "../state"
import { MemoryFixture } from "./memory-fixture"
import { createMemoryRenderer } from "./memory-renderer"

const TRIALS = 3
const KEYS = 128
const WARMUP_KEYS = 5
const BUDGET_MS = 16
const requireThat = (value: unknown, message: string) => { if (!value) throw new Error(message) }

/** Explicit compiled App input-to-native-frame kernel; preparation is outside each sample. */
export async function runClientInputProbe(reportPath: string, directory: string): Promise<void> {
  const allocations = new ClientAllocationOwner()
  const { setup, treeSitter, terminal } = await createMemoryRenderer()
  const trials: { samplesMs: number[]; p99Ms: number; finalUtf8Bytes: number; exactContent: boolean; nativeFrameContainsInput: boolean; allocationBytes: number }[] = []
  let failure: string | null = null
  let ownedFixture: MemoryFixture | undefined
  let ownedApp: ReturnType<typeof createRottweilerApp> | undefined
  try {
    const fixture = new MemoryFixture(join(directory, `input-${process.pid}.sock`), allocations)
    ownedFixture = fixture
    const initial = createInitialState()
    const app = createRottweilerApp(setup.renderer, { allocations, treeSitterClient: treeSitter,
      sessionId: "memory-probe", clientId: "memory-client", sessionReader: fixture.reader,
      initialState: { ...initial, connection: { ...initial.connection, phase: "connected" }, driverClientId: "memory-client" } })
    ownedApp = app
    setup.renderer.root.add(app)
    app.handleEvent(fixture.historyReady())

    const readyBy = performance.now() + 10_000
    while (app.transcript.mountedCards.size === 0 || app.state.todos.phase !== "ready" || fixture.client.commandUsage.reads.bytes !== 0) {
      if (performance.now() >= readyBy) throw new Error("input probe preparation did not settle")
      await Bun.sleep(1); await setup.renderOnce()
    }
    app.composer.focus()
    for (let trial = 0; trial < TRIALS; trial++) {
      const original = "é".repeat((MAX_COMPOSER_TEXT_BYTES - 256) / 2)
      app.composer.value = original
      app.composer.editor.cursorOffset = original.length
      await setup.renderOnce(); await setup.flush(); Bun.gc(true)
      requireThat(app.composer.value === original, "near-limit draft was not admitted exactly")
      const samplesMs: number[] = []
      let frame = ""
      for (let key = 0; key < KEYS; key++) {
        const started = performance.now()
        setup.mockInput.pressKey("x")
        await setup.renderOnce()
        frame = setup.captureCharFrame()
        samplesMs.push(performance.now() - started)
      }
      const ordered = samplesMs.slice(WARMUP_KEYS).sort((left, right) => left - right)
      const observed = { samplesMs, p99Ms: ordered[Math.ceil(ordered.length * 0.99) - 1] ?? Infinity,
        finalUtf8Bytes: Buffer.byteLength(app.composer.value), exactContent: app.composer.value === original + "x".repeat(KEYS),
        nativeFrameContainsInput: frame.includes("x".repeat(24)), allocationBytes: allocations.usage.bytes }
      trials.push(observed)
      requireThat(observed.exactContent && observed.nativeFrameContainsInput, "input or native painted frame lost admitted content")
      requireThat(observed.finalUtf8Bytes <= MAX_COMPOSER_TEXT_BYTES && (allocations.usage.domains.drafts ?? 0) > 0,
        "admitted draft lost its bounded allocation owner")
      requireThat(terminal.snapshot.queuedBytes === 0 && terminal.snapshot.bytes > 0, "native terminal output did not drain")
    }
  } catch (error) { failure = error instanceof Error ? error.message : String(error) }
  finally {
    try { ownedApp?.destroy() }
    finally { try { await ownedFixture?.close() } finally { setup.renderer.destroy() } }
  }
  const settledBy = performance.now() + 10_000
  while (allocations.usage.bytes !== 0 && performance.now() < settledBy) await Bun.sleep(1)
  if (allocations.usage.bytes !== 0) failure ??= "input probe teardown retained allocation"
  const passed = failure === null && trials.length === TRIALS && trials.every(trial => trial.p99Ms < BUDGET_MS)
  await writeFile(reportPath, JSON.stringify({ schemaVersion: 1, pid: process.pid, bunVersion: Bun.version,
    platform: process.platform, architecture: process.arch,
    measurement: "wall-clock App key dispatch through native frame capture; draining terminal sink",
    preparation: "parser and history setup, full draft admission, cursor placement, frame flush and GC excluded from samples",
    width: 110, height: 36, maximumComposerUtf8Bytes: MAX_COMPOSER_TEXT_BYTES, keysPerTrial: KEYS,
    warmupKeysExcludedPerTrial: WARMUP_KEYS, budgetMs: BUDGET_MS, trials,
    terminal: terminal.snapshot, finalAllocationBytes: allocations.usage.bytes, failure, passed }) + "\n", { mode: 0o600 })
  requireThat(passed, failure ?? `compiled input/render p99 exceeds ${BUDGET_MS}ms; inspect raw trials`)
}
