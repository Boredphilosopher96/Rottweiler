import { ClientAllocationOwner } from "../src/client-allocation"
import { chmod, mkdir, readFile, writeFile } from "node:fs/promises"
import { join } from "node:path"
import { tmpdir } from "node:os"
import { mkdtemp, rm } from "node:fs/promises"
import { afterEach, describe, expect, test } from "bun:test"

import {
  type AppClientState,
  MAX_RECYCLE_STATE_BYTES,
  parseTuiRecycleState,
  readTuiRecycleState,
  writeTuiRecycleState,
} from "../src/recycle-state"

const roots: string[] = []
const clientState: AppClientState = {
  schemaVersion: 4, child: null, parentComposer: null, interaction: null, sessionId: "session-local",
  composer: { content: "unfinished prompt", attachments: [], cursorOffset: 3, selection: { start: 1, end: 3 } },
  subagentDrafts: [], primaryView: "conversation", history: { following: false, anchor: { id: "37", offset: -2 } }, toolsScrollTop: 0,
  inputMode: "standard", focus: "composer", theme: "kennel", picker: null,
  tools: { selectedId: null, expanded: [] },
  transcript: { blocks: { selectedId: null, expanded: [] }, tools: [], reasoning: [] },
}

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { force: true, recursive: true })))
})

describe("TUI recycle state", () => {
  test("atomically hands a private draft and source anchor to one replacement", async () => {
    const root = await mkdtemp(join(tmpdir(), "rw-tui-recycle-"))
    roots.push(root)
    await chmod(root, 0o700)
    const path = join(root, "state.json")
    const state = clientState

    expect(writeTuiRecycleState(path, state)).toBe(true)
    expect(JSON.parse(await readFile(path, "utf8"))).toEqual(state)
    using allocation = new ClientAllocationOwner().reserve("decoding", 0)
    const decoded = readTuiRecycleState(path, allocation)
    expect(decoded?.state).toEqual(state)
    expect(JSON.parse(await readFile(path, "utf8"))).toEqual(state)
    decoded!.consume()
    expect(readTuiRecycleState(path, allocation)).toBeNull()
  })

  test("rejects malformed and permission-broad handoffs", async () => {
    expect(parseTuiRecycleState({ ...clientState, history: { following: false, anchor: { id: "-1", offset: 0 } } })).toBeNull()
    const root = await mkdtemp(join(tmpdir(), "rw-tui-recycle-"))
    roots.push(root)
    await chmod(root, 0o700)
    const broad = join(root, "broad.json")
    await writeFile(broad, '{"schemaVersion":1,"draft":"x","scrollTop":1}\n', { mode: 0o644 })
    await chmod(broad, 0o644)
    using allocation = new ClientAllocationOwner().reserve("decoding", 0)
    expect(readTuiRecycleState(broad, allocation)).toBeNull()

    const unsafeParent = join(root, "unsafe")
    await mkdir(unsafeParent, { mode: 0o755 })
    await chmod(unsafeParent, 0o755)
    expect(writeTuiRecycleState(join(unsafeParent, "state.json"), clientState)).toBe(false)
  })
  test("rejects secret routes, invalid editing offsets, malformed attachments and over-cap state", () => {
    expect(parseTuiRecycleState({ ...clientState, picker: {
      kind: "providerApiKey", anchored: false, query: "sk-secret", selectedId: null, scrollOffset: 0,
      modelProviderFilter: null, onboarding: false, themeBeforePreview: null,
    } })).toBeNull()
    expect(parseTuiRecycleState({ ...clientState, composer: { ...clientState.composer, cursorOffset: 999 } })).toBeNull()
    expect(parseTuiRecycleState({ ...clientState, composer: { ...clientState.composer, attachments: [{}] } })).toBeNull()
    expect(parseTuiRecycleState({ ...clientState, composer: {
      ...clientState.composer, content: "x".repeat(MAX_RECYCLE_STATE_BYTES),
    } }) === null).toBe(true)
    expect(parseTuiRecycleState({ ...clientState, tools: {
      selectedId: null, expanded: Array.from({ length: 4097 }, () => ({ id: "x", expanded: false })),
    } })).toBeNull()
  })

})

test("handoff rejects child ancestry substitution and refuses decoding before allocation", async () => {
  const { parseRecycleChild } = await import("../src/recycle-child")
  expect(parseRecycleChild({ type: "live", target: { session_id: "different", ancestry: [{ subagent_id: "a", session_id: "child" }] } }, "root")).toBeNull()
  expect(parseRecycleChild({ type: "live", target: { session_id: "root", ancestry: [{ subagent_id: "a", session_id: "root" }] } }, "root")).toBeNull()
  expect(parseRecycleChild({ type: "historical", target: { sessionId: "child", scope: { type: "descendant", root_session_id: "another-root",
    ancestry: [{ subagent_id: "a", session_id: "child", source_sequence: "2" }] } } }, "root")).toBeNull()
  const root = await mkdtemp(join(tmpdir(), "rw-handoff-admission-")); roots.push(root); await chmod(root, 0o700)
  const path = join(root, "state.json")
  expect(writeTuiRecycleState(path, clientState)).toBe(true)
  const allocations = new ClientAllocationOwner(undefined, 1024)
  using refused = allocations.reserve("decoding", 0)
  expect(() => readTuiRecycleState(path, refused)).toThrow("allocation admission")
  expect(allocations.usage.bytes).toBe(0)
  // Admission refusal does not consume the private draft.
  expect(JSON.parse(await readFile(path, "utf8"))).toEqual(clientState)
})

test("near-limit editing state uses the same writer, decoder and mounted transfer budget", async () => {
  const { createTestRenderer } = await import("@opentui/core/testing")
  const { createRottweilerApp } = await import("../src/app")
  const { emptySessionReader } = await import("./fixtures/history")
  const { recycleTuiIfNeeded, MAX_RECYCLE_PREPARED_BYTES } = await import("../src/recycle-state")
  const root = await mkdtemp(join(tmpdir(), "rw-large-handoff-")); roots.push(root); await chmod(root, 0o700)
  const path = join(root, "state.json"), allocations = new ClientAllocationOwner()
  const bytes = 5 * 1024 * 1024
  const state: AppClientState = { ...clientState, composer: { content: "x".repeat(bytes), attachments: [], cursorOffset: bytes, selection: null } }
  let exits = 0
  expect(recycleTuiIfNeeded({ allocations, observedBytes: 2, thresholdBytes: 1, path, capture: () => state, recycle: () => { exits++ } })).toBe(true)
  expect(exits).toBe(1)
  using decoding = allocations.reserve("decoding", 0)
  const decoded = readTuiRecycleState(path, decoding)
  expect(decoded?.state.composer.content.length).toBe(bytes)
  expect(decoding.bytes).toBeLessThanOrEqual(MAX_RECYCLE_PREPARED_BYTES)
  const setup = await createTestRenderer({ width: 40, height: 10, useThread: false })
  const app = createRottweilerApp(setup.renderer, { allocations, sessionId: state.sessionId, sessionReader: emptySessionReader })
  setup.renderer.root.add(app)
  try {
    expect(app.restoreRecycleState(decoded!.state)).toBe(true)
    expect(app.composer.value.length).toBe(bytes)
    decoded!.consume(); decoding.release()
    expect(allocations.usage.bytes).toBeLessThan(allocations.maximumBytes)
  } finally { app.destroy(); setup.renderer.destroy(); await Bun.sleep(0) }
  expect(allocations.usage.bytes).toBe(0)
})

test("a handoff that fits encoded bytes but exceeds prepared bytes never exits or replaces a draft", async () => {
  const { recycleTuiIfNeeded } = await import("../src/recycle-state")
  const root = await mkdtemp(join(tmpdir(), "rw-handoff-prepared-")); roots.push(root); await chmod(root, 0o700)
  const path = join(root, "state.json"), allocations = new ClientAllocationOwner()
  expect(writeTuiRecycleState(path, clientState)).toBe(true)
  const state: AppClientState = { ...clientState, composer: { content: "", attachments: [{ name: "large.txt", media_type: "text/plain", data: { type: "text", content: "x".repeat(7 * 1024 * 1024) } }], cursorOffset: 0, selection: null } }
  let exits = 0
  expect(recycleTuiIfNeeded({ allocations, observedBytes: 2, thresholdBytes: 1, path, capture: () => state, recycle: () => { exits++ } })).toBe(false)
  expect(exits).toBe(0)
  const overDraft: AppClientState = { ...clientState, composer: { content: "x".repeat(6 * 1024 * 1024), attachments: [], cursorOffset: 0, selection: null } }
  expect(recycleTuiIfNeeded({ allocations, observedBytes: 2, thresholdBytes: 1, path, capture: () => overDraft, recycle: () => { exits++ } })).toBe(false)
  expect(exits).toBe(0)
  expect(JSON.parse(await readFile(path, "utf8"))).toEqual(clientState)
  expect(allocations.usage.bytes).toBe(0)
})
