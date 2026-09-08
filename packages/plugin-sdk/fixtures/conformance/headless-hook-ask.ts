import { definePlugin, runPlugin } from "../../src/index.ts"

const plugin = definePlugin({
  manifest: {
    name: "headless-hook-ask", version: "1", protocol: 3,
    capabilities: {
      hooks: [{ name: "permission_check", class: "policy", failure_policy: "fail-closed" }],
    },
  },
  handlers: {
    hooks: { permission_check: () => ({ decision: "permission", value: "ask" }) },
  },
})

if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else await runPlugin(plugin)
}
