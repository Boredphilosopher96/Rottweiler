import { cp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises"
import { dirname, extname, join, relative, resolve, sep } from "node:path"
import { fileURLToPath } from "node:url"
import { referenceSources } from "../src/data/reference-sources.mjs"
import { stableTag, stableTargets, stableVersion } from "../src/data/product.mjs"

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const repoRoot = resolve(packageRoot, "../..")
const contentRoot = join(packageRoot, "src/content/docs")
const publicRoot = join(packageRoot, "public")
const generatedSourceRoot = join(packageRoot, "src/generated")
const baseUrl = "https://boredphilosopher96.github.io/Rottweiler"
const projectTokens = (value) => value
  .replaceAll("{{stable_tag}}", stableTag)
  .replaceAll("{{stable_version}}", stableVersion)
  .replaceAll("{stableTag}", stableTag)

const stableTargetsMarkdown = [
  "| Target | Operating system | Signed archive |",
  "|---|---|---|",
  ...stableTargets.map((target) => `| \`${target.id}\` | ${target.operatingSystem} (${target.machine}) | [Download](${target.archiveUrl}) |`),
].join("\n")

const walk = async (directory) => {
  const entries = await readdir(directory, { withFileTypes: true })
  const paths = []
  for (const entry of entries.sort((left, right) => left.name.localeCompare(right.name))) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) paths.push(...await walk(path))
    else paths.push(path)
  }
  return paths
}

const stripFrontmatter = (source) => {
  const normalized = source.replaceAll("\r\n", "\n")
  if (!normalized.startsWith("---\n")) return normalized.trim() + "\n"
  const end = normalized.indexOf("\n---\n", 4)
  if (end === -1) throw new Error("unterminated Markdown frontmatter")
  return normalized.slice(end + 5).trim() + "\n"
}

const markdownForAgents = (source, path) => {
  let body = stripFrontmatter(source)
  if (extname(path) !== ".mdx") return body
  body = body
    .replace(/^import .+$/gm, "")
    .replace(/^<ProductHero\s*\/>$/gm, "")
    .replace(/^<StableTargets\s*\/>$/gm, stableTargetsMarkdown)
    .replace(/^<CardGrid>$/gm, "")
    .replace(/^<\/CardGrid>$/gm, "")
    .replace(/^[ \t]*<Card title="([^"]+)"[^>]*>[ \t]*$/gm, "### $1")
    .replace(/^[ \t]*<\/Card>[ \t]*$/gm, "")
    .replace(/^<div class="feature-ledger">$/gm, "")
    .replace(/^[ \t]*<div><strong>(.*?)<\/strong><span>(.*?)<\/span><\/div>[ \t]*$/gm, "- **$1:** $2")
    .replace(/^<\/div>$/gm, "")
    .replace(/^ {4}/gm, "")
  return body.replace(/\n{3,}/g, "\n\n").trim() + "\n"
}

const scalar = (source, key) => {
  const match = new RegExp(`^${key}:\\s*(.+)$`, "m").exec(source)
  if (!match) throw new Error(`missing ${key} frontmatter`)
  return match[1].trim().replace(/^['"]|['"]$/g, "")
}

const slugFor = (path) => {
  const fromRoot = relative(contentRoot, path).split(sep).join("/")
  const withoutExtension = fromRoot.slice(0, -extname(fromRoot).length)
  return withoutExtension.endsWith("/index") ? withoutExtension.slice(0, -6) : withoutExtension
}

const sectionFor = (slug) => slug === "contributing" || slug.startsWith("contributing/") ? "contributing" : "product"

const rawSlugFor = (slug) => {
  if (slug === "index") return "overview"
  if (slug === "docs") return "documentation"
  if (slug === "contributing") return "overview"
  return slug.replace(/^(docs|contributing)\/?/, "")
}

await Promise.all([
  rm(join(publicRoot, "generated"), { recursive: true, force: true }),
  rm(join(publicRoot, "raw"), { recursive: true, force: true }),
])

await Promise.all([
  mkdir(join(publicRoot, "generated/plugin"), { recursive: true }),
  mkdir(join(publicRoot, "generated/client"), { recursive: true }),
  mkdir(join(publicRoot, "generated/session"), { recursive: true }),
])
await mkdir(generatedSourceRoot, { recursive: true })
await writeFile(
  join(generatedSourceRoot, "product.mjs"),
  `export const stableTag = ${JSON.stringify(stableTag)}\nexport const stableTargets = ${JSON.stringify(stableTargets)}\n`,
)
await Promise.all([
  cp(join(repoRoot, "docs/assets/rottweiler-logo.png"), join(publicRoot, "rottweiler-logo.png")),
  cp(join(repoRoot, "docs/assets/rottweiler-hero.png"), join(publicRoot, "rottweiler-hero.png")),
  cp(join(repoRoot, "packages/plugin-sdk/fixtures/wire/protocol-3.schema.json"), join(publicRoot, "generated/plugin/schema.json")),
  cp(join(repoRoot, "packages/plugin-sdk/fixtures/wire/protocol-3.json"), join(publicRoot, "generated/plugin/wire-example.json")),
  cp(join(repoRoot, "protocol/schema"), join(publicRoot, "generated/client"), { recursive: true }),
  cp(join(repoRoot, "protocol/types.ts"), join(publicRoot, "generated/client/types.ts")),
  cp(join(repoRoot, "protocol/session-event-envelope.schema.json"), join(publicRoot, "generated/session/event-envelope.schema.json")),
])

const pages = []
for (const path of await walk(contentRoot)) {
  if (![".md", ".mdx"].includes(extname(path))) continue
  const source = projectTokens(await readFile(path, "utf8"))
  const slug = slugFor(path)
  const owners = referenceSources[slug]
  if (!owners) throw new Error(`documentation page ${slug} has no source-owner declaration`)
  for (const owner of owners) await readFile(join(repoRoot, owner))
  const section = sectionFor(slug)
  const rawSlug = rawSlugFor(slug)
  const rawPath = `raw/${section}/${rawSlug}.md`
  await mkdir(dirname(join(publicRoot, rawPath)), { recursive: true })
  const body = markdownForAgents(source, path)
  await writeFile(join(publicRoot, rawPath), body)
  pages.push({
    title: scalar(source, "title"),
    description: scalar(source, "description"),
    section,
    url: `${baseUrl}/${slug === "index" ? "" : `${slug}/`}`,
    raw_url: `${baseUrl}/${rawPath}`,
    source_owners: owners,
    body,
  })
}

pages.sort((left, right) => left.url.localeCompare(right.url))
const index = {
  schema_version: 1,
  product: "Rottweiler",
  documentation_sections: ["product", "contributing"],
  pages: pages.map(({ body: _body, ...page }) => page),
}

const llms = [
  "# Rottweiler",
  "",
  "> A fast, local coding-agent harness with a Rust engine, durable sessions, explicit permissions, and open extension protocols.",
  "",
  "This is the complete product documentation. Prefer raw Markdown links when loading context.",
  "",
  ...["product", "contributing"].flatMap((section) => [
    `## ${section === "product" ? "Product documentation" : "Contributing"}`,
    "",
    ...pages.filter((page) => page.section === section).flatMap((page) => [`- [${page.title}](${page.raw_url}): ${page.description}`]),
    "",
  ]),
  "## Machine-readable artifacts",
  "",
  `- [Documentation index](${baseUrl}/docs-index.json): Versioned page catalog with source owners.`,
  `- [Plugin API schema](${baseUrl}/generated/plugin/schema.json): Canonical plugin contract.`,
  `- [Client command schema](${baseUrl}/generated/client/client-command.schema.json): Canonical Client API command schema.`,
  `- [Session envelope schema](${baseUrl}/generated/session/event-envelope.schema.json): Durable session-event envelope.`,
  "",
].join("\n")

const llmsFull = pages.map((page) => [
  `# ${page.title}`,
  `Section: ${page.section}`,
  `Canonical URL: ${page.url}`,
  `Source owners: ${page.source_owners.join(", ")}`,
  "",
  page.body.trim(),
  "",
].join("\n")).join("\n---\n\n")

await Promise.all([
  writeFile(join(publicRoot, "docs-index.json"), JSON.stringify(index, null, 2) + "\n"),
  writeFile(join(publicRoot, "llms.txt"), llms),
  writeFile(join(publicRoot, "llms-full.txt"), llmsFull),
])
