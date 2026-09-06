import { expect, test } from "bun:test"
import { EngineHttpSseClient } from "../src/transport"
import { PROTOCOL_VERSION, type EngineEvent } from "../src/protocol"
import { AuthenticatedMockEngine, encodeSseJson } from "./support/mock-engine"

test("reconnect opens the event stream at the snapshot cursor installed by its callback", async () => {
  const event = (sequence: string): EngineEvent => ({ type: "model_changed", model: "main", meta: {
    protocol_version: PROTOCOL_VERSION, session_id: "session-transport", sequence_id: sequence, emitted_at: "2026-01-01T00:00:00Z",
  } })
  const engine = new AuthenticatedMockEngine([
    { chunks: [encodeSseJson(event("1"))] },
    { chunks: [encodeSseJson(event("6"))], holdOpen: true },
  ])
  await engine.start()
  const controller = new AbortController()
  let cursor: string | null = null
  const client = new EngineHttpSseClient({ socketPath: engine.socketPath, bootstrapToken: engine.bootstrapToken,
    scheduler: { async sleep() {} }, backoff: { initialDelayMs: 1, maximumDelayMs: 1, multiplier: 1 },
  })
  try {
    await client.subscribe({
      attach: { type: "attach_session", meta: { protocol_version: PROTOCOL_VERSION, client_id: "c", request_id: "r" },
        session_id: "session-transport", role: "driver", last_seen_sequence: null },
      signal: controller.signal, getLastSeenSequence: () => cursor,
      async onReconnect() { await Promise.resolve(); cursor = "5" },
      onEvent(event) {
        if (event.type !== "model_changed") throw new Error("unexpected fixture event")
        cursor = event.meta.sequence_id
        if (cursor === "6") controller.abort()
      },
    })
    expect(engine.requests.filter(request => request.path === "/v1/events").map(request => request.search)).toEqual([
      "?session_id=session-transport", "?session_id=session-transport&last_seen_sequence=5",
    ])
  } finally { controller.abort(); await engine.stop() }
})
