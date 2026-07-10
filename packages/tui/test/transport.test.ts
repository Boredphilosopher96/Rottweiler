import { afterEach, describe, expect, test } from "bun:test"

import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
import {
  EngineHttpSseClient,
  EngineTransportError,
  type BackoffScheduler,
} from "../src/transport"
import {
  AuthenticatedMockEngine,
  encodeSseJson,
  splitBytes,
} from "./support/mock-engine"

const attach = {
  type: "attach_session",
  meta: {
    protocol_version: PROTOCOL_VERSION,
    client_id: "spoofed-client",
    request_id: "attach-request",
  },
  session_id: "session-transport",
  last_seen_sequence: null,
  role: "driver",
} satisfies ClientCommand

function durableMeta(sequence: string) {
  return {
    protocol_version: PROTOCOL_VERSION,
    session_id: "session-transport",
    sequence_id: sequence,
    emitted_at: "2026-01-01T00:00:00Z",
  }
}

describe("authenticated UDS engine transport", () => {
  let engine: AuthenticatedMockEngine | undefined

  afterEach(async () => {
    await engine?.stop()
    engine = undefined
  })

  test("mints a client credential and never permits command client-id spoofing", async () => {
    engine = new AuthenticatedMockEngine()
    await engine.start()
    const client = new EngineHttpSseClient({
      socketPath: engine.socketPath,
      bootstrapToken: engine.bootstrapToken,
    })

    await client.postCommand(attach)

    expect(engine.requests.map((request) => request.path)).toEqual([
      "/v1/connect",
      "/v1/command",
    ])
    expect(engine.requests[0]?.authorization).toBe(`Bearer ${engine.bootstrapToken}`)
    expect(engine.requests[0]?.clientId).toBeNull()
    expect(engine.requests[1]?.authorization).toBe(`Bearer ${engine.clientToken}`)
    expect(engine.requests[1]?.clientId).toBe(engine.clientId)
    expect(engine.commands[0]?.meta.client_id).toBe(engine.clientId)
    expect(engine.requests.every((request) => !request.path.includes("token"))).toBe(true)
    expect(engine.requests[1]?.body).not.toContain(engine.bootstrapToken)
    expect(engine.requests[1]?.body).not.toContain(engine.clientToken)
  })

  test("rejects missing or wrong bootstrap and minted credentials", async () => {
    engine = new AuthenticatedMockEngine()
    await engine.start()

    const missing = await fetch("http://rottweiler.local/v1/connect", {
      unix: engine.socketPath,
      method: "POST",
    })
    expect(missing.status).toBe(401)

    const wrongClient = await fetch("http://rottweiler.local/v1/events", {
      unix: engine.socketPath,
      headers: {
        Authorization: `Bearer ${engine.clientToken}`,
        "x-rottweiler-client": "spoofed-client",
      },
    })
    expect(wrongClient.status).toBe(401)

    const client = new EngineHttpSseClient({
      socketPath: engine.socketPath,
      bootstrapToken: "wrong-bootstrap",
    })
    expect(client.postCommand(attach)).rejects.toEqual(
      new EngineTransportError("engine bootstrap connection rejected", 401),
    )
  })

  test("reconnects with last_seen_sequence and reducer suppresses replay duplicates", async () => {
    const first = {
      type: "mode_changed",
      meta: durableMeta("1"),
      mode: "plan",
    } satisfies EngineEvent
    const second = {
      type: "model_changed",
      meta: durableMeta("2"),
      model: "fast",
    } satisfies EngineEvent
    engine = new AuthenticatedMockEngine([
      { chunks: splitBytes(encodeSseJson(first), [1, 2, 3, 5, 8]) },
      {
        chunks: [encodeSseJson(first), ...splitBytes(encodeSseJson(second), [7, 1, 4])],
        holdOpen: true,
      },
    ])
    await engine.start()
    const delays: number[] = []
    const scheduler: BackoffScheduler = {
      async sleep(delayMs, signal) {
        delays.push(delayMs)
        if (signal.aborted) {
          throw signal.reason
        }
      },
    }
    const client = new EngineHttpSseClient({
      socketPath: engine.socketPath,
      bootstrapToken: engine.bootstrapToken,
      scheduler,
      backoff: { initialDelayMs: 1, maximumDelayMs: 8, multiplier: 2 },
    })
    const controller = new AbortController()
    let state = createInitialState()

    await client.subscribe({
      attach,
      signal: controller.signal,
      getLastSeenSequence: () => state.lastSequence,
      onEvent(event) {
        state = reduceRottweilerState(state, engineEvent(event))
        if (state.lastSequence === "2") {
          controller.abort()
        }
      },
    })

    expect(state.lastSequence).toBe("2")
    expect(state.mode).toBe("plan")
    expect(state.model).toBe("fast")
    expect(state.protocol.duplicateEvents).toBe(1)
    const attaches = engine.commands.filter(
      (command): command is Extract<ClientCommand, { type: "attach_session" }> =>
        command.type === "attach_session",
    )
    expect(attaches.map((command) => command.last_seen_sequence)).toEqual([null, "1"])
    expect(attaches.every((command) => command.meta.client_id === engine?.clientId)).toBe(true)
    expect(delays).toEqual([1])
  })

  test("AbortController cancellation closes a quiet subscription", async () => {
    engine = new AuthenticatedMockEngine([
      { chunks: [new TextEncoder().encode(": connected\n\n")], holdOpen: true },
    ])
    await engine.start()
    const client = new EngineHttpSseClient({
      socketPath: engine.socketPath,
      bootstrapToken: engine.bootstrapToken,
    })
    const controller = new AbortController()
    let connected = false
    const done = client.subscribe({
      attach,
      signal: controller.signal,
      onEvent() {},
      onConnection(update) {
        if (update.phase === "connected") {
          connected = true
          controller.abort()
        }
      },
    })

    await done
    expect(connected).toBe(true)
    await Bun.sleep(0)
    expect(engine.cancelledStreams).toBe(1)
  })
})
