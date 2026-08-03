import { definePlugin, runPlugin, type ProviderEvent } from "../../src/index"

export const plugin = definePlugin({
  manifest: {
    name: "conformance-provider-auth-v2",
    version: "1.0.0",
    protocol: 2,
    capabilities: {
      providers: [{
        "alias-prefix": "auth-v2/",
        capabilities: ["models"],
        "credential-references": ["fixture-token"],
      }],
    },
  },
  handlers: {
    providers: {
      "auth-v2/": async function* (params, context) {
        const credentialReference = params.request.model === "undeclared"
          ? "undeclared-token"
          : "fixture-token"
        const response = await context.providerHttp.request(credentialReference, {
          method: "POST",
          url: `https://api.example.test/v1/${params.request.model}`,
          headers: [{ name: "content-type", value: "application/json" }],
          body: new TextEncoder().encode("{}"),
          credential_header: "authorization",
          credential_prefix: "Bearer ",
        })
        if (response.headers.some((header) => header.value.includes("PLUGIN_HTTP_SECRET"))) {
          throw new Error("host exposed credential material in response headers")
        }
        let buffered = ""
        for await (const chunk of response.body) {
          buffered += new TextDecoder().decode(chunk, { stream: true })
          while (buffered.includes("\n")) {
            const newline = buffered.indexOf("\n")
            const line = buffered.slice(0, newline)
            buffered = buffered.slice(newline + 1)
            if (line.length > 0) yield JSON.parse(line) as ProviderEvent
          }
        }
        if (buffered.length > 0) yield JSON.parse(buffered) as ProviderEvent
      },
    },
    providerModels: {
      "auth-v2/": () => ({ models: [{
        id: "tool-model",
        capabilities: {
          tool_calling: true,
          vision: false,
          thinking: false,
          cache_breakpoints: "none",
        },
      }] }),
    },
  },
})

if (import.meta.main) await runPlugin(plugin)
