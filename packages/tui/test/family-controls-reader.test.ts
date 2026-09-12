import { expect, test } from "bun:test"
import { familyControlsReader } from "../src/family-controls-reader"
import { PROTOCOL_VERSION, type CommandReply, type EngineEvent } from "../src/protocol"
import { createInitialState, engineEvent, reduceRottweilerState } from "../src/state"

const meta = () => ({ protocol_version: PROTOCOL_VERSION, client_id: "client", request_id: "read" })
const ack = { ...meta(), emitted_at: "2026-01-01T00:00:00Z" }
const target = { session_id: "child", ancestry: [{ subagent_id: "agent", session_id: "child" }] }
const signal = new AbortController().signal
const allocation = { admit() {} }

test("family readers reject foreign root and changed live ancestry reply identities", async () => {
  let event: EngineEvent = { type: "family_controls_ready", meta: ack, session_id: "other", snapshot: { revision: "1", children: [] } }
  const reader = familyControlsReader(async command => {
    if (command.type === "read_family_controls") expect(command.after_revision).toBeNull()
    return { type: "read", outcome: { type: "accepted" }, events: [event] } satisfies CommandReply
  }, meta)
  await expect(reader.watch("root", null, signal, allocation)).rejects.toThrow("root-bound")
  event = { type: "child_controls_ready", meta: ack, session_id: "root", target: { ...target, ancestry: [{ subagent_id: "different", session_id: "child" }] }, snapshot: { revision: "2", snapshot: { through: "7", controls: { questions: [], approvals: [], pending_plan: null } } } }
  await expect(reader.child("root", target, signal, allocation)).rejects.toThrow("target-bound")
})

test("source-referenced input commits advance the cursor without duplicating accepted body storage", () => {
  const initial = createInitialState()
  const accepted = reduceRottweilerState(initial, engineEvent({ type: "user_message_accepted", meta: { protocol_version: PROTOCOL_VERSION, session_id: "root", sequence_id: "1", emitted_at: ack.emitted_at }, agent_turn: "1", content: "original", attachments: [] }))
  const committed = reduceRottweilerState(accepted, engineEvent({ type: "conversation_input_committed", meta: { protocol_version: PROTOCOL_VERSION, session_id: "root", sequence_id: "2", emitted_at: ack.emitted_at }, agent_turn: "1", accepted_source: "1", selection: { type: "transformed", text: "normalized" } }))
  expect(committed.lastSequence).toBe("2")
  expect(committed.hasActivity).toBe(true)
  expect(committed.streamingTail).toBe(accepted.streamingTail)
  expect("transcript" in committed).toBe(false)
})

test("child scalar and scope replies retain full root and ancestry correlation", async () => {
  const reader = familyControlsReader(async command => {
    const common = { meta: ack, session_id: "root", target: { ...target, ancestry: [{ subagent_id: "foreign", session_id: "child" }] } }
    if (command.type === "resolve_child_read_scope") return { type: "read", outcome: { type: "accepted" }, events: [{ ...common, type: "child_read_scope_ready", result: { type: "ready", scope: { type: "session" } } }] }
    return { type: "read", outcome: { type: "accepted" }, events: [{ ...common, type: "child_state_ready", snapshot: {
      through: "1", driver_client_id: null, title: null, model_alias: "main", provider: null, thinking: "off", mode_id: "execute",
      active_turn: null, completed_turns: "0", shell: null, compaction: null, plugin_statuses: [], queued_messages: [], budget: null,
    } }] }
  }, meta)
  await expect(reader.state("root", target, signal, allocation)).rejects.toThrow("target-bound")
  await expect(reader.scope("root", target, signal, allocation)).rejects.toThrow("target-bound")
})

test("source-referenced context commits advance the durable cursor without creating a display body", () => {
  const initial = createInitialState()
  const state = reduceRottweilerState(initial, engineEvent({ type: "conversation_context_committed",
    meta: { protocol_version: PROTOCOL_VERSION, session_id: "root", sequence_id: "5", emitted_at: ack.emitted_at }, agent_turn: "1",
    selection: { type: "continuation" },
  }))
  expect(state.lastSequence).toBe("5")
  expect(state.hasActivity).toBe(true)
  expect(state.streamingTail).toBeNull()
})
