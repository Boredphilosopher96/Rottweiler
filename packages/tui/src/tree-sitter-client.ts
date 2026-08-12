import type { TreeSitterClient } from "@opentui/core"

const stabilizedClients = new WeakSet<TreeSitterClient>()

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
