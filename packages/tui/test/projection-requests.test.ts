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

function mcpApprovalReview(
  requestId: string,
  sessionId: string,
  server: string,
): WireEngineEvent {
  return {
    type: "mcp_server_approval_reviewed",
    meta: {
      protocol_version: PROTOCOL_VERSION,
      client_id: "projection-test",
      request_id: requestId,
      emitted_at: "2026-08-19T00:00:00Z",
    },
    session_id: sessionId,
    review: {
      server,
      transport: "streamable_http",
      endpoint: `https://${server}/mcp`,
      origin: "user",
      defer_tools: true,
      fingerprint: server.padEnd(64, "0").slice(0, 64),
      previously_approved: false,
    },
  }
}

function mcpCatalog(requestId: string, sessionId = "session-test"): WireEngineEvent {
  return {
    type: "mcp_servers_listed",
    meta: {
      protocol_version: PROTOCOL_VERSION,
      client_id: "projection-test",
      request_id: requestId,
      emitted_at: "2026-08-19T00:00:00Z",
    },
    session_id: sessionId,
    servers: [],
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

  test("rejects an older MCP approval review after a newer review completes", () => {
    let nextRequest = 0
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => `mcp-review-${++nextRequest}`,
      replayActive: () => false,
      emit: () => ({ type: "accepted" }),
      onProjectionFailure: () => {},
      onCommandFailure: () => {},
    })

    const older = broker.command({ type: "review_mcp_server", name: "docs.remote" })
    const newer = broker.command({ type: "review_mcp_server", name: "build.local" })
    if (older === null || newer === null) throw new Error("missing MCP review request")

    const newerReply = mcpApprovalReview(newer, "session-test", "build.local")
    expect(broker.acceptsEvent(newerReply)).toBeTrue()
    expect(broker.completeEvent(newerReply)).toBe("mcp")
    expect(broker.acceptsEvent(mcpApprovalReview(older, "session-test", "docs.remote"))).toBeFalse()
  })

  test("rejects an MCP approval review from another session", () => {
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => "mcp-review",
      replayActive: () => false,
      emit: () => ({ type: "accepted" }),
      onProjectionFailure: () => {},
      onCommandFailure: () => {},
    })

    const requestId = broker.command({ type: "review_mcp_server", name: "docs.remote" })
    if (requestId === null) throw new Error("missing MCP review request")

    expect(broker.acceptsEvent(
      mcpApprovalReview(requestId, "foreign-session", "docs.remote"),
    )).toBeFalse()
  })

  test("rejects an MCP approval review after its command was rejected", async () => {
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => "mcp-review",
      replayActive: () => false,
      emit: () => ({
        type: "rejected",
        error: { category: "protocol", code: "unavailable", message: "review unavailable", retryable: true },
      }),
      onProjectionFailure: () => {},
      onCommandFailure: () => {},
    })

    const requestId = broker.command({ type: "review_mcp_server", name: "docs.remote" })
    if (requestId === null) throw new Error("missing MCP review request")
    await Bun.sleep(0)

    expect(broker.acceptsEvent(
      mcpApprovalReview(requestId, "session-test", "docs.remote"),
    )).toBeFalse()
  })

  test("rejects an MCP inventory reply after its list command was rejected", async () => {
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => "mcp-list",
      replayActive: () => false,
      emit: () => ({
        type: "rejected",
        error: { category: "protocol", code: "unavailable", message: "inventory unavailable", retryable: true },
      }),
      onProjectionFailure: () => {},
      onCommandFailure: () => {},
    })

    const requestId = broker.command({ type: "list_mcp_servers" })
    if (requestId === null) throw new Error("missing MCP list request")
    await Bun.sleep(0)

    expect(broker.acceptsEvent(mcpCatalog(requestId))).toBeFalse()
  })

  test("rejects an MCP inventory reply after its mutation command was rejected", async () => {
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => "mcp-enable",
      replayActive: () => false,
      emit: () => ({
        type: "rejected",
        error: { category: "protocol", code: "unavailable", message: "enable unavailable", retryable: true },
      }),
      onProjectionFailure: () => {},
      onCommandFailure: () => {},
    })

    const requestId = broker.command({
      type: "set_mcp_server_enabled",
      name: "docs.remote",
      enabled: true,
    })
    if (requestId === null) throw new Error("missing MCP enable request")
    await Bun.sleep(0)

    expect(broker.acceptsEvent(mcpCatalog(requestId))).toBeFalse()
  })

  test("rejects an MCP inventory reply from another session", () => {
    const broker = new ProjectionRequestBroker({
      clientId: () => "projection-test",
      sessionId: () => "session-test",
      requestId: () => "mcp-list",
      replayActive: () => false,
      emit: () => ({ type: "accepted" }),
      onProjectionFailure: () => {},
      onCommandFailure: () => {},
    })

    const requestId = broker.command({ type: "list_mcp_servers" })
    if (requestId === null) throw new Error("missing MCP list request")

    expect(broker.acceptsEvent(mcpCatalog(requestId, "foreign-session"))).toBeFalse()
  })
})
