import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"

import { afterEach, describe, expect, test } from "bun:test"

describe("TUI visual evidence", () => {
  let evidenceDirectory: string | null = null

  afterEach(async () => {
    if (evidenceDirectory !== null) {
      await rm(evidenceDirectory, { recursive: true, force: true })
      evidenceDirectory = null
    }
  })

  test("emits terminal-native ANSI evidence and no character SVG", async () => {
    evidenceDirectory = await mkdtemp(join(tmpdir(), "rottweiler-svg-test-"))
    const harness = resolve(import.meta.dir, "../scripts/tui-visual-harness.ts")
    const process = Bun.spawn(["bun", "run", harness, "conversation", evidenceDirectory], {
      cwd: resolve(import.meta.dir, ".."),
      stdout: "pipe",
      stderr: "pipe",
    })
    const [exitCode, stderr] = await Promise.all([
      process.exited,
      new Response(process.stderr).text(),
    ])

    expect(stderr).toBe("")
    expect(exitCode).toBe(0)
    const ansiPath = join(evidenceDirectory, "conversation.ansi")
    const pngPath = join(evidenceDirectory, "conversation.png")
    const svgPath = join(evidenceDirectory, "conversation.svg")
    expect(await Bun.file(ansiPath).exists()).toBeTrue()
    expect(await Bun.file(pngPath).exists()).toBeTrue()
    expect(await Bun.file(svgPath).exists()).toBeFalse()
    const ansi = await Bun.file(ansiPath).text()
    const visible = ansi.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "")
    expect(visible).toContain("reasoning")
    expect(visible).toContain("edit  core/cursor.rs")
  })
})
