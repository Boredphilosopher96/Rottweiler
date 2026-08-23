import { readFile, readdir } from "node:fs/promises"
import { dirname, extname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"
import { spawnSync } from "node:child_process"

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const content = join(root, "src/content/docs")

const walk = async (directory) => (await Promise.all((await readdir(directory, { withFileTypes: true })).map(async (entry) => {
  const path = join(directory, entry.name)
  return entry.isDirectory() ? walk(path) : [path]
}))).flat()

const banned = [
  ["--line", "the deleted line-client flag"],
  ["sessions recent", "the deleted session-list alias"],
  ["npm install @rottweiler/plugin", "an unpublished package install"],
  ["cargo install rw-cli", "an incomplete product installation"],
  ["docs/spec/toon", "the old TOON path"],
  ["next release", "iteration language instead of the product surface"],
  ["unreleased", "iteration language instead of the product surface"],
  ["current main", "a source-track distinction"],
  ["current `main`", "a source-track distinction"],
  ["protocol 2", "an internal plugin protocol version"],
  ["protocol 1", "an internal client protocol version"],
  ["not published", "a package-publication staging note"],
]

const pages = (await walk(content)).filter((path) => [".md", ".mdx"].includes(extname(path)))
if (pages.length < 20) throw new Error(`expected at least 20 documentation pages, found ${pages.length}`)
for (const path of pages) {
  const source = await readFile(path, "utf8")
  for (const [needle, explanation] of banned) {
    if (source.toLowerCase().includes(needle.toLowerCase())) throw new Error(`${path} contains ${explanation}: ${needle}`)
  }
  if (!/^title:\s*.+$/m.test(source) || !/^description:\s*.+$/m.test(source)) {
    throw new Error(`${path} must declare title and description`)
  }
}

const prepared = spawnSync(process.execPath, [join(root, "scripts/prepare.mjs")], { encoding: "utf8" })
if (prepared.status !== 0) throw new Error(prepared.stderr || prepared.stdout)
const index = JSON.parse(await readFile(join(root, "public/docs-index.json"), "utf8"))
if (index.schema_version !== 1 || index.pages.length !== pages.length) throw new Error("agent index does not cover the content collection")
console.log(`checked ${pages.length} public documentation pages and ${index.pages.length} agent projections`)
