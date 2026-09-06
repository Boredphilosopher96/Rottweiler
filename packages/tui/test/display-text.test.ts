import { expect, test } from "bun:test"
import { bold, resolveRenderLib, t } from "@opentui/core"
import { createTestRenderer } from "@opentui/core/testing"
import { TextRenderable } from "../src/components/text"

test("empty display replacements reuse their native allocation owner", async () => {
  const { renderer, renderOnce } = await createTestRenderer({ width: 40, height: 6, useThread: false, bufferedOutput: "stdout" })
  const label = new TextRenderable(renderer, { content: "", width: 30, height: 1 })
  renderer.root.add(label)
  const sample = () => resolveRenderLib().getAllocatorStats()
  try {
    for (let index = 0; index < 100; index++) label.content = ""
    await renderOnce()
    const warm = sample()
    for (let index = 0; index < 10_000; index++) {
      label.content = ""
      if (index % 100 === 0) label.clear()
    }
    await renderOnce()
    const settled = sample()
    expect(settled.largeAllocations).toBeLessThanOrEqual(warm.largeAllocations + 1)
    expect(settled.smallAllocations).toBeLessThanOrEqual(warm.smallAllocations + 4)
    label.content = t`${bold("Ready")}`
    await renderOnce()
    expect(label.plainText).toBe("Ready")
    label.content = ""
    await renderOnce()
    expect(label.plainText).toBe("")
    label.content = "Restored ✓"
    expect(label.plainText).toBe("Restored ✓")
  } finally { renderer.destroy() }
})
