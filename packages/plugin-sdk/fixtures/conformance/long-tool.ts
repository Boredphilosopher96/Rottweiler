import { definePlugin, runPlugin } from "../../src/index.ts"

const plugin = definePlugin({
  manifest: {
    name: "long-tool", version: "1", protocol: 3,
    capabilities: {
      tools: [{ name: "work", description: "Cancellable long operation", schema: { type: "object" }, caps: [] }],
      hooks: [{ name: "pre_tool", class: "policy", failure_policy: "fail-closed" }],
    },
  },
  handlers: {
    hooks: { pre_tool: () => ({ decision: "continue" }) },
    tools: {
      work: async ({ input }, context) => {
        const mode = (input as { mode: "long" | "silent" | "chatty" }).mode
        context.progress({ message: "started" })
        const timer = mode === "silent" ? undefined : setInterval(() => {
          const count = mode === "chatty" ? 100 : 1
          for (let index = 0; index < count; index++) context.progress({ message: "working" })
        }, mode === "chatty" ? 1 : 250)
        try {
          await new Promise<void>(resolve => {
            if (context.signal.aborted) resolve()
            else context.signal.addEventListener("abort", () => resolve(), { once: true })
          })
          return { content: "settled after cancellation", data: null, truncated: false }
        } finally { if (timer !== undefined) clearInterval(timer) }
      },
    },
  },
})

if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else await runPlugin(plugin)
}
