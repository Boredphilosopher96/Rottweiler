import { expect, test } from "bun:test"
import { hostStateContext } from "../src/host-state"
import { RPC_METHODS, type JsonValue } from "../src/generated/protocol-3"

const snapshot = {
  revision: "7", entries: [{ key: "review/count", value: 2 }],
  acknowledged: null, delivery_start: null,
} satisfies JsonValue

test("state commit preserves explicit CAS and host-bound identity", async () => {
  const calls: [string, JsonValue][] = []
  const { state } = hostStateContext(async (method, params) => {
    calls.push([method, params])
    if (method === RPC_METHODS.stateRead) return snapshot
    return { outcome: "conflict", actual_revision: "8" }
  })
  expect(await state.read()).toEqual(snapshot)
  expect(await state.commit({ expected_revision: "7", mutations: [{ action: "delete", key: "review/count" }] }))
    .toEqual({ outcome: "conflict", actual_revision: "8" })
  expect(calls).toEqual([
    [RPC_METHODS.stateRead, {}],
    [RPC_METHODS.stateCommit, { expected_revision: "7", mutations: [{ action: "delete", key: "review/count" }], acknowledged: null }],
  ])
})

test("state callers cannot acknowledge delivery or attach routing identities", async () => {
  let calls = 0
  const { state } = hostStateContext(async () => { calls++; return null })
  const forged = { expected_revision: null, mutations: [], acknowledged: { session_id: "session", sequence: "9" } }
  await expect(state.commit(forged)).rejects.toThrow("host-owned")
  const crossNamespace = { expected_revision: null, mutations: [], plugin_id: "another" }
  await expect(state.commit(crossNamespace)).rejects.toThrow("invalid extension state")
  expect(calls).toBe(0)
})

test("state and session replies reject missing nullable fields and additive data", async () => {
  for (const value of [
    { revision: "7", entries: [], acknowledged: null },
    { ...snapshot, credentials: "must not cross" },
    { ...snapshot, revision: 7 },
  ]) {
    await expect(hostStateContext(async () => value).state.read()).rejects.toThrow("invalid host")
  }
  await expect(hostStateContext(async () => ({ outcome: "committed", revision: "9", ignored: true }))
    .state.commit({ expected_revision: null, mutations: [{ action: "set", key: "a", value: 1 }] }))
    .rejects.toThrow("invalid host")
  const session = {
    session_id: "session", title: null, mode_id: "execute", model_alias: "default",
    active_turn: null, queued_messages: 0, last_sequence: null,
  }
  const calls: [string, JsonValue][] = []
  expect(await hostStateContext(async (method, params) => { calls.push([method, params]); return session }).session.query()).toEqual(session)
  expect(calls).toEqual([[RPC_METHODS.sessionQuery, {}]])
  await expect(hostStateContext(async () => ({ ...session, provider_config: {} })).session.query()).rejects.toThrow("invalid host")
})
