import { definePlugin, runPlugin } from "@rottweiler/plugin"

export const plugin = definePlugin({
  manifest: {
    name: "__ROTTWEILER_PLUGIN_NAME__",
    version: "0.1.0",
    protocol: 1,
    capabilities: {
      tools: [{
        name: "hello",
        description: "Return a greeting",
        schema: { type: "object", properties: { name: { type: "string" } } },
        caps: ["reads-fs"],
      }],
      hooks: [{ name: "pre_tool", failure_policy: "fail-closed" }],
    },
  },
  handlers: {
    tools: {
      hello: ({ input }) => ({
        content: `Hello, ${String(input.name ?? "world")}!`,
        data: { text: `Hello, ${String(input.name ?? "world")}!` },
      }),
    },
    hooks: {
      pre_tool: ({ payload }) =>
        payload.name === "bash"
          ? { decision: "deny", message: "This plugin blocks shell execution" }
          : { decision: "allow" },
    },
  },
})

if (import.meta.main) await runPlugin(plugin)
