import { definePlugin, runPlugin } from "../../src/index.ts"

const plugin = definePlugin({
  manifest: {
    name: "context-workflow", version: "1", protocol: 3,
    capabilities: {
      commands: [{ name: "manage-context", description: "Pin and evict a conversation source", allowed_tools: [] }],
      push: ["session/context_read", "session/control", "extension/state_read", "extension/state_commit"],
    },
  },
  handlers: {
    commands: {
      "manage-context": async (_params, { session, state }) => {
        const initial = await session.readContext({ expected_sequence: null, after_item_id: null })
        if (initial.outcome !== "ready") throw new Error("initial context is not ready")
        const selected = initial.items.find(item => item.source === "conversation")
        if (!selected) throw new Error("committed conversation context is absent")
        const pinned = await session.control({ action: "pin_context", item_id: selected.item_id })
        if (pinned.outcome !== "applied") throw new Error("same-command pin was rejected")
        const stale = await session.readContext({ expected_sequence: initial.sequence, after_item_id: selected.item_id })
        if (stale.outcome !== "restart") throw new Error("stale context prefix was accepted")
        const current = await session.readContext({ expected_sequence: null, after_item_id: null })
        if (current.outcome !== "ready" || !current.items.find(item => item.item_id === selected.item_id)?.state.pinned) throw new Error("pin was not reflected by canonical context")
        const evicted = await session.control({ action: "evict_context", item_id: selected.item_id })
        if (evicted.outcome !== "applied") throw new Error("same-command eviction was rejected")
        const final = await session.readContext({ expected_sequence: null, after_item_id: null })
        if (final.outcome !== "ready" || !final.items.find(item => item.item_id === selected.item_id)?.state.evicted) throw new Error("eviction was not reflected by canonical context")
        const before = await state.read()
        const committed = await state.commit({ expected_revision: before.revision, mutations: [{ action: "set", key: "managed", value: selected.item_id }] })
        if (committed.outcome !== "committed") throw new Error("context receipt conflict")
        return { managed: selected.item_id }
      },
    },
  },
})

if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else await runPlugin(plugin)
}
