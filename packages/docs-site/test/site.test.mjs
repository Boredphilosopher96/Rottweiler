import assert from "node:assert/strict"
import { readFile } from "node:fs/promises"
import { dirname, join, resolve } from "node:path"
import { spawnSync } from "node:child_process"
import { test } from "node:test"
import { fileURLToPath } from "node:url"

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..")

const prepare = () => {
  const result = spawnSync(process.execPath, [join(root, "scripts/prepare.mjs")], { encoding: "utf8" })
  assert.equal(result.status, 0, result.stderr || result.stdout)
}

test("the agent index is complete, unique, and source-owned", async () => {
  prepare()
  const document = JSON.parse(await readFile(join(root, "public/docs-index.json"), "utf8"))
  assert.equal(document.schema_version, 1)
  assert.ok(document.pages.length >= 20)
  assert.equal(new Set(document.pages.map((page) => page.url)).size, document.pages.length)
  for (const page of document.pages) {
    assert.ok(["product", "contributing"].includes(page.section))
    assert.ok(page.raw_url.startsWith("https://boredphilosopher96.github.io/Rottweiler/raw/"))
    assert.ok(page.source_owners.length > 0)
  }
})

test("machine artifacts remain exact projections of repository owners", async () => {
  prepare()
  const pairs = [
    ["public/generated/plugin/schema.json", "../plugin-sdk/fixtures/wire/protocol-3.schema.json"],
    ["public/generated/plugin/wire-example.json", "../plugin-sdk/fixtures/wire/protocol-3.json"],
    ["public/generated/client/client-command.schema.json", "../../protocol/schema/client-command.schema.json"],
    ["public/generated/session/event-envelope.schema.json", "../../protocol/session-event-envelope.schema.json"],
  ]
  for (const [projection, owner] of pairs) {
    assert.equal(await readFile(join(root, projection), "utf8"), await readFile(resolve(root, owner), "utf8"))
  }
})

test("the compact agent map presents one product truth", async () => {
  prepare()
  const llms = await readFile(join(root, "public/llms.txt"), "utf8")
  assert.match(llms, /complete product documentation/i)
  assert.match(llms, /## Product documentation/)
  assert.doesNotMatch(llms, /unreleased|current main|protocol [123]/i)
  assert.match(llms, /docs-index\.json/)
})

test("agent-facing Markdown contains content instead of MDX presentation code", async () => {
  prepare()
  const overview = await readFile(join(root, "public/raw/product/overview.md"), "utf8")
  assert.match(overview, /Productive in a terminal/)
  assert.doesNotMatch(overview, /^import /m)
  assert.doesNotMatch(overview, /<ProductHero|<Card/)
  assert.doesNotMatch(overview, /\{stableTag\}/)
})

test("released targets are projected from the signed update owner", async () => {
  prepare()
  const signed = JSON.parse(await readFile(resolve(root, "../../release/update/stable.spec.json"), "utf8"))
  const generated = await import(`${new URL("../src/generated/product.mjs", import.meta.url)}?test=${Date.now()}`)
  assert.deepEqual(
    Object.fromEntries(generated.stableTargets.map((target) => [target.id, target.archiveUrl])),
    Object.fromEntries(Object.entries(signed.targets).map(([id, target]) => [id, target.url])),
  )
  const raw = await readFile(join(root, "public/raw/product/reference/platforms-and-releases.md"), "utf8")
  for (const target of generated.stableTargets) assert.match(raw, new RegExp(target.id))
  assert.doesNotMatch(raw, /<StableTargets/)
})
