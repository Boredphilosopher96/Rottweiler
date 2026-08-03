import { definePlugin, runPlugin } from "../../src/index.ts"

export const plugin = definePlugin({
  manifest: {
    name: "conformance-provider-v2",
    version: "1.0.0",
    protocol: 2,
    capabilities: {
      providers: [{ "alias-prefix": "fixture-v2/", capabilities: ["models"] }],
    },
  },
  handlers: {
    providers: {
      "fixture-v2/": async function* ({ alias }) {
        yield { type: "message_start", model: alias }
        yield { type: "text_delta", text: `fixture response for ${alias}` }
        yield { type: "finished", reason: "stop" }
      },
    },
    providerModels: {
      "fixture-v2/": () => ({
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
