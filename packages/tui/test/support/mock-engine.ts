import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import type { ClientCommand } from "../../src/protocol"

export interface StreamPlan {
  readonly chunks: readonly Uint8Array[]
  readonly holdOpen?: boolean
}

export class AuthenticatedMockEngine {
  readonly bootstrapToken = "bootstrap-test-token"
  readonly clientId = "minted-client"
  readonly clientToken = "minted-client-token"
  readonly requests: Array<{
    path: string
    authorization: string | null
    clientId: string | null
    body: string
  }> = []
  readonly commands: ClientCommand[] = []
  cancelledStreams = 0
  socketPath = ""

  #directory = ""
  #server: Bun.Server<undefined> | undefined
  #plans: StreamPlan[]

  constructor(plans: readonly StreamPlan[] = []) {
    this.#plans = [...plans]
  }

  async start(): Promise<void> {
    this.#directory = await mkdtemp(join(tmpdir(), "rw-tui-"))
    this.socketPath = join(this.#directory, "engine.sock")
    this.#server = Bun.serve({
      unix: this.socketPath,
      fetch: (request, server) => this.#handle(request, server),
    })
  }

  enqueue(plan: StreamPlan): void {
    this.#plans.push(plan)
  }

  async stop(): Promise<void> {
    await this.#server?.stop(true)
    this.#server = undefined
    if (this.#directory.length > 0) {
      await rm(this.#directory, { recursive: true, force: true })
    }
  }

  async #handle(request: Request, server: Bun.Server<undefined>): Promise<Response> {
    const url = new URL(request.url)
    const body = request.method === "POST" ? await request.text() : ""
    const authorization = request.headers.get("Authorization")
    const clientId = request.headers.get("x-rottweiler-client")
    this.requests.push({ path: url.pathname, authorization, clientId, body })

    if (url.pathname === "/v1/connect") {
      if (authorization !== `Bearer ${this.bootstrapToken}` || clientId !== null) {
        return new Response("unauthorized", { status: 401 })
      }
      return Response.json({ client_id: this.clientId, token: this.clientToken })
    }

    if (!this.#isClientAuthorized(authorization, clientId)) {
      return new Response("unauthorized", { status: 401 })
    }

    if (url.pathname === "/v1/command" && request.method === "POST") {
      this.commands.push(JSON.parse(body) as ClientCommand)
      return new Response(null, { status: 204 })
    }
    if (url.pathname === "/v1/events" && request.method === "GET") {
      server.timeout(request, 0)
      const plan = this.#plans.shift() ?? { chunks: [], holdOpen: true }
      const owner = this
      return new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            for (const chunk of plan.chunks) {
              controller.enqueue(chunk)
            }
            if (plan.holdOpen !== true) {
              controller.close()
            }
          },
          cancel() {
            owner.cancelledStreams += 1
          },
        }),
        {
          headers: {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache",
          },
        },
      )
    }
    return new Response("not found", { status: 404 })
  }

  #isClientAuthorized(authorization: string | null, clientId: string | null): boolean {
    return authorization === `Bearer ${this.clientToken}` && clientId === this.clientId
  }
}

export function encodeSseJson(value: unknown): Uint8Array {
  return new TextEncoder().encode(`data: ${JSON.stringify(value)}\n\n`)
}

export function splitBytes(bytes: Uint8Array, sizes: readonly number[]): Uint8Array[] {
  const chunks: Uint8Array[] = []
  let offset = 0
  for (const size of sizes) {
    if (offset >= bytes.byteLength) {
      break
    }
    chunks.push(bytes.slice(offset, Math.min(bytes.byteLength, offset + size)))
    offset += size
  }
  if (offset < bytes.byteLength) {
    chunks.push(bytes.slice(offset))
  }
  return chunks
}
