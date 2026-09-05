import { definePlugin, runPlugin, type EventHandler } from "../../src/index.ts"

const observe: EventHandler = async ({ cursor, event }, { state }) => {
  if (event === "mode_changed") return { mutations: [{ action: "set", key: "barrier", value: cursor.sequence }] }
  const snapshot = await state.read()
  const attempt = snapshot.entries.find(entry => entry.key === "attempt")
  if (attempt === undefined) {
    const committed = await state.commit({
      expected_revision: snapshot.revision,
      mutations: [{ action: "set", key: "attempt", value: { sequence: cursor.sequence, pid: process.pid } }],
    })
    if (committed.outcome !== "committed") throw new Error("attempt did not commit")
    // No event outcome exists; only an atomic host acknowledgement can retire
    // this cursor. The next process must receive it again.
    process.exit(23)
  }
  const count = snapshot.entries.find(entry => entry.key === "deliveries")?.value
  return { mutations: [
    { action: "set", key: "delivered", value: cursor.sequence },
    { action: "set", key: "deliveries", value: typeof count === "number" ? count + 1 : 1 },
  ] }
}
export const plugin = definePlugin({
  manifest: {
    name: "event-recovery", version: "1", protocol: 3,
    capabilities: {
      event_subscriptions: ["session_created", "mode_changed"],
      push: ["extension/state_read", "extension/state_commit"],
    },
  },
  handlers: { events: { session_created: observe, mode_changed: observe } },
})
if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else await runPlugin(plugin)
}
