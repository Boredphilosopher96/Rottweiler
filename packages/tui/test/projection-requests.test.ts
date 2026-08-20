import { describe, expect, test } from "bun:test"

import { ProjectionRequestBroker } from "../src/projection-requests"
import { PROTOCOL_VERSION, type ClientCommand } from "../src/protocol"
import type { WireEngineEvent } from "../src/transport"

function modelCatalog(requestId: string): WireEngineEvent {
  return {
    type: "models_listed",
    meta: {
      protocol_version: PROTOCOL_VERSION,
      client_id: "projection-test",
      request_id: requestId,
      emitted_at: "2026-08-19T00:00:00Z",
    },
    models: [],
  }
}

describe("projection request correlation", () => {
  test("rejects an older reply after the newer request has completed", () => {
    let nextRequest = 0
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => `request-${++nextRequest}`,
      replayActive: () => false,
      emit: (_command: ClientCommand) => ({ type: "accepted" }),
      onProjectionFailure: () => {},
      onCommandFailure: () => {},
    })

    const older = broker.issue("models").request_id
    const newer = broker.issue("models").request_id
    const newerReply = modelCatalog(newer)

    expect(broker.acceptsEvent(newerReply)).toBeTrue()
    expect(broker.completeEvent(newerReply)).toBe("models")
    expect(broker.current("models")).toBeNull()
    expect(broker.acceptsEvent(modelCatalog(older))).toBeFalse()
  })
})
