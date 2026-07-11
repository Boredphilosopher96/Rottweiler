import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { createConnection, createServer, type Server, type Socket } from "node:net"
import { tmpdir } from "node:os"
import { fileURLToPath } from "node:url"
import { join } from "node:path"

import { PROTOCOL_VERSION, type ClientCommand, type EngineEvent } from "../../src/protocol"
import { EngineHttpSseClient } from "../../src/transport"
import { AuthenticatedMockEngine, encodeSseJson } from "../support/mock-engine"

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

describe("M4 transport and process acceptance gates", () => {
  const cleanups: Array<() => void | Promise<void>> = []

  afterEach(async () => {
    while (cleanups.length > 0) {
      await cleanups.pop()?.()
    }
  })

  test("packaged headless OpenTUI harness reaches a real first render", async () => {
    const worker = fileURLToPath(new URL("./first-paint-worker.ts", import.meta.url))
    const directory = await mkdtemp(join(tmpdir(), "rw-m4-first-paint-"))
    cleanups.push(() => rm(directory, { recursive: true, force: true }))
    const executable = join(directory, "first-paint")
    const build = Bun.spawnSync(
      [process.execPath, "build", "--compile", worker, "--outfile", executable],
      {
        cwd: fileURLToPath(new URL("../..", import.meta.url)),
        stdin: "ignore",
        stdout: "pipe",
        stderr: "pipe",
        env: { ...process.env, ROTTWEILER_CREDENTIAL_BACKEND: "file" },
      },
    )
    expect(build.exitCode, build.stderr.toString()).toBe(0)
    const samples: number[] = []

    for (let index = 0; index < 12; index += 1) {
      const started = Bun.nanoseconds()
      const child = Bun.spawn([executable], {
        cwd: fileURLToPath(new URL("../..", import.meta.url)),
        stdin: "ignore",
        stdout: "pipe",
        stderr: "pipe",
        env: { ...process.env, ROTTWEILER_CREDENTIAL_BACKEND: "file" },
      })
      await waitForStreamMarker(child.stdout, "ROTTWEILER_FIRST_PAINT")
      samples.push((Bun.nanoseconds() - started) / 1_000_000)
      child.kill()
      await child.exited
    }

    // The release-path 150ms gate is owned by m4_release_gate.py. The testing
    // package deliberately loads extra headless and mock-tree-sitter code that
    // is absent from the production TUI, so this test is a liveness regression
    // guard rather than a substitute performance measurement.
    expect(percentile(samples.slice(2), 0.99)).toBeLessThan(1_000)
  }, 10_000)

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

    await client.subscribe({
      attach,
      signal: controller.signal,
      onEvent(event) {
        const sentAt = Reflect.get(event, "_sent_at_ns")
        if (typeof sentAt !== "string") {
          throw new Error("timed event omitted its monotonic send marker")
        }
        samples.push(Number(BigInt(Bun.nanoseconds()) - BigInt(sentAt)) / 1_000_000)
        if (samples.length === engine.eventCount) {
          controller.abort()
        }
      },
    })

    expect(samples).toHaveLength(engine.eventCount)
    expect(percentile(samples.slice(20), 0.99)).toBeLessThan(2)
  }, 10_000)

  test("SIGKILLed TUI runtime rebuilds the complete durable transcript with no lost sequence", async () => {
    const events = [1, 2, 3, 4, 5].map(modeEvent)
    const engine = new AuthenticatedMockEngine([
      { chunks: [encodeSseJson(events[0])], holdOpen: true },
      { chunks: events.map(encodeSseJson), holdOpen: true },
    ])
    await engine.start()
    cleanups.push(() => engine.stop())

    const directory = await mkdtemp(join(tmpdir(), "rw-m4-reattach-"))
    cleanups.push(() => rm(directory, { recursive: true, force: true }))
    const tokenFile = join(directory, "auth.token")
    const cursorFile = join(directory, "last-seen")
    const reportFile = join(directory, "report.json")
    await writeFile(tokenFile, `${engine.bootstrapToken}\n`, { mode: 0o600 })

    const first = spawnReattachWorker(engine.socketPath, tokenFile, cursorFile, reportFile, "999")
    await waitFor(async () => (await readOptional(cursorFile))?.trim() === "1")
    first.kill(9)
    expect(await first.exited).not.toBe(0)

    expect((await readFile(cursorFile, "utf8")).trim()).toBe("1")
    const second = spawnReattachWorker(
      engine.socketPath,
      tokenFile,
      cursorFile,
      reportFile,
      "5",
      "1",
    )
    await waitFor(async () => (await readOptional(reportFile)) !== null)
    expect(await second.exited).toBe(0)

    expect(JSON.parse((await readFile(reportFile, "utf8")).trim())).toEqual({
      lastSequence: "5",
      duplicateEvents: 0,
      invalidEvents: 0,
      gap: null,
      receivedSequences: ["1", "2", "3", "4", "5"],
    })
    const attaches = engine.commands.filter(
      (command): command is Extract<ClientCommand, { type: "attach_session" }> =>
        command.type === "attach_session",
    )
    expect(attaches.map((command) => command.last_seen_sequence)).toEqual([null, null])
    expect(
      engine.requests
        .filter((request) => request.path === "/v1/events")
        .map((request) => request.search),
    ).toEqual([
      `?session_id=${SESSION_ID}`,
      `?session_id=${SESSION_ID}`,
    ])
  }, 10_000)

  test("loopback remote forwarding produces a byte-identical local transcript", async () => {
    const payload = [modeEvent(1), modeEvent(2), modeEvent(3)].map(encodeSseJson)
    const engine = new AuthenticatedMockEngine([
      { chunks: payload, holdOpen: true },
      { chunks: payload, holdOpen: true },
    ])
    await engine.start()
    cleanups.push(() => engine.stop())

    const forwarder = await UnixSocketForwarder.start(engine.socketPath)
    cleanups.push(() => forwarder.stop())

    const local = await collectTranscript(engine.socketPath, engine.bootstrapToken, 3)
    const forwarded = await collectTranscript(forwarder.socketPath, engine.bootstrapToken, 3)

    expect(forwarded).toEqual(local)
    expect(new TextDecoder().decode(forwarded)).toContain('"sequence_id":"3"')
  })
})

function percentile(values: readonly number[], quantile: number): number {
  const sorted = [...values].sort((left, right) => left - right)
  const index = Math.min(sorted.length - 1, Math.ceil(sorted.length * quantile) - 1)
  return sorted[Math.max(0, index)] ?? Number.POSITIVE_INFINITY
}

function spawnReattachWorker(
  socketPath: string,
  tokenFile: string,
  cursorFile: string,
  reportFile: string,
  target: string,
  lastSeenSequence?: string,
): Bun.Subprocess<"ignore", "pipe", "pipe"> {
  const worker = fileURLToPath(new URL("./reattach-worker.ts", import.meta.url))
  return Bun.spawn([process.execPath, worker], {
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ...process.env,
      ROTTWEILER_CREDENTIAL_BACKEND: "file",
      ROTTWEILER_ENGINE_SOCKET: socketPath,
      ROTTWEILER_ENGINE_TOKEN_FILE: tokenFile,
      ROTTWEILER_SESSION_ID: SESSION_ID,
      ROTTWEILER_LAST_SEEN_FILE: cursorFile,
      ROTTWEILER_TEST_TARGET_SEQUENCE: target,
      ROTTWEILER_TEST_REPORT_FILE: reportFile,
      ...(lastSeenSequence === undefined
        ? {}
        : { ROTTWEILER_LAST_SEEN_SEQUENCE: lastSeenSequence }),
    },
  })
}

async function readOptional(path: string): Promise<string | null> {
  return readFile(path, "utf8").catch((error: unknown) => {
    if (
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      Reflect.get(error, "code") === "ENOENT"
    ) {
      return null
    }
    throw error
  })
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

async function waitForStreamMarker(
  stream: ReadableStream<Uint8Array>,
  marker: string,
): Promise<void> {
  const reader = stream.getReader()
  const decoder = new TextDecoder()
  let output = ""
  try {
    while (!output.includes(marker)) {
      const next = await reader.read()
      if (next.done) {
        throw new Error(`first-paint process exited before emitting ${marker}`)
      }
      output = `${output}${decoder.decode(next.value, { stream: true })}`.slice(-4096)
    }
  } finally {
    await reader.cancel().catch(() => {})
    reader.releaseLock()
  }
}

async function collectTranscript(
  socketPath: string,
  bootstrapToken: string,
  count: number,
): Promise<Uint8Array> {
  const client = new EngineHttpSseClient({ socketPath, bootstrapToken })
  const controller = new AbortController()
  const frames: Uint8Array[] = []
  await client.subscribe({
    attach,
    signal: controller.signal,
    onEvent(event) {
      frames.push(new TextEncoder().encode(`${JSON.stringify(event)}\n`))
      if (frames.length === count) {
        controller.abort()
      }
    },
  })
  const length = frames.reduce((total, frame) => total + frame.byteLength, 0)
  const transcript = new Uint8Array(length)
  let offset = 0
  for (const frame of frames) {
    transcript.set(frame, offset)
    offset += frame.byteLength
  }
  return transcript
}

class UnixSocketForwarder {
  readonly socketPath: string
  readonly #directory: string
  readonly #server: Server
  readonly #sockets = new Set<Socket>()

  private constructor(socketPath: string, directory: string, server: Server) {
    this.socketPath = socketPath
    this.#directory = directory
    this.#server = server
  }

  static async start(upstreamPath: string): Promise<UnixSocketForwarder> {
    const directory = await mkdtemp(join(tmpdir(), "rw-m4-forward-"))
    const socketPath = join(directory, "forwarded.sock")
    const sockets = new Set<Socket>()
    const server = createServer((downstream) => {
      const upstream = createConnection(upstreamPath)
      sockets.add(downstream)
      sockets.add(upstream)
      downstream.pipe(upstream)
      upstream.pipe(downstream)
      const close = () => {
        downstream.destroy()
        upstream.destroy()
        sockets.delete(downstream)
        sockets.delete(upstream)
      }
      downstream.on("error", close)
      upstream.on("error", close)
      downstream.on("close", close)
      upstream.on("close", close)
    })
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject)
      server.listen(socketPath, resolve)
    })
    const forwarder = new UnixSocketForwarder(socketPath, directory, server)
    for (const socket of sockets) {
      forwarder.#sockets.add(socket)
    }
    server.on("connection", (socket) => forwarder.#sockets.add(socket))
    return forwarder
  }

  async stop(): Promise<void> {
    for (const socket of this.#sockets) {
      socket.destroy()
    }
    await new Promise<void>((resolve) => this.#server.close(() => resolve()))
    await rm(this.#directory, { recursive: true, force: true })
  }
}

class TimedEventEngine {
  readonly bootstrapToken = "m4-latency-bootstrap"
  readonly eventCount: number
  socketPath = ""
  #directory = ""
  #server: Bun.Server<undefined> | null = null
  #sequence = 0

  constructor(eventCount: number) {
    this.eventCount = eventCount
  }

  async start(): Promise<void> {
    this.#directory = await mkdtemp(join(tmpdir(), "rw-m4-latency-"))
    this.socketPath = join(this.#directory, "engine.sock")
    this.#server = Bun.serve({
      unix: this.socketPath,
      fetch: (request, server) => this.#handle(request, server),
    })
  }

  async stop(): Promise<void> {
    await this.#server?.stop(true)
    await rm(this.#directory, { recursive: true, force: true })
  }

  async #handle(request: Request, server: Bun.Server<undefined>): Promise<Response> {
    const url = new URL(request.url)
    if (url.pathname === "/v1/connect") {
      if (request.headers.get("Authorization") !== `Bearer ${this.bootstrapToken}`) {
        return new Response("unauthorized", { status: 401 })
      }
      return Response.json({ client_id: "m4-latency-client", token: "m4-latency-token" })
    }
    if (
      request.headers.get("Authorization") !== "Bearer m4-latency-token" ||
      request.headers.get("x-rottweiler-client") !== "m4-latency-client"
    ) {
      return new Response("unauthorized", { status: 401 })
    }
    if (url.pathname === "/v1/command") {
      return Response.json({ type: "accepted" }, { status: 202 })
    }
    if (url.pathname !== "/v1/events") {
      return new Response("not found", { status: 404 })
    }

    server.timeout(request, 0)
    this.#sequence += 1
    const sequence = this.#sequence
    const event = {
      ...modeEvent(sequence),
      _sent_at_ns: Bun.nanoseconds().toString(),
    }
    return new Response(
      new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(encodeSseJson(event))
          controller.close()
        },
      }),
      { headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" } },
    )
  }
}
