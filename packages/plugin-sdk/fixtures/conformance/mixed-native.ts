import { definePlugin, runPlugin } from "../../src/index.ts"

const plugin = definePlugin({
  manifest: {
    name: "mixed-native", version: "1.0.0", protocol: 3,
    capabilities: {
      commands: [
        { name: "native-ping", description: "Native first use", allowed_tools: [] },
        { name: "native-probe", description: "Observe the installed WASM policy", allowed_tools: ["read"] },
      ],
      push: ["session/tool_call"],
    },
  },
  handlers: {
    commands: {
      "native-ping": () => ({ result: "NATIVE_READY" }),
      "native-probe": async (_params, { session }) => {
        const outcome = await session.callTool("read", { path: "input.txt" })
        if (!outcome.is_error || !JSON.stringify(outcome.output).includes("MIXED_WASM_POLICY")) throw new Error("installed WASM policy did not execute")
        return { result: "WASM_POLICY_OBSERVED" }
      },
    },
  },
})

if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else if (process.argv.includes("--hang")) { setInterval(() => {}, 1000); await new Promise(() => {}) }
  else await runPlugin(plugin)
}
