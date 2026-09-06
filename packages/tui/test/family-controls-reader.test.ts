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
