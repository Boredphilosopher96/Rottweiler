import { join, resolve } from "node:path"

import { afterEach, describe, expect, test } from "bun:test"
import { TestProcessScope } from "./support/owned-process"

const HARNESS_DEADLINE_MS = 20_000
// Two serial renderers plus artifact comparison; product latency has separate gates.
const PROOF_DEADLINE_MS = 60_000

describe("TUI visual evidence", () => {
  const owners: TestProcessScope[] = []

  // A 20s child deadline plus bounded group teardown and supervisor startup.
  afterEach(async () => {
    for (const owner of owners.splice(0)) {
      await owner.close()
    }
  }, 40_000)

  async function render(scenario: string): Promise<string> {
    const owner = await TestProcessScope.create(`rottweiler-${scenario}-test-`)
    owners.push(owner)
    const directory = owner.directory
    const { code, stderr } = await owner.run(
      [process.execPath, "run", resolve(import.meta.dir, "../scripts/tui-visual-harness.ts"), scenario, directory],
      { cwd: resolve(import.meta.dir, ".."), timeoutMs: HARNESS_DEADLINE_MS },
    )
    expect({ code, stderr }).toEqual({ code: 0, stderr: "" })
    return directory
  }

  test("emits terminal-native ANSI evidence and no character SVG", async () => {
    const directory = await render("conversation")
    expect(await Bun.file(join(directory, "conversation.ansi")).exists()).toBeTrue()
    expect(await Bun.file(join(directory, "conversation.png")).exists()).toBeTrue()
    expect(await Bun.file(join(directory, "conversation.svg")).exists()).toBeFalse()
    const ansi = await Bun.file(join(directory, "conversation.ansi")).text()
    const visible = ansi.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "")
    expect(visible).toContain("reasoning")
    expect(visible).toContain("edit  core/cursor.rs")
  }, PROOF_DEADLINE_MS)

  for (const scenario of ["theme-browser", "settings-browser", "mcp-browser", "session-review"]) {
    test(`proves the production ${scenario} deterministically at each supported size`, async () => {
      const first = await render(scenario)
      const second = await render(scenario)
      const artifacts = scenario === "theme-browser" ? [scenario] : [scenario, `${scenario}-narrow`]
      for (const artifact of artifacts) {
        for (const extension of ["txt", "ansi", "png", "json"]) {
          const firstArtifact = Bun.file(join(first, `${artifact}.${extension}`))
          const secondArtifact = Bun.file(join(second, `${artifact}.${extension}`))
          expect(await firstArtifact.exists()).toBeTrue()
          expect(await secondArtifact.exists()).toBeTrue()
          expect(await firstArtifact.arrayBuffer()).toEqual(await secondArtifact.arrayBuffer())
        }
        expect(await Bun.file(join(first, `${artifact}.svg`)).exists()).toBeFalse()
        const proof = await Bun.file(join(first, `${artifact}.json`)).json()
        expect(proof.assertions.length).toBeGreaterThan(0)
        expect(proof.assertions.every((assertion: { passed: boolean }) => assertion.passed)).toBeTrue()
      }
    }, PROOF_DEADLINE_MS)
  }
})
