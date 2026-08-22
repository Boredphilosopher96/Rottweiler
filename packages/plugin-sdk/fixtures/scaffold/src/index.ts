import { definePlugin, parsePluginManifest, runPlugin } from "@rottweiler/plugin"
import manifestDocument from "../manifest.json"

export const plugin = definePlugin({
  manifest: parsePluginManifest(manifestDocument),
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
