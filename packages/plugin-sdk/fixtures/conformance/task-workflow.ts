import { definePlugin, runPlugin } from "../../src/index.ts"

const fields = [{ kind: "text" as const, id: "phase", label: "Phase", path: [{ step: "field" as const, name: "phase" }] }]
const actions = [{ id: "complete", label: "Complete task", command: "task-workflow", arguments: { action: "complete" } }]

const plugin = definePlugin({
  manifest: {
    name: "task-workflow", version: "1", protocol: 3,
    capabilities: {
      commands: [{ name: "task-workflow", description: "Resume a durable task", allowed_tools: ["read", "task_summary"] }],
      tools: [{ name: "task_summary", description: "Read the durable task", schema: { type: "object" }, caps: [] }],
      push: ["session/tool_call", "extension/state_read", "extension/state_commit", "ui/publish_panel"],
      ui: [
        { surface: "panel", id: "task", title: "Persistent task", fields, actions },
        { surface: "tool", id: "summary", tool_name: "task_summary", title: "Task summary", fields, actions },
      ],
    },
  },
  handlers: {
    tools: {
      task_summary: async (_params, { state }) => {
        const snapshot = await state.read()
        const task = snapshot.entries.find(entry => entry.key === "task")?.value
        if (task === undefined) throw new Error("task is absent")
        return { content: "Durable task summary", data: task, truncated: false }
      },
    },
    commands: {
      "task-workflow": async (params, { session, state, push }) => {
        const action = params.arguments.startsWith("{") ? JSON.parse(params.arguments).action : params.arguments
        const before = await state.read()
        const task = before.entries.find(entry => entry.key === "task")?.value as { phase: string, read_invocation: string } | undefined
        if (action === "complete") {
          if (!task) throw new Error("task is absent")
          const result = await state.commit({ expected_revision: before.revision, mutations: [{ action: "set", key: "task", value: { ...task, phase: "done" } }] })
          if (result.outcome !== "committed") throw new Error("completion conflict")
          return { completed: true }
        }
        if (!task) {
          if (action !== "start") throw new Error("task must be started explicitly")
          const read = await session.callTool("read", { path: "broker.txt" })
          if (read.is_error) throw new Error("task input read failed")
          const result = await state.commit({ expected_revision: before.revision, mutations: [{ action: "set", key: "task", value: { phase: "ready", read_invocation: read.invocation_id } }] })
          if (result.outcome !== "committed") throw new Error("start conflict")
        }
        const current = await state.read()
        const value = current.entries.find(entry => entry.key === "task")?.value
        if (value === undefined) throw new Error("committed task is absent")
        const summary = await session.callTool("task_summary", {})
        if (summary.is_error) throw new Error("task summary failed")
        if (action === "summary") return { summary: true }
        const revision = await push.publishPanel("task", value)
        return { revision }
      },
    },
  },
})

if (import.meta.main) {
  if (process.argv.includes("--manifest")) console.log(JSON.stringify(plugin.manifest))
  else await runPlugin(plugin)
}
