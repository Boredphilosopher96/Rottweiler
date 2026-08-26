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

  test("proves the production theme browser deterministically without character SVG", async () => {
    const firstDirectory = await mkdtemp(join(tmpdir(), "rottweiler-theme-browser-first-"))
    const secondDirectory = await mkdtemp(join(tmpdir(), "rottweiler-theme-browser-second-"))
    evidenceDirectory = firstDirectory
    const harness = resolve(import.meta.dir, "../scripts/tui-visual-harness.ts")
    const run = (directory: string) => Bun.spawn(
      ["bun", "run", harness, "theme-browser", directory],
      { cwd: resolve(import.meta.dir, ".."), stdout: "pipe", stderr: "pipe" },
    )
    const first = run(firstDirectory)
    const [firstExit, firstStderr] = await Promise.all([
      first.exited,
      new Response(first.stderr).text(),
    ])

    expect(firstStderr).toBe("")
    expect(firstExit).toBe(0)
    const second = run(secondDirectory)
    const [secondExit, secondStderr] = await Promise.all([
      second.exited,
      new Response(second.stderr).text(),
    ])
    expect(secondStderr).toBe("")
    expect(secondExit).toBe(0)

    for (const extension of ["txt", "ansi", "png", "json"]) {
      const firstArtifact = Bun.file(join(firstDirectory, `theme-browser.${extension}`))
      const secondArtifact = Bun.file(join(secondDirectory, `theme-browser.${extension}`))
      expect(await firstArtifact.exists()).toBeTrue()
      expect(await firstArtifact.arrayBuffer()).toEqual(await secondArtifact.arrayBuffer())
    }
    expect(await Bun.file(join(firstDirectory, "theme-browser.svg")).exists()).toBeFalse()
    const proof = await Bun.file(join(firstDirectory, "theme-browser.json")).json()
    expect(proof.assertions.every((assertion: { passed: boolean }) => assertion.passed)).toBeTrue()

    await rm(secondDirectory, { recursive: true, force: true })
  })
})
