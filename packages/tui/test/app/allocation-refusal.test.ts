import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { PROTOCOL_VERSION } from "../../src/protocol"
import { emptySessionReader } from "../fixtures/history"

test("failed live appends preserve the mounted draft, source cursor and admitted tool prefix", async () => {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  const app = createRottweilerApp(setup.renderer, { sessionId: "s", sessionReader: emptySessionReader })
  setup.renderer.root.add(app)
  const meta = (sequence: string) => ({ protocol_version: PROTOCOL_VERSION, session_id: "s", sequence_id: sequence, emitted_at: "2026-01-01T00:00:00Z" })
  const delta = { type: "tool_output_delta" as const, meta: meta("3"), turn_id: "1", tool_call_id: "provider", invocation_id: "invocation", stream: "stdout" as const, chunk: "x".repeat(8192) }
  try {
    app.composer.value = "keep my draft"
    app.handleEvent({ type: "tool_call_started", meta: meta("1"), turn_id: "1", tool_call_id: "provider", invocation_id: "invocation", name: "bash", args: {}, call_index: 0 })
    app.handleEvent({ ...delta, meta: meta("2"), chunk: "original" })
    await setup.renderOnce()
    const before = app.state, owner = app.historyCache.allocations, retained = owner.usage.bytes
    const blocker = owner.reserve("live", owner.limits.live - (owner.usage.domains.live ?? 0) - 4096)
    try {
      for (let index = 0; index < 50; index++) {
        expect(() => app.handleEvent(delta)).toThrow("admission")
        expect(app.state).toBe(before)
        expect(app.state.lastSequence).toBe("2")
        expect(app.composer.value).toBe("keep my draft")
        expect(owner.usage.bytes).toBe(retained + blocker.bytes)
      }
    } finally { blocker.release() }
    app.handleEvent(delta)
    expect(app.state.lastSequence).toBe("3")
    expect(app.state.tools.invocation?.chunks.retainedBytes).toBe(8 + 8192)
    expect(owner.usage.bytes - retained).toBeLessThan(50_000)
  } finally { app.destroy(); setup.renderer.destroy() }
})
