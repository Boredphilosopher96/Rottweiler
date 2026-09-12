import { definePlugin, runPlugin, type HandlerContext } from "../../src/index.ts"

const instance = crypto.randomUUID()

async function remember(context: HandlerContext, key: string) {
  const identity = await context.session.query()
  const previous = await context.state.read()
  const old = previous.entries.find(entry => entry.key === key)?.value
  const count = typeof old === "object" && old !== null && "count" in old && typeof old.count === "number" ? old.count + 1 : 1
  const value = { count, session: identity.session_id, instance }
  const committed = await context.state.commit({ expected_revision: previous.revision, mutations: [{ action: "set", key, value }] })
  if (committed.outcome !== "committed") throw new Error("namespace commit rejected")
  return value
}

export const plugin = definePlugin({
  manifest: {
    name: "child-namespace", version: "1", protocol: 3,
    capabilities: {
      commands: [{ name: "namespace", description: "Publish the bound session namespace", allowed_tools: [] }],
      providers: [{ "alias-prefix": "child/" }],
      push: ["session/query", "extension/state_read", "extension/state_commit", "ui/publish_panel"],
      ui: [{ surface: "panel", id: "identity", title: "Identity", fields: [{ kind: "text", id: "session", label: "Session", path: [{ step: "field", name: "session" }] }], actions: [] }],
    },
  },
  handlers: {
    commands: {
      namespace: async (_params, context) => {
        const value = await remember(context, "command")
        await context.push.publishPanel("identity", value)
        return value
      },
    },
    providers: {
      "child/": async function* (_params, context) {
        const value = await remember(context, "provider")
        yield { type: "text_delta", text: value.session }
        yield { type: "finished", reason: "stop" }
      },
    },
  },
})
if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else await runPlugin(plugin)
}
