import { directSessionRead } from "../src/session-reader"
import { expect, test } from "bun:test"
import { HistoryPresentation } from "../src/history/presentation"
import { fixturePage } from "./fixtures/history"

test("source bursts coalesce without cancelling an admitted history read", async () => {
  let reads = 0
  let firstSignal: AbortSignal | undefined
  let finish!: () => void
  const presentation = new HistoryPresentation({
    page: async ({ sessionId: session }, read, signal) => {
      reads++
      if (reads === 1) {
        firstSignal = signal
        await new Promise<void>(resolve => { finish = resolve })
      }
      return { type: "ready", page: fixturePage(session, read) }
    },
    content: async () => { throw new Error("unused") },
  }, () => { })
  try {
    presentation.present(directSessionRead("history"))
    for (let event = 0; event < 100; event++) presentation.invalidate("history")
    expect(reads).toBe(1)
    expect(firstSignal?.aborted).toBe(false)
    finish()
    const deadline = performance.now() + 1000
    while (presentation.controller.snapshot.loading || reads < 2) {
      if (performance.now() > deadline) throw new Error("coalesced refresh did not complete")
      await Bun.sleep(1)
    }
    expect(reads).toBe(2)
    expect(presentation.controller.snapshot.page?.items.length).toBe(32)
  } finally { presentation.dispose() }
})

test("scheduled invalidation waits for explicit navigation and refreshes its admitted anchor", async () => {
  const navigation = Promise.withResolvers<void>()
  let selectedSignal: AbortSignal | undefined
  const positions: unknown[] = []
  const presentation = new HistoryPresentation({
    page: async ({ sessionId: session }, read, signal) => {
      positions.push(read.position)
      if (positions.length === 2) { selectedSignal = signal; await navigation.promise }
      return { type: "ready", page: fixturePage(session, read) }
    },
    content: async () => { throw new Error("unused") },
  }, () => { })
  const until = async (ready: () => boolean) => {
    const deadline = performance.now() + 1000
    while (!ready()) {
      if (performance.now() >= deadline) throw new Error("navigation did not settle")
      await Bun.sleep(1)
    }
  }
  try {
    presentation.present(directSessionRead("history"))
    await until(() => !presentation.controller.snapshot.loading)
    presentation.controller.setAnchor({ id: "999", offset: -2 })
    presentation.invalidate("history")
    const selected = presentation.controller.around("400")
    await Bun.sleep(150)
    expect(positions.length).toBe(2)
    expect(selectedSignal?.aborted).toBe(false)
    navigation.resolve()
    await selected
    await until(() => positions.length === 3 && !presentation.controller.snapshot.loading)
    expect(positions[2]).toEqual({ type: "around", item: "400" })
    expect(presentation.controller.snapshot.anchor).toEqual({ id: "400", offset: 0 })
  } finally { navigation.resolve(); presentation.dispose() }
})
