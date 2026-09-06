import { expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { createRottweilerApp } from "../../src/app"
import { TuiEngineRuntime, type RuntimeEngineClient } from "../../src/runtime"
import { ScriptedClient } from "../runtime/fixtures"
import { emptySessionReader } from "../fixtures/history"
import { ClientAllocationOwner } from "../../src/client-allocation"
import { ProjectionRequestBroker } from "../../src/projection-requests"
import { PROTOCOL_VERSION, type CommandOutcome } from "../../src/protocol"

const rejection: CommandOutcome = { type: "rejected", error: {
  category: "protocol", code: "driver_required", message: "take over the driver lease first", retryable: false,
} }

for (const end of ["consume", "switch", "destroy"] as const) {
  test(`mutation reply remains admitted through Runtime and UI continuation: ${end}`, async () => {
    const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
    const scripted = new ScriptedClient()
    const client: RuntimeEngineClient = {
      restartStream: () => false,
      subscribe: options => scripted.subscribe(options),
      async postCommand(command, _signal, allocation) {
        if (command.type !== "send_message") return scripted.postCommand(command)
        if (allocation === undefined) throw new Error("missing caller allocation")
        allocation.admit(8192)
        return { type: "command", outcome: rejection }
      },
    }
    const runtime = new TuiEngineRuntime({ socketPath: "/private/fixture.sock", bootstrapToken: "fixture",
      sessionId: "s", lastSeenSequence: null, lastSeenFile: null, replayMode: false }, client)
    const decoded = Promise.withResolvers<void>(), consume = Promise.withResolvers<void>()
    const app = createRottweilerApp(setup.renderer, { sessionReader: emptySessionReader,
      async onCommand(command, allocation) {
        const outcome = await runtime.sendCommand(command, allocation)
        if (command.type === "send_message") { decoded.resolve(); await consume.promise }
        return outcome
      },
    })
    setup.renderer.root.add(app); runtime.bind(app)
    try {
      await runtime.start()
      app.composer.value = "preserve the draft"
      const sending = app.composer.submit()
      await decoded.promise
      expect(app.historyCache.allocations.usage.domains.decoding).toBe(8192)
      if (end === "switch") app.setSessionId("next")
      if (end === "destroy") app.destroy()
      const state = app.state
      expect(app.historyCache.allocations.usage.domains.decoding).toBe(8192)
      consume.resolve()
      expect(await sending).toBeFalse()
      expect(app.historyCache.allocations.usage.domains.decoding).toBe(0)
      if (end === "consume") {
        expect(app.state.errors.at(-1)?.code).toBe("driver_required")
        expect(app.composer.value).toBe("preserve the draft")
      } else expect(app.state).toBe(state)
    } finally { consume.resolve(); await runtime.stop(); app.destroy(); setup.renderer.destroy() }
  })
}

test("broker dispatch and throwing consumers release only after result handling", async () => {
  const allocations = new ClientAllocationOwner(), handled = Promise.withResolvers<void>()
  let failures = 0, throwTransport = false
  const broker = new ProjectionRequestBroker({ allocations,
    clientId: () => "client", sessionId: () => "s", requestId: () => "request", replayActive: () => false,
    emit: async (_command, allocation) => { allocation.admit(4096); await Promise.resolve(); if (throwTransport) throw new Error("transport decode failure"); return rejection },
    onProjectionFailure: () => { throw new Error("unexpected projection") },
    onCommandFailure: () => { expect(allocations.usage.domains.decoding).toBe(4096); failures++; handled.resolve() },
  })
  const command = { type: "interrupt" as const, meta: { protocol_version: PROTOCOL_VERSION, client_id: "client", request_id: "request" }, session_id: "s" }
  broker.dispatch(command)
  await handled.promise
  expect(failures).toBe(1)
  expect(allocations.usage.bytes).toBe(0)
  await expect(broker.consume(command, outcome => {
    expect(allocations.usage.domains.decoding).toBe(4096)
    expect(outcome).toBe(rejection)
    throw new Error("consumer refused")
  })).rejects.toThrow("consumer refused")
  expect(allocations.usage.bytes).toBe(0)
  const consuming = Promise.withResolvers<void>(), release = Promise.withResolvers<void>()
  const pending = broker.consume(command, async () => { consuming.resolve(); await release.promise })
  await consuming.promise
  expect(allocations.usage.domains.decoding).toBe(4096)
  release.resolve(); await pending
  expect(allocations.usage.bytes).toBe(0)
  throwTransport = true
  broker.dispatch(command)
  for (let index = 0; index < 8 && failures < 2; index++) await Promise.resolve()
  expect(failures).toBe(2)
  expect(allocations.usage.bytes).toBe(0)
})
