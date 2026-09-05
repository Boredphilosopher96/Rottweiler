import { afterEach, describe, expect, test } from "bun:test"

import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"
import {
  EngineHttpSseClient,
  EngineTransportError,
  EngineProtocolError,
  SseLimitError,
  durableSequenceId,
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

async function waitFor(
  predicate: () => boolean,
  timeoutMs = 1_000,
): Promise<void> {
  const deadline = performance.now() + timeoutMs
  while (!predicate()) {
    if (performance.now() >= deadline) {
      throw new Error("timed out waiting for observable transport teardown")
    }
    await Bun.sleep(5)
  }
}

function createPlannedFetch(
  plans: readonly { readonly chunks: readonly Uint8Array[] }[],
): {
  readonly fetch: typeof fetch
  readonly requests: string[]
  readonly commands: ClientCommand[]
  cancelledStreams: number
} {
  const remaining = [...plans]
  const harness = {
    requests: [] as string[],
    commands: [] as ClientCommand[],
    cancelledStreams: 0,
    fetch: undefined as unknown as typeof fetch,
  }
  harness.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    const url = new URL(String(input))
    harness.requests.push(`${url.pathname}${url.search}`)
    if (url.pathname === "/v1/connect") {
      return Response.json({ client_id: "mock-client", token: "mock-token" }, { status: 201 })
    }
    if (url.pathname === "/v1/command") {
      harness.commands.push(JSON.parse(String(init?.body)) as ClientCommand)
      return Response.json({ type: "command", outcome: { type: "accepted" } }, { status: 202 })
    }
    const plan = remaining.shift() ?? { chunks: [] }
    return new Response(
      new ReadableStream<Uint8Array>({
        start(controller) {
          for (const chunk of plan.chunks) controller.enqueue(chunk)
        },
        cancel() {
          harness.cancelledStreams += 1
        },
      }),
      { headers: { "Content-Type": "text/event-stream" } },
    )
  }) as typeof fetch
  return harness
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

  test("rereads a refreshable bootstrap token after minted auth expires", async () => {
    const bootstrapTokens = ["bootstrap-before-restart", "bootstrap-after-restart"]
    const bootstrapHeaders: string[] = []
    let connectCount = 0
    let commandCount = 0
    const client = new EngineHttpSseClient({
      socketPath: "/private/restarted-engine.sock",
      bootstrapToken: async () => bootstrapTokens[connectCount] ?? "",
      fetch: (async (_input: string | URL | Request, init?: RequestInit) => {
        const headers = new Headers(init?.headers)
        if (headers.get("x-rottweiler-client") === null) {
          bootstrapHeaders.push(headers.get("Authorization") ?? "")
          connectCount += 1
          return Response.json({
            client_id: `client-${connectCount}`,
            token: `minted-${connectCount}`,
          })
        }
        commandCount += 1
        if (commandCount === 1) {
          return new Response("engine restarted", { status: 401 })
        }
        return Response.json({ type: "command", outcome: { type: "accepted" } }, { status: 202 })
      }) as typeof fetch,
    })

    await expect(client.postCommand(attach)).rejects.toEqual(
      new EngineTransportError("engine command rejected", 401),
    )
    expect(await client.postCommand(attach)).toEqual({ type: "command", outcome: { type: "accepted" } })
    expect(bootstrapHeaders).toEqual([
      "Bearer bootstrap-before-restart",
      "Bearer bootstrap-after-restart",
    ])
  })

  test("reconnects with last_seen_sequence and reducer suppresses replay duplicates", async () => {
    const first = {
      type: "mode_changed",
      meta: durableMeta("1"),
      mode: "plan",
      definition_fingerprint: "fixture",
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
    let attachRequest = 0
    let reconnectTakeovers = 0

    await client.subscribe({
      attach,
      signal: controller.signal,
      requestId: () => `attach-reconnect-${(attachRequest += 1)}`,
      onReconnect: () => {
        reconnectTakeovers += 1
      },
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
    expect(attaches.map((command) => command.role)).toEqual(["driver", "observer"])
    expect(reconnectTakeovers).toBe(1)
    expect(attaches.map((command) => command.meta.request_id)).toEqual([
      "attach-reconnect-1",
      "attach-reconnect-2",
    ])
    expect(attaches.every((command) => command.meta.client_id === engine?.clientId)).toBe(true)
    expect(
      engine.requests
        .filter((request) => request.path === "/v1/events")
        .map((request) => request.search),
    ).toEqual([
      "?session_id=session-transport",
      "?session_id=session-transport&last_seen_sequence=1",
    ])
    expect(delays).toEqual([1])
  })

  test("resets an ahead replay cursor once and retries from the beginning", async () => {
    const replayCompleted = {
      type: "session_replay_completed",
      meta: {
        protocol_version: PROTOCOL_VERSION,
        client_id: "mock-client",
        request_id: "replay-complete",
        emitted_at: "2026-01-01T00:00:00Z",
      },
      session_id: "session-transport",
      through_sequence: null,
    } satisfies EngineEvent
    const eventPaths: string[] = []
    let eventRequests = 0
    const client = new EngineHttpSseClient({
      socketPath: "/private/cursor-ahead.sock",
      bootstrapToken: "bootstrap",
      fetch: (async (input: string | URL | Request) => {
        const url = new URL(String(input))
        if (url.pathname === "/v1/connect") {
          return Response.json({ client_id: "mock-client", token: "mock-token" }, { status: 201 })
        }
        if (url.pathname === "/v1/command") {
          return Response.json({ type: "command", outcome: { type: "accepted" } }, { status: 202 })
        }
        eventPaths.push(`${url.pathname}${url.search}`)
        eventRequests += 1
        if (eventRequests === 1) {
          return Response.json(
            {
              error: {
                code: "replay_cursor_ahead",
                message: "last seen sequence is ahead of the durable log",
              },
            },
            { status: 409 },
          )
        }
        return new Response(encodeSseJson(replayCompleted), {
          headers: { "Content-Type": "text/event-stream" },
        })
      }) as typeof fetch,
      scheduler: {
        async sleep() {
          throw new Error("cursor reset must retry without backoff")
        },
      },
    })
    const controller = new AbortController()
    let cursor: string | null = "9"
    let resets = 0

    await client.subscribe({
      attach,
      signal: controller.signal,
      getLastSeenSequence: () => cursor,
      onReplayCursorAhead() {
        resets += 1
        cursor = null
      },
      onEvent() {
        controller.abort()
      },
    })

    expect(resets).toBe(1)
    expect(eventPaths).toEqual([
      "/v1/events?session_id=session-transport&last_seen_sequence=9",
      "/v1/events?session_id=session-transport",
    ])
  })

  test("aborts a gapped attempt and immediately resumes from the last verified cursor", async () => {
    const events = {
      one: { type: "mode_changed", meta: durableMeta("1"), mode: "plan", definition_fingerprint: "fixture" },
      two: { type: "model_changed", meta: durableMeta("2"), model: "slow" },
      three: { type: "mode_changed", meta: durableMeta("3"), mode: "default", definition_fingerprint: "fixture" },
      four: { type: "model_changed", meta: durableMeta("4"), model: "fast" },
    } satisfies Record<string, EngineEvent>
    const harness = createPlannedFetch([
      {
        chunks: [encodeSseJson(events.one), encodeSseJson(events.three)],
      },
      {
        chunks: [
          encodeSseJson(events.two),
          encodeSseJson(events.three),
          encodeSseJson(events.four),
        ],
      },
    ])
    const delays: number[] = []
    const client = new EngineHttpSseClient({
      socketPath: "/private/mock-engine.sock",
      bootstrapToken: "mock-bootstrap",
      fetch: harness.fetch,
      backoff: { initialDelayMs: 5, maximumDelayMs: 20, multiplier: 2 },
      scheduler: {
        async sleep(delayMs) {
          delays.push(delayMs)
        },
      },
    })
    const controller = new AbortController()
    let state = createInitialState()
    const accepted: string[] = []

    await client.subscribe({
      attach,
      signal: controller.signal,
      getLastSeenSequence: () => state.lastSequence,
      onEvent(event) {
        const previousGap = state.connection.gap
        const previousSequence = state.lastSequence
        state = reduceRottweilerState(state, engineEvent(event))
        if (state.lastSequence !== previousSequence && state.lastSequence !== null) {
          accepted.push(state.lastSequence)
        }
        if (previousGap === null && state.connection.gap !== null) {
          client.restartStream("immediate")
        }
        if (state.lastSequence === "4") controller.abort()
      },
    })

    expect(accepted).toEqual(["1", "2", "3", "4"])
    expect(state.connection.gap).toBeNull()
    expect(state.mode).toBe("default")
    expect(state.model).toBe("fast")
    const attaches = harness.commands.filter(
      (command): command is Extract<ClientCommand, { type: "attach_session" }> =>
        command.type === "attach_session",
    )
    expect(attaches.map((command) => command.last_seen_sequence)).toEqual([null, "1"])
    expect(
      harness.requests.filter((request) => request.startsWith("/v1/events")),
    ).toEqual([
      "/v1/events?session_id=session-transport",
      "/v1/events?session_id=session-transport&last_seen_sequence=1",
    ])
    expect(delays).toEqual([])
    expect(harness.cancelledStreams).toBe(2)
  })

  test("backs off when an immediate gap replay is itself gapped", async () => {
    const one = {
      type: "mode_changed",
      meta: durableMeta("1"),
      mode: "plan",
      definition_fingerprint: "fixture",
    } satisfies EngineEvent
    const two = {
      type: "model_changed",
      meta: durableMeta("2"),
      model: "slow",
    } satisfies EngineEvent
    const three = {
      type: "mode_changed",
      meta: durableMeta("3"),
      mode: "default",
      definition_fingerprint: "fixture",
    } satisfies EngineEvent
    const four = {
      type: "model_changed",
      meta: durableMeta("4"),
      model: "fast",
    } satisfies EngineEvent
    const harness = createPlannedFetch([
      { chunks: [encodeSseJson(one), encodeSseJson(three)] },
      { chunks: [encodeSseJson(three)] },
      {
        chunks: [encodeSseJson(two), encodeSseJson(three), encodeSseJson(four)],
      },
    ])
    const delays: number[] = []
    const reconnectAttempts: number[] = []
    const client = new EngineHttpSseClient({
      socketPath: "/private/mock-engine.sock",
      bootstrapToken: "mock-bootstrap",
      fetch: harness.fetch,
      backoff: { initialDelayMs: 5, maximumDelayMs: 20, multiplier: 2 },
      scheduler: {
        async sleep(delayMs) {
          delays.push(delayMs)
        },
      },
    })
    const controller = new AbortController()
    let state = createInitialState()
    let recoveringGap = false

    await client.subscribe({
      attach,
      signal: controller.signal,
      getLastSeenSequence: () => state.lastSequence,
      onConnection(update) {
        if (update.phase === "reconnecting") reconnectAttempts.push(update.attempt)
      },
      onEvent(event) {
        const previousGap = state.connection.gap
        const previousSequence = state.lastSequence
        state = reduceRottweilerState(state, engineEvent(event))
        const nextGap = state.connection.gap
        if (nextGap === null) {
          recoveringGap = false
        } else if (previousGap === null) {
          recoveringGap = client.restartStream("immediate")
        } else if (
          recoveringGap &&
          state.lastSequence === previousSequence &&
          durableSequenceId(event) === nextGap.received
        ) {
          client.restartStream("backoff")
        }
        if (state.lastSequence === "4") controller.abort()
      },
    })

    expect(state.lastSequence).toBe("4")
    expect(state.connection.gap).toBeNull()
    expect(delays).toEqual([5])
    expect(reconnectAttempts).toEqual([0, 1])
    const attaches = harness.commands.filter(
      (command): command is Extract<ClientCommand, { type: "attach_session" }> =>
        command.type === "attach_session",
    )
    expect(attaches.map((command) => command.last_seen_sequence)).toEqual([null, "1", "1"])
  })

  test("advances past a committed event larger than 64 KiB without reconnecting", async () => {
    const large = {
      type: "conversation_turn_committed",
      meta: durableMeta("1"),
      agent_turn: "1",
      turn: {
        role: "assistant",
        blocks: [{ type: "text", text: "x".repeat(96 * 1024) }],
        meta: { synthetic: false, summary: false },
      },
    } satisfies EngineEvent
    const finished = {
      type: "turn_finished",
      meta: durableMeta("2"),
      turn_id: "1",
      status: "completed",
      usage: {
        input_tokens: "1",
        output_tokens: "1",
        cache_read_tokens: "0",
        cache_write_tokens: "0",
        reasoning_tokens: "0",
      },
      cost: { kind: "unavailable", reason: "fixture" },
    } satisfies EngineEvent
    engine = new AuthenticatedMockEngine([
      {
        chunks: [...splitBytes(encodeSseJson(large), [17, 4_096]), encodeSseJson(finished)],
        holdOpen: true,
      },
    ])
    await engine.start()
    const client = new EngineHttpSseClient({
      socketPath: engine.socketPath,
      bootstrapToken: engine.bootstrapToken,
    })
    const controller = new AbortController()
    const seen: string[] = []
    let reconnects = 0

    await client.subscribe({
      attach,
      signal: controller.signal,
      onReconnect: () => {
        reconnects += 1
      },
      onEvent(event) {
        const sequence = durableSequenceId(event)
        if (sequence === null) return
        seen.push(sequence)
        if (sequence === "2") controller.abort()
      },
    })

    expect(seen).toEqual(["1", "2"])
    expect(reconnects).toBe(0)
  })

  test("does not reconnect forever on a deterministic parser limit rejection", async () => {
    const event = {
      type: "mode_changed",
      meta: durableMeta("1"),
      mode: "plan",
      definition_fingerprint: "fixture",
    } satisfies EngineEvent
    engine = new AuthenticatedMockEngine([{ chunks: [encodeSseJson(event)], holdOpen: true }])
    await engine.start()
    const client = new EngineHttpSseClient({
      socketPath: engine.socketPath,
      bootstrapToken: engine.bootstrapToken,
      sse: { maxLineBytes: 32, maxDataBytes: 32 },
    })
    const controller = new AbortController()
    let reconnects = 0

    await expect(
      client.subscribe({
        attach,
        signal: controller.signal,
        onReconnect() {
          reconnects += 1
        },
        onEvent() {},
      }),
    ).rejects.toBeInstanceOf(SseLimitError)

    expect(reconnects).toBe(0)
    expect(engine.requests.filter((request) => request.path === "/v1/events")).toHaveLength(1)
    await waitFor(() => engine?.cancelledStreams === 1)
    expect(engine.cancelledStreams).toBe(1)
  })

  for (const [name, frame] of [
    ["known event without payload", encodeSseJson({ type: "command_acknowledged" })],
    ["connection event addressed to another authenticated client", encodeSseJson({ type: "session_navigation_requested", meta: { protocol_version: PROTOCOL_VERSION, client_id: "foreign-client", request_id: "forged-navigation", emitted_at: "2026-01-01T00:00:00Z" }, session_id: "session-transport", target: { kind: "session", session_id: "foreign-session" } })],
    ["known event with malformed nested data", encodeSseJson({ type: "text_delta", meta: durableMeta("2"), turn_id: "1", text: [] })],
    ["invalid JSON", new TextEncoder().encode("data: {broken\n\n")],
  ] as const) {
    test(`stops on ${name} before reduction or cursor advancement`, async () => {
      const valid = { type: "text_delta", meta: durableMeta("1"), turn_id: "1", text: "accepted" } satisfies EngineEvent
      engine = new AuthenticatedMockEngine([{ chunks: [encodeSseJson(valid), frame], holdOpen: true }])
      await engine.start()
      const client = new EngineHttpSseClient({ socketPath: engine.socketPath, bootstrapToken: engine.bootstrapToken })
      const controller = new AbortController()
      let state = createInitialState()
      let reduced = 0
      let reconnects = 0
      await expect(client.subscribe({
        attach,
        signal: controller.signal,
        getLastSeenSequence: () => state.lastSequence,
        onReconnect() { reconnects += 1 },
        onEvent(event) {
          reduced += 1
          state = reduceRottweilerState(state, engineEvent(event))
        },
      })).rejects.toBeInstanceOf(EngineProtocolError)
      expect(reduced).toBe(1)
      expect(state.lastSequence).toBe("1")
      expect(state.streamingTail?.text).toBe("accepted")
      expect(reconnects).toBe(0)
      expect(engine.requests.filter((request) => request.path === "/v1/events")).toHaveLength(1)
      await waitFor(() => engine?.cancelledStreams === 1)
    })
  }

  test("encodes the session and replay cursor in the SSE request", async () => {
    const urls: string[] = []
    const controller = new AbortController()
    const client = new EngineHttpSseClient({
      socketPath: "/private/mock-engine.sock",
      bootstrapToken: "mock-bootstrap",
      fetch: (async (input: string | URL | Request) => {
        const url = String(input)
        urls.push(url)
        if (url.endsWith("/v1/connect")) {
          return Response.json({ client_id: "mock-client", token: "mock-token" }, { status: 201 })
        }
        if (url.endsWith("/v1/command")) {
          return Response.json({ type: "command", outcome: { type: "accepted" } }, { status: 202 })
        }
        controller.abort()
        return new Response(new ReadableStream<Uint8Array>(), {
          headers: { "Content-Type": "text/event-stream" },
        })
      }) as typeof fetch,
    })
    let events = 0

    await client.subscribe({
      attach: {
        ...attach,
        session_id: "session /?&",
        last_seen_sequence: "9",
      },
      signal: controller.signal,
      onEvent() {
        events += 1
      },
    })

    expect(events).toBe(0)
    expect(urls[2]).toBe(
      "http://rottweiler.local/v1/events?session_id=session+%2F%3F%26&last_seen_sequence=9",
    )
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
    await waitFor(() => engine?.cancelledStreams === 1)
    expect(engine.cancelledStreams).toBe(1)
  })
})
