import { chmod, mkdir, readFile, writeFile } from "node:fs/promises"
import { join } from "node:path"
import { tmpdir } from "node:os"
import { mkdtemp, rm } from "node:fs/promises"
import { afterEach, describe, expect, test } from "bun:test"

import {
  parseTuiRecycleState,
  readTuiRecycleState,
  writeTuiRecycleState,
} from "../src/recycle-state"

const roots: string[] = []

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { force: true, recursive: true })))
})

describe("TUI recycle state", () => {
  test("atomically hands a private draft and scroll offset to one replacement", async () => {
    const root = await mkdtemp(join(tmpdir(), "rw-tui-recycle-"))
    roots.push(root)
    await chmod(root, 0o700)
    const path = join(root, "state.json")
    const state = { schemaVersion: 1 as const, draft: "unfinished prompt", scrollTop: 37 }

    expect(writeTuiRecycleState(path, state)).toBe(true)
    expect(JSON.parse(await readFile(path, "utf8"))).toEqual(state)
    expect(readTuiRecycleState(path)).toEqual(state)
    expect(readTuiRecycleState(path)).toBeNull()
  })

  test("rejects malformed and permission-broad handoffs", async () => {
    expect(parseTuiRecycleState({ schemaVersion: 1, draft: "x", scrollTop: -1 })).toBeNull()
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
    expect(writeTuiRecycleState(join(unsafeParent, "state.json"), {
      schemaVersion: 1,
      draft: "x",
      scrollTop: 1,
    })).toBe(false)
  })
})
