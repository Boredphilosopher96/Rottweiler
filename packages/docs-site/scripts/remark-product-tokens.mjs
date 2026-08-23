import { stableTag, stableVersion } from "../src/data/product.mjs"
import { dirname, extname, relative, resolve, sep } from "node:path"
import { fileURLToPath } from "node:url"

const contentRoot = fileURLToPath(new URL("../src/content/docs", import.meta.url))
const siteBase = "/Rottweiler"

const replacements = Object.freeze({
  "{{stable_tag}}": stableTag,
  "{{stable_version}}": stableVersion,
})

const project = (value) => {
  let projected = value
  for (const [token, replacement] of Object.entries(replacements)) {
    projected = projected.replaceAll(token, replacement)
  }
  return projected
}

const routeFor = (url, filePath) => {
  const [path, hash = ""] = url.split("#", 2)
  if (!filePath || path.startsWith("/") || /^[a-z][a-z0-9+.-]*:/i.test(path) || !/\.mdx?$/.test(path)) return url
  const target = resolve(dirname(filePath), path)
  const fromContent = relative(contentRoot, target)
  if (!fromContent || fromContent.startsWith(`..${sep}`) || fromContent === "..") return url
  const withoutExtension = fromContent.slice(0, -extname(fromContent).length).split(sep).join("/")
  const slug = withoutExtension.endsWith("/index") ? withoutExtension.slice(0, -6) : withoutExtension
  return `${siteBase}/${slug ? `${slug}/` : ""}${hash ? `#${hash}` : ""}`
}

const visit = (node, filePath) => {
  if (!node || typeof node !== "object") return
  if (node.type === "link" && typeof node.url === "string") node.url = routeFor(node.url, filePath)
  for (const [key, value] of Object.entries(node)) {
    if (typeof value === "string") node[key] = project(value)
    else if (Array.isArray(value)) value.forEach((item) => visit(item, filePath))
    else visit(value, filePath)
  }
}

export default function remarkProductTokens() {
  return (tree, file) => visit(tree, file.path)
}
