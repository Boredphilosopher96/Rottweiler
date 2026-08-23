import { afterEach, describe, expect, test } from "bun:test"
import { readFile, rm } from "node:fs/promises"
import { join } from "node:path"
import { buildSite, renderMarkdown } from "../build"

const output = join(import.meta.dir, "../dist")

afterEach(async () => {
  await rm(output, { recursive: true, force: true })
})

describe("plugin protocol documentation site", () => {
  test("renders protocol headings and escapes untrusted markup", () => {
    const rendered = renderMarkdown("# Title\n\n## Wire <unsafe>\n\nUse `call`.\n")
    expect(rendered.navigation).toContain("Wire &lt;unsafe&gt;")
    expect(rendered.body).toContain("<code>call</code>")
    expect(rendered.body).not.toContain("<unsafe>")
  })

  test("builds a deterministic accessible site from the frozen sources", async () => {
    await buildSite()
    const first = await readFile(join(output, "index.html"), "utf8")
    await buildSite()
    const second = await readFile(join(output, "index.html"), "utf8")
    expect(second).toBe(first)
    expect(first).toContain("Rottweiler plugin protocol 2")
    expect(first).toContain('aria-label="Filter protocol reference"')
    expect(first).toContain("provider/complete")
    expect(first).toContain("protocol-2.schema.json")
    expect(JSON.parse(await readFile(join(output, "protocol-2.schema.json"), "utf8"))).toBeTruthy()
  })
})
