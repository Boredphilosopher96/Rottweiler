import { definePlugin, runPlugin } from "../../src/index.ts"

export const plugin = definePlugin({
  manifest: {
    name: "conformance-event-subscriber",
    version: "1.0.0",
    protocol: 3,
    capabilities: {
      event_subscriptions: ["turn_finished"],
      push: ["session/set_status"],
    },
  },
  handlers: {
    events: {
      turn_finished: async ({ cursor }, { push }) => {
        await push.setStatus(cursor.session_id, "turn complete")
        return { mutations: [] }
      },
    },
  },
})

if (import.meta.main) await runPlugin(plugin)
