import { embeddedParserConfigurations, materializeTreeSitterRuntime } from "../tree-sitter-runtime"
import { exerciseLiveOwners } from "./memory-live"
import { addDefaultParsers, getTreeSitterClient } from "@opentui/core"
import { stabilizeTreeSitterClient } from "../tree-sitter-client"
import { createTestRenderer } from "@opentui/core/testing"
import { writeFile } from "node:fs/promises"
import { join } from "node:path"
import { createRottweilerApp, type RottweilerApp } from "../app"
import { ClientAllocationOwner } from "../client-allocation"
import { createInitialState } from "../state"
import { observedResidentBytes } from "../process-memory"
import { readTuiRecycleState, recycleTuiIfNeeded } from "../recycle-state"
import { MEMORY_LOAD, MEMORY_CHILD, MemoryFixture } from "./memory-fixture"

interface Sample { cycle: number; stage: string; rssBytes: number; highWaterBytes: number; allocation: ReturnType<typeof usage> }
function usage(owner: ClientAllocationOwner) { return owner.usage }
function requireThat(value: unknown, message: string): asserts value { if (!value) throw new Error(message) }

/** Invoked only by the explicitly requested compiled acceptance path. */
export async function runClientMemoryProbe(reportPath: string, workDirectory: string, cycles: number, shouldRecycle: boolean): Promise<void> {
  if (!Number.isSafeInteger(cycles) || cycles < 1 || cycles > 200) throw new Error("memory probe cycles must be 1..200")
  const allocations = new ClientAllocationOwner()
  const fixture = new MemoryFixture(join(workDirectory, `transport-${process.pid}.sock`), allocations)
  const parserRuntime = await materializeTreeSitterRuntime()
  process.env.OTUI_ASSET_ROOT = parserRuntime.root
  process.env.OTUI_TREE_SITTER_WORKER_PATH = parserRuntime.workerPath
  addDefaultParsers(embeddedParserConfigurations(parserRuntime.assetsPath))
  const treeSitter = stabilizeTreeSitterClient(getTreeSitterClient())
  await treeSitter.initialize()
  const setup = await createTestRenderer({ width: 110, height: 36, useThread: false })
  const samples: Sample[] = []
  let app: RottweilerApp | null = null
  let captured = false
  let restored = false
  const handoffPath = join(workDirectory, "client-handoff.json")
  using handoffAllocation = allocations.reserve("decoding", 0)
  let handoff = readTuiRecycleState(handoffPath, handoffAllocation)
  const sample = (cycle: number, stage: string) => samples.push({ cycle, stage, rssBytes: process.memoryUsage.rss(), highWaterBytes: observedResidentBytes(), allocation: allocations.usage })
  const until = async (condition: () => boolean) => {
    const deadline = performance.now() + 10_000
    while (!condition()) {
      if (performance.now() >= deadline) throw new Error(`memory probe condition did not settle: ${JSON.stringify(allocations.usage)}`)
      await Bun.sleep(1); await setup.renderOnce()
    }
    await setup.renderOnce()
  }
  sample(-1, "initialized-parser-runtime")
  try {
    for (let cycle = 0; cycle < cycles; cycle++) {
      fixture.cycle = cycle
      const initial = createInitialState()
      const familyEnabled = cycle === cycles - 1 || (cycle === 0 && handoff !== null)
      let mutationDecoded = false
      const consumeMutation = Promise.withResolvers<void>()
      app = createRottweilerApp(setup.renderer, { allocations, treeSitterClient: treeSitter, sessionId: "memory-probe", clientId: "memory-client", sessionReader: fixture.reader, ...(familyEnabled ? { familyControls: fixture.family } : {}),
        initialState: { ...initial, connection: { ...initial.connection, phase: "connected" }, driverClientId: "memory-client" },
        async onCommand(command, allocation) {
          const current = app
          const reply = await fixture.command(command, allocation)
          if (command.type === "send_message") { mutationDecoded = true; await consumeMutation.promise }
          if (current === app && reply.type === "read") for (const event of reply.events) current?.handleEvent(event)
          return reply.outcome
        },
      })
      setup.renderer.root.add(app)
      if (cycle === 0 && handoff !== null) {
        requireThat(app.restoreRecycleState(handoff.state), "private handoff adoption failed")
        const savedSelection = handoff.state.interaction
        handoff.consume()
        handoff = null
        handoffAllocation.release()
        await until(() => { app!.applyPendingRecycleScroll(); return app!.activeSubagentId === "agent-0" && app!.interactionPanel.usesComposer && app!.composer.value.startsWith("handoff child draft ") })
        requireThat(app.interactionPanel.captureSelection()?.fingerprint === savedSelection?.fingerprint, "child control was not rebound from authoritative reads")
        sample(cycle, "restored-pending-child-with-authoritative-controls")
        restored = true
        setup.mockInput.pressEscape()
        await until(() => app!.activeSubagentId === null)
        requireThat(app.composer.value.startsWith("handoff parent draft "), "parent draft was not restored after leaving child")
      }
      await until(() => app!.transcript.mountedCards.size > 0)
      app.composer.restoreDraft(`draft ${cycle} ${"d".repeat(MEMORY_LOAD.draftBytes)}`, [{ name: "notes.txt", media_type: "text/plain", data: { type: "text", content: "attachment ".repeat(24_000) } }])
      app.openSubagentPicker()
      await until(() => app!.picker.select.options.length === MEMORY_LOAD.catalogRows)
      sample(cycle, "mounted-history-draft-catalog-picker")
      setup.mockInput.pressEscape()

      // Hold two real reads and one mutation; retain decoded results until consumer settlement.
      fixture.hold()
      const abort = new AbortController()
      const readLease = allocations.reserve("decoding", 1024)
      const otherLease = allocations.reserve("decoding", 1024)
      const query = { type: "read_transcript" as const, meta: fixture.meta(), session_id: "memory-probe", scope: { type: "session" as const },
        read: { known_view: null, position: { type: "latest" as const }, max_items: MEMORY_LOAD.pageRows, max_bytes: 256 * 1024 } }
      const consumeRead = Promise.withResolvers<void>()
      let readDecoded = false
      const cancelled = fixture.client.postCommand(query, abort.signal, readLease).then(() => false, () => true).finally(() => readLease.release())
      const other = fixture.client.postCommand({ ...query, meta: fixture.meta() }, undefined, otherLease).then(async reply => {
        readDecoded = true; await consumeRead.promise
        requireThat(reply.type === "read" && reply.events[0]?.type === "transcript_page_ready", "held decoded history lost its typed body")
      }).finally(() => otherLease.release())
      const pendingSend = app.composer.submit()
      await until(() => fixture.pending === 3)
      requireThat(app.recycleState() === null, "unsettled mutation allowed recycle")
      requireThat((allocations.usage.domains.outbound ?? 0) > 0, "outbound body is uncharged")
      sample(cycle, "overlapping-reads-mutation")
      abort.abort(new DOMException("selection replaced", "AbortError"))
      requireThat(await cancelled, "cancelled query delivered a result")
      fixture.release()
      try {
        await until(() => readDecoded && mutationDecoded)
        requireThat((allocations.usage.domains.decoding ?? 0) > 256 * 1024, "decoded bodies lost credit before consumers settled")
        requireThat(app.recycleState() === null, "decoded unsettled mutation allowed recycle")
        sample(cycle, "decoded-history-and-mutation-awaiting-consumers")
      } finally { consumeRead.resolve(); consumeMutation.resolve() }
      await other
      requireThat(await pendingSend === false, "fixture mutation rejection was lost")
      requireThat(app.composer.value.startsWith(`draft ${cycle} `), "failed mutation lost draft")
      const commandUsage = fixture.client.commandUsage
      requireThat((allocations.usage.domains.decoding ?? 0) === 0 && commandUsage.reads.bytes === 0
        && commandUsage.controls.normal === 0 && commandUsage.controls.urgent === 0
        && commandUsage.watches === (familyEnabled ? 1 : 0), "settled foreground transport retained allocation")

      const prior = app.state
      const pressure = allocations.reserve("live", Math.min(allocations.limits.live - (allocations.usage.domains.live ?? 0), allocations.normalCapacity - allocations.usage.bytes))
      const pressure2 = allocations.reserve("decoding", allocations.normalCapacity - allocations.usage.bytes)
      try {
        let refused = false
        try { app.setState({ ...prior, model: `refused-${cycle}` }) } catch { refused = true }
        requireThat(refused && app.state === prior, "failed projection handoff mutated prior state")
      } finally { pressure2.release(); pressure.release() }
      const malformedLease = allocations.reserve("decoding", 1024)
      fixture.invalidateNext()
      let malformed = false
      try { await fixture.client.postCommand({ ...query, meta: fixture.meta() }, undefined, malformedLease) } catch { malformed = true }
      finally { malformedLease.release() }
      requireThat(malformed, "invalid reply passed generated validation")
      sample(cycle, "failure-and-cancellation-settled")
      await exerciseLiveOwners(app, fixture, allocations, async () => { await setup.renderOnce(); await setup.flush() }, stage => sample(cycle, stage))
      app.composer.restoreDraft(`handoff parent draft ${cycle}`, [])
      if (cycle === cycles - 1) {
        app.openSubagentPicker()
        await until(() => app!.picker.select.options[0]?.name.includes("Response needed") === true)
        app.picker.select.selectCurrent()
        await until(() => app!.activeSubagentId === "agent-0" && app!.interactionPanel.usesComposer && app!.recycleState() !== null)
        app.composer.restoreDraft(`handoff child draft ${cycle}`, [])
        const selected = app.recycleState()
        requireThat(selected?.child?.type === "live" && selected.child.target.session_id === MEMORY_CHILD.session_id, "pending child source was not captured")
        sample(cycle, "pending-child-before-process-handoff")
      }
      if (cycle === cycles - 1 && shouldRecycle) {
        captured = recycleTuiIfNeeded({ allocations, observedBytes: 1, thresholdBytes: 1, path: handoffPath, capture: () => app!.recycleState(), recycle: () => { process.exitCode = 75 } })
        requireThat(captured, "explicit handoff did not capture restorable state")
      }
      app.destroy(); app = null
      await until(() => allocations.usage.bytes === 0)
      Bun.gc(true)
      sample(cycle, "destroyed-and-collected")
    }
  } finally { fixture.release(); app?.destroy(); await fixture.close(); setup.renderer.destroy() }
  requireThat(fixture.resolvedChildControls === 0, "probe settled a child control to permit handoff")
  requireThat(allocations.usage.bytes === 0, `final client allocation did not retire: ${JSON.stringify(allocations.usage)}`)
  await writeFile(reportPath, `${JSON.stringify({ schemaVersion: 1, bunVersion: Bun.version, platform: process.platform, pid: process.pid,
    cycles, load: MEMORY_LOAD, fixture: "bounded protocol server in measured process", recycle: { mode: "forced capture path; not RSS threshold evidence", captured, restored },
    requests: fixture.requests, resolvedChildControls: fixture.resolvedChildControls, finalAllocationBytes: allocations.usage.bytes, samples })}\n`, { mode: 0o600 })
}
