import { expect, test } from "bun:test"
import { access, mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { main } from "../src/index"
import { SOURCE_BUNDLE_FORMAT, SOURCE_HOST_ABI } from "../src/protocol"

test("publishes one valid semantic ABI and bundle format", () => {
  expect(SOURCE_HOST_ABI).toBeGreaterThan(0)
  expect(SOURCE_BUNDLE_FORMAT).toMatch(/^[a-z0-9-]+$/u)
})

test("rejects a dynamic import before publishing a bundle", async () => {
  const root = await mkdtemp(join(tmpdir(), "rottweiler-plugin-host-test-"))
  try {
    await mkdir(join(root, "src"))
    await writeFile(join(root, "src/index.ts"), "const name = './child.ts'; export const plugin = import(name)\n")
    await writeFile(join(root, "src/child.ts"), "export const value = 1\n")
    await expect(main(["graph", root, join(root, "src/index.ts")])).rejects.toThrow("dynamic import")
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test("graph discovery never executes plugin top-level code", async () => {
  const root = await mkdtemp(join(tmpdir(), "rottweiler-plugin-host-inert-"))
  const marker = join(root, "executed")
  try {
    await mkdir(join(root, "src"))
    await writeFile(join(root, "package.json"), '{"type":"module"}\n')
    await writeFile(
      join(root, "src/index.ts"),
      `await Bun.write(${JSON.stringify(marker)}, "executed"); export const plugin = {}\n`,
    )
    await main(["graph", root, join(root, "src/index.ts")])
    await expect(access(marker)).rejects.toThrow()
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test("rejects source graph symlinks before publishing identity", async () => {
  const root = await mkdtemp(join(tmpdir(), "rottweiler-plugin-host-symlink-"))
  try {
    await mkdir(join(root, "src"))
    await writeFile(join(root, "package.json"), '{"type":"module"}\n')
    await writeFile(join(root, "src/target.ts"), "export const value = 1\n")
    await symlink("target.ts", join(root, "src/link.ts"))
    await writeFile(
      join(root, "src/index.ts"),
      "import { value } from './link.ts'; export const plugin = { value }\n",
    )
    await expect(main(["graph", root, join(root, "src/index.ts")])).rejects.toThrow("symlink")
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})
