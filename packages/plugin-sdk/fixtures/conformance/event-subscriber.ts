import { definePlugin, runPlugin } from "../../src/index.ts"

export const plugin = definePlugin({
  manifest: {
    name: "conformance-event-subscriber",
    version: "1.0.0",
    protocol: 2,
    capabilities: {
      event_subscriptions: ["TurnFinished"],
      push: ["session/set_status"],
    },
  },
  handlers: {
    events: {
      TurnFinished: async ({ payload }, { push }) => {
        if (typeof payload.session_id === "string") await push.setStatus(payload.session_id, "turn complete")
      },
    },
  },
})

if (import.meta.main) await runPlugin(plugin)
