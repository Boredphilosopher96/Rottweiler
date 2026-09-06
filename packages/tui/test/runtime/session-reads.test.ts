import { directSessionRead, descendantSessionRead } from "../../src/session-reader"
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
    const target = descendantSessionRead(directSessionRead("parent"), { session_id: "child", subagent_id: "agent", source_sequence: "9" })
    expect(await runtime.sessionReader.todos(target, new AbortController().signal)).toMatchObject({ type: "ready", todos: { through: "12" } })
    expect(app.sessionId).toBe("parent")
    expect(client.commands.at(-1)).toMatchObject({ type: "get_todos", session_id: "child", scope: target.scope })
    replySession = "foreign"
    await expect(runtime.sessionReader.todos(directSessionRead("child"), new AbortController().signal)).rejects.toThrow("session-bound result")
  } finally { await runtime.stop(); await running }
})


test.each(["uiCatalog", "uiPanels"] as const)("%s reads remain session-bound direct replies", async method => {
  const client = new SwitchingClient()
  const post = client.postCommand.bind(client)
  let foreign = false
  client.postCommand = async (command: ClientCommand): Promise<CommandReply> => {
    if (command.type !== "get_ui_catalog" && command.type !== "get_ui_panels") return post(command)
    client.commands.push(command)
    const base = { meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: foreign ? "foreign" : command.session_id }
    return { type: "read", outcome: { type: "accepted" }, events: [command.type === "get_ui_catalog"
      ? { ...base, type: "ui_catalog_ready", catalog: { entries: [] } }
      : { ...base, type: "ui_panels_ready", panels: { panels: [] } }] }
  }
  const runtime = new TuiEngineRuntime({ socketPath: "/private/engine.sock", bootstrapToken: "secret", sessionId: "parent", lastSeenSequence: null, lastSeenFile: null, replayMode: false }, client, new MemoryFiles())
  const app = new TestApp()
  runtime.bind(app)
  const running = runtime.start()
  try {
    await waitFor(() => client.commands.some(command => command.type === "list_commands"))
    expect(await runtime.sessionReader[method]("parent", new AbortController().signal)).toEqual(method === "uiCatalog" ? { entries: [] } : { panels: [] })
    expect(app.sessionId).toBe("parent")
    foreign = true
    await expect(runtime.sessionReader[method]("parent", new AbortController().signal)).rejects.toThrow("session-bound result")
  } finally { await runtime.stop(); await running }
})

test("live tail reads retain ancestry, allocation and reply session identity", async () => {
  const client = new SwitchingClient()
  const post = client.postCommand.bind(client)
  let foreign = false
  let admitted = 0
  client.postCommand = async (command: ClientCommand, _signal?: AbortSignal, allocation?: import("../../src/transport/reply-allocation").ReplyAllocation): Promise<CommandReply> => {
    if (command.type !== "read_transcript_tail") return post(command)
    client.commands.push(command)
    allocation?.admit(4096)
    return { type: "read", outcome: { type: "accepted" }, events: [{
      type: "transcript_tail_ready", meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" },
      session_id: foreign ? "foreign" : command.session_id, result: { type: "catching_up", through: "10", target: "20" },
    }] }
  }
  const runtime = new TuiEngineRuntime({ socketPath: "/private/engine.sock", bootstrapToken: "secret", sessionId: "parent", lastSeenSequence: null, lastSeenFile: null, replayMode: false }, client, new MemoryFiles())
  const app = new TestApp()
  runtime.bind(app)
  const running = runtime.start()
  try {
    await waitFor(() => client.commands.some(command => command.type === "list_commands"))
    const target = descendantSessionRead(directSessionRead("parent"), { session_id: "child", subagent_id: "agent", source_sequence: "9" })
    const read = { expected: null, part: { type: "text" as const }, max_items: 1, max_bytes: 524288 }
    const allocation = { admit(bytes: number) { admitted = bytes } }
    const signal = new AbortController().signal
    expect(await runtime.sessionReader.tail(target, read, signal, allocation)).toEqual({ type: "catching_up", through: "10", target: "20" })
    expect(admitted).toBe(4096)
    expect(client.commands.at(-1)).toMatchObject({ type: "read_transcript_tail", session_id: "child", scope: target.scope })
    expect(app.sessionId).toBe("parent")
    foreign = true
    await expect(runtime.sessionReader.tail(target, read, signal, allocation)).rejects.toThrow("session-bound result")
  } finally { await runtime.stop(); await running }
})

test("active child reads preserve source ancestry and the caller's decoding admission", async () => {
  const client = new SwitchingClient(), post = client.postCommand.bind(client)
  let foreign = false, admitted = 0
  client.postCommand = async (command: ClientCommand, _signal?: AbortSignal, allocation?: import("../../src/transport/reply-allocation").ReplyAllocation): Promise<CommandReply> => {
    if (command.type !== "read_session_children") return post(command)
    client.commands.push(command)
    allocation?.admit(2048)
    return { type: "read", outcome: { type: "accepted" }, events: [{ type: "session_children_ready",
      meta: { ...command.meta, emitted_at: "2026-01-01T00:00:00Z" }, session_id: foreign ? "foreign" : command.session_id,
      result: { type: "ready", snapshot: { through: "10", children: [] } } }] }
  }
  const runtime = new TuiEngineRuntime({ socketPath: "/private/engine.sock", bootstrapToken: "secret", sessionId: "parent", lastSeenSequence: null, lastSeenFile: null, replayMode: false }, client, new MemoryFiles())
  const app = new TestApp(); runtime.bind(app)
  const running = runtime.start()
  try {
    await waitFor(() => client.commands.some(command => command.type === "list_commands"))
    const target = descendantSessionRead(directSessionRead("parent"), { session_id: "child", subagent_id: "agent", source_sequence: "9" })
    const signal = new AbortController().signal, allocation = { admit(bytes: number) { admitted = bytes } }
    expect(await runtime.sessionReader.children(target, signal, allocation)).toMatchObject({ type: "ready", snapshot: { through: "10" } })
    expect(admitted).toBe(2048)
    expect(client.commands.at(-1)).toMatchObject({ type: "read_session_children", session_id: "child", scope: target.scope })
    foreign = true
    await expect(runtime.sessionReader.children(target, signal, allocation)).rejects.toThrow("session-bound result")
  } finally { await runtime.stop(); await running }
})
