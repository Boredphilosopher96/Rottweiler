import { directSessionRead, type SessionReadTarget } from "../src/session-reader"
import { expect, test } from "bun:test"
import { createTestRenderer, MockTreeSitterClient } from "@opentui/core/testing"
import { TranscriptRenderable } from "../src/components/transcript"
import { HistoryController } from "../src/history/controller"
import { createInitialState } from "../src/state"
import { createSyntaxStyle, kennelTheme } from "../src/theme"
import { fixturePage } from "./fixtures/history"

function visibleAnchor(transcript: TranscriptRenderable) {
  const top = transcript.scroller.viewport.y
  const visible = [...transcript.mountedCards.values()].filter(row => row.visible && row.y + row.height > top)
    .sort((left, right) => left.y - right.y)[0]
  if (visible === undefined) throw new Error("missing visible anchor")
  return { id: visible.item.id, offset: visible.y - top }
}

test("visible source row and pixel offset survive page replacement and rewrapping", async () => {
  const harness = await createTestRenderer({ width: 80, height: 20, useThread: false })
  const style = createSyntaxStyle(kennelTheme)
  const reader = {
    page: async ({ sessionId: session }: SessionReadTarget, read: Parameters<typeof fixturePage>[1]) => {
      const page = fixturePage(session, read)
      for (const item of page.items) if (item.content.type === "command") {
        item.content.message.text = `Row ${item.id} has enough text to wrap across narrow terminal widths. `.repeat(3)
      }
      return { type: "ready" as const, page }
    },
    content: async () => { throw new Error("unused") },
  }
  const controller = new HistoryController(reader, () => transcript.setHistory(controller.snapshot))
  const transcript = new TranscriptRenderable(harness.renderer, kennelTheme, {
    syntaxStyle: style, treeSitterClient: new MockTreeSitterClient({ autoResolveTimeout: 0 }),
    onHistoryAnchor: anchor => controller.setAnchor(anchor),
    onHistoryFollowing: following => controller.setFollowing(following),
  })
  transcript.update(createInitialState())
  harness.renderer.root.add(transcript)
  try {
    await controller.open(directSessionRead("session"))
    await controller.seek(400n)
    await harness.flush()
    transcript.setScrollOffset(12)
    await harness.flush()
    const before = visibleAnchor(transcript)
    await controller.around(before.id)
    await harness.flush()
    expect(visibleAnchor(transcript)).toEqual(before)
    harness.resize(35, 20)
    await harness.flush()
    expect(visibleAnchor(transcript)).toEqual(before)
    expect(transcript.mountedCards.size).toBeLessThanOrEqual(16)
    await controller.open(directSessionRead("child"))
    await harness.flush()
    await controller.open(directSessionRead("session"))
    await harness.flush()
    expect(visibleAnchor(transcript)).toEqual(before)
    await controller.seek(405n)
    await harness.flush()
    expect(visibleAnchor(transcript)).toEqual({ id: "405", offset: 0 })
  } finally { controller.dispose(); transcript.destroy(); style.destroy(); harness.renderer.destroy() }
})
