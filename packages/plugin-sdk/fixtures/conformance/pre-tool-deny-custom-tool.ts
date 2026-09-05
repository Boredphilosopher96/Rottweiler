import { definePlugin, runPlugin } from "../../src/index.ts"

export const plugin = definePlugin({
  manifest: {
    name: "conformance-policy-tool",
    version: "1.0.0",
    protocol: 3,
    capabilities: {
      tools: [{
        name: "fixture_echo",
        description: "Echo bounded fixture input",
        schema: { type: "object", required: ["text"], properties: { text: { type: "string" } } },
        caps: [],
      }],
      hooks: [{ name: "pre_tool", class: "policy", failure_policy: "fail-closed" }],
    },
  },
  handlers: {
    tools: { fixture_echo: ({ input }) => ({
      content: String(input.text ?? ""),
      truncated: false,
      data: { text: String(input.text ?? "") },
    }) },
    hooks: {
      pre_tool: ({ payload }) =>
        payload.name === "bash"
          ? { decision: "block", message: "conformance policy denies bash" }
          : { decision: "continue" },
    },
  },
})

if (import.meta.main) await runPlugin(plugin)
