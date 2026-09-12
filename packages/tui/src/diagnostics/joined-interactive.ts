import { appendFile, lstat, writeFile } from "node:fs/promises"
import { join } from "node:path"
import type { EngineEvent, SessionSearchMatch } from "../protocol"
import { connectedApp, requireThat } from "./connected-app"
import { joinedInteractiveInput, readBoundedPrivateFile } from "./connected-input"
import { FrameSamples } from "./connected/frame-samples"

/** Compiled client/renderer, real EngineHost and durable store; the host HTTP adapter is a fixture. */
export async function runJoinedInteractive(directory: string): Promise<void> {
  const input = await joinedInteractiveInput(directory)
  const history = input.history
  const line = input.streamLine
  let streamBytes = 0, streamLines = 0, parentFinished = false, childEvents = 0
  let firstStreamAt: number | null = null, lastStreamAt: number | null = null
  let phase = "bootstrap", expectedDraft = "", inputKeys = 0
  const observeEvent = (event: EngineEvent) => {
    if (event.type === "text_delta" && event.meta.session_id === input.sessionId) {
      if (event.text === "joined parent completed") parentFinished = true
      else {
        const count = event.text.length / line.length
        requireThat(Number.isInteger(count) && event.text === line.repeat(count), "live stream bytes changed")
        firstStreamAt ??= performance.now(); lastStreamAt = performance.now()
        streamLines += count; streamBytes += Buffer.byteLength(event.text)
      }
    }
    if (event.type === "subagent_progress") childEvents++
  }
  const client = await connectedApp(input, () => {}, false, observeEvent)
  const { app, setup, until } = client
  const samples = new FrameSamples()
  const milestones: Record<string, unknown>[] = []
  const token = new TextDecoder("utf-8", { fatal: true })
    .decode(await readBoundedPrivateFile(input.bootstrapTokenFile, 128)).trim()
  requireThat(/^[\x21-\x7e]{1,128}$/.test(token), "fixture control token is invalid")
  const control = async (name: string) => {
    const reply = await fetch(`http://localhost/fixture/${name}`, { unix: input.socketPath, method: "POST",
      headers: { authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(3000) })
    requireThat(reply.status === 202, `fixture control ${name} refused`)
    await reply.body?.cancel()
  }
  const exists = async (name: string) => lstat(join(directory, name)).then(info => info.isFile(), error => {
    if (error.code === "ENOENT") return false
    throw error
  })
  let failure: string | null = null, released = false, armed = false
  let pump: Promise<void> | undefined, stop = false
  let approval = false, inspected = false, scrolled = false, resized = false
  try {
    await until("actual host driver and initial history", () => client.ready() && app.transcript.mountedCards.size > 0, 30_000)
    app.composer.value = "Begin joined interactive workload"; await app.composer.submit()
    await until("actual paced stream", () => streamLines >= 10, 30_000)
    phase = "streaming"
    const started = performance.now()
    pump = (async () => {
      let scheduled = started
      for (let frame = 0; frame < samples.maximum && !stop; frame++) {
        await Bun.sleep(Math.max(0, scheduled - performance.now()))
        const began = performance.now()
        let keyStarted: number | null = null
        if (inputKeys < 256 && app.composer.editor.focused && !app.interactionPanel.visible && !app.picker.visible && app.activeSubagentId === null) {
          keyStarted = performance.now(); setup.mockInput.pressKey("x"); expectedDraft += "x"; inputKeys++
        }
        const compute = performance.now()
        await setup.renderOnce()
        const rendered = performance.now()
        if (keyStarted !== null) {
          requireThat(app.composer.value === expectedDraft, "joined stream lost unsent draft input")
          requireThat(setup.captureCharFrame().includes("x".repeat(Math.min(inputKeys, 8))), "native input frame is absent")
        }
        samples.record({ phase, scheduledMs: scheduled - started, startedMs: began - started,
          computeMs: rendered - compute, inputMs: keyStarted === null ? null : performance.now() - keyStarted,
          retainedBytes: client.runtime.allocations.usage.bytes })
        requireThat(client.terminal.snapshot.queuedBytes === 0, "joined native terminal output did not drain")
        // Count missed schedule slots once; never fabricate catch-up frames.
        scheduled += Math.max(1, Math.floor((began - scheduled) / samples.intervalMs) + 1) * samples.intervalMs
        if ((frame + 1) % 60 === 0) await appendFile(join(directory, "joined-frames.jsonl"), samples.frames.slice(frame - 59, frame + 1).map(value => JSON.stringify(value)).join("\n") + "\n", { mode: 0o600 })
        if (frame % 60 === 0) await writeFile(join(directory, "joined-progress.json"), JSON.stringify({ phase, frame, streamLines, childEvents,
          samples: samples.summary(), terminal: client.terminal.snapshot }) + "\n", { mode: 0o600 })
      }
    })()
    // Observe failures immediately while the finite scenario awaits host transitions.
    let pumpFailure: unknown
    const pumping = pump.catch(error => { pumpFailure = error; throw error })
    pumping.catch(() => {})
    const wait = async (label: string, predicate: () => boolean) => {
      const deadline = performance.now() + 30_000
      while (!predicate()) { if (pumpFailure !== undefined) throw pumpFailure; requireThat(performance.now() < deadline, `joined phase ${label} timed out`); await Bun.sleep(2) }
    }
    await wait("initial admitted keys", () => inputKeys >= 32)
    await control("stall"); armed = true
    const heldDeadline = performance.now() + 10_000
    while (!await exists("storage-held.json")) { if (pumpFailure !== undefined) throw pumpFailure; requireThat(performance.now() < heldDeadline, "actual storage hold readiness expired"); await Bun.sleep(2) }
    phase = "admitted-storage-stall"
    const beforeKeys = inputKeys
    await wait("keys while actual storage is held", () => inputKeys >= beforeKeys + 16)
    const source: SessionSearchMatch = { session_id: input.sessionId, source_sequence: history.first_source,
      through: history.source_through, digest: history.source_digest as SessionSearchMatch["digest"] }
    await app.transcript.revealSearchMatch(source)
    await wait("historical scroll while streaming", () => app.transcript.captureHistoryViewport()?.anchor != null)
    const anchor = app.transcript.captureHistoryViewport()
    requireThat(anchor !== null, "joined historical anchor absent")
    scrolled = true
    const beforeResize = samples.frames.length
    setup.resize(72, 30)
    await wait("resize while storage is held", () => samples.frames.length >= beforeResize + 3)
    requireThat(JSON.stringify(app.transcript.captureHistoryViewport()) === JSON.stringify(anchor), "joined resize moved historical anchor")
    resized = true
    milestones.push({ phase, inputBefore: beforeKeys, inputAfter: inputKeys, anchor, streamLines })
    await control("release"); released = true; phase = "storage-recovery"
    const settledDeadline = performance.now() + 10_000
    while (!await exists("storage-settled.json")) { if (pumpFailure !== undefined) throw pumpFailure; requireThat(performance.now() < settledDeadline, "actual storage retirement expired"); await Bun.sleep(2) }
    setup.resize(110, 36)
    phase = "approval-and-child"
    await wait("actual parent approval or child activity", () => app.interactionPanel.visible || childEvents > 0)
    if (app.interactionPanel.visible) {
      requireThat(app.interactionPanel.prompt.plainText.includes("joined-approved.txt"), "unrelated approval displayed")
      app.interactionPanel.select.selectCurrent(); approval = true
    }
    await wait("actual child activity", () => childEvents > 0 && Object.values(app.state.subagents).some(child => child.status === "running"))
    app.openSubagentPicker()
    await wait("child picker response", () => app.picker.select.options.length > 0)
    app.picker.select.selectCurrent()
    await wait("source-qualified child pane", () => app.activeSubagentId !== null && app.transcript.mountedCards.size > 0)
    const beforeChild = childEvents
    await wait("live child advances while selected", () => childEvents > beforeChild)
    inspected = true
    setup.mockInput.pressEscape()
    await wait("parent draft after child inspection", () => app.activeSubagentId === null)
    requireThat(app.composer.value === expectedDraft, "child inspection lost parent draft")
    setup.mockInput.pressEscape()
    if (!approval) {
      await wait("actual parent tool approval", () => app.interactionPanel.visible)
      requireThat(app.interactionPanel.prompt.plainText.includes("joined-approved.txt"), "unrelated approval displayed")
      app.interactionPanel.select.selectCurrent(); approval = true
    }
    await wait("child interruption completes parent tool", () => parentFinished)
    phase = "settled-stream"
    await pumping
    requireThat(streamLines === input.streamLines && streamBytes === input.streamLines * Buffer.byteLength(line), "joined canonical stream byte oracle failed")
    requireThat(inputKeys === 256 && approval && inspected && scrolled && resized, "joined interaction phase omitted")
    requireThat(samples.frames.length === samples.maximum, "joined frame sample schedule was truncated")
    await control("done")
  } catch (error) { failure = error instanceof Error ? error.message : String(error) }
  finally {
    stop = true
    try { if (armed && !released) await control("release") } finally {
      try { await pump?.catch(error => { failure ??= String(error) }) } finally { await client.close() }
    }
  }
  const summary = samples.summary()
  const compute95 = process.platform === "darwin" ? 20 : 40
  const compute999 = process.platform === "darwin" ? 33 : 66
  const passed = failure === null && summary.inputP99Ms !== null && summary.inputP99Ms < 16
    && summary.computeP95Ms !== null && summary.computeP95Ms < compute95
    && summary.computeP999Ms !== null && summary.computeP999Ms < compute999
  await writeFile(join(directory, "joined-client.json"), JSON.stringify({ schemaVersion: 1, pid: process.pid,
    platform: process.platform, architecture: process.arch, input, milestones, summary, frames: samples.frames,
    streamLines, streamBytes, childEvents, approval, inspected, scrolled, resized,
    streamObservedMs: firstStreamAt === null || lastStreamAt === null ? null : lastStreamAt - firstStreamAt,
    providerPacing: "existing deterministic provider requests 5ms between events; observed delivery duration retained",
    stages: client.diagnostics.snapshot(), terminal: client.terminal.snapshot,
    finalAllocationBytes: client.runtime.allocations.usage.bytes, failure, passed,
    ceilings: { inputP99Ms:16, computeP95Ms:compute95, computeP999Ms:compute999 },
    physicalDisplayCadence: "unobserved; native renderer uses draining terminal sink", hostTransport: "test HTTP forwarder; EngineHost/store are production owners" }) + "\n", { mode: 0o600 })
  requireThat(passed, failure ?? "joined native frame/input ceiling failed; retain every raw sample")
}
