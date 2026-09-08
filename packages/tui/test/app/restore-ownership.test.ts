import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { retainedJsonBytes } from "../../src/retained-json"
import { ClientAllocationError } from "../../src/client-allocation"
import { emptySessionReader, sessionReaderFor, toolItem, waitForHistory } from "../fixtures/history"

for (const surface of ["transcript", "picker"] as const) {
  test(`absent ${surface} restoration retains hints without the adopted composer envelope`, async () => {
    const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
    const app = createRottweilerApp(setup.renderer, { sessionId: "s", sessionReader: emptySessionReader })
    setup.renderer.root.add(app)
    try {
      app.composer.value = "retained draft ".repeat(1024)
      if (surface === "picker") app.openCommandPicker()
      await setup.flush()
      const saved = app.recycleState()!
      const state = surface === "transcript"
        ? { ...saved, transcript: { ...saved.transcript, blocks: { selectedId: "removed", expanded: [{ id: "removed", expanded: true }] } } }
        : { ...saved, picker: { ...saved.picker!, selectedId: "removed" } }
      let attempts = 0
      if (surface === "transcript") {
        const restore = app.transcript.restoreClientState.bind(app.transcript)
        app.transcript.restoreClientState = value => { attempts++; return restore(value) }
      } else {
        const restore = app.commandPalette.restoreViewport.bind(app.commandPalette)
        app.commandPalette.restoreViewport = value => { attempts++; restore(value) }
      }
      expect(app.restoreRecycleState(state)).toBe(true)
      const pendingBytes = app.historyCache.allocations.usage.bytes
      await setup.flush()
      for (let frame = 0; frame < 5; frame++) { app.applyPendingRecycleScroll(); await setup.renderOnce() }
      expect(attempts).toBe(1)
      expect(app.composer.value).toBe(saved.composer.content)
      expect(pendingBytes - app.historyCache.allocations.usage.bytes).toBeGreaterThan(Buffer.byteLength(saved.composer.content))
    } finally { app.destroy(); setup.renderer.destroy() }
    expect(app.historyCache.allocations.usage.bytes).toBe(0)
  })
}

test("a delayed source page still restores its selected expanded tool after the draft envelope retires", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  let release!: () => void
  const ready = new Promise<void>(resolve => { release = resolve })
  const source = sessionReaderFor([toolItem(1, "read", "{}", "retained result")])
  const app = createRottweilerApp(setup.renderer, { sessionId: "s", sessionReader: {
    ...source, async page(...args) { await ready; return source.page(...args) },
  } })
  setup.renderer.root.add(app)
  try {
    app.composer.value = "retained draft"
    const saved = app.recycleState()!
    expect(app.restoreRecycleState({ ...saved, transcript: { ...saved.transcript,
      blocks: { selectedId: "tool:invocation-1", expanded: [{ id: "tool:invocation-1", expanded: true }] },
    } })).toBe(true)
    app.applyPendingRecycleScroll()
    expect(app.transcript.mountedEntryCount).toBe(0)
    release()
    await waitForHistory(setup, () => { app.applyPendingRecycleScroll(); return app.transcript.selectedBlockId === "tool:invocation-1" })
    expect(app.transcript.mountedCards.get("1")?.expanded).toBe(true)
    expect(app.composer.value).toBe("retained draft")
  } finally { release(); app.destroy(); setup.renderer.destroy(); await Bun.sleep(0) }
  expect(app.historyCache.allocations.usage.bytes).toBe(0)
})

test("refused interaction credit rolls back the pending handoff owner", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  const app = createRottweilerApp(setup.renderer, { sessionId: "s", sessionReader: emptySessionReader })
  setup.renderer.root.add(app)
  try {
    app.composer.value = "accepted draft ".repeat(1024)
    await setup.flush()
    const saved = app.recycleState()!
    const owner = app.historyCache.allocations
    const reserve = owner.reserve.bind(owner)
    const interaction = { fingerprint: "f".repeat(64), index: 0, composer: false }
    const interactionBytes = retainedJsonBytes(interaction, 4096)
    let rejected = false, released = false
    owner.reserve = (domain, bytes) => {
      if (domain === "drafts" && bytes === interactionBytes) { rejected = true; throw new ClientAllocationError("injected interaction admission refusal") }
      const lease = reserve(domain, bytes)
      if (domain !== "drafts" || bytes < 4096) return lease
      const release = lease.release
      lease.release = () => { released = true; release() }
      return lease
    }
    expect(() => app.restoreRecycleState({ ...saved, interaction })).toThrow("injected interaction admission refusal")
    owner.reserve = reserve
    expect(rejected).toBe(true)
    expect(released).toBe(true)
    expect(app.composer.value).toBe(saved.composer.content)
    app.applyPendingRecycleScroll()
  } finally { app.destroy(); setup.renderer.destroy(); await Bun.sleep(0) }
  expect(app.historyCache.allocations.usage.bytes).toBe(0)
})
