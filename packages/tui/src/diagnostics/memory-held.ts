import { writeFile } from "node:fs/promises"
import { join } from "node:path"
import { createRottweilerApp, type RottweilerAppOptions } from "../app"
import { ClientAllocationOwner } from "../client-allocation"
import { DocumentController } from "../history/document"
import { PROTOCOL_VERSION } from "../protocol"
import { createInitialState } from "../state"
import { observedResidentBytes } from "../process-memory"
import { MEMORY_LOAD, MemoryFixture } from "./memory-fixture"
import { createMemoryRenderer } from "./memory-renderer"

export const HELD_MEMORY_VIEWS = ["output", "review", "secret", "action"] as const
export type HeldMemoryView = typeof HELD_MEMORY_VIEWS[number]
const requireThat = (value: unknown, message: string) => { if (!value) throw new Error(message) }

/** One mounted renderer and one unresolved view survive every streaming cycle. */
export async function runHeldViewMemoryProbe(reportPath: string, directory: string, cycles: number, view: HeldMemoryView): Promise<void> {
  if (!HELD_MEMORY_VIEWS.includes(view) || !Number.isSafeInteger(cycles) || cycles < 1 || cycles > 1000) throw new Error("invalid held-view workload")
  const allocations = new ClientAllocationOwner()
  const fixture = new MemoryFixture(join(directory, `held-${process.pid}.sock`), allocations)
  const { setup, treeSitter, terminal } = await createMemoryRenderer()
  type Credential = Awaited<ReturnType<NonNullable<RottweilerAppOptions["onProviderApiKey"]>>>
  const credential = Promise.withResolvers<Credential>()
  let credentialStarted = false
  let credentialCorrect = false
  const initial = createInitialState()
  const app = createRottweilerApp(setup.renderer, {
    allocations, treeSitterClient: treeSitter, sessionId: "memory-probe", clientId: "memory-client", sessionReader: fixture.reader,
    initialState: { ...initial, connection: { ...initial.connection, phase: "connected" }, driverClientId: "memory-client" },
    async onCommand(command, allocation) {
      const reply = await fixture.command(command, allocation)
      if (reply.type === "read") for (const event of reply.events) app.handleEvent(event)
      const completion = fixture.connectionCompletion(command)
      if (completion !== null) app.handleEvent(completion)
      return reply.outcome
    },
    onProviderApiKey(_provider, key, allocation) { allocation.admit(4096); credentialCorrect = key === "synthetic-secret-canary"; credentialStarted = true; return credential.promise },
  })
  setup.renderer.root.add(app)
  const until = async (condition: () => boolean) => {
    const deadline = performance.now() + 10_000
    while (!condition()) {
      if (performance.now() >= deadline) throw new Error("held-view setup did not settle")
      await Bun.sleep(1); await setup.renderOnce()
    }
  }
  const document = new DocumentController(fixture.reader, app.historyCache, snapshot => {
    if (snapshot.open) app.outputViewer.showDocument(snapshot)
    else app.outputViewer.closePresentation()
  })
  const samples: Array<{ cycle: number; elapsedMs: number; rssBytes: number; highWaterBytes: number; allocation: typeof allocations.usage; terminal: typeof terminal.snapshot }> = []
  let sequence = 20000
  const meta = () => ({ protocol_version: PROTOCOL_VERSION, session_id: "memory-probe", sequence_id: String(sequence++), emitted_at: "2026-09-06T00:00:00Z" })
  const identity = (index: number) => ({ turn_id: "held-turn", tool_call_id: `held-provider-${index}`, invocation_id: `held-invocation-${index}` })
  try {
    await until(() => app.transcript.mountedCards.size > 0 && (allocations.usage.domains.outbound ?? 0) === 0
      && (allocations.usage.domains.decoding ?? 0) === 0)
    app.composer.value = "retained draft " + "d".repeat(MEMORY_LOAD.draftBytes)
    app.handleEvent({ type: "turn_started", meta: meta(), turn_id: "held-turn" })
    for (let index = 0; index < MEMORY_LOAD.toolInvocations; index++) {
      app.handleEvent({ type: "tool_call_started", meta: meta(), ...identity(index), name: "bash", args: { command: "held output" }, call_index: index })
    }
    if (view === "output") {
      await document.openSource({ sessionId: "memory-probe", scope: { type: "session" } }, { sequence: "19999", selector: { type: "command_message" } })
      requireThat(document.snapshot.page !== null, "held document source unavailable")
    } else if (view === "review") {
      app.openReview()
      await until(() => (app.state.review?.files.length ?? 0) > 0)
    } else {
      app.openProviderApiKeyPrompt("held-provider")
      await setup.mockInput.typeText("synthetic-secret-canary")
      if (view === "action") { setup.mockInput.pressEnter(); await until(() => credentialStarted) }
    }
    const heldAt = performance.now()
    for (let cycle = 0; cycle < cycles; cycle++) {
      for (let index = 0; index < MEMORY_LOAD.toolInvocations; index++) {
        for (let chunk = 0; chunk < 4; chunk++) app.handleEvent({ type: "tool_output_delta", meta: meta(), ...identity(index), stream: "stdout", chunk: (`${cycle}:${index}:${chunk} ` + "output ".repeat(585)).slice(0, 4096) })
      }
      app.handleEvent({ type: "text_delta", meta: meta(), turn_id: "held-turn", text: (`cycle ${cycle} ` + "assistant ".repeat(409)).slice(0, 4096) })
      await Bun.sleep(50)
      await setup.renderOnce(); await setup.flush()
      requireThat(terminal.writableLength === 0, "terminal output did not drain")
      requireThat(terminal.bytes > 0, "native terminal output was not delivered")
      requireThat(app.state.lastSequence === String(sequence - 1), "streaming prefix was not consumed")
      for (let index = 0; index < MEMORY_LOAD.toolInvocations; index++) {
        const chunks = app.state.tools[identity(index).invocation_id]?.chunks
        requireThat(chunks !== undefined && chunks.retainedBytes + chunks.omittedBytes === (cycle + 1) * 4 * 4096, "tool bytes were not consumed exactly")
      }
      requireThat(app.recycleState() === null, "an unresolved held view was discarded for recycle")
      if (view === "output") requireThat(app.outputViewer.visible && document.snapshot.page?.source.sequence === "19999", "held output changed source")
      if (view === "review") requireThat(app.reviewPanel.visible, "held review lost focus")
      if (view === "secret") requireThat(app.picker.input.value === "•".repeat("synthetic-secret-canary".length), "unsubmitted secret changed")
      if (view === "action") requireThat((allocations.usage.domains.decoding ?? 0) >= 4096, "pending credential action lost its result owner")
      if (cycle % 10 === 0 || cycle + 1 === cycles) {
        Bun.gc(true)
        samples.push({ cycle, elapsedMs: performance.now() - heldAt, rssBytes: process.memoryUsage.rss(), highWaterBytes: observedResidentBytes(), allocation: allocations.usage, terminal: terminal.snapshot })
      }
    }
    // Settlement occurs only after the complete held-view measurement interval.
    if (view === "secret") { setup.mockInput.pressEnter(); await until(() => credentialStarted) }
    if (view === "secret" || view === "action") requireThat(credentialCorrect, "held credential input was not preserved")
    credential.resolve({ stored: true, activated: false, warnings: [] })
    if (view === "action" || view === "secret") await until(() => (allocations.usage.domains.decoding ?? 0) === 0)
    document.close(); app.outputViewer.closePresentation()
    if (view === "review") setup.mockInput.pressEscape()
    app.closePicker()
    await until(() => app.recycleState() !== null)
  } finally {
    credential.resolve({ stored: true, activated: false, warnings: [] })
    document.close(); app.destroy(); await fixture.close(); setup.renderer.destroy()
  }
  const settledBy = performance.now() + 10_000
  while (allocations.usage.bytes !== 0 && performance.now() < settledBy) await Bun.sleep(1)
  requireThat(allocations.usage.bytes === 0, "held-view teardown retained allocation")
  await writeFile(reportPath, JSON.stringify({ schemaVersion: 1, pid: process.pid, bunVersion: Bun.version, view, cycles,
    load: { tools: MEMORY_LOAD.toolInvocations, chunksPerCyclePerTool: 4, chunkBytes: 4096, assistantBytesPerCycle: 4096, cycleIntervalMs: 50 },
    fixture: "bounded in-process protocol server; credential result is synthetic",
    terminalOutput: "streamed to a draining sink; native ANSI history is not retained", samples, finalAllocationBytes: allocations.usage.bytes }) + "\n", { mode: 0o600 })
}
