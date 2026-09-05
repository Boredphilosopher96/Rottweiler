import { expect, test } from "bun:test"
import { TuiEngineRuntime } from "../../src/runtime"
import type { ClientCommand, CommandReply } from "../../src/protocol"
import { MemoryFiles, SwitchingClient, TestApp, waitFor } from "./fixtures"

test("session task capability reads a child without changing the driver and rejects a foreign reply", async () => {
  const client = new SwitchingClient()
  const post = client.postCommand.bind(client)
  let replySession = "child"
  client.postCommand = async (command: ClientCommand): Promise<CommandReply> => {
    if (command.type !== "get_todos") return post(command)
    client.commands.push(command)
    return { type: "read", outcome: { type: "accepted" }, events: [{ type: "todos_read",
      meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: replySession,
      result: { type: "ready", todos: { through: "12", snapshot: { items: [] } } },
    }] }
  }
  const runtime = new TuiEngineRuntime({ socketPath: "/private/engine.sock", bootstrapToken: "secret", sessionId: "parent", lastSeenSequence: null, lastSeenFile: null, replayMode: false }, client, new MemoryFiles())
  const app = new TestApp()
  runtime.bind(app)
  const running = runtime.start()
  try {
    await waitFor(() => client.commands.some(command => command.type === "list_commands"))
    expect(await runtime.sessionReader.todos("child", new AbortController().signal)).toMatchObject({ type: "ready", todos: { through: "12" } })
    expect(app.sessionId).toBe("parent")
    expect(client.commands.at(-1)).toMatchObject({ type: "get_todos", session_id: "child" })
    replySession = "foreign"
    await expect(runtime.sessionReader.todos("child", new AbortController().signal)).rejects.toThrow("session-bound result")
  } finally { await runtime.stop(); await running }
})
