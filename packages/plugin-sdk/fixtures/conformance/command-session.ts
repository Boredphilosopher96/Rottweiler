import { definePlugin, runPlugin } from "../../src/index.ts"

export const plugin = definePlugin({
  manifest: {
    name: "command-session", version: "1", protocol: 3,
    capabilities: {
      commands: [{ name: "context-panel", description: "Inspect context and publish session state", allowed_tools: ["read"] }],
      push: ["session/tool_call", "session/context_read", "session/control", "extension/state_read", "extension/state_commit", "ui/publish_panel", "session/query", "ui/notify"],
      ui: [{ surface: "panel", id: "context", title: "Context", fields: [{ kind: "text", id: "summary", label: "Summary", path: [{ step: "field", name: "summary" }] }], actions: [] }],
    },
  },
  handlers: {
    commands: {
      "context-panel": async (params, { session, state, push }) => {
        const tool = await session.callTool("read", { path: "broker.txt" })
        if (tool.is_error || tool.output === null || !JSON.stringify(tool.output).includes("broker owned bytes")) throw new Error("host tool did not return its canonical output")
        const context = await session.readContext({ expected_sequence: null, after_item_id: null })
        if (context.outcome !== "ready") throw new Error("context inventory was not ready")
        const changed = await session.control({ action: "select_mode", mode: "plan" })
        if (changed.outcome !== "applied") throw new Error("same-command mode control was rejected")
        const previous = await state.read()
        const committed = await state.commit({ expected_revision: previous.revision, mutations: [{ action: "set", key: "context/items", value: context.items.length }] })
        if (committed.outcome !== "committed") throw new Error("state commit did not settle")
        const revision = await push.publishPanel("context", { summary: `Context has ${context.items.length} items` })
        if (params.arguments === "navigate") {
          const navigation = await session.control({ action: "navigate", target: { kind: "transcript", sequence: "0" } })
          if (navigation.outcome !== "applied") throw new Error("navigation was not deferred")
          const identity = await session.query()
          await push.notify("Navigation waiting", "The command callback is still active", identity.session_id)
          while (!(await state.read()).entries.some(entry => entry.key === "navigation/release")) {
            await new Promise(resolve => setTimeout(resolve, 5))
          }
          const beforeFinish = await state.read()
          const finished = await state.commit({ expected_revision: beforeFinish.revision, mutations: [{ action: "set", key: "navigation/completed", value: true }] })
          if (finished.outcome !== "committed") throw new Error("callback completion marker did not commit")
        }
        return { revision, state_revision: committed.revision, items: context.items.length }
      },
    },
  },
})

if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else await runPlugin(plugin)
}
