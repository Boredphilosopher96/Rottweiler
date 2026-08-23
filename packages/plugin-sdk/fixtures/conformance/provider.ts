import { definePlugin, runPlugin } from "../../src/index.ts"

export const plugin = definePlugin({
  manifest: {
    name: "conformance-provider",
    version: "1.0.0",
    protocol: 2,
    capabilities: { providers: [{ "alias-prefix": "fixture/" }] },
  },
  handlers: {
    providers: {
      "fixture/": async function* ({ alias }, { signal }) {
        yield { type: "message_start", model: alias }
        yield { type: "text_delta", text: `fixture response for ${alias}` }
        await new Promise<void>((resolve, reject) => {
          const timer = setTimeout(resolve, 75)
          signal.addEventListener("abort", () => {
            clearTimeout(timer)
            reject(new Error("cancelled"))
          }, { once: true })
        })
        yield { type: "usage", usage: {
          input_tokens: 1, output_tokens: 1, cache_read_tokens: 0,
          cache_write_tokens: 0, reasoning_tokens: 0,
        } }
        yield { type: "finished", reason: "stop" }
      },
    },
  },
})

if (import.meta.main) await runPlugin(plugin)
