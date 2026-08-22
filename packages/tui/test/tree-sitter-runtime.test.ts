import { afterEach, describe, expect, test } from "bun:test"
import { chmod, lstat, mkdir, mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { materializeTreeSitterRuntime } from "../src/tree-sitter-runtime"

describe("Tree-sitter runtime cache", () => {
  let root: string | undefined
  const originalHome = process.env.ROTTWEILER_HOME

  afterEach(async () => {
    if (originalHome === undefined) delete process.env.ROTTWEILER_HOME
    else process.env.ROTTWEILER_HOME = originalHome
    if (root !== undefined) await rm(root, { recursive: true, force: true })
    root = undefined
  })

  test("reuses one private content-addressed runtime across launches", async () => {
    root = await mkdtemp(join(tmpdir(), "rottweiler-tree-sitter-cache-test-"))
    process.env.ROTTWEILER_HOME = root

    const first = await materializeTreeSitterRuntime()
    const second = await materializeTreeSitterRuntime()

    expect(second.root).toBe(first.root)
    expect((await lstat(first.root)).mode & 0o777).toBe(0o700)
    expect((await lstat(join(first.root, ".complete"))).mode & 0o777).toBe(0o600)
    await first.cleanup()
    expect((await lstat(first.workerPath)).isFile()).toBeTrue()
  })

  test("refuses a cache parent that is not owner-private", async () => {
    root = await mkdtemp(join(tmpdir(), "rottweiler-tree-sitter-cache-test-"))
    process.env.ROTTWEILER_HOME = root
    const cache = join(root, "cache", "tree-sitter")
    await mkdir(cache, { recursive: true })
    await chmod(cache, 0o755)

    await expect(materializeTreeSitterRuntime()).rejects.toThrow("is not private")
  })
})
