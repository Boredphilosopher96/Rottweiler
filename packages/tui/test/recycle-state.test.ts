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
  schemaVersion: 3, sessionId: "session-local",
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
    expect(readTuiRecycleState(path)).toEqual(state)
    expect(readTuiRecycleState(path)).toBeNull()
  })

  test("rejects malformed and permission-broad handoffs", async () => {
    expect(parseTuiRecycleState({ ...clientState, history: { following: false, anchor: { id: "-1", offset: 0 } } })).toBeNull()
    const root = await mkdtemp(join(tmpdir(), "rw-tui-recycle-"))
    roots.push(root)
    await chmod(root, 0o700)
    const broad = join(root, "broad.json")
    await writeFile(broad, '{"schemaVersion":1,"draft":"x","scrollTop":1}\n', { mode: 0o644 })
    await chmod(broad, 0o644)
    expect(readTuiRecycleState(broad)).toBeNull()

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
