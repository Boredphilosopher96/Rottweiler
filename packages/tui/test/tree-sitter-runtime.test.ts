import { afterEach, describe, expect, test } from "bun:test"
import { chmod, lstat, mkdir, mkdtemp, readFile, readdir, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { embeddedParserConfigurations, materializeTreeSitterRuntime } from "../src/tree-sitter-runtime"

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

  test("concurrent launches publish a complete parser catalog without temporary writers", async () => {
    root = await mkdtemp(join(tmpdir(), "rottweiler-tree-sitter-cache-test-"))
    process.env.ROTTWEILER_HOME = root
    const runtimes = await Promise.all(Array.from({ length: 3 }, () => materializeTreeSitterRuntime()))
    const runtime = runtimes[0]!
    expect(runtimes.every(({ root }) => root === runtime.root)).toBeTrue()
    expect(await readdir(join(root, "cache", "tree-sitter"))).toEqual([runtime.root.split("/").at(-1)!])
    for (const parser of embeddedParserConfigurations(runtime.assetsPath)) {
      const bytes = await readFile(parser.wasm)
      expect(Array.from(bytes.subarray(0, 4))).toEqual([0, 97, 115, 109])
      expect((await lstat(parser.wasm)).mode & 0o777).toBe(0o600)
      for (const query of parser.queries.highlights) expect((await readFile(query)).length).toBeGreaterThan(0)
    }
  })
})
