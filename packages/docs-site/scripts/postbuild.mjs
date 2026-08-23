import { lstat, readdir, readFile, writeFile } from "node:fs/promises"
import { dirname, join, relative, resolve, sep } from "node:path"
import { fileURLToPath } from "node:url"

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const dist = join(root, "dist")

const walk = async (directory) => {
  const entries = await readdir(directory, { withFileTypes: true })
  const files = []
  for (const entry of entries.sort((left, right) => left.name.localeCompare(right.name))) {
    const path = join(directory, entry.name)
    const metadata = await lstat(path)
    if (metadata.isSymbolicLink()) throw new Error(`site output must not contain symlinks: ${path}`)
    if (metadata.isDirectory()) files.push(...await walk(path))
    else files.push(relative(dist, path).split(sep).join("/"))
  }
  return files
}

for (const required of ["index.html", "docs/index.html", "llms.txt", "llms-full.txt", "docs-index.json", "generated/plugin/schema.json"]) {
  await readFile(join(dist, required))
}

const files = (await walk(dist)).sort()
if (files.some((file) => file === "updates" || file.startsWith("updates/"))) {
  throw new Error("documentation output must never own updates")
}
await writeFile(join(dist, ".rottweiler-docs-manifest.json"), JSON.stringify({ schema_version: 1, files }, null, 2) + "\n")
