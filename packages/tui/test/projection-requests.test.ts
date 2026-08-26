import { describe, expect, test } from "bun:test"

import { ProjectionRequestBroker } from "../src/projection-requests"
import { PROTOCOL_VERSION, type ClientCommand, type CommandOutcome } from "../src/protocol"
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

function settingsCatalog(requestId: string): WireEngineEvent {
  return {
    type: "settings_listed",
    meta: {
      protocol_version: PROTOCOL_VERSION,
      client_id: "projection-test",
      request_id: requestId,
      emitted_at: "2026-08-19T00:00:00Z",
    },
    session_id: "session-test",
    settings: [],
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

  test("reports the latest list-settings failure through its pending slot", async () => {
    const failures: string[] = []
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => "settings-list",
      replayActive: () => false,
      emit: () => ({
        type: "rejected",
        error: { category: "protocol", code: "unavailable", message: "settings unavailable", retryable: true },
      }),
      onProjectionFailure: (_kind, _type, requestId) => failures.push(requestId),
      onCommandFailure: () => {},
    })

    broker.command({ type: "list_settings" })
    await Bun.sleep(0)

    expect(failures).toEqual(["settings-list"])
  })

  test("ignores an older list-settings failure after a newer write supersedes it", async () => {
    let nextRequest = 0
    let resolveList!: (outcome: CommandOutcome) => void
    const listOutcome = new Promise<CommandOutcome>((resolve) => {
      resolveList = resolve
    })
    const failures: string[] = []
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => `settings-${++nextRequest}`,
      replayActive: () => false,
      emit: (command) => command.type === "list_settings" ? listOutcome : { type: "accepted" },
      onProjectionFailure: (_kind, _type, requestId) => failures.push(requestId),
      onCommandFailure: () => {},
    })

    broker.command({ type: "list_settings" })
    broker.command({ type: "set_setting", key: "compaction.auto", value: "false" })
    resolveList({
      type: "rejected",
      error: { category: "protocol", code: "stale", message: "older list failed", retryable: true },
    })
    await Bun.sleep(0)

    expect(failures).toEqual([])
  })

  test("restores the prior authoritative settings request when a newer write is rejected", async () => {
    let nextRequest = 0
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => `settings-${++nextRequest}`,
      replayActive: () => false,
      emit: (command) => command.type === "set_setting"
        ? {
            type: "rejected",
            error: {
              category: "protocol",
              code: "setting_rejected",
              message: "setting rejected",
              retryable: true,
            },
          }
        : { type: "accepted" },
      onProjectionFailure: () => {},
      onCommandFailure: () => {},
    })

    const listRequest = broker.command({ type: "list_settings" })
    broker.command({ type: "set_setting", key: "compaction.auto", value: "false" })
    await Bun.sleep(0)

    expect(listRequest).not.toBeNull()
    expect(broker.acceptsEvent(settingsCatalog(listRequest!))).toBeTrue()
  })
})
