import { mkdir, readFile, rm, writeFile } from "node:fs/promises"
import { join, resolve } from "node:path"

const root = resolve(import.meta.dir, "../..")
const source = join(root, "packages/plugin-sdk/PROTOCOL.md")
const schema = join(root, "packages/plugin-sdk/fixtures/wire/protocol-1.schema.json")
const fixture = join(root, "packages/plugin-sdk/fixtures/wire/protocol-1.json")
const output = join(import.meta.dir, "dist")

const escapeHtml = (value: string): string =>
  value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")

const slug = (value: string): string =>
  value
    .toLowerCase()
    .replaceAll(/[^a-z0-9]+/g, "-")
    .replaceAll(/^-|-$/g, "")

const inline = (value: string): string => {
  const escaped = escapeHtml(value)
  return escaped.replaceAll(/`([^`]+)`/g, "<code>$1</code>")
}

type RenderedMarkdown = {
  readonly body: string
  readonly navigation: string
}

export const renderMarkdown = (markdown: string): RenderedMarkdown => {
  const body: string[] = []
  const navigation: string[] = []
  const paragraph: string[] = []
  const list: string[] = []
  let code: string[] | null = null

  const flushParagraph = (): void => {
    if (paragraph.length > 0) {
      body.push(`<p>${inline(paragraph.join(" "))}</p>`)
      paragraph.length = 0
    }
  }
  const flushList = (): void => {
    if (list.length > 0) {
      body.push(`<ul>${list.map((item) => `<li>${inline(item)}</li>`).join("")}</ul>`)
      list.length = 0
    }
  }

  for (const line of markdown.replaceAll("\r\n", "\n").split("\n")) {
    if (line.startsWith("```")) {
      flushParagraph()
      flushList()
      if (code === null) {
        code = []
      } else {
        body.push(`<pre><code>${escapeHtml(code.join("\n"))}</code></pre>`)
        code = null
      }
      continue
    }
    if (code !== null) {
      code.push(line)
      continue
    }
    const heading = /^(#{1,3})\s+(.+)$/.exec(line)
    if (heading !== null) {
      flushParagraph()
      flushList()
      const level = heading[1]?.length ?? 2
      const text = heading[2] ?? ""
      const id = slug(text)
      body.push(`<h${level} id="${id}">${inline(text)}</h${level}>`)
      if (level >= 2) {
        navigation.push(`<a href="#${id}">${escapeHtml(text)}</a>`)
      }
      continue
    }
    if (line.startsWith("- ")) {
      flushParagraph()
      list.push(line.slice(2))
      continue
    }
    if (line.trim().length === 0) {
      flushParagraph()
      flushList()
      continue
    }
    paragraph.push(line.trim())
  }
  flushParagraph()
  flushList()
  if (code !== null) {
    throw new Error("unterminated protocol code fence")
  }
  return { body: body.join("\n"), navigation: navigation.join("\n") }
}

const styles = `
:root { color-scheme: dark; --ink:#eef2ff; --muted:#9da7be; --line:#283249; --panel:#111827; --accent:#6ee7b7; --hot:#fbbf24; }
* { box-sizing:border-box; }
html { scroll-behavior:smooth; background:#080b12; }
body { margin:0; color:var(--ink); background:radial-gradient(circle at 78% -10%,#173c35 0,transparent 34rem),#080b12; font:16px/1.65 ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; }
a { color:inherit; }
.shell { display:grid; grid-template-columns:17rem minmax(0,1fr); max-width:92rem; margin:auto; min-height:100vh; }
aside { position:sticky; top:0; height:100vh; padding:2rem 1.4rem; border-right:1px solid var(--line); background:rgba(8,11,18,.86); backdrop-filter:blur(16px); }
.brand { display:flex; gap:.75rem; align-items:center; font-weight:800; letter-spacing:-.02em; }
.mark { display:grid; place-items:center; width:2rem; height:2rem; border:1px solid #3d8f78; border-radius:.65rem; color:#07110e; background:var(--accent); font:900 .8rem/1 ui-monospace,monospace; }
.eyebrow { margin-top:1.6rem; color:var(--accent); font:700 .72rem/1.2 ui-monospace,monospace; text-transform:uppercase; letter-spacing:.14em; }
nav { display:grid; gap:.18rem; margin-top:.7rem; }
nav a { padding:.42rem .65rem; border-radius:.5rem; color:var(--muted); text-decoration:none; font-size:.88rem; }
nav a:hover,nav a:focus-visible { color:var(--ink); background:#172033; outline:none; }
.links { display:grid; gap:.5rem; margin-top:1.4rem; }
.links a { border:1px solid var(--line); border-radius:.55rem; padding:.55rem .7rem; text-decoration:none; font:600 .78rem/1.2 ui-monospace,monospace; }
main { width:min(100%,76rem); padding:4.5rem clamp(1.4rem,5vw,6.5rem) 8rem; }
.hero { padding-bottom:3rem; border-bottom:1px solid var(--line); }
.pill { display:inline-flex; gap:.45rem; align-items:center; padding:.35rem .65rem; border:1px solid #285e50; border-radius:99px; color:var(--accent); font:700 .75rem/1 ui-monospace,monospace; }
h1 { max-width:13ch; margin:.9rem 0 1rem; font-size:clamp(2.8rem,7vw,6rem); line-height:.92; letter-spacing:-.065em; }
.lede { max-width:45rem; color:#c5cde0; font-size:1.15rem; }
.facts { display:grid; grid-template-columns:repeat(3,minmax(0,1fr)); gap:.8rem; margin-top:2rem; }
.fact { padding:1rem; border:1px solid var(--line); border-radius:.8rem; background:rgba(17,24,39,.76); }
.fact b { display:block; color:var(--accent); font:800 1.15rem/1 ui-monospace,monospace; }
.fact span { color:var(--muted); font-size:.8rem; }
article { max-width:54rem; padding-top:2.5rem; }
article h1 { display:none; }
article h2 { margin:3rem 0 .8rem; padding-top:1rem; font-size:1.75rem; letter-spacing:-.035em; }
article h3 { margin:2rem 0 .5rem; font-size:1.15rem; }
article p,article li { color:#c5cde0; }
article code { padding:.12rem .35rem; border:1px solid #2d3b55; border-radius:.35rem; color:#d9fff2; background:#101827; font:500 .86em/1.4 ui-monospace,SFMono-Regular,Menlo,monospace; }
pre { overflow:auto; padding:1.1rem; border:1px solid var(--line); border-radius:.75rem; background:#060910; }
pre code { padding:0; border:0; background:none; }
.search { width:100%; margin-top:1.2rem; padding:.7rem .8rem; border:1px solid var(--line); border-radius:.55rem; color:var(--ink); background:#0d1320; font:inherit; }
.search:focus { border-color:var(--accent); outline:none; }
.hidden { display:none; }
@media (max-width:800px) { .shell{display:block} aside{position:relative;height:auto;border-right:0;border-bottom:1px solid var(--line)} nav{grid-template-columns:repeat(2,minmax(0,1fr))} main{padding-top:2.5rem}.facts{grid-template-columns:1fr} }
`

const script = `
const input=document.querySelector('#search');
const sections=[...document.querySelectorAll('article h2')].map((heading)=>{const nodes=[heading];let cursor=heading.nextElementSibling;while(cursor&&cursor.tagName!=='H2'){nodes.push(cursor);cursor=cursor.nextElementSibling}return {nodes,text:nodes.map((node)=>node.textContent||'').join(' ').toLowerCase()}});
input?.addEventListener('input',()=>{const query=input.value.trim().toLowerCase();for(const section of sections){const hidden=query!==''&&!section.text.includes(query);for(const node of section.nodes)node.classList.toggle('hidden',hidden)}});
`

export const buildSite = async (): Promise<void> => {
  const [markdown, schemaText, fixtureText] = await Promise.all([
    readFile(source, "utf8"),
    readFile(schema, "utf8"),
    readFile(fixture, "utf8"),
  ])
  JSON.parse(schemaText)
  JSON.parse(fixtureText)
  const rendered = renderMarkdown(markdown)
  const html = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="description" content="Frozen Rottweiler plugin protocol 1 reference"><title>Rottweiler plugin protocol 1</title><style>${styles}</style></head>
<body><div class="shell"><aside><div class="brand"><span class="mark">RW</span><span>Plugin protocol</span></div><input id="search" class="search" type="search" placeholder="Filter reference…" aria-label="Filter protocol reference"><div class="eyebrow">On this page</div><nav>${rendered.navigation}</nav><div class="links"><a href="protocol-1.schema.json">JSON schema ↗</a><a href="protocol-1.json">Wire fixture ↗</a></div></aside><main><header class="hero"><span class="pill">● Frozen · protocol 1</span><h1>Build extensions without hidden doors.</h1><p class="lede">The language-neutral contract for tools, commands, hooks, providers, events, and safe host interaction. Every built-in crosses the same extension boundaries.</p><div class="facts"><div class="fact"><b>JSON-RPC 2.0</b><span>newline-framed stdio transport</span></div><div class="fact"><b>4 MiB</b><span>hard maximum wire value</span></div><div class="fact"><b>5 seconds</b><span>default bounded request deadline</span></div></div></header><article>${rendered.body}</article></main></div><script>${script}</script></body></html>`
  await rm(output, { recursive: true, force: true })
  await mkdir(output, { recursive: true })
  await Promise.all([
    writeFile(join(output, "index.html"), html),
    writeFile(join(output, "protocol-1.schema.json"), schemaText),
    writeFile(join(output, "protocol-1.json"), fixtureText),
  ])
}

if (resolve(process.argv[1] ?? "") === resolve(import.meta.path)) {
  await buildSite()
  console.log(output)
}
