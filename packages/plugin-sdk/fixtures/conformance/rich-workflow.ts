import { definePlugin, runPlugin } from "../../src/index.ts"

const field = (name: string) => [{ step: "field" as const, name }]
const fields = [
  { kind: "text" as const, id: "summary", label: "Summary", path: field("summary") },
  { kind: "badge" as const, id: "phase", label: "Phase", path: field("phase") },
  { kind: "list" as const, id: "checks", label: "Checks", path: field("checks"), max_items: 32 },
  { kind: "table" as const, id: "files", label: "Files", path: field("files"), max_rows: 32,
    columns: [{ label: "Path", path: field("path") }, { label: "Status", path: field("status") }] },
]
const actions = [{ id: "advance", label: "Advance workflow", command: "rich-workflow", arguments: { action: "advance" } }]
const data = (advances: number) => ({
  summary: "Native SDK artifact workflow", phase: advances === 0 ? "ready" : `advanced:${advances}`, advances,
  checks: ["Authenticated engine", "Canonical journal", "Declared SDK action"],
  files: [{ path: "α.ts", status: "verified" }, { path: "engine.rs", status: "verified" }],
})

export const plugin = definePlugin({
  manifest: {
    name: "rich-workflow", version: "1", protocol: 3,
    capabilities: {
      commands: [{ name: "rich-workflow", description: "Inspect a canonical rich artifact", allowed_tools: ["rich_artifact"] }],
      tools: [{ name: "rich_artifact", description: "Create a canonical paged artifact", schema: { type: "object", additionalProperties: false }, caps: [] }],
      push: ["session/tool_call", "extension/state_read", "extension/state_commit", "ui/publish_panel"],
      ui: [
        { surface: "panel", id: "artifact", title: "Native artifact panel", fields, actions },
        { surface: "tool", id: "result", tool_name: "rich_artifact", title: "Native artifact result", fields, actions },
      ],
    },
  },
  handlers: {
    tools: {
      rich_artifact: async () => ({
        content: "Canonical SDK artifact α.ts: verified native ownership\n".repeat(3072),
        data: data(0), truncated: false,
      }),
    },
    commands: {
      "rich-workflow": async (params, { session, state, push }) => {
        const action: unknown = params.arguments.startsWith("{") ? JSON.parse(params.arguments).action : params.arguments
        if (action !== "start" && action !== "advance") throw new Error("unsupported rich workflow action")
        const before = await state.read()
        const saved = before.entries.find(entry => entry.key === "advances")?.value
        if (action === "start" && saved !== undefined) throw new Error("workflow already started")
        if (action === "advance" && (typeof saved !== "number" || !Number.isSafeInteger(saved) || saved < 0)) throw new Error("workflow is absent")
        const advances = action === "start" ? 0 : Number(saved) + 1
        if (action === "start") {
          const result = await session.callTool("rich_artifact", {})
          if (result.is_error) throw new Error("canonical artifact failed")
        }
        const committed = await state.commit({ expected_revision: before.revision,
          mutations: [{ action: "set", key: "advances", value: advances }] })
        if (committed.outcome !== "committed") throw new Error("workflow state conflict")
        const revision = await push.publishPanel("artifact", data(advances))
        return { revision, advances }
      },
    },
  },
})

if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else await runPlugin(plugin)
}
