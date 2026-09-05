import { definePlugin, runPlugin } from "../../src/index.ts"

export const plugin = definePlugin({
  manifest: {
    name: "conformance-provider-v3",
    version: "1.0.0",
    protocol: 3,
    capabilities: {
      providers: [{ "alias-prefix": "fixture-v3/", capabilities: ["models"] }],
    },
  },
  handlers: {
    providers: {
      "fixture-v3/": async function* ({ alias }) {
        yield { type: "message_start", model: alias }
        if (alias.endsWith("numeric-credit")) {
          for (let n = 0; n < 256; n += 1) {
            yield { type: "tool_call_end", id: String(n), arguments: {
              decimal: 0.000001, large: 100000000000000000000,
              tiny: 1e-7, exponent: 1e21, escaped: "é\n\"\\/",
            } }
          }
        } else yield { type: "text_delta", text: `fixture response for ${alias}` }
        yield { type: "finished", reason: "stop" }
      },
    },
    providerModels: {
      "fixture-v3/": () => ({
        models: [{
          id: "vision-thinking",
          display_name: "Vision Thinking",
          capabilities: {
            tool_calling: true,
            vision: true,
            thinking: true,
            cache_breakpoints: "explicit",
          },
          max_context_tokens: 200_000,
          max_output_tokens: 16_000,
          pricing: {
            input_per_million_micros_usd: 3_000_000,
            output_per_million_micros_usd: 15_000_000,
            cache_read_per_million_micros_usd: 300_000,
            cache_write_per_million_micros_usd: 3_750_000,
          },
        }],
      }),
    },
  },
})

if (import.meta.main) await runPlugin(plugin)
