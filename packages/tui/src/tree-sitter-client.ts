import type { FiletypeParserOptions, TreeSitterClient } from "@opentui/core"

const stabilizedClients = new WeakSet<TreeSitterClient>()
const lazyClients = new WeakSet<TreeSitterClient>()

function isExpectedShutdown(error: unknown): boolean {
  return error instanceof Error && error.message === "TreeSitter client destroyed"
}

/**
 * Keep expected parser shutdown from surfacing as a rendering warning while
 * preserving every real highlighting failure for OpenTUI to report.
 */
export function stabilizeTreeSitterClient(client: TreeSitterClient): TreeSitterClient {
  if (stabilizedClients.has(client)) return client

  const highlightOnce = client.highlightOnce.bind(client)
  client.highlightOnce = async (...arguments_: Parameters<TreeSitterClient["highlightOnce"]>) => {
    try {
      return await highlightOnce(...arguments_)
    } catch (error) {
      if (isExpectedShutdown(error)) return {}
      throw error
    }
  }
  stabilizedClients.add(client)
  return client
}

/** Register non-Markdown grammars only when visible content first requests them. */
export function registerTreeSitterParsersLazily(
  client: TreeSitterClient,
  parsers: readonly FiletypeParserOptions[],
): TreeSitterClient {
  if (lazyClients.has(client)) return client
  const byFiletype = new Map<string, FiletypeParserOptions>()
  for (const parser of parsers) {
    byFiletype.set(parser.filetype, parser)
    for (const alias of parser.aliases ?? []) byFiletype.set(alias, parser)
  }
  const registered = new Set<string>()
  const register = (filetype: string): void => {
    const parser = byFiletype.get(filetype)
    if (parser === undefined || registered.has(parser.filetype)) return
    registered.add(parser.filetype)
    client.addFiletypeParser(parser)
  }
  const registerForContent = (content: string, filetype: string): void => {
    register(filetype)
    if (filetype !== "markdown") return
    register("markdown_inline")
    const mapping = byFiletype.get("markdown")?.injectionMapping?.infoStringMap
    if (mapping === undefined) return
    for (const match of content.matchAll(/^ {0,3}(?:```|~~~)([^\s`~]*)/gm)) {
      const language = match[1]
      if (language !== undefined && language.length > 0) register(mapping[language] ?? language)
    }
  }

  const highlightOnce = client.highlightOnce.bind(client)
  client.highlightOnce = async (content, filetype) => {
    registerForContent(content, filetype)
    return await highlightOnce(content, filetype)
  }
  const createBuffer = client.createBuffer.bind(client)
  client.createBuffer = async (id, content, filetype, version, autoInitialize) => {
    registerForContent(content, filetype)
    return await createBuffer(id, content, filetype, version, autoInitialize)
  }
  const resetBuffer = client.resetBuffer.bind(client)
  client.resetBuffer = async (bufferId, version, content) => {
    const filetype = client.getBuffer(bufferId)?.filetype
    if (filetype !== undefined) registerForContent(content, filetype)
    return await resetBuffer(bufferId, version, content)
  }
  const updateBuffer = client.updateBuffer.bind(client)
  client.updateBuffer = async (bufferId, edits, newContent, version) => {
    const filetype = client.getBuffer(bufferId)?.filetype
    if (filetype !== undefined) registerForContent(newContent, filetype)
    await updateBuffer(bufferId, edits, newContent, version)
  }
  lazyClients.add(client)
  return client
}
