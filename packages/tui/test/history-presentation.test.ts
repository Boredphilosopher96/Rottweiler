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
