import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import {
  createServer as createHttpServer,
  type IncomingMessage,
  type Server as HttpServer,
  type ServerResponse,
} from "node:http"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent } from "../../src/protocol"
import { EngineHttpSseClient } from "../../src/transport"
import { encodeSseJson } from "../support/mock-engine"

const SESSION_ID = "session-m4-acceptance"
const attach = {
  type: "attach_session",
  meta: {
    protocol_version: PROTOCOL_VERSION,
    client_id: "m4-perf-client",
    request_id: "m4-perf-attach",
  },
  session_id: SESSION_ID,
  last_seen_sequence: null,
  role: "driver",
} satisfies ClientCommand

function modeEvent(sequence: number): EngineEvent {
  return {
    type: "mode_changed",
    meta: {
      protocol_version: PROTOCOL_VERSION,
      session_id: SESSION_ID,
      sequence_id: String(sequence),
      emitted_at: "2026-07-10T00:00:00Z",
    },
    mode: sequence % 2 === 0 ? "plan" : "execute",
  }
}

describe("M4 transport performance gate", () => {
  const cleanups: Array<() => void | Promise<void>> = []

  afterEach(async () => {
    while (cleanups.length > 0) {
      await cleanups.pop()?.()
    }
  })

  test("engine-to-TUI authenticated UDS event latency remains below 2ms p99", async () => {
    const engine = new TimedEventEngine(320)
    await engine.start()
    cleanups.push(() => engine.stop())
    const client = new EngineHttpSseClient({
      socketPath: engine.socketPath,
      bootstrapToken: engine.bootstrapToken,
      scheduler: {
        async sleep(_delayMs, signal) {
          if (signal.aborted) {
            throw signal.reason
          }
        },
      },
    })
    const controller = new AbortController()
    const samples: number[] = []
    const streamIds = new Set<string>()
    let notifySample: (() => void) | null = null

    const subscription = client.subscribe({
      attach,
      signal: controller.signal,
      onEvent(event) {
        const sentAt = Reflect.get(event, "_sent_at_ns")
        const streamId = Reflect.get(event, "_event_stream_id")
        if (typeof sentAt !== "string") {
          throw new Error("timed event omitted its monotonic send marker")
        }
        if (typeof streamId !== "string") {
          throw new Error("timed event omitted its persistent-stream marker")
        }
        streamIds.add(streamId)
        samples.push(Number(process.hrtime.bigint() - BigInt(sentAt)) / 1_000_000)
        notifySample?.()
        notifySample = null
      },
    })

    await waitFor(async () => engine.eventStreamRequests === 1)
    for (let index = 0; index < engine.eventCount; index += 1) {
      await client.postCommand({
        type: "list_models",
        session_id: attach.session_id,
        refresh: false,
        meta: {
          protocol_version: PROTOCOL_VERSION,
          client_id: "m4-perf-client",
          request_id: `m4-perf-probe-${index}`,
        },
      })
      if (samples.length <= index) {
        await new Promise<void>((resolve) => {
          notifySample = resolve
          if (samples.length > index) {
            notifySample = null
            resolve()
          }
        })
      }
    }
    controller.abort()
    await subscription

    expect(samples).toHaveLength(engine.eventCount)
    expect([...streamIds]).toEqual(["1"])
    const p99 = percentile(samples.slice(20), 0.99)
    console.info(`M4 persistent-SSE transport latency: p99=${p99.toFixed(3)}ms`)
    expect(p99).toBeLessThan(2)
  }, 10_000)
})

function percentile(values: readonly number[], quantile: number): number {
  const sorted = [...values].sort((left, right) => left - right)
  const index = Math.min(sorted.length - 1, Math.ceil(sorted.length * quantile) - 1)
  return sorted[Math.max(0, index)] ?? Number.POSITIVE_INFINITY
}

async function waitFor(predicate: () => Promise<boolean>, timeoutMs = 3_000): Promise<void> {
  const deadline = Bun.nanoseconds() + timeoutMs * 1_000_000
  while (!(await predicate())) {
    if (Bun.nanoseconds() >= deadline) {
      throw new Error("timed out waiting for acceptance-test process state")
    }
    await Bun.sleep(2)
  }
}

class TimedEventEngine {
  readonly bootstrapToken = "m4-latency-bootstrap"
  readonly eventCount: number
  socketPath = ""
  #directory = ""
  #server: HttpServer | null = null
  #sequence = 0
  #eventStreamRequests = 0
  #eventResponse: ServerResponse | null = null

  constructor(eventCount: number) {
    this.eventCount = eventCount
  }

  async start(): Promise<void> {
    this.#directory = await mkdtemp(join(tmpdir(), "rw-m4-latency-"))
    this.socketPath = join(this.#directory, "engine.sock")
    this.#server = createHttpServer((request, response) => {
      this.#handle(request, response)
    })
    await new Promise<void>((resolve, reject) => {
      this.#server?.once("error", reject)
      this.#server?.listen(this.socketPath, resolve)
    })
  }

  async stop(): Promise<void> {
    this.#server?.closeAllConnections()
    await new Promise<void>((resolve) => this.#server?.close(() => resolve()))
    await rm(this.#directory, { recursive: true, force: true })
  }

  get eventStreamRequests(): number {
    return this.#eventStreamRequests
  }

  #handle(request: IncomingMessage, response: ServerResponse): void {
    request.resume()
    const url = new URL(request.url ?? "/", "http://rottweiler.local")
    if (url.pathname === "/v1/connect") {
      if (request.headers.authorization !== `Bearer ${this.bootstrapToken}`) {
        response.writeHead(401).end("unauthorized")
        return
      }
      writeJson(response, 200, {
        client_id: "m4-latency-client",
        token: "m4-latency-token",
      })
      return
    }
    if (
      request.headers.authorization !== "Bearer m4-latency-token" ||
      request.headers["x-rottweiler-client"] !== "m4-latency-client"
    ) {
      response.writeHead(401).end("unauthorized")
      return
    }
    if (url.pathname === "/v1/command") {
      if (this.#eventResponse !== null && this.#sequence < this.eventCount) {
        const sequence = ++this.#sequence
        this.#eventResponse.write(
          encodeSseJson({
            ...modeEvent(sequence),
            _sent_at_ns: process.hrtime.bigint().toString(),
            _event_stream_id: String(this.#eventStreamRequests),
          }),
        )
      }
      writeJson(response, 202, { type: "accepted" })
      return
    }
    if (url.pathname !== "/v1/events") {
      response.writeHead(404).end("not found")
      return
    }

    this.#eventStreamRequests += 1
    this.#eventResponse = response
    response.writeHead(200, {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
    })
    response.flushHeaders()
    response.once("close", () => {
      if (this.#eventResponse === response) {
        this.#eventResponse = null
      }
    })
  }
}

function writeJson(response: ServerResponse, status: number, value: unknown): void {
  const body = JSON.stringify(value)
  response.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(body),
  })
  response.end(body)
}
