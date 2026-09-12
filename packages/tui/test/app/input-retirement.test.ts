import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { emptySessionReader } from "../fixtures/history"

for (const recursive of [false, true]) {
  test(`App teardown retires queued native draft callbacks: recursive=${recursive}`, async () => {
    const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
    const app = createRottweilerApp(setup.renderer, { sessionReader: emptySessionReader })
    setup.renderer.root.add(app)
    await setup.renderOnce()
    expect(app.historyCache.insert("idle-surface", { kind: "ui_catalog", catalog: { entries: [] } })).toBe(true)
    app.composer.restoreDraft("handoff draft", [])
    if (recursive) app.destroyRecursively()
    else app.destroy()
    await setup.renderOnce()
    await new Promise<void>(resolve => setImmediate(resolve))
    const beforeRendererDestroy = app.historyCache.allocations.usage.bytes
    setup.renderer.destroy()
    await new Promise<void>(resolve => setImmediate(resolve))
    expect(beforeRendererDestroy).toBe(0)
    expect(app.historyCache.allocations.usage.bytes).toBe(0)
  })
}
