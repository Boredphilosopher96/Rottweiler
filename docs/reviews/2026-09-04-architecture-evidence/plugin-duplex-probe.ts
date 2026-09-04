import {
  PluginServer,
  type PluginDefinition,
  type ServerTransport,
} from "../../../packages/plugin-sdk/src/server"
import type { JsonValue } from "../../../packages/plugin-sdk/src/generated/protocol-2"

const queue: Uint8Array[] = []
const output: unknown[] = []
let wake: (() => void) | undefined

function send(frame: JsonValue): void {
  queue.push(new TextEncoder().encode(`${JSON.stringify(frame)}\n`))
  wake?.()
}

const input: AsyncIterable<Uint8Array> = {
  async *[Symbol.asyncIterator]() {
    for (;;) {
      if (queue.length === 0) await new Promise<void>((resolve) => { wake = resolve })
      let next = queue.shift()
      while (next !== undefined) {
        yield next
        next = queue.shift()
      }
    }
  },
}

const definition: PluginDefinition = {
  manifest: {
    name: "review-repro",
    version: "1.0.0",
    protocol: 2,
    capabilities: {
      providers: [{
        "alias-prefix": "probe/",
        capabilities: ["models"],
        "credential-references": ["probe-key"],
      }],
    },
  },
  handlers: {
    providers: { "probe/": async function* () { yield { type: "finished", reason: "stop" } } },
    providerModels: {
      "probe/": async (_params, context) => {
        await context.providerHttp.request("probe-key", {
          method: "GET",
          url: "https://example.com/models",
          credential_header: "authorization",
        })
        return { models: [] }
      },
    },
  },
}

const transport: ServerTransport = {
  input,
  output: {
    write(bytes) {
      const frame: unknown = JSON.parse(new TextDecoder().decode(bytes))
      output.push(frame)
      if (frame === null || typeof frame !== "object") throw new Error("Invalid SDK frame")
      if ("method" in frame && frame.method === "provider/http" && "id" in frame) {
        if (typeof frame.id !== "string") throw new Error("Invalid SDK HTTP id")
        send({ jsonrpc: "2.0", method: "provider/http_event", params: {
          request_id: frame.id, event: { type: "head", status: 200, headers: [] },
        } })
        send({ jsonrpc: "2.0", method: "provider/http_event", params: {
          request_id: frame.id, event: { type: "finished" },
        } })
        send({ jsonrpc: "2.0", id: frame.id, result: null })
      }
      if ("id" in frame && frame.id === 2) send({ jsonrpc: "2.0", id: 3, method: "shutdown" })
    },
  },
}

const server = new PluginServer(definition, transport, 4 * 1024 * 1024, 80)
send({ jsonrpc: "2.0", id: 1, method: "initialize", params: {
  host: "rottweiler", protocol: 2, min_protocol: 2, max_frame_bytes: 4 * 1024 * 1024,
  capabilities: ["provider-models", "provider-http"],
} })
send({ jsonrpc: "2.0", id: 2, method: "provider/models", params: { alias_prefix: "probe/" } })
await server.serve(input)
console.log(JSON.stringify(output, null, 2))

const response = output.find((frame) => frame !== null && typeof frame === "object" && "id" in frame && frame.id === 2)
if (response === undefined) throw new Error("Missing provider/models response")
if (JSON.stringify(response).includes("plugin handler timed out")) {
  console.log("REPRODUCED: provider/models cannot consume its queued host HTTP response while its handler is running.")
} else {
  throw new Error("The original timeout did not reproduce; inspect the provider/models response.")
}
